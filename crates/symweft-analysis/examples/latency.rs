//! Repeatable synthetic baseline, not a whole-workspace semantic benchmark.
use serde_json::json;
use std::{hint::black_box, time::Instant};
use symweft_analysis::Engine;

fn percentile(values: &mut [u64], fraction: f64) -> u64 {
    values.sort_unstable();
    values[((values.len() - 1) as f64 * fraction).ceil() as usize]
}

fn main() {
    let mut rows = Vec::new();
    for (path, unit) in [
        ("bench.py", "def f_INDEX():\n    service.work(helper())\n\n"),
        ("bench.c", "int f_INDEX(void) { return helper(); }\n"),
        (
            "bench.cpp",
            "int f_INDEX() { return obj.work(helper()); }\n",
        ),
        ("bench.rs", "fn f_INDEX() { service.work(helper()); }\n"),
        ("Bench.java", "void f_INDEX() { service.work(helper()); }\n"),
    ] {
        let body: String = (0..400)
            .map(|i| unit.replace("INDEX", &i.to_string()))
            .collect();
        let source = if path.ends_with("java") {
            format!("class Bench {{\n{body}}}\n")
        } else {
            body
        };
        let mut cold = Vec::new();
        for _ in 0..15 {
            let mut engine = Engine::default();
            cold.push(engine.analyze(path, source.clone()).unwrap().total_micros);
        }
        let mut engine = Engine::default();
        let first = engine.analyze(path, source.clone()).unwrap();
        let symbol_count = engine
            .document(path, first.revision)
            .unwrap()
            .facts
            .symbols
            .len();
        let call_count = engine
            .document(path, first.revision)
            .unwrap()
            .facts
            .calls
            .len();
        let mut revision = first.revision;
        let at = source.find("helper").unwrap();
        let mut edits = Vec::new();
        for i in 0..100 {
            let new_text = if i % 2 == 0 { "target" } else { "helper" };
            let update = engine.edit(path, revision, at, at + 6, new_text).unwrap();
            edits.push(update.total_micros);
            revision = update.revision;
        }
        let mut unchanged = Vec::new();
        for _ in 0..100 {
            unchanged.push(engine.analyze(path, source.clone()).unwrap().total_micros);
        }
        let mut queries = Vec::new();
        for i in 0..10_000 {
            let started = Instant::now();
            black_box(engine.context(path, revision, i % source.len()).unwrap());
            queries.push(started.elapsed().as_nanos() as u64);
        }
        rows.push(json!({
            "path": path, "bytes": source.len(), "symbols": symbol_count, "calls": call_count,
            "cold_p50_us": percentile(&mut cold, 0.5), "cold_p95_us": percentile(&mut cold, 0.95),
            "edit_p50_us": percentile(&mut edits, 0.5), "edit_p95_us": percentile(&mut edits, 0.95),
            "unchanged_p50_us": percentile(&mut unchanged, 0.5),
            "context_p50_ns": percentile(&mut queries, 0.5), "context_p95_ns": percentile(&mut queries, 0.95),
            "cache": engine.stats(),
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"benchmark": "synthetic-current-file-syntax-v1", "results": rows})
        )
        .unwrap()
    );
}
