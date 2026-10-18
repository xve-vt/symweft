use crate::{
    AnalysisError, Budget, CallSite, Evidence, Facts, Resolution, SourceLanguage, SourceRange,
    Symbol,
};
use std::{mem::size_of, time::Instant};
use tree_sitter::{Node, Tree};

fn range(node: Node<'_>) -> SourceRange {
    SourceRange {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

fn spelling(node: Node<'_>, source: &str) -> String {
    let text = &source[node.byte_range()];
    // An adversarial expression must not be copied into every fact unboundedly.
    if text.len() <= 256 {
        return text.to_owned();
    }
    let mut end = 253;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn declarator_name(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..64 {
        match node.kind() {
            "identifier"
            | "field_identifier"
            | "qualified_identifier"
            | "destructor_name"
            | "operator_name"
            | "type_identifier" => return Some(node),
            _ => {
                node = node
                    .child_by_field_name("declarator")
                    .or_else(|| node.named_child(0))?
            }
        }
    }
    None
}

/// kind, name node, callable flag. Definitions only, not a complete symbol table.
fn definition(
    node: Node<'_>,
    language: SourceLanguage,
) -> Option<(&'static str, Option<Node<'_>>, bool)> {
    let kind = node.kind();
    let name = node.child_by_field_name("name");
    Some(match language {
        SourceLanguage::Python => match kind {
            "function_definition" => ("function", name, true),
            "class_definition" => ("class", name, false),
            "lambda" => ("closure", None, true),
            _ => return None,
        },
        SourceLanguage::Rust => match kind {
            "function_item" | "function_signature_item" => ("function", name, true),
            "struct_item" => ("struct", name, false),
            "enum_item" => ("enum", name, false),
            "trait_item" => ("trait", name, false),
            "mod_item" => ("module", name, false),
            "impl_item" => ("impl", node.child_by_field_name("type"), false),
            "closure_expression" => ("closure", None, true),
            _ => return None,
        },
        SourceLanguage::C | SourceLanguage::Cpp => match kind {
            "function_definition" => (
                "function",
                node.child_by_field_name("declarator")
                    .and_then(declarator_name),
                true,
            ),
            "struct_specifier" => ("struct", name, false),
            "class_specifier" => ("class", name, false),
            "enum_specifier" => ("enum", name, false),
            "namespace_definition" => ("namespace", name, false),
            "lambda_expression" => ("closure", None, true),
            _ => return None,
        },
        SourceLanguage::Java => match kind {
            "method_declaration" => ("method", name, true),
            "constructor_declaration" | "compact_constructor_declaration" => {
                ("constructor", name, true)
            }
            "class_declaration" => ("class", name, false),
            "interface_declaration" => ("interface", name, false),
            "enum_declaration" => ("enum", name, false),
            "record_declaration" => ("record", name, false),
            "lambda_expression" => ("closure", None, true),
            _ => return None,
        },
    })
}

fn invocation(
    node: Node<'_>,
    source: &str,
    language: SourceLanguage,
) -> Option<(String, &'static str)> {
    let (callee, kind) = match (language, node.kind()) {
        (SourceLanguage::Python, "call") => (node.child_by_field_name("function")?, "call"),
        (SourceLanguage::Rust | SourceLanguage::C | SourceLanguage::Cpp, "call_expression") => {
            (node.child_by_field_name("function")?, "call")
        }
        (SourceLanguage::Rust, "macro_invocation") => (node.child_by_field_name("macro")?, "macro"),
        (SourceLanguage::Java, "method_invocation") => {
            let name = node.child_by_field_name("name")?;
            if let Some(object) = node.child_by_field_name("object") {
                // Preserve the receiver as source evidence, not a type-resolved target.
                let text = &source[object.start_byte()..name.end_byte()];
                if text.len() <= 256 {
                    return Some((text.to_owned(), "call"));
                }
            }
            (name, "call")
        }
        (SourceLanguage::Java, "object_creation_expression") => {
            (node.child_by_field_name("type")?, "constructor")
        }
        _ => return None,
    };
    Some((spelling(callee, source), kind))
}

struct Scope {
    node_id: usize,
    id: u32,
    body: Option<SourceRange>,
}

/// One iterative cursor walk; no retained Node handles, recursive Rust stack or query compilation.
pub(crate) fn extract(
    tree: &Tree,
    source: &str,
    language: SourceLanguage,
    budget: &Budget,
    deadline: Instant,
) -> Result<(Facts, usize), AnalysisError> {
    let mut facts = Facts {
        symbols: Vec::new(),
        calls: Vec::new(),
        has_syntax_errors: tree.root_node().has_error(),
    };
    let mut cursor = tree.walk();
    let mut scopes: Vec<Scope> = Vec::new();
    let mut walked = 0;
    loop {
        let node = cursor.node();
        walked += 1;
        if walked > budget.max_walk_nodes || (walked & 255 == 0 && Instant::now() >= deadline) {
            return Err(AnalysisError::new(
                "analysis_budget",
                "Syntax traversal exceeded its node/time budget; no partial snapshot published.",
            ));
        }
        if node.is_named() {
            if let Some((kind, name, callable)) = definition(node, language) {
                // Anonymous types are omitted; anonymous executable scopes get an explicit label.
                if name.is_some() || callable {
                    let local_id = facts.symbols.len() as u32;
                    let name_range = range(name.unwrap_or(node));
                    facts.symbols.push(Symbol {
                        local_id,
                        name: name.map_or_else(
                            || format!("<{}:{}>", kind, node.start_position().row + 1),
                            |name| spelling(name, source),
                        ),
                        kind: kind.to_owned(),
                        range: range(node),
                        name_range,
                        parent_id: scopes.last().map(|s| s.id),
                        is_callable: callable,
                    });
                    scopes.push(Scope {
                        node_id: node.id(),
                        id: local_id,
                        body: if callable {
                            node.child_by_field_name("body").map(range)
                        } else {
                            None
                        },
                    });
                }
            }
            if let Some((callee, kind)) = invocation(node, source, language) {
                let owner_id = scopes
                    .iter()
                    .rev()
                    .find(|s| {
                        s.body
                            .as_ref()
                            .is_some_and(|body| body.contains(node.start_byte()))
                    })
                    .map(|s| s.id);
                facts.calls.push(CallSite {
                    callee,
                    kind: kind.to_owned(),
                    range: range(node),
                    owner_id,
                    evidence: Evidence::Syntax,
                    resolution: Resolution::Unresolved,
                });
            }
        }
        if facts.symbols.len() + facts.calls.len() > budget.max_facts {
            return Err(AnalysisError::new(
                "analysis_budget",
                "Document fact limit exceeded; no partial snapshot published.",
            ));
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if scopes
                .last()
                .is_some_and(|s| s.node_id == cursor.node().id())
            {
                scopes.pop();
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                if Instant::now() >= deadline {
                    return Err(AnalysisError::new(
                        "analysis_timeout",
                        "Fact extraction deadline exceeded.",
                    ));
                }
                return Ok((facts, walked));
            }
        }
    }
}

pub(crate) fn fact_bytes(facts: &Facts) -> usize {
    facts.symbols.capacity() * size_of::<Symbol>()
        + facts.calls.capacity() * size_of::<CallSite>()
        + facts
            .symbols
            .iter()
            .map(|s| s.name.capacity() + s.kind.capacity())
            .sum::<usize>()
        + facts
            .calls
            .iter()
            .map(|c| c.callee.capacity() + c.kind.capacity())
            .sum::<usize>()
}
