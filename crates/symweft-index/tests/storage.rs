use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use symweft_analysis::Engine;
use symweft_index::{
    Result, Snapshot,
    cache::{Cache, Settings},
    hash,
    pack::Pack,
    store::Database,
    worker::Worker,
};
struct Temp(PathBuf);
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "symweft-index-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

#[test]
fn sequential_worker_restarts_never_lose_flushed_events() -> Result<()> {
    for _ in 0..32 {
        let temp = Temp::new();
        let path = temp.0.join("index.sqlite3");
        let saved = snapshot("void run(void) { target(); }");
        let hash = saved.source_hash;
        {
            let worker = Worker::open(path.clone(), Settings::default())?;
            worker.store(saved, vec![])?;
            worker.flush()?;
        }
        let worker = Worker::open(path, Settings::default())?;
        assert!(worker.load("main.c", hash)?.is_some());
    }
    Ok(())
}

#[test]
fn hybrid_has_same_facts_secondary_heat_and_atomic_failed_publication() -> Result<()> {
    let temp = Temp::new();
    let mut backend = symweft_index::hybrid::Hybrid::open(&temp.0)?;
    let original = snapshot("void run(void) { target(); }");
    backend.put_batch(std::slice::from_ref(&original), true)?;
    let mut bad = original.clone();
    bad.payload.push(0);
    assert!(backend.put_batch(&[bad], true).is_err());
    assert_eq!(backend.get("main.c")?.unwrap().payload, original.payload);
    drop(backend);
    let mut reopened = symweft_index::hybrid::Hybrid::open(&temp.0)?;
    assert_eq!(
        reopened.get("main.c")?.unwrap().decode()?.calls[0].callee,
        "target"
    );
    Ok(())
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn snapshot(source: &str) -> Snapshot {
    let mut e = Engine::default();
    let u = e.analyze("main.c", source.into()).unwrap();
    Snapshot::new(
        "main.c",
        source,
        &e.document("main.c", u.revision).unwrap().facts,
    )
    .unwrap()
}

#[test]
fn restart_reuses_verified_facts_without_parse_and_edits_rehydrate_lazily() -> Result<()> {
    let temp = Temp::new();
    let path = temp.0.join("index.sqlite3");
    let source = "void run(void) { target(); }";
    let saved = snapshot(source);
    {
        let mut db = Database::open(&path)?;
        db.put_batch(std::slice::from_ref(&saved), true)?;
        db.checkpoint()?;
    }
    let db = Database::open(&path)?;
    let restored = db.get("main.c")?.unwrap();
    assert_eq!(saved.payload, restored.payload);
    let facts = restored.decode()?;
    let mut e = Engine::default();
    let opened = e.restore("main.c", source.into(), facts)?;
    assert_eq!(e.stats().parses, 0);
    assert_eq!(opened.mode, "restored");
    let at = source.find("target").unwrap();
    let update = e.edit("main.c", opened.revision, at, at + 6, "better")?;
    assert_eq!(
        e.document("main.c", update.revision)?.facts.calls[0].callee,
        "better"
    );
    assert_eq!(e.stats().parses, 1);
    Ok(())
}

#[test]
fn batch_failure_is_atomic_and_deleted_calls_do_not_leave_ghost_heat() -> Result<()> {
    let temp = Temp::new();
    let mut db = Database::open(&temp.0.join("index.sqlite3"))?;
    let first = snapshot("void target(void) {} void run(void) { target(); }");
    db.put_batch(std::slice::from_ref(&first), true)?;
    let target = db
        .heat("symbol", "structure", 20)?
        .into_iter()
        .find(|x| x.name == "target")
        .unwrap();
    assert_eq!(target.syntactic_incoming_proxy, 1);
    assert_eq!(target.resolved_incoming, None);
    let next = snapshot("void target(void) {} void run(void) {}");
    let mut broken = first.clone();
    broken.path = "other.c".into();
    broken.payload.push(1);
    assert!(db.put_batch(&[next.clone(), broken], true).is_err());
    assert_eq!(db.get("main.c")?.unwrap().payload, first.payload);
    db.put_batch(&[next], true)?;
    let target = db
        .heat("symbol", "structure", 20)?
        .into_iter()
        .find(|x| x.name == "target")
        .unwrap();
    assert_eq!(target.syntactic_incoming_proxy, 0);
    db.remove_paths(&["main.c".into()])?;
    assert!(db.get("main.c")?.is_none());
    assert!(db.heat("symbol", "structure", 20)?.is_empty());
    Ok(())
}

#[test]
fn pack_roundtrip_updates_and_detects_payload_corruption() -> Result<()> {
    let temp = Temp::new();
    let saved = snapshot("void run(void) { target(); }");
    {
        let mut pack = Pack::open(&temp.0)?;
        pack.put(&saved)?;
        pack.commit()?;
    }
    {
        let mut pack = Pack::open(&temp.0)?;
        assert_eq!(
            pack.get("main.c")?.unwrap().decode()?.calls[0].callee,
            "target"
        );
        let next = snapshot("void run(void) { better(); }");
        pack.put(&next)?;
        pack.commit()?;
    }
    let mut pack = Pack::open(&temp.0)?;
    assert_eq!(
        pack.get("main.c")?.unwrap().decode()?.calls[0].callee,
        "better"
    );
    let mut damaged = pack.get("main.c")?.unwrap();
    damaged.payload[5] ^= 1;
    assert!(damaged.decode().is_err());
    Ok(())
}

#[test]
fn user_heat_is_independent_from_indexing_clamps_heartbeats_and_keeps_symbol_identity() -> Result<()>
{
    let temp = Temp::new();
    let mut db = Database::open(&temp.0.join("index.sqlite3"))?;
    let saved = snapshot("void run(void) { target(); }");
    db.put_batch(std::slice::from_ref(&saved), true)?;
    assert!(
        db.heat("symbol", "attention", 20)?
            .iter()
            .all(|r| r.active_ms == 0)
    );
    db.record_usage("main.c", saved.decode()?.as_ref(), Some(0), 100_000, true)?;
    let symbol = db.heat("symbol", "attention", 20)?.remove(0);
    assert_eq!(symbol.active_ms, 5000);
    assert_eq!(symbol.open_count, 1);
    assert!(symbol.attention_score > 6.0);
    let file = db.heat("file", "attention", 20)?.remove(0);
    assert_eq!(file.active_ms, 5000);
    db.clear_usage()?;
    assert!(
        db.heat("symbol", "attention", 20)?
            .iter()
            .all(|r| r.active_ms == 0 && r.open_count == 0)
    );
    Ok(())
}

#[test]
fn cache_bounds_pressure_modes_and_invalidated_content() -> Result<()> {
    let facts = snapshot("void run(void) { target(); }").decode()?;
    let mut cache = Cache::new(Settings {
        mode: "manual".into(),
        max_bytes: 16 * 1024,
        prefetch: true,
    });
    let original = hash(b"old");
    for i in 0..100 {
        cache.insert(&format!("{i}.c"), original, Arc::clone(&facts), false, 0.0);
    }
    assert!(cache.stats.used_bytes <= cache.stats.fact_limit_bytes);
    assert!(cache.stats.evictions > 0);
    cache.insert("a.c", original, Arc::clone(&facts), false, 0.0);
    assert!(cache.get("a.c", &hash(b"new")).is_none());
    cache.observe("critical", true, 1000);
    assert!(!cache.prefetch_enabled());
    assert!(cache.stats.used_bytes <= cache.stats.fact_limit_bytes);
    cache.configure(Settings {
        mode: "off".into(),
        max_bytes: 16 * 1024,
        prefetch: true,
    })?;
    assert_eq!(cache.stats.used_bytes, 0);
    assert!(!cache.insert("x.c", original, facts, false, 0.0));
    Ok(())
}

#[test]
fn worker_restart_validation_and_queue_ordering() -> Result<()> {
    let temp = Temp::new();
    let path = temp.0.join("index.sqlite3");
    let saved = snapshot("void run(void) { target(); }");
    let source_hash = saved.source_hash;
    {
        let worker = Worker::open(path.clone(), Settings::default())?;
        worker.store(saved, vec![])?;
        worker.flush()?;
    }
    let direct = Database::open(&path)?;
    assert_eq!(direct.paths()?, vec!["main.c"]);
    assert_eq!(direct.get("main.c")?.unwrap().source_hash, source_hash);
    drop(direct);
    let worker = Worker::open(path, Settings::default())?;
    let opened = worker
        .load("main.c", source_hash)?
        .expect("worker must read persisted row");
    assert_eq!(opened.source, "sqlite");
    assert_eq!(
        worker.load("main.c", source_hash)?.unwrap().source,
        "memory"
    );
    assert!(worker.load("main.c", hash(b"changed"))?.is_none());
    Ok(())
}

#[test]
fn repeated_parent_scopes_do_not_collide_in_persisted_symbol_identity() -> Result<()> {
    let source = "class A:\n    def run(self): pass\nclass A:\n    def run(self): pass\n";
    let mut e = Engine::default();
    let u = e.analyze("a.py", source.into())?;
    let facts = &e.document("a.py", u.revision)?.facts;
    let keys: std::collections::HashSet<_> = facts
        .symbols
        .iter()
        .map(|s| symweft_index::symbol_key("a.py", facts, s.local_id))
        .collect();
    assert_eq!(keys.len(), facts.symbols.len());
    Ok(())
}
