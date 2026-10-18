//! Explicit streaming bulk command; never runs implicitly on the interactive protocol loop.
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::Path,
    time::{Duration, Instant},
};
use symweft_analysis::{Budget, Engine, SourceLanguage};
use symweft_index::{
    Result, Snapshot,
    store::Database,
    worker::{cache_directory, local_includes},
};
use walkdir::WalkDir;

pub fn run(root: &Path) -> Result<Value> {
    let root = root.canonicalize()?;
    let output = cache_directory(&root).join("index.sqlite3");
    let mut db = Database::open(&output)?;
    let mut engine = Engine::new(Budget {
        max_documents: 1,
        max_document_bytes: 2 * 1024 * 1024,
        max_source_bytes: 2 * 1024 * 1024,
        max_estimated_bytes: 256 * 1024 * 1024,
        max_facts: 200_000,
        max_walk_nodes: 2_000_000,
        work_timeout: Duration::from_secs(10),
    });
    let mut batch = Vec::new();
    let mut batch_bytes = 0;
    let (mut analyzed, mut reused, mut failures) = (0, 0, Vec::new());
    let start = Instant::now();
    let mut seen = HashSet::new();
    let mut complete_walk = true;
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".git" | "target" | "build" | ".venv" | "__pycache__" | "node_modules")
            )
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(error) => {
                complete_walk = false;
                failures.push(json!({"walk_error":error.to_string()}));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(path) = entry.path().strip_prefix(&root)?.to_str() else {
            continue;
        };
        if SourceLanguage::for_path(path).is_none() {
            continue;
        }
        seen.insert(path.to_owned());
        if entry.metadata()?.len() > 2 * 1024 * 1024 {
            failures.push(json!({"path":path,"error":"bulk document limit is 2 MiB"}));
            continue;
        }
        let source = match fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(e) => {
                failures.push(json!({"path":path,"error":e.to_string()}));
                continue;
            }
        };
        let input = symweft_index::hash(source.as_bytes());
        if db
            .get(path)?
            .is_some_and(|s| s.source_hash == input && s.decode().is_ok())
        {
            reused += 1;
            continue;
        }
        match engine.analyze(path, source.clone()) {
            Ok(update) => {
                let mut snapshot = Snapshot::new(
                    path,
                    &source,
                    &engine.document(path, update.revision)?.facts,
                )?;
                snapshot.dependencies = local_includes(&root, path, &source);
                batch_bytes += snapshot.payload.len();
                batch.push(snapshot);
                analyzed += 1;
                engine.close(path);
            }
            Err(error) => {
                failures.push(json!({"path":path,"code":error.code,"error":error.message}))
            }
        }
        if batch.len() >= 32 || batch_bytes >= 4 * 1024 * 1024 {
            db.put_batch(&batch, true)?;
            batch.clear();
            batch_bytes = 0;
        }
    }
    db.put_batch(&batch, true)?;
    let removed: Vec<_> = if complete_walk {
        db.paths()?
            .into_iter()
            .filter(|p| !seen.contains(p))
            .collect()
    } else {
        Vec::new()
    };
    db.remove_paths(&removed)?;
    db.checkpoint()?;
    Ok(
        json!({"database":output,"level":"syntax","analyzed":analyzed,"reused":reused,"removed":removed.len(),"failures":failures,"wall_ms":start.elapsed().as_millis(),"bytes":fs::metadata(&output)?.len()}),
    )
}
