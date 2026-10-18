use serde::Deserialize;
mod indexer;
mod semantic;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
};
use symweft_analysis::{AnalysisError, Engine};
use symweft_core::{CoreError, MAX_DOCUMENT_BYTES, MAX_ENTRIES, PROTOCOL_VERSION, Workspace};
use symweft_index::{
    Snapshot,
    cache::Settings,
    worker::{Worker, cache_directory, local_includes},
};

const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct Session {
    workspace: Option<Workspace>,
    engine: Engine,
    index: Option<Worker>,
    root: Option<PathBuf>,
    settings: Settings,
    semantic: Option<symweft_semantic::service::Controller>,
    semantic_dirty: std::collections::HashSet<String>,
    opened_hash: Option<(String, [u8; 32])>,
}

fn index_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::new("index_error", error.to_string())
}

fn analysis_error(error: AnalysisError) -> CoreError {
    CoreError::new(error.code, error.message)
}

fn number(params: &Value, name: &str) -> Result<u64, CoreError> {
    params.get(name).and_then(Value::as_u64).ok_or_else(|| {
        CoreError::new(
            "invalid_params",
            format!("Missing unsigned integer: {name}"),
        )
    })
}

fn offset(params: &Value, name: &str) -> Result<usize, CoreError> {
    usize::try_from(number(params, name)?)
        .map_err(|_| CoreError::new("invalid_params", format!("Offset too large: {name}")))
}

#[derive(Deserialize)]
struct Request {
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}

fn parameter<'a>(params: &'a Value, name: &str) -> Result<&'a str, CoreError> {
    params.get(name).and_then(Value::as_str).ok_or_else(|| {
        CoreError::new(
            "invalid_params",
            format!("Missing string parameter: {name}"),
        )
    })
}

fn dispatch(request: &Request, session: &mut Session) -> Result<Value, CoreError> {
    match request.method.as_str() {
        "initialize" => Ok(json!({
            "name": "symweft-core", "version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "capabilities": ["workspace.open", "document.read", "document.open", "document.edit", "document.close", "analysis.get", "analysis.at", "analysis.stats", "cache.configure", "cache.status", "cache.pressure", "usage.record", "usage.clear", "heat.query", "index.flush", "semantic.configure", "semantic.index", "semantic.status", "semantic.progress", "semantic.cancel", "semantic.symbols", "semantic.at", "semantic.relations", "semantic.invalidate", "semantic.preview", "semantic.preview.start", "semantic.preview.progress", "semantic.preview.cancel", "semantic.preview.at", "semantic.preview.relations", "semantic.project.at", "semantic.project.relations", "semantic.watch", "semantic.refresh"],
            "limits": {"max_entries": MAX_ENTRIES, "max_document_bytes": MAX_DOCUMENT_BYTES,
                "max_request_bytes": MAX_REQUEST_BYTES, "max_analysis_document_bytes": session.engine.budget().max_document_bytes,
                "max_hot_documents": session.engine.budget().max_documents},
            "analysis_level": "syntax_on_demand"
        })),
        "workspace.open" => {
            let next = Workspace::open(Path::new(parameter(&request.params, "root")?))?;
            let snapshot = next.scan()?;
            session.semantic.take();
            session.semantic_dirty.clear();
            session.opened_hash = None;
            session.index.take();
            let root = PathBuf::from(&snapshot.root);
            session.index = match Worker::open(
                cache_directory(&root).join("index.sqlite3"),
                session.settings.clone(),
            ) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    eprintln!("Persistent index unavailable: {error}");
                    None
                }
            };
            session.root = Some(root);
            session.workspace = Some(next);
            session.engine.clear();
            Ok(serde_json::to_value(snapshot).expect("serializable snapshot"))
        }
        "document.read" => {
            let current = session
                .workspace
                .as_ref()
                .ok_or_else(|| CoreError::new("workspace_required", "Open a workspace first."))?;
            Ok(
                serde_json::to_value(current.read_document(parameter(&request.params, "path")?)?)
                    .expect("serializable document"),
            )
        }
        "document.open" => {
            let current = session
                .workspace
                .as_ref()
                .ok_or_else(|| CoreError::new("workspace_required", "Open a workspace first."))?;
            let path = parameter(&request.params, "path")?;
            let document = current.read_document(path)?;
            if let Some(semantic) = session.semantic.as_mut() {
                semantic.invalidate_preview("document opened or reloaded");
            }
            session.semantic_dirty.remove(path);
            let source_hash = symweft_index::hash(document.content.as_bytes());
            session.opened_hash = Some((path.into(), source_hash));
            if let Some(semantic) = session.semantic.as_mut() {
                let _ = semantic.focus(path);
            }
            let mut storage_source = "analysis";
            let mut load_micros = 0;
            let cached =
                session
                    .index
                    .as_ref()
                    .and_then(|index| match index.load(path, source_hash) {
                        Ok(value) => value,
                        Err(error) => {
                            eprintln!("index read fallback: {error}");
                            None
                        }
                    });
            let analysis_result = if let Some(cached) = cached {
                storage_source = cached.source;
                load_micros = cached.load_micros;
                session
                    .engine
                    .restore(path, document.content.clone(), cached.facts)
            } else {
                session.engine.analyze(path, document.content.clone())
            };
            // Read once; unsupported/over-budget analysis never disables text reading.
            match analysis_result {
                Ok(update) => {
                    let analysis = session
                        .engine
                        .document(path, update.revision)
                        .map_err(analysis_error)?;
                    let mut persistence = "reused";
                    if storage_source == "analysis" {
                        persistence = "unavailable";
                        if let (Some(index), Some(root)) = (&session.index, &session.root) {
                            match Snapshot::new(path, &document.content, &analysis.facts).and_then(
                                |snapshot| {
                                    index.store(
                                        snapshot,
                                        local_includes(root, path, &document.content),
                                    )
                                },
                            ) {
                                Ok(()) => persistence = "queued",
                                Err(error) => eprintln!("index queue: {error}"),
                            }
                        }
                    }
                    Ok(
                        json!({"document":document,"analysis":analysis,"update":update,"analysis_error":null,"storage":{"source":storage_source,"load_micros":load_micros,"persistence":persistence}}),
                    )
                }
                Err(error) => Ok(
                    json!({"document": document, "analysis": null, "update": null, "analysis_error": analysis_error(error)}),
                ),
            }
        }
        "document.edit" => {
            let path = parameter(&request.params, "path")?;
            let update = session
                .engine
                .edit(
                    path,
                    number(&request.params, "base_revision")?,
                    offset(&request.params, "start_byte")?,
                    offset(&request.params, "old_end_byte")?,
                    parameter(&request.params, "new_text")?,
                )
                .map_err(analysis_error)?;
            session.semantic_dirty.insert(path.into());
            session.opened_hash = Some((
                path.into(),
                symweft_index::hash(
                    session
                        .engine
                        .source(path, update.revision)
                        .map_err(analysis_error)?
                        .as_bytes(),
                ),
            ));
            if let Some(semantic) = session.semantic.as_mut() {
                semantic
                    .buffer_changed(path)
                    .unwrap_or_else(|error| eprintln!("semantic buffer invalidation: {error}"));
                if let (Ok(full), Ok(store)) = (semantic.absolute_path(path), semantic.store()) {
                    if let Err(error) = store.invalidate(&semantic.database.configuration, &full) {
                        eprintln!("semantic invalidation: {error}");
                    }
                }
            }
            Ok(
                json!({"analysis": session.engine.document(path, update.revision).map_err(analysis_error)?, "update": update}),
            )
        }
        "document.close" => {
            let path = parameter(&request.params, "path")?;
            if let Some(semantic) = session.semantic.as_mut() {
                semantic.invalidate_preview("document closed");
            }
            session.semantic_dirty.remove(path);
            if session
                .opened_hash
                .as_ref()
                .is_some_and(|(opened, _)| opened == path)
            {
                session.opened_hash = None;
            }
            Ok(json!({"closed": session.engine.close(path)}))
        }
        "analysis.get" => Ok(serde_json::to_value(
            session
                .engine
                .document(
                    parameter(&request.params, "path")?,
                    number(&request.params, "revision")?,
                )
                .map_err(analysis_error)?,
        )
        .expect("serializable analysis")),
        "analysis.at" => Ok(serde_json::to_value(
            session
                .engine
                .context(
                    parameter(&request.params, "path")?,
                    number(&request.params, "revision")?,
                    offset(&request.params, "byte_offset")?,
                )
                .map_err(analysis_error)?,
        )
        .expect("serializable context")),
        "analysis.stats" => {
            Ok(serde_json::to_value(session.engine.stats()).expect("serializable stats"))
        }
        "cache.configure" => {
            let settings: Settings =
                serde_json::from_value(request.params.clone()).map_err(index_error)?;
            let index = session.index.as_ref().ok_or_else(|| {
                CoreError::new(
                    "workspace_required",
                    "Open a workspace with an available index.",
                )
            })?;
            let status = index.configure(settings.clone()).map_err(index_error)?;
            session.settings = settings;
            Ok(serde_json::to_value(status).expect("status"))
        }
        "cache.status" => Ok(serde_json::to_value(
            session
                .index
                .as_ref()
                .ok_or_else(|| CoreError::new("workspace_required", "Open a workspace."))?
                .status()
                .map_err(index_error)?,
        )
        .expect("status")),
        "cache.pressure" => {
            let index = session
                .index
                .as_ref()
                .ok_or_else(|| CoreError::new("workspace_required", "Open a workspace."))?;
            let level = parameter(&request.params, "level")?;
            if !matches!(level, "normal" | "warning" | "critical") {
                return Err(CoreError::new("invalid_params", "Invalid memory pressure."));
            }
            if level == "critical" {
                if let Some(semantic) = session.semantic.as_mut() {
                    semantic.pressure(true);
                }
            } else if let Some(semantic) = session.semantic.as_mut() {
                semantic.pressure(false);
            }
            index
                .pressure(
                    level,
                    request
                        .params
                        .get("busy")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    number(&request.params, "now_ms")?,
                )
                .map_err(index_error)?;
            Ok(json!({"accepted":true}))
        }
        "usage.record" => {
            let path = parameter(&request.params, "path")?;
            let revision = number(&request.params, "revision")?;
            let analysis = session
                .engine
                .document(path, revision)
                .map_err(analysis_error)?;
            let local_id = request
                .params
                .get("local_id")
                .and_then(Value::as_u64)
                .and_then(|id| u32::try_from(id).ok());
            if local_id.is_some_and(|id| id as usize >= analysis.facts.symbols.len()) {
                return Err(CoreError::new("invalid_params", "Unknown local symbol."));
            }
            session
                .index
                .as_ref()
                .ok_or_else(|| CoreError::new("index_error", "Index unavailable."))?
                .usage(
                    path,
                    analysis.facts.clone(),
                    local_id,
                    number(&request.params, "active_ms")?,
                    request
                        .params
                        .get("opened")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
                .map_err(index_error)?;
            Ok(json!({"queued":true}))
        }
        "usage.clear" => {
            session
                .index
                .as_ref()
                .ok_or_else(|| CoreError::new("index_error", "Index unavailable."))?
                .clear_usage()
                .map_err(index_error)?;
            Ok(json!({"cleared":true}))
        }
        "heat.query" => {
            let kind = parameter(&request.params, "kind")?;
            let sort = parameter(&request.params, "sort")?;
            if !matches!(kind, "file" | "symbol") || !matches!(sort, "structure" | "attention") {
                return Err(CoreError::new(
                    "invalid_params",
                    "Expected file/symbol and structure/attention.",
                ));
            }
            let rows = session
                .index
                .as_ref()
                .ok_or_else(|| CoreError::new("index_error", "Index unavailable."))?
                .heat(kind, sort, offset(&request.params, "limit")?)
                .map_err(index_error)?;
            Ok(json!({"evidence":"syntax_proxy","rows":rows}))
        }
        "index.flush" => {
            session
                .index
                .as_ref()
                .ok_or_else(|| CoreError::new("index_error", "Index unavailable."))?
                .flush()
                .map_err(index_error)?;
            Ok(json!({"flushed":true}))
        }
        method if method.starts_with("semantic.") => semantic::dispatch(request, session),
        _ => Err(CoreError::new(
            "method_not_found",
            format!("Unsupported method: {}", request.method),
        )),
    }
}

fn response(line: &[u8], session: &mut Session) -> Value {
    match serde_json::from_slice::<Request>(line) {
        Ok(request) => match dispatch(&request, session) {
            Ok(result) => {
                json!({"id": request.id, "protocol_version": PROTOCOL_VERSION, "result": result})
            }
            Err(error) => {
                json!({"id": request.id, "protocol_version": PROTOCOL_VERSION, "error": error})
            }
        },
        Err(_) => json!({"id": null, "protocol_version": PROTOCOL_VERSION,
            "error": {"code": "invalid_request", "message": "Expected a JSON request with id, method and params."}}),
    }
}

fn serve() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    let mut session = Session::default();
    loop {
        let mut line = Vec::new();
        let count = (&mut input)
            .take(MAX_REQUEST_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        let result = if count > MAX_REQUEST_BYTES {
            json!({"id": null, "protocol_version": PROTOCOL_VERSION,
                "error": {"code": "request_too_large", "message": "Request limit is 4 MiB; closing connection."}})
        } else {
            response(&line, &mut session)
        };
        serde_json::to_writer(&mut output, &result)?;
        output.write_all(b"\n")?;
        output.flush()?;
        if count > MAX_REQUEST_BYTES {
            break;
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result: Result<(), Box<dyn std::error::Error>> = match args.get(1).map(String::as_str) {
        Some("serve") => serve().map_err(Into::into),
        Some("semantic-worker") => {
            symweft_semantic::service::worker_main().map_err(|e| e as Box<dyn std::error::Error>)
        }
        Some("semantic-index") if args.len() >= 4 => {
            semantic::index_cli(&args[2..]).map_err(|e| e as Box<dyn std::error::Error>)
        }
        Some("scan") if args.len() == 3 => Workspace::open(Path::new(&args[2]))
            .and_then(|w| w.scan())
            .map_err(Into::into)
            .and_then(|snapshot| {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
                Ok(())
            }),
        Some("index") if args.len() == 3 => indexer::run(Path::new(&args[2]))
            .map(|value| {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).expect("index report")
                )
            })
            .map_err(|e| e as Box<dyn std::error::Error>),
        Some("--version") => {
            println!(
                "symweft-core {} (protocol {})",
                env!("CARGO_PKG_VERSION"),
                PROTOCOL_VERSION
            );
            Ok(())
        }
        _ => {
            eprintln!(
                "Usage: symweft-core serve | scan <directory> | index <directory> | semantic-index <root> <compile_commands.json> [paths...] [--limit N] | --version"
            );
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_reports_errors_and_preserves_correlation() {
        let mut workspace = Session::default();
        assert_eq!(
            response(b"not json", &mut workspace)["error"]["code"],
            "invalid_request"
        );
        let result = response(
            br#"{"id":9,"method":"document.read","params":{"path":"a.rs"}}"#,
            &mut workspace,
        );
        assert_eq!(result["id"], 9);
        assert_eq!(result["error"]["code"], "workspace_required");
        assert_eq!(
            response(br#"{"id":10,"method":"missing"}"#, &mut workspace)["error"]["code"],
            "method_not_found"
        );
    }
}
