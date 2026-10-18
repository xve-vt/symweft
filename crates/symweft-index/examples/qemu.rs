//! Real-source A1 + identical-payload backend comparison. No fabricated user attention.
use serde_json::{Value, json};
use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use symweft_analysis::{Budget, Engine, SourceLanguage};
use symweft_index::{
    Result, Snapshot,
    cache::{Cache, Settings},
    hash,
    hybrid::Hybrid,
    pack::Pack,
    store::Database,
    worker::{Worker, local_includes},
};
use walkdir::WalkDir;

fn quantile(v: &mut [u64], q: f64) -> u64 {
    v.sort_unstable();
    if v.is_empty() {
        0
    } else {
        v[((v.len() as f64 * q).ceil() as usize)
            .saturating_sub(1)
            .min(v.len() - 1)]
    }
}
fn timings(v: &mut [u64]) -> Value {
    json!({"p50_us":quantile(v,0.5),"p95_us":quantile(v,0.95),"p99_us":quantile(v,0.99)})
}
fn directory_bytes(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}
fn sources(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".git" | "target" | "build" | ".venv" | "__pycache__")
            )
        })
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && SourceLanguage::for_path(e.path().to_str().unwrap_or("")).is_some()
        })
        .map(|e| e.into_path())
        .collect();
    files.sort();
    files
}

fn analyze(root: &Path, out: &Path) -> Result<Value> {
    if out.join("pack/snapshots.dir").exists() {
        return Err("use a fresh output directory".into());
    }
    let started = Instant::now();
    let files = sources(root);
    let mut pack = Pack::open(&out.join("pack"))?;
    let mut engine = Engine::new(Budget {
        max_documents: 1,
        max_document_bytes: 2 * 1024 * 1024,
        max_source_bytes: 2 * 1024 * 1024,
        max_estimated_bytes: 256 * 1024 * 1024,
        max_facts: 200_000,
        max_walk_nodes: 2_000_000,
        work_timeout: Duration::from_secs(10),
    });
    let (
        mut total_bytes,
        mut total_lines,
        mut symbols,
        mut calls,
        mut syntax_errors,
        mut payload_bytes,
        mut peak_hot,
        mut parse_us,
        mut extract_us,
        mut encode_us,
        mut write_us,
    ) = (
        0u64, 0usize, 0usize, 0usize, 0usize, 0u64, 0usize, 0u64, 0u64, 0u64, 0u64,
    );
    let mut failures = Vec::new();
    let mut durations = Vec::new();
    for (i, path) in files.iter().enumerate() {
        let relative = path.strip_prefix(root)?.to_str().ok_or("path encoding")?;
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                failures.push(json!({"path":relative,"read_error":e.to_string()}));
                continue;
            }
        };
        total_bytes += source.len() as u64;
        total_lines += source.bytes().filter(|&c| c == b'\n').count() + 1;
        match engine.analyze(relative, source.clone()) {
            Ok(update) => {
                parse_us += update.parse_micros;
                extract_us += update.extract_micros;
                durations.push(update.total_micros);
                peak_hot = peak_hot.max(engine.stats().estimated_retained_bytes);
                let facts = &engine.document(relative, update.revision)?.facts;
                symbols += facts.symbols.len();
                calls += facts.calls.len();
                syntax_errors += usize::from(facts.has_syntax_errors);
                let t = Instant::now();
                let mut snapshot = Snapshot::new(relative, &source, facts)?;
                snapshot.dependencies = local_includes(root, relative, &source);
                encode_us += t.elapsed().as_micros() as u64;
                payload_bytes += snapshot.payload.len() as u64;
                let t = Instant::now();
                pack.put(&snapshot)?;
                write_us += t.elapsed().as_micros() as u64;
                engine.close(relative);
            }
            Err(e) => failures.push(json!({"path":relative,"code":e.code,"message":e.message})),
        }
        if i % 1000 == 0 {
            eprintln!("analyze {}/{}", i + 1, files.len());
        }
    }
    let t = Instant::now();
    pack.commit()?;
    write_us += t.elapsed().as_micros() as u64;
    Ok(
        json!({"level":"A1 syntax only","supported_files":files.len(),"analyzed_files":durations.len(),"source_bytes":total_bytes,"source_lines":total_lines,"symbols":symbols,"explicit_call_sites":calls,"files_with_syntax_errors":syntax_errors,"wall_ms":started.elapsed().as_millis(),"parse_us":parse_us,"extract_us":extract_us,"encode_and_dependencies_us":encode_us,"pack_write_commit_us":write_us,"payload_bytes":payload_bytes,"pack_bytes":directory_bytes(&out.join("pack")),"pack_directory_estimated_bytes":pack.directory_estimated_bytes(),"peak_accounted_syntax_bytes":peak_hot,"analysis_latency":timings(&mut durations),"failures":failures}),
    )
}

fn backends(out: &Path) -> Result<Value> {
    let mut pack = Pack::open(&out.join("pack"))?;
    let paths = pack.paths();
    let mut results = Vec::new();
    for secondary in [false, true] {
        let label = if secondary {
            "sqlite-indexed"
        } else {
            "sqlite-raw"
        };
        let dir = out.join(label);
        fs::create_dir_all(&dir)?;
        let mut db = Database::open(&dir.join("index.sqlite3"))?;
        let started = Instant::now();
        let mut batch = Vec::new();
        let (mut read_us, mut write_us) = (0u64, 0u64);
        for path in &paths {
            let t = Instant::now();
            batch.push(pack.get(path)?.ok_or("missing pack")?);
            read_us += t.elapsed().as_micros() as u64;
            if batch.len() == 32 {
                let t = Instant::now();
                db.put_batch(&batch, secondary)?;
                write_us += t.elapsed().as_micros() as u64;
                batch.clear();
            }
        }
        let t = Instant::now();
        db.put_batch(&batch, secondary)?;
        write_us += t.elapsed().as_micros() as u64;
        let pre_checkpoint_bytes = directory_bytes(&dir);
        let t = Instant::now();
        db.checkpoint()?;
        let checkpoint_us = t.elapsed().as_micros();
        let mut heat_us = 0;
        if secondary {
            let t = Instant::now();
            let symbol_heat = db.heat("symbol", "structure", 1000)?;
            let file_heat = db.heat("file", "structure", 1000)?;
            heat_us = t.elapsed().as_micros();
            fs::write(
                out.join("symbol-heat.json"),
                serde_json::to_vec_pretty(&symbol_heat)?,
            )?;
            fs::write(
                out.join("file-heat.json"),
                serde_json::to_vec_pretty(&file_heat)?,
            )?;
        }
        results.push(json!({"backend":label,"sqlite":db.sqlite_version(),"files":paths.len(),"wall_ms":started.elapsed().as_millis(),"spool_read_us":read_us,"put_batch_us":write_us,"checkpoint_us":checkpoint_us,"bytes_before_checkpoint":pre_checkpoint_bytes,"steady_bytes":fs::metadata(dir.join("index.sqlite3"))?.len(),"heat_export_top1000_us":heat_us}));
    }
    Ok(json!({"results":results,"pack_bytes":directory_bytes(&out.join("pack"))}))
}

fn reads(root: &Path, out: &Path) -> Result<Value> {
    let begin = Instant::now();
    let mut pack = Pack::open(&out.join("pack"))?;
    let pack_open_us = begin.elapsed().as_micros();
    let paths = pack.paths();
    let mut state = 42u64;
    let sample: Vec<_> = (0..1500)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            paths[state as usize % paths.len()].clone()
        })
        .collect();
    let mut results = Vec::new();
    for backend in ["sqlite-raw", "pack", "sqlite-indexed"] {
        let mut db = if backend == "pack" {
            None
        } else {
            Some(Database::open(&out.join(backend).join("index.sqlite3"))?)
        };
        let mut raw = Vec::new();
        let mut decode = Vec::new();
        let mut end_to_end = Vec::new();
        let mut bytes = 0u64;
        for path in &sample {
            let t = Instant::now();
            let snapshot = if let Some(db) = &mut db {
                db.get(path)?
            } else {
                pack.get(path)?
            }
            .ok_or("missing snapshot")?;
            raw.push(t.elapsed().as_micros() as u64);
            bytes += snapshot.payload.len() as u64;
            let t = Instant::now();
            black_box(snapshot.decode()?);
            decode.push(t.elapsed().as_micros() as u64);
        }
        for path in sample.iter().take(500) {
            let t = Instant::now();
            let source = fs::read(root.join(path))?;
            let source_hash = hash(&source);
            let snapshot = if let Some(db) = &mut db {
                db.get(path)?
            } else {
                pack.get(path)?
            }
            .ok_or("missing")?;
            if source_hash != snapshot.source_hash {
                return Err("source changed during benchmark".into());
            }
            black_box(snapshot.decode()?);
            end_to_end.push(t.elapsed().as_micros() as u64);
        }
        results.push(json!({"backend":backend,"requests":sample.len(),"payload_bytes_read":bytes,"raw_read":timings(&mut raw),"checksum_decode":timings(&mut decode),"read_source_hash_snapshot_decode":timings(&mut end_to_end)}));
    }
    // Same existing snapshot updates: measures backend update mechanics, NOT semantic edits.
    let mut update = Vec::new();
    let selected: Vec<_> = sample
        .iter()
        .take(100)
        .map(|p| pack.get(p).map(|s| s.unwrap()))
        .collect::<Result<_>>()?;
    for backend in ["sqlite-raw", "sqlite-indexed", "pack"] {
        let t = Instant::now();
        if backend == "pack" {
            for s in &selected {
                pack.put(s)?;
            }
            pack.commit()?;
        } else {
            let mut db = Database::open(&out.join(backend).join("index.sqlite3"))?;
            for batch in selected.chunks(10) {
                db.put_batch(batch, backend == "sqlite-indexed")?;
            }
            db.checkpoint()?;
        }
        update.push(json!({"backend":backend,"replacements":selected.len(),"wall_us":t.elapsed().as_micros(),"bytes_after":if backend=="pack"{directory_bytes(&out.join("pack"))}else{directory_bytes(&out.join(backend))}}));
    }
    Ok(
        json!({"os_cache":"not purged; application caches initially empty; artifacts recently written","pack_open_us":pack_open_us,"pack_directory_estimated_bytes":pack.directory_estimated_bytes(),"reads":results,"snapshot_replacement":update}),
    )
}

fn replay(out: &Path) -> Result<Value> {
    let db = Database::open(&out.join("sqlite-indexed/index.sqlite3"))?;
    let mut pack = Pack::open(&out.join("pack"))?;
    let paths = pack.paths();
    let hot: Vec<_> = paths
        .iter()
        .filter(|p| p.starts_with("block/") && p.ends_with(".c"))
        .take(8)
        .cloned()
        .collect();
    let mut trace = Vec::new();
    for cycle in 0..24 {
        for i in 0..160 {
            trace.push(hot[i % hot.len()].clone());
        }
        for i in 0..250 {
            trace.push(paths[(cycle * 250 + i) % paths.len()].clone());
        }
    }
    let mut results = Vec::new();
    for round in 0..3 {
        for lru in if round % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        } {
            let mut cache = Cache::new(Settings {
                mode: "manual".into(),
                max_bytes: 2 * 1024 * 1024,
                prefetch: false,
            });
            cache.lru_only = lru;
            let mut durations = Vec::new();
            let started = Instant::now();
            for path in &trace {
                let t = Instant::now();
                let hash = pack.source_hash(path).unwrap();
                if cache.get(path, &hash).is_none() {
                    let snapshot = db.get(path)?.unwrap();
                    let facts = snapshot.decode()?;
                    cache.insert(path, hash, facts, false, 0.0);
                }
                durations.push(t.elapsed().as_micros() as u64);
            }
            results.push(json!({"round":round,"policy":if lru{"lru"}else{"sampled-frequency-byte-aware"},"requests":trace.len(),"wall_ms":started.elapsed().as_millis(),"latency":timings(&mut durations),"cache":cache.stats}));
        }
    }
    // Controlled include-following trace: favorable to dependency prefetch, explicitly synthetic.
    let mut related = Vec::new();
    for path in paths.iter().filter(|p| p.ends_with(".c")).take(400) {
        related.push(path.clone());
        related.extend(db.neighbors(path, 3)?);
    }
    let mut prefetch_results = Vec::new();
    for round in 0..3 {
        for enabled in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let mut cache = Cache::new(Settings {
                mode: "manual".into(),
                max_bytes: 8 * 1024 * 1024,
                prefetch: enabled,
            });
            let mut durations = Vec::new();
            let mut extra = 0u64;
            let started = Instant::now();
            for path in &related {
                let t = Instant::now();
                let h = pack.source_hash(path).unwrap();
                if cache.get(path, &h).is_none() {
                    let s = db.get(path)?.unwrap();
                    cache.insert(path, h, s.decode()?, false, 0.0);
                }
                durations.push(t.elapsed().as_micros() as u64);
                if enabled {
                    for next in db.neighbors(path, 3)? {
                        if !cache.contains(&next) {
                            if let Some(s) = db.get(&next)? {
                                if s.payload.len() <= 256 * 1024 {
                                    extra += s.payload.len() as u64;
                                    cache.insert(&next, s.source_hash, s.decode()?, true, 0.0);
                                }
                            }
                        }
                    }
                }
            }
            prefetch_results.push(json!({"round":round,"prefetch":enabled,"requests":related.len(),"wall_ms":started.elapsed().as_millis(),"foreground_latency_excludes_speculation":timings(&mut durations),"extra_payload_bytes":extra,"cache":cache.stats}));
        }
    }
    let mut controller = Cache::new(Settings::default());
    let mut states = Vec::new();
    for (pressure, busy, t) in [
        ("normal", false, 0),
        ("warning", false, 1000),
        ("critical", true, 2000),
        ("normal", false, 35000),
    ] {
        controller.observe(pressure, busy, t);
        states.push(json!({"pressure":pressure,"busy":busy,"cache":controller.stats}));
    }
    let _ = &mut pack;
    Ok(
        json!({"trace":"synthetic locality + scanning, not real user attention","cache_comparison":results,"include_following_replay":prefetch_results,"controller_injected_pressure":states}),
    )
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err(
            "usage: qemu analyze|backends|reads|replay <source-root> <output-directory>".into(),
        );
    }
    let root = Path::new(&args[2]).canonicalize()?;
    let out = Path::new(&args[3]);
    fs::create_dir_all(out)?;
    let value = match args[1].as_str() {
        "analyze" => analyze(&root, out)?,
        "backends" => backends(out)?,
        "reads" => reads(&root, out)?,
        "replay" => replay(out)?,
        "hybrid" => hybrid_phase(out)?,
        "compare" => compare(&root, out)?,
        "service" => service(&root, out)?,
        "export" => export(out)?,
        _ => return Err("unknown phase".into()),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn export(out: &Path) -> Result<Value> {
    use std::io::Write;
    let db = Database::open(&out.join("sqlite-indexed/index.sqlite3"))?;
    let mut counts = serde_json::Map::new();
    for kind in ["symbol", "file"] {
        let rows = db.heat(kind, "structure", 100_000)?;
        fs::write(
            out.join(format!("{kind}-heat-all.json")),
            serde_json::to_vec_pretty(&rows)?,
        )?;
        let mut file =
            std::io::BufWriter::new(fs::File::create(out.join(format!("{kind}-heat.csv")))?);
        writeln!(
            file,
            "entity_key,path,name,kind,syntactic_incoming_proxy,ambiguous,resolved_incoming,evidence_level,active_ms,open_count,attention_score,last_seen"
        )?;
        for row in &rows {
            let fields = vec![
                row.entity_key.clone(),
                row.path.clone(),
                row.name.clone(),
                row.kind.clone(),
                row.syntactic_incoming_proxy.to_string(),
                row.ambiguous.to_string(),
                "unknown".into(),
                row.evidence_level.into(),
                row.active_ms.to_string(),
                row.open_count.to_string(),
                row.attention_score.to_string(),
                row.last_seen.to_string(),
            ];
            writeln!(
                file,
                "{}",
                fields
                    .iter()
                    .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(",")
            )?;
        }
        counts.insert(kind.into(), json!(rows.len()));
    }
    Ok(
        json!({"pack_directory_estimated_bytes": Pack::open(&out.join("pack"))?.directory_estimated_bytes(), "rows":counts,"behavior":"actual index contains no user reading events; all zero, synthetic replays excluded","output":out}),
    )
}

fn hybrid_phase(out: &Path) -> Result<Value> {
    let mut pack = Pack::open(&out.join("pack"))?;
    let mut hybrid = Hybrid::open(&out.join("hybrid-indexed"))?;
    let started = Instant::now();
    let mut batch = Vec::new();
    let (mut input_us, mut put_us) = (0u64, 0u64);
    for path in pack.paths() {
        let t = Instant::now();
        batch.push(pack.get(&path)?.unwrap());
        input_us += t.elapsed().as_micros() as u64;
        if batch.len() == 32 {
            let t = Instant::now();
            hybrid.put_batch(&batch, true)?;
            put_us += t.elapsed().as_micros() as u64;
            batch.clear();
        }
    }
    let t = Instant::now();
    hybrid.put_batch(&batch, true)?;
    hybrid.db.checkpoint()?;
    put_us += t.elapsed().as_micros() as u64;
    Ok(
        json!({"backend":"hybrid-indexed","files":pack.paths().len(),"wall_ms":started.elapsed().as_millis(),"spool_read_us":input_us,"put_and_checkpoint_us":put_us,"steady_bytes":directory_bytes(&out.join("hybrid-indexed")),"data_before_metadata_sync":true}),
    )
}

fn compare(root: &Path, out: &Path) -> Result<Value> {
    let mut pack = Pack::open(&out.join("pack"))?;
    let paths = pack.paths();
    let mut seed = 42u64;
    let sample: Vec<_> = (0..1500)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            paths[seed as usize % paths.len()].clone()
        })
        .collect();
    let mut rows = Vec::new();
    for round in 0..3 {
        let orders = ["sqlite-indexed", "hybrid-indexed", "pack", "sqlite-raw"];
        for i in 0..4 {
            let name = orders[(i + round) % 4];
            let t = Instant::now();
            let mut db = if name.starts_with("sqlite") {
                Some(Database::open(&out.join(name).join("index.sqlite3"))?)
            } else {
                None
            };
            let mut hybrid = if name == "hybrid-indexed" {
                Some(Hybrid::open(&out.join(name))?)
            } else {
                None
            };
            let mut fresh_pack = if name == "pack" {
                Some(Pack::open(&out.join("pack"))?)
            } else {
                None
            };
            let open_us = t.elapsed().as_micros();
            let mut reads = Vec::new();
            let mut full = Vec::new();
            for path in &sample {
                let t = Instant::now();
                let s = if let Some(db) = &db {
                    db.get(path)?
                } else if let Some(h) = &mut hybrid {
                    h.get(path)?
                } else {
                    fresh_pack.as_mut().unwrap().get(path)?
                }
                .unwrap();
                black_box(s.decode()?);
                reads.push(t.elapsed().as_micros() as u64);
            }
            for path in sample.iter().take(500) {
                let t = Instant::now();
                let source = fs::read(root.join(path))?;
                let h = hash(&source);
                let s = if let Some(db) = &db {
                    db.get(path)?
                } else if let Some(h) = &mut hybrid {
                    h.get(path)?
                } else {
                    fresh_pack.as_mut().unwrap().get(path)?
                }
                .unwrap();
                if s.source_hash != h {
                    return Err("changed corpus".into());
                }
                black_box(s.decode()?);
                full.push(t.elapsed().as_micros() as u64);
            }
            rows.push(json!({"round":round,"backend":name,"open_us":open_us,"snapshot_read_checksum_decode":timings(&mut reads),"source_hash_snapshot_decode":timings(&mut full)}));
            let _ = &mut db;
        }
    }
    let selected: Vec<_> = sample
        .iter()
        .take(100)
        .map(|p| pack.get(p).map(|s| s.unwrap()))
        .collect::<Result<_>>()?;
    let mut updates = Vec::new();
    for name in ["sqlite-raw", "sqlite-indexed", "hybrid-indexed", "pack"] {
        let before = if name == "pack" {
            directory_bytes(&out.join("pack"))
        } else {
            directory_bytes(&out.join(name))
        };
        let t = Instant::now();
        if name == "pack" {
            for batch in selected.chunks(10) {
                for s in batch {
                    pack.put(s)?;
                }
                pack.commit()?;
            }
        } else if name == "hybrid-indexed" {
            let mut hybrid = Hybrid::open(&out.join(name))?;
            hybrid.db.set_durable_writes()?;
            for batch in selected.chunks(10) {
                hybrid.put_batch(batch, true)?;
            }
            hybrid.db.checkpoint()?;
        } else {
            let mut db = Database::open(&out.join(name).join("index.sqlite3"))?;
            db.set_durable_writes()?;
            for batch in selected.chunks(10) {
                db.put_batch(batch, name == "sqlite-indexed")?;
            }
            db.checkpoint()?;
        }
        updates.push(json!({"backend":name,"replacements":100,"batch_size":10,"wall_us":t.elapsed().as_micros(),"file_growth_bytes":(if name=="pack"{directory_bytes(&out.join("pack"))}else{directory_bytes(&out.join(name))}).saturating_sub(before)}));
    }
    Ok(
        json!({"os_cache":"not purged; alternating backend order, application connections reopened each round","reads":rows,"durable_replacement":updates,"replacement_semantics":"identical snapshot replacement; SQLite FULL, pack and hybrid sync data before metadata publication; not actual source edits"}),
    )
}

fn service(root: &Path, out: &Path) -> Result<Value> {
    let pack = Pack::open(&out.join("pack"))?;
    let paths = pack.paths();
    let worker = Worker::open(
        out.join("sqlite-indexed/index.sqlite3"),
        Settings {
            mode: "manual".into(),
            max_bytes: 16 * 1024 * 1024,
            prefetch: true,
        },
    )?;
    let mut engine = Engine::default();
    let mut times = Vec::new();
    let mut failed = Vec::new();
    for path in paths.iter().step_by(5).take(1000) {
        let t = Instant::now();
        let source = fs::read_to_string(root.join(path))?;
        let h = hash(source.as_bytes());
        let loaded = worker.load(path, h)?.ok_or("missing verified facts")?;
        match engine.restore(path, source, loaded.facts) {
            Ok(_) => times.push(t.elapsed().as_micros() as u64),
            Err(e) => failed.push(json!({"path":path,"code":e.code})),
        }
    }
    let status = worker.status()?;
    Ok(
        json!({"requests":times.len(),"pipeline":"read source + hash + worker queue + SQLite or hot facts + engine restore; excludes UI/JSON stdio","latency":timings(&mut times),"cache":status,"engine":engine.stats(),"failures":failed}),
    )
}
