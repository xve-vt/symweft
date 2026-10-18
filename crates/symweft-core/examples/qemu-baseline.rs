//! Original interactive engine over every supported source file, before optimization.
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path, time::Instant};
use symweft_analysis::{Engine, SourceLanguage};
use walkdir::WalkDir;

fn percentile(values: &mut [u64], q: f64) -> u64 {
    values.sort_unstable();
    if values.is_empty() {
        return 0;
    }
    values[((values.len() as f64 * q).ceil() as usize)
        .saturating_sub(1)
        .min(values.len() - 1)]
}
fn main() {
    let root_arg = std::env::args().nth(1).expect("source root");
    let root = Path::new(&root_arg).canonicalize().unwrap();
    let started = Instant::now();
    let mut files: Vec<_> = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".git" | "target" | "build" | ".venv" | "__pycache__")
            )
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| SourceLanguage::for_path(e.path().to_str().unwrap_or("")).is_some())
        .map(|e| e.into_path())
        .collect();
    files.sort();
    let mut engine = Engine::default();
    let (mut bytes, mut lines, mut symbols, mut calls, mut success, mut syntax_errors) =
        (0, 0, 0, 0, 0, 0);
    let (mut peak_estimate, mut peak_source, mut peak_docs) = (0, 0, 0);
    let (mut parse_us, mut extract_us) = (0u64, 0u64);
    let mut failures = Vec::new();
    let mut failure_codes = BTreeMap::<String, usize>::new();
    let mut durations = Vec::new();
    for (i, path) in files.iter().enumerate() {
        let relative = path.strip_prefix(&root).unwrap().to_string_lossy();
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                failures.push(json!({"path":relative,"error":e.to_string()}));
                continue;
            }
        };
        bytes += source.len();
        lines += source.bytes().filter(|&b| b == b'\n').count() + 1;
        match engine.analyze(&relative, source) {
            Ok(update) => {
                success += 1;
                parse_us += update.parse_micros;
                extract_us += update.extract_micros;
                durations.push(update.total_micros);
                let facts = &engine.document(&relative, update.revision).unwrap().facts;
                symbols += facts.symbols.len();
                calls += facts.calls.len();
                syntax_errors += usize::from(facts.has_syntax_errors);
            }
            Err(error) => {
                *failure_codes.entry(error.code.to_owned()).or_default() += 1;
                failures.push(json!({"path":relative,"code":error.code,"message":error.message}));
            }
        }
        peak_estimate = peak_estimate.max(engine.stats().estimated_retained_bytes);
        peak_source = peak_source.max(engine.stats().source_bytes);
        peak_docs = peak_docs.max(engine.stats().cached_documents);
        if i % 1000 == 0 {
            eprintln!("baseline {}/{}", i + 1, files.len());
        }
    }
    println!("{}",serde_json::to_string_pretty(&json!({
        "benchmark":"qemu-original-interactive-v1","root":root,"level":"A1 syntax only",
        "supported_files":files.len(),"source_bytes":bytes,"source_lines":lines,"analyzed_files":success,
        "symbols":symbols,"explicit_call_sites":calls,"files_with_syntax_errors":syntax_errors,
        "wall_ms":started.elapsed().as_millis(),"parse_us":parse_us,"extract_us":extract_us,
        "file_p50_us":percentile(&mut durations,0.5),"file_p95_us":percentile(&mut durations,0.95),
        "file_max_us":durations.last(),"peak_accounted_hot_bytes":peak_estimate,"peak_source_bytes":peak_source,"peak_hot_documents":peak_docs,
        "end_cache":engine.stats(),"failure_codes":failure_codes,"failures":failures,
        "persistent_index_bytes":null,"read_latency":null
    })).unwrap());
}
