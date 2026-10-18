//! Shared, bounded interactive analysis. Syntax evidence is never a resolved call edge.
mod syntax;

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tree_sitter::{InputEdit, Language, ParseOptions, Parser, Point, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceLanguage {
    Python,
    C,
    Cpp,
    Rust,
    Java,
}

impl SourceLanguage {
    pub fn for_path(path: &str) -> Option<Self> {
        Some(
            match Path::new(path)
                .extension()?
                .to_str()?
                .to_ascii_lowercase()
                .as_str()
            {
                "py" | "pyi" => Self::Python,
                "c" | "h" => Self::C,
                "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Self::Cpp,
                "rs" => Self::Rust,
                "java" => Self::Java,
                _ => return None,
            },
        )
    }
    fn grammar(self) -> Language {
        match self {
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }
}

/// UTF-8 byte offsets, half-open; display lines are one-based.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}
impl SourceRange {
    pub fn contains(&self, offset: usize) -> bool {
        self.start_byte <= offset && offset < self.end_byte
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    /// Local to this document revision; not a persistent semantic identity.
    pub local_id: u32,
    pub name: String,
    pub kind: String,
    pub range: SourceRange,
    pub name_range: SourceRange,
    pub parent_id: Option<u32>,
    pub is_callable: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Syntax,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Unresolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallSite {
    pub callee: String,
    pub kind: String,
    pub range: SourceRange,
    /// Syntactic body containment, NOT a resolved caller/callee relationship.
    pub owner_id: Option<u32>,
    pub evidence: Evidence,
    pub resolution: Resolution,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Facts {
    pub symbols: Vec<Symbol>,
    pub calls: Vec<CallSite>,
    pub has_syntax_errors: bool,
}
impl Facts {
    pub fn estimated_bytes(&self) -> usize {
        syntax::fact_bytes(self)
    }
}

#[derive(Debug, Serialize)]
pub struct DocumentAnalysis {
    pub path: String,
    pub revision: u64,
    pub language: SourceLanguage,
    pub level: &'static str,
    pub facts: Arc<Facts>,
}

#[derive(Debug, Serialize)]
pub struct Context<'a> {
    pub revision: u64,
    pub symbol: Option<&'a Symbol>,
    pub call: Option<&'a CallSite>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Update {
    pub revision: u64,
    pub mode: &'static str,
    pub parse_micros: u64,
    pub extract_micros: u64,
    pub total_micros: u64,
    pub changed_range_count: usize,
}

#[derive(Debug, Clone)]
pub struct Budget {
    pub max_documents: usize,
    pub max_source_bytes: usize,
    pub max_document_bytes: usize,
    /// Conservative accounting estimate; NOT an OS RSS guarantee.
    pub max_estimated_bytes: usize,
    pub max_facts: usize,
    pub max_walk_nodes: usize,
    pub work_timeout: Duration,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            max_documents: 8,
            max_source_bytes: 4 * 1024 * 1024,
            max_document_bytes: 512 * 1024,
            max_estimated_bytes: 64 * 1024 * 1024,
            max_facts: 20_000,
            max_walk_nodes: 200_000,
            work_timeout: Duration::from_millis(200),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub cached_documents: usize,
    pub source_bytes: usize,
    pub estimated_retained_bytes: usize,
    pub parses: u64,
    pub unchanged_hits: u64,
    pub evictions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisError {
    pub code: &'static str,
    pub message: String,
}
impl AnalysisError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for AnalysisError {}

struct CachedDocument {
    source: String,
    tree: Option<Tree>,
    analysis: DocumentAnalysis,
    touched: u64,
    estimated_bytes: usize,
}

pub struct Engine {
    parser: Parser,
    documents: HashMap<String, CachedDocument>,
    budget: Budget,
    clock: u64,
    next_revision: u64,
    stats: Stats,
}
impl Default for Engine {
    fn default() -> Self {
        Self::new(Budget::default())
    }
}

impl Engine {
    pub fn new(budget: Budget) -> Self {
        Self {
            parser: Parser::new(),
            documents: HashMap::new(),
            budget,
            clock: 0,
            next_revision: 0,
            stats: Stats::default(),
        }
    }
    pub fn budget(&self) -> &Budget {
        &self.budget
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The host owns IO and scheduling. This library never scans a workspace or starts an LSP.
    pub fn analyze(&mut self, path: &str, source: String) -> Result<Update, AnalysisError> {
        let started = Instant::now();
        let language = SourceLanguage::for_path(path).ok_or_else(|| {
            AnalysisError::new(
                "unsupported_language",
                "Syntax analysis supports Python, C, C++, Rust and Java.",
            )
        })?;
        self.check_size(source.len())?;
        self.clock += 1;
        if let Some(previous) = self.documents.get_mut(path) {
            previous.touched = self.clock;
            if previous.source == source {
                self.stats.unchanged_hits += 1;
                return Ok(Update {
                    revision: previous.analysis.revision,
                    mode: "unchanged",
                    parse_micros: 0,
                    extract_micros: 0,
                    total_micros: micros(started),
                    changed_range_count: 0,
                });
            }
        }
        let old_tree = self.documents.get(path).and_then(|previous| {
            previous.tree.as_ref().map(|old| {
                let mut tree = old.clone();
                tree.edit(&replacement_edit(&previous.source, &source));
                tree
            })
        });
        self.publish(path, source, language, old_tree, started)
    }

    /// One edit, in coordinates of base_revision. Invalid/stale edits publish nothing.
    pub fn edit(
        &mut self,
        path: &str,
        base_revision: u64,
        start_byte: usize,
        old_end_byte: usize,
        new_text: &str,
    ) -> Result<Update, AnalysisError> {
        let started = Instant::now();
        let previous = self.checked(path, base_revision)?;
        let old_source = &previous.source;
        if start_byte > old_end_byte
            || old_end_byte > old_source.len()
            || !old_source.is_char_boundary(start_byte)
            || !old_source.is_char_boundary(old_end_byte)
        {
            return Err(AnalysisError::new(
                "invalid_edit",
                "Edit offsets must be valid UTF-8 byte boundaries in the base revision.",
            ));
        }
        self.check_size(old_source.len() - (old_end_byte - start_byte) + new_text.len())?;
        if &old_source[start_byte..old_end_byte] == new_text {
            let revision = previous.analysis.revision;
            self.stats.unchanged_hits += 1;
            self.clock += 1;
            self.documents
                .get_mut(path)
                .expect("checked document")
                .touched = self.clock;
            return Ok(Update {
                revision,
                mode: "unchanged",
                parse_micros: 0,
                extract_micros: 0,
                total_micros: micros(started),
                changed_range_count: 0,
            });
        }
        let start_position = point_at(old_source, start_byte);
        let inserted_end = point_at(new_text, new_text.len());
        let new_end_position = Point {
            row: start_position.row + inserted_end.row,
            column: if inserted_end.row == 0 {
                start_position.column + inserted_end.column
            } else {
                inserted_end.column
            },
        };
        let mut tree = previous.tree.clone();
        if let Some(tree) = &mut tree {
            tree.edit(&InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte: start_byte + new_text.len(),
                start_position,
                old_end_position: point_at(old_source, old_end_byte),
                new_end_position,
            });
        }
        let language = previous.analysis.language;
        let mut source = old_source.clone();
        source.replace_range(start_byte..old_end_byte, new_text);
        self.publish(path, source, language, tree, started)
    }

    pub fn document(&self, path: &str, revision: u64) -> Result<&DocumentAnalysis, AnalysisError> {
        Ok(&self.checked(path, revision)?.analysis)
    }

    pub fn source(&self, path: &str, revision: u64) -> Result<&str, AnalysisError> {
        Ok(&self.checked(path, revision)?.source)
    }

    pub fn context(
        &self,
        path: &str,
        revision: u64,
        offset: usize,
    ) -> Result<Context<'_>, AnalysisError> {
        let cached = self.checked(path, revision)?;
        if offset > cached.source.len() || !cached.source.is_char_boundary(offset) {
            return Err(AnalysisError::new(
                "invalid_position",
                "Position must be a UTF-8 byte boundary in the requested revision.",
            ));
        }
        let facts = &cached.analysis.facts;
        Ok(Context {
            revision,
            symbol: facts
                .symbols
                .iter()
                .filter(|s| s.range.contains(offset))
                .min_by_key(|s| s.range.end_byte - s.range.start_byte),
            call: facts
                .calls
                .iter()
                .filter(|c| c.range.contains(offset))
                .min_by_key(|c| c.range.end_byte - c.range.start_byte),
        })
    }

    pub fn close(&mut self, path: &str) -> bool {
        let removed = self.documents.remove(path).is_some();
        self.recount();
        removed
    }

    /// Workspace switches discard facts but never reuse a revision in this process.
    pub fn clear(&mut self) {
        self.documents.clear();
        self.parser.reset();
        self.recount();
    }

    /// Restores verified A1 facts without parsing or allocating an AST.
    pub fn restore(
        &mut self,
        path: &str,
        source: String,
        facts: Arc<Facts>,
    ) -> Result<Update, AnalysisError> {
        let started = Instant::now();
        if source.len() > 2 * 1024 * 1024 || source.len() > self.budget.max_source_bytes {
            return Err(AnalysisError::new(
                "analysis_budget",
                "Restored document exceeds source budget.",
            ));
        }
        let language = SourceLanguage::for_path(path)
            .ok_or_else(|| AnalysisError::new("unsupported_language", "No grammar."))?;
        let estimated_bytes = source.capacity() + facts.estimated_bytes();
        if estimated_bytes > self.budget.max_estimated_bytes {
            return Err(AnalysisError::new(
                "analysis_budget",
                "Restored facts exceed syntax cache budget.",
            ));
        }
        self.next_revision += 1;
        self.clock += 1;
        let revision = self.next_revision;
        self.documents.insert(
            path.into(),
            CachedDocument {
                source,
                tree: None,
                analysis: DocumentAnalysis {
                    path: path.into(),
                    revision,
                    language,
                    level: "syntax",
                    facts,
                },
                touched: self.clock,
                estimated_bytes,
            },
        );
        self.trim(Some(path));
        Ok(Update {
            revision,
            mode: "restored",
            parse_micros: 0,
            extract_micros: 0,
            total_micros: micros(started),
            changed_range_count: 0,
        })
    }

    pub fn set_memory_budget(&mut self, bytes: usize) {
        self.budget.max_estimated_bytes = bytes;
        self.trim(None);
    }

    fn trim(&mut self, keep: Option<&str>) {
        self.recount();
        while self.stats.cached_documents > self.budget.max_documents
            || self.stats.source_bytes > self.budget.max_source_bytes
            || self.stats.estimated_retained_bytes > self.budget.max_estimated_bytes
        {
            let victim = self
                .documents
                .iter()
                .filter(|(key, _)| Some(key.as_str()) != keep)
                .min_by_key(|(_, v)| v.touched)
                .map(|(k, _)| k.clone());
            let Some(victim) = victim else { break };
            self.documents.remove(&victim);
            self.stats.evictions += 1;
            self.recount();
        }
    }

    fn checked(&self, path: &str, revision: u64) -> Result<&CachedDocument, AnalysisError> {
        let cached = self.documents.get(path).ok_or_else(|| {
            AnalysisError::new(
                "analysis_missing",
                "Analyze this document first; it may have been evicted.",
            )
        })?;
        if revision != cached.analysis.revision {
            return Err(AnalysisError::new(
                "stale_revision",
                "Analysis revision changed; request a fresh snapshot.",
            ));
        }
        Ok(cached)
    }

    fn check_size(&self, bytes: usize) -> Result<(), AnalysisError> {
        if bytes > self.budget.max_document_bytes
            || bytes > self.budget.max_source_bytes
            || self.budget.max_documents == 0
        {
            return Err(AnalysisError::new(
                "analysis_budget",
                "Document exceeds the interactive syntax budget; text reading remains available.",
            ));
        }
        Ok(())
    }

    fn publish(
        &mut self,
        path: &str,
        source: String,
        language: SourceLanguage,
        old_tree: Option<Tree>,
        started: Instant,
    ) -> Result<Update, AnalysisError> {
        let deadline = started + self.budget.work_timeout;
        self.parser.reset();
        self.parser
            .set_language(&language.grammar())
            .map_err(|e| AnalysisError::new("grammar_error", e.to_string()))?;
        let parse_started = Instant::now();
        let mut cancelled = |_: &tree_sitter::ParseState| Instant::now() >= deadline;
        let tree = self.parser.parse_with_options(
            &mut |offset, _| source.as_bytes().get(offset..).unwrap_or_default(),
            old_tree.as_ref(),
            Some(ParseOptions::new().progress_callback(&mut cancelled)),
        );
        let parse_micros = micros(parse_started);
        let Some(tree) = tree else {
            self.parser.reset();
            return Err(AnalysisError::new(
                "analysis_timeout",
                "Interactive parse budget exceeded; last successful snapshot is preserved.",
            ));
        };
        let extract_started = Instant::now();
        let (facts, walked) = syntax::extract(&tree, &source, language, &self.budget, deadline)?;
        let extract_micros = micros(extract_started);
        // Tree-sitter does not expose per-tree allocation size. Keep honest estimated accounting.
        let estimated_bytes = source.capacity() + walked * 96 + syntax::fact_bytes(&facts);
        if estimated_bytes > self.budget.max_estimated_bytes {
            return Err(AnalysisError::new(
                "analysis_budget",
                "Document exceeds the retained syntax cache estimate.",
            ));
        }
        let changed_range_count = old_tree
            .as_ref()
            .map_or(0, |old| old.changed_ranges(&tree).count());
        self.next_revision += 1;
        let update = Update {
            revision: self.next_revision,
            mode: if old_tree.is_some() {
                "incremental"
            } else {
                "cold"
            },
            parse_micros,
            extract_micros,
            total_micros: 0,
            changed_range_count,
        };
        self.clock += 1;
        self.documents.insert(
            path.to_owned(),
            CachedDocument {
                source,
                tree: Some(tree),
                analysis: DocumentAnalysis {
                    path: path.to_owned(),
                    revision: update.revision,
                    language,
                    level: "syntax",
                    facts: Arc::new(facts),
                },
                touched: self.clock,
                estimated_bytes,
            },
        );
        self.stats.parses += 1;
        self.recount();
        while self.stats.cached_documents > self.budget.max_documents
            || self.stats.source_bytes > self.budget.max_source_bytes
            || self.stats.estimated_retained_bytes > self.budget.max_estimated_bytes
        {
            let victim = self
                .documents
                .iter()
                .filter(|(key, _)| key.as_str() != path)
                .min_by_key(|(_, value)| value.touched)
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else { break };
            self.documents.remove(&victim);
            self.stats.evictions += 1;
            self.recount();
        }
        Ok(Update {
            total_micros: micros(started),
            ..update
        })
    }

    fn recount(&mut self) {
        self.stats.cached_documents = self.documents.len();
        self.stats.source_bytes = self.documents.values().map(|d| d.source.len()).sum();
        self.stats.estimated_retained_bytes =
            self.documents.values().map(|d| d.estimated_bytes).sum();
    }
}

fn micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u64::MAX as u128) as u64
}

fn point_at(source: &str, offset: usize) -> Point {
    let prefix = &source.as_bytes()[..offset];
    Point {
        row: prefix.iter().filter(|&&b| b == b'\n').count(),
        column: prefix
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(offset, |newline| offset - newline - 1),
    }
}

fn replacement_edit(old: &str, new: &str) -> InputEdit {
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let mut suffix = old.as_bytes()[start..]
        .iter()
        .rev()
        .zip(new.as_bytes()[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_at(old, start),
        old_end_position: point_at(old, old_end),
        new_end_position: point_at(new, new_end),
    }
}
