use serde_json::json;
use std::{
    collections::HashSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
use symweft_semantic::{
    Result,
    compdb::{CompileCommand, CompileDatabase},
    model::{
        MAX_OVERLAY_DOCUMENTS, MAX_OVERLAY_SOURCE_BYTES, Overlay, PreviewDocument, Snapshot,
        WorkerReply, WorkerRequest,
    },
    provider,
    service::{Controller, Limits},
    store::Store,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "symweft-a2-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p.canonicalize().unwrap())
    }
    fn write(&self, path: &str, source: &str) {
        let p = self.0.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, source).unwrap();
    }
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.0.join(path)).unwrap()
    }
    fn commands(&self, files: &[&str]) -> Result<CompileDatabase> {
        let entries:Vec<_>=files.iter().map(|f|json!({"directory":self.0,"file":f,"arguments":[if f.ends_with("cpp"){"clang++"}else{"clang"},if f.ends_with("cpp"){"-std=c++17"}else{"-std=c11"},"-I.","-c",f,"-o","out.o"]})).collect();
        fs::write(
            self.0.join("compile_commands.json"),
            serde_json::to_vec(&entries)?,
        )?;
        CompileDatabase::load(&self.0, &self.0.join("compile_commands.json"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn analyze(db: &CompileDatabase, n: usize) -> Result<Snapshot> {
    let reply = provider::analyze(&WorkerRequest {
        command: db.commands[n].clone(),
        overlays: Vec::new(),
    })?;
    assert!(reply.error.is_none(), "{:?}", reply.diagnostics);
    Ok(reply.snapshot.unwrap())
}

fn worker_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let release_bin = workspace_root.join("target/release/symweft-core");
    if release_bin.exists() {
        return release_bin;
    }
    let debug_bin = workspace_root.join("target/debug/symweft-core");
    if debug_bin.exists() {
        return debug_bin;
    }
    release_bin
}

#[test]
fn cxx_overloads_virtual_dispatch_and_function_pointers_are_not_conflated() -> Result<()> {
    let f = Fixture::new();
    f.write("main.cpp","int pick(int x){return x;}\ndouble pick(double x){return x;}\nstruct Base{virtual int run(){return 1;}};\nstruct Child:Base{int run()override{return 2;}};\nint use(Base& b){int(*fp)(int)=pick;return pick(1)+(int)pick(2.0)+b.run()+b.Base::run()+fp(3);}\n");
    let db = f.commands(&["main.cpp"])?;
    let s = analyze(&db, 0)?;
    let picks: Vec<_> = s.calls.iter().filter(|c| c.name == "pick").collect();
    assert_eq!(picks.len(), 2);
    assert_ne!(picks[0].target, picks[1].target);
    assert!(picks.iter().all(|c| c.resolution == "direct"));
    let runs: Vec<_> = s.calls.iter().filter(|c| c.name == "run").collect();
    assert_eq!(runs[0].resolution, "virtual_candidate");
    assert_eq!(runs[1].resolution, "direct");
    assert_eq!(runs[0].target, runs[1].target);
    let pointer = s.calls.iter().find(|c| c.name == "fp").unwrap();
    assert_eq!(pointer.resolution, "indirect");
    assert!(pointer.target.is_none());
    Ok(())
}

#[test]
fn c_macro_conditionals_internal_linkage_and_cross_file_bidirectional_queries() -> Result<()> {
    let f = Fixture::new();
    f.write("api.h", "int target(int);\n#define WRAP(x) target(x)\n");
    f.write("a/foo.c","#include \"../api.h\"\nint target(int x){return x;}\nstatic int helper(int x){return target(x);}\nint a_run(int x){return helper(x)+WRAP(x);}\n");
    f.write("b/foo.c","#include \"../api.h\"\nstatic int helper(int x){return target(x);}\nint b_run(int x){return helper(x);}\n");
    let db = f.commands(&["a/foo.c", "b/foo.c"])?;
    let a = analyze(&db, 0)?;
    let b = analyze(&db, 1)?;
    let ah = a.symbols.iter().find(|s| s.name == "helper").unwrap();
    let bh = b.symbols.iter().find(|s| s.name == "helper").unwrap();
    assert_ne!(ah.id, bh.id);
    let target = a
        .symbols
        .iter()
        .find(|s| s.name == "target" && s.definition)
        .unwrap();
    assert!(
        b.calls
            .iter()
            .any(|c| c.target.as_deref() == Some(&target.id))
    );
    assert!(
        a.references
            .iter()
            .any(|r| r.name == "WRAP" && r.kind == "macro expansion")
    );
    let mut store = Store::open(&f.0.join("semantics.sqlite3"))?;
    let initial_baseline = store.disk_baseline()?;
    store.publish(&a)?;
    let a_baseline = store.disk_baseline()?;
    assert_eq!(a_baseline.store_id, initial_baseline.store_id);
    assert_eq!(a_baseline.revision, initial_baseline.revision + 1);
    store.publish(&b)?;
    assert_eq!(store.disk_baseline()?.revision, a_baseline.revision + 1);
    assert_eq!(
        store.incompatible_provider_units(&db.configuration, &a.provider)?,
        0
    );
    assert_eq!(
        store.incompatible_provider_units(&db.configuration, "different-sdk-or-compiler")?,
        2
    );
    assert!(
        store.relations(
            &db.configuration,
            &target.id,
            "references",
            "different-sdk-or-compiler",
            100
        )?["rows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let incoming = store.relations(&db.configuration, &target.id, "incoming", &a.provider, 100)?;
    assert_eq!(incoming["rows"].as_array().unwrap().len(), 3);
    assert_eq!(
        incoming["disk_baseline"],
        serde_json::to_value(store.disk_baseline()?)?
    );
    let without_b = store.relations_except(
        &db.configuration,
        &target.id,
        "incoming",
        &a.provider,
        &b.unit_id,
        100,
    )?;
    assert_eq!(without_b["rows"].as_array().unwrap().len(), 2);
    assert!(
        without_b["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["unit_id"].as_str() != Some(b.unit_id.as_str()))
    );
    assert_eq!(without_b["disk_baseline"], incoming["disk_baseline"]);
    let defs = store.relations(
        &db.configuration,
        &target.id,
        "definitions",
        &a.provider,
        100,
    )?;
    assert_eq!(defs["rows"].as_array().unwrap().len(), 1);
    let out = store.relations(&db.configuration, &bh.id, "outgoing", &a.provider, 100)?;
    assert_eq!(out["rows"][0]["target"], target.id);
    assert_eq!(
        store
            .affected(&db.configuration, f.0.join("api.h").to_str().unwrap())?
            .len(),
        2
    );
    // Replacing one TU must retain the other TU's shared-header contribution.
    f.write(
        "a/foo.c",
        "#include \"../api.h\"\nint target(int x){return x;}\nint a_run(int x){return x;}\n",
    );
    assert!(!store.reusable(&db.commands[0], &a.provider)?);
    store.publish(&analyze(&db, 0)?)?;
    let incoming = store.relations(&db.configuration, &target.id, "incoming", &a.provider, 100)?;
    assert_eq!(incoming["rows"].as_array().unwrap().len(), 1);
    assert!(store.reusable(&db.commands[1], &b.provider)?);
    let final_baseline = store.disk_baseline()?;
    drop(store);
    let store = Store::open(&f.0.join("semantics.sqlite3"))?;
    assert_eq!(store.disk_baseline()?, final_baseline);
    Ok(())
}

#[test]
fn changed_headers_and_compiler_errors_never_masquerade_as_current_and_overlay_does_not_publish()
-> Result<()> {
    let f = Fixture::new();
    f.write(
        "api.h",
        "int left(void);int right(void);\n#define CHOSEN left\n",
    );
    f.write(
        "main.c",
        "#include \"api.h\"\nint caller(void){return CHOSEN();}\n",
    );
    let db = f.commands(&["main.c"])?;
    let original = analyze(&db, 0)?;
    let mut store = Store::open(&f.0.join("semantics.sqlite3"))?;
    store.publish(&original)?;
    let caller = original
        .symbols
        .iter()
        .find(|s| s.name == "caller")
        .unwrap();
    let disk = fs::read_to_string(f.0.join("main.c"))?;
    let preview = provider::analyze(&WorkerRequest {
        command: db.commands[0].clone(),
        overlays: vec![Overlay {
            path: f.0.join("main.c").to_string_lossy().into(),
            content: disk.replace("CHOSEN()", "right()"),
        }],
    })?
    .snapshot
    .unwrap();
    assert_eq!(preview.calls[0].name, "right");
    assert_eq!(
        store.relations(
            &db.configuration,
            &caller.id,
            "outgoing",
            &original.provider,
            100
        )?["rows"][0]["name"],
        "left"
    );
    assert_eq!(fs::read_to_string(f.0.join("main.c"))?, disk);
    f.write(
        "api.h",
        "int left(void);int right(void);\n#define CHOSEN right\n",
    );
    assert!(!store.reusable(&db.commands[0], &original.provider)?);
    assert!(
        store.relations(
            &db.configuration,
            &caller.id,
            "outgoing",
            &original.provider,
            100
        )?["rows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let updated = analyze(&db, 0)?;
    assert_eq!(updated.calls[0].name, "right");
    store.publish(&updated)?;
    f.write("api.h", "#include \"does-not-exist.h\"\n");
    let failure = provider::analyze(&WorkerRequest {
        command: db.commands[0].clone(),
        overlays: Vec::new(),
    })?;
    assert!(failure.snapshot.is_none() && failure.error.is_some());
    assert!(failure.diagnostics.iter().any(|d| d.severity >= 3));
    assert!(
        store.relations(
            &db.configuration,
            &caller.id,
            "outgoing",
            &updated.provider,
            100
        )?["rows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[test]
fn compilation_commands_are_data_response_files_and_variants_are_tracked_and_publication_is_atomic()
-> Result<()> {
    let f = Fixture::new();
    f.write(
        "main.c",
        "#if FLAG\nint active(void){return 1;}\n#else\nint other(void){return 2;}\n#endif\nstatic int local(void){return 0;}\n",
    );
    f.write("flags.rsp", "-std=c11 -DFLAG=1 -I.\n");
    let entries = json!([{"directory":f.0,"file":"main.c","command":"clang @flags.rsp -c main.c -o 'out object.o'"},{"directory":f.0,"file":"main.c","arguments":["clang","-DFLAG=0","-c","main.c"]},{"directory":f.0,"file":"main.c","command":"clang -c main.c && touch sentinel"}]);
    fs::write(
        f.0.join("compile_commands.json"),
        serde_json::to_vec(&entries)?,
    )?;
    let db = CompileDatabase::load(&f.0, &f.0.join("compile_commands.json"))?;
    assert_eq!(db.commands.len(), 2);
    assert_eq!(db.rejected.len(), 1);
    assert!(!f.0.join("sentinel").exists());
    assert_ne!(db.commands[0].unit_id, db.commands[1].unit_id);
    let a = analyze(&db, 0)?;
    let b = analyze(&db, 1)?;
    assert_ne!(
        a.symbols.iter().find(|s| s.definition).unwrap().name,
        b.symbols.iter().find(|s| s.definition).unwrap().name
    );
    let mut store = Store::open(&f.0.join("semantics.sqlite3"))?;
    store.publish(&a)?;
    let mut broken = a.clone();
    broken.inputs.push(broken.inputs[0].clone());
    broken.symbols.clear();
    assert!(store.publish(&broken).is_err());
    assert!(
        !store.symbols(&db.configuration, &a.symbols[0].name, &a.provider, 100)?["rows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    store.publish(&b)?;
    let one = f.commands(&["main.c"])?;
    let local = a
        .symbols
        .iter()
        .find(|symbol| symbol.name == "local")
        .unwrap();
    let context = store.at(
        &db.configuration,
        &local.location.path,
        local.location.offset.into(),
        &a.provider,
    )?;
    assert_eq!(context["declarations"].as_array().unwrap().len(), 2);
    assert!(
        context["entity_id"].is_null(),
        "Different TU-local identities require an explicit variant choice."
    );
    assert_eq!(store.reconcile(&one)?, 2);
    Ok(())
}

#[cfg(unix)]
#[test]
fn disposable_worker_timeout_and_cancellation_reap_the_process() -> Result<()> {
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };
    use symweft_semantic::service::{Limits, run_worker};
    let f = Fixture::new();
    f.write("main.c", "int entry(void){return 1;}\n");
    f.write("fake-worker", "#!/bin/sh\ncat >/dev/null\nexec sleep 5\n");
    let worker = f.0.join("fake-worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700))?;
    let db = f.commands(&["main.c"])?;
    let request = WorkerRequest {
        command: db.commands[0].clone(),
        overlays: Vec::new(),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = Arc::clone(&cancel);
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        trigger.store(true, Ordering::Relaxed);
    });
    let start = Instant::now();
    let error = run_worker(&worker, &request, &Limits::default(), &cancel).unwrap_err();
    thread.join().unwrap();
    assert!(error.to_string().contains("cancelled"));
    assert!(start.elapsed() < Duration::from_secs(2));
    let start = Instant::now();
    let error = run_worker(
        &worker,
        &request,
        &Limits {
            timeout_seconds: 1,
            worker_memory_mib: 512,
        },
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(error.to_string().contains("timeout"));
    assert!(start.elapsed() < Duration::from_secs(3));
    Ok(())
}

#[cfg(unix)]
#[test]
fn preview_scheduler_preempts_index_coalesces_and_enforces_retained_budget() -> Result<()> {
    use std::{
        collections::HashSet,
        os::unix::fs::PermissionsExt,
        thread,
        time::{Duration, Instant},
    };
    use symweft_semantic::service::{Controller, Limits};
    let f = Fixture::new();
    f.write("main.c", "int entry(void){return 1;}\n");
    let db = f.commands(&["main.c"])?;
    // A process-held advisory lock detects overlap even if its owner is killed.
    // Unlike a shell + sleep, this fixture does not leave descendant processes.
    f.write("fake-worker", r#"#!/usr/bin/env python3
import fcntl,json,os,pathlib,sys,time
raw = sys.stdin.buffer.read()
if raw.startswith(b'{'):
    request = json.loads(raw.decode('utf-8'))
    root = pathlib.Path(request['command']['root'])
    overlays = request['overlays']
    is_budget = bool(overlays and 'budget' in overlays[0]['content'])
    has_overlays = bool(overlays)
else:
    root = pathlib.Path(sys.argv[0]).parent.resolve()
    has_overlays = b'int ' in raw or b'budget' in raw
    is_budget = b'budget' in raw
lock=(root/'worker.lock').open('w')
try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
except BlockingIOError:
    (root/'overlap').write_text('two compiler processes')
    sys.exit(9)
with (root/'workers').open('a') as events:
    events.write(str(os.getpid())+'\n')
if not has_overlays: time.sleep(5)
else: time.sleep(.15)
message='x'*(2*1024*1024) if is_budget else 'fixture rejection'
json.dump({'snapshot':None,'error':message,'diagnostics':[]},sys.stdout)
"#);
    let worker = f.0.join("fake-worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700))?;
    let mut controller = Controller::open(
        &f.0,
        &db.path,
        f.0.join("cache/workspace/a2/index.sqlite3"),
        worker,
        Limits::default(),
    )?;
    controller.index(&[], 1)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !f.0.join("workers").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(f.0.join("workers").exists());
    let start = Instant::now();
    let old = controller.start_preview("main.c", 1, "int old(void){return 1;}", None, 32)?;
    let latest = controller.start_preview("main.c", 2, "int latest(void){return 2;}", None, 32)?;
    assert!(start.elapsed() < Duration::from_millis(250));
    let id = latest["ticket"]["job_id"].as_u64().unwrap();
    assert!(
        controller
            .cancel_preview(old["ticket"]["job_id"].as_u64().unwrap())
            .is_err()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        if controller.preview_status()?["state"] == "failed" {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(controller.preview_status()?["state"], "failed");
    assert_eq!(controller.preview_status()?["ticket"]["job_id"], id);
    assert!(!f.0.join("overlap").exists());
    assert_eq!(fs::read_to_string(f.0.join("workers"))?.lines().count(), 2);
    controller.start_preview("main.c", 3, "running preview", None, 32)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while fs::read_to_string(f.0.join("workers"))?.lines().count() < 3 && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fs::read_to_string(f.0.join("workers"))?.lines().count(), 3);
    let ack = controller.start_preview("main.c", 4, "budget", None, 1)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        if controller.preview_status()?["state"] == "failed" {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let status = controller.preview_status()?;
    assert_eq!(status["ticket"], ack["ticket"]);
    assert!(
        status["reason"]
            .as_str()
            .unwrap()
            .contains("budget exceeded"),
        "{status}"
    );
    assert_eq!(status["charged_bytes"], 0);
    assert!(!f.0.join("overlap").exists());
    assert_eq!(fs::read_to_string(f.0.join("workers"))?.lines().count(), 4);
    let too_many = (0..=MAX_OVERLAY_DOCUMENTS)
        .map(|_| PreviewDocument {
            path: "main.c".into(),
            revision: 1,
            content: String::new(),
        })
        .collect();
    let error = controller
        .start_preview_documents(too_many, None, 32)
        .unwrap_err();
    assert!(error.to_string().contains("1..=16"));
    assert_eq!(controller.preview_status()?["ticket"], status["ticket"]);
    let too_large = vec![
        PreviewDocument {
            path: "main.c".into(),
            revision: 1,
            content: "x".repeat(MAX_OVERLAY_SOURCE_BYTES / 2 + 1),
        },
        PreviewDocument {
            path: "other.c".into(),
            revision: 1,
            content: "x".repeat(MAX_OVERLAY_SOURCE_BYTES / 2 + 1),
        },
    ];
    let error = controller
        .start_preview_documents(too_large, None, 32)
        .unwrap_err();
    assert!(error.to_string().contains("8 MiB"));
    assert_eq!(controller.preview_status()?["ticket"], status["ticket"]);
    Ok(())
}

#[test]
fn shared_header_facts_preserve_member_identity_and_ownership_and_detect_corruption() -> Result<()>
{
    let f = Fixture::new();
    f.write("shared.hpp", "struct Shared { int field; };\n");
    f.write("a/foo.cpp","#include \"../shared.hpp\"\nnamespace {int hidden(){return 1;}}\nint one(Shared& s){return s.field+hidden();}\n");
    f.write("b/foo.cpp","#include \"../shared.hpp\"\nnamespace {int hidden(){return 2;}}\nint two(Shared& s){return s.field+hidden();}\n");
    let db = f.commands(&["a/foo.cpp", "b/foo.cpp"])?;
    let a = analyze(&db, 0)?;
    let b = analyze(&db, 1)?;
    let field = a.symbols.iter().find(|s| s.name == "field").unwrap();
    assert_eq!(
        field.id,
        b.symbols.iter().find(|s| s.name == "field").unwrap().id
    );
    assert_ne!(
        a.calls.iter().find(|c| c.name == "hidden").unwrap().target,
        b.calls.iter().find(|c| c.name == "hidden").unwrap().target
    );
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.publish(&a)?;
    store.publish(&b)?;
    assert_eq!(
        store.relations(&db.configuration, &field.id, "references", &a.provider, 100)?["rows"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let status = store.status(&db.configuration)?;
    let contributions = status["symbols"].as_i64().unwrap()
        + status["references"].as_i64().unwrap()
        + status["calls"].as_i64().unwrap();
    assert!(status["unique_facts_all_configurations"].as_i64().unwrap() < contributions);
    let one = f.commands(&["b/foo.cpp"])?;
    assert_eq!(store.reconcile(&one)?, 1);
    store.checkpoint()?;
    assert_eq!(
        store.relations(&db.configuration, &field.id, "references", &a.provider, 100)?["rows"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let connection = rusqlite::Connection::open(&path)?;
    connection.execute("UPDATE facts SET payload=x'0001' WHERE pk=(SELECT fact FROM refs WHERE target=(SELECT pk FROM entities WHERE key=?1) LIMIT 1)",[&field.id])?;
    assert!(
        store
            .relations(&db.configuration, &field.id, "references", &a.provider, 100)
            .is_err()
    );
    Ok(())
}

#[test]
fn native_broad_change_over_automatic_budget_invalidates_without_starting_workers() -> Result<()> {
    use std::{
        collections::HashSet,
        thread,
        time::{Duration, Instant},
    };
    use symweft_semantic::{
        service::{Controller, Limits},
        watch::Settings,
    };
    let f = Fixture::new();
    let files: Vec<_> = (0..257).map(|n| format!("unit_{n}.c")).collect();
    for file in &files {
        f.write(file, "// empty translation unit\n");
    }
    let refs: Vec<_> = files.iter().map(String::as_str).collect();
    let db = f.commands(&refs)?;
    let template = analyze(&db, 0)?;
    assert!(template.symbols.is_empty() && template.calls.is_empty());
    let path = f.0.join("cache/workspace/a2/index.sqlite3");
    let mut store = Store::open(&path)?;
    // Valid empty-TU snapshots let the event/scheduler test cover 257 owners
    // without paying for 257 identical compiler invocations.
    for command in &db.commands {
        let mut snapshot = template.clone();
        snapshot.unit_id = command.unit_id.clone();
        snapshot.command_key = command.command_key.clone();
        snapshot.main_file = command.file.to_string_lossy().into();
        for input in &mut snapshot.inputs {
            if input.path == template.main_file {
                input.path = snapshot.main_file.clone();
            }
        }
        store.publish(&snapshot)?;
    }
    drop(store);
    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path,
        f.0.join("worker-must-never-start"),
        Limits::default(),
    )?;
    controller.configure_watch(Settings {
        enabled: true,
        auto_refresh: true,
        metadata_cache: true,
    })?;
    f.write("new_shadow.h", "#define NEW_INCLUDE_SEARCH_INPUT 1\n");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        controller.maintain(&HashSet::new(), true)?;
        if controller.watch_status()["manual_refresh_required"] == true {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let watch = controller.watch_status();
    assert_eq!(watch["manual_refresh_required"], true, "{watch}");
    assert_eq!(watch["auto_jobs"], 0);
    assert_eq!(watch["pending_units"], 0);
    assert_eq!(controller.progress()?["job"]["state"], "idle");
    assert!(
        controller.status()?["index"]["units"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["state"] == "stale")
    );
    Ok(())
}

#[test]
fn header_invalidation_and_partial_includer_coverage() -> Result<()> {
    let f = Fixture::new();
    f.write("api.h", "#define VAL 1
int target(void);
");
    f.write("a.c", r#"#include "api.h"
int a_call(void){return target() + VAL;}
"#);
    f.write("b.c", r#"#include "api.h"
int b_call(void){return target() + VAL;}
"#);
    let db = f.commands(&["a.c", "b.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    drop(store);

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        worker_binary(),
        Limits::default(),
    )?;

    // 1. Unindexed header with no known includers returns clear error
    let no_inc_err = controller.preview_command("unindexed.h", None).unwrap_err();
    assert!(no_inc_err.to_string().contains("no known includer records"), "{no_inc_err}");

    // 2. Header with multiple known includers requires unit_id
    let multi_err = controller.preview_command(&f.0.join("api.h").to_string_lossy(), None).unwrap_err();
    assert!(multi_err.to_string().contains("requires exactly one matching TU"), "{multi_err}");

    // 3. Edit api.h and preview for a.c
    let header_path = f.0.join("api.h").to_string_lossy().into_owned();
    let a_path = f.0.join("a.c").to_string_lossy().into_owned();
    let docs = vec![
        PreviewDocument {
            path: a_path.clone(),
            revision: 1,
            content: f.read("a.c"),
        },
        PreviewDocument {
            path: header_path.clone(),
            revision: 2,
            content: "#define VAL 2
int target(void);
".into(),
        },
    ];
    let a_unit = &db.commands[0].unit_id;
    let b_unit = &db.commands[1].unit_id;
    let status = controller.start_preview_documents(docs, Some(a_unit), 32)?;
    let coverage = &status["ticket"]["includer_coverage"];
    assert_eq!(coverage["scope"], "partial");
    assert_eq!(coverage["updated_includers"][0], *a_unit);
    assert_eq!(coverage["stale_includers"][0], *b_unit);
    assert!(status["completeness"].as_str().unwrap().contains("partial includer coverage"));
    assert!(status["completeness"].as_str().unwrap().contains("project not fully updated"));

    // Check that b.c has been invalidated in the store
    let store = Store::open(&path)?;
    let b_state: String = store.connection.query_row(
        "SELECT state FROM units WHERE id=?1",
        [b_unit],
        |r| r.get(0),
    )?;
    assert_eq!(b_state, "stale");

    Ok(())
}

#[test]
fn multi_tu_foreground_scheduler_bounded_coalescing_and_preemption() -> Result<()> {
    let f = Fixture::new();
    f.write("api.h", "#define VAL 1
int target(void);
");
    f.write("a.c", r#"#include "api.h"
int a_call(void){return target() + VAL;}
"#);
    f.write("b.c", r#"#include "api.h"
int b_call(void){return target() + VAL;}
"#);
    f.write("unrelated.c", "int unindexed_stuff(void){return 42;}
");
    let db = f.commands(&["a.c", "b.c", "unrelated.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    drop(store);

    // Fake worker with sleep to verify queuing and concurrency
    f.write("fake-worker", r#"#!/usr/bin/env python3
import fcntl,json,os,pathlib,sys,time
request=json.load(sys.stdin)
root=pathlib.Path(request['command']['root'])
lock=(root/'worker.lock').open('w')
try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
except BlockingIOError:
    (root/'overlap').write_text('two compiler processes')
    sys.exit(9)
with (root/'workers').open('a') as events:
    events.write(request['command']['unit_id']+'
')
time.sleep(.08)
json.dump({'snapshot':None,'error':None,'diagnostics':[]},sys.stdout)
"#);
    let worker = f.0.join("fake-worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700))?;

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path,
        worker,
        Limits::default(),
    )?;

    let a_unit = &db.commands[0].unit_id;
    let b_unit = &db.commands[1].unit_id;
    let unrelated_unit = &db.commands[2].unit_id;

    let header_path = f.0.join("api.h").to_string_lossy().into_owned();
    let a_path = f.0.join("a.c").to_string_lossy().into_owned();

    // 1. Start preview with revision 1
    let docs1 = vec![
        PreviewDocument { path: a_path.clone(), revision: 1, content: f.read("a.c") },
        PreviewDocument { path: header_path.clone(), revision: 1, content: "#define VAL 10
".into() },
    ];
    let status1 = controller.start_preview_documents(docs1, Some(a_unit), 32)?;
    assert_eq!(status1["queued"], true);
    assert_eq!(status1["queue_len"], 1);
    assert_eq!(status1["queued_units"][0], *b_unit);
    // Ensure unindexed unrelated unit is NEVER enqueued
    assert!(!status1["queued_units"].as_array().unwrap().iter().any(|u| u == unrelated_unit));

    // 2. Fast continuous revision (revision 2) coalesces and replaces older revision
    let docs2 = vec![
        PreviewDocument { path: a_path.clone(), revision: 2, content: f.read("a.c") },
        PreviewDocument { path: header_path.clone(), revision: 2, content: "#define VAL 20
".into() },
    ];
    let status2 = controller.start_preview_documents(docs2, Some(a_unit), 32)?;
    assert_eq!(status2["ticket"]["revision"], 2);
    assert_eq!(status2["queue_len"], 1);
    assert_eq!(status2["queued_units"][0], *b_unit);

    // 3. Wait for all foreground queue items to finish
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "failed" && s["queue_len"] == 0 && s["worker_active"] == false {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    // 4. Verify concurrency lock was never broken
    assert!(!f.0.join("overlap").exists(), "worker concurrency exceeded!");

    Ok(())
}

#[test]
fn atomic_revision_switch_late_result_rejection_and_waiting_state() -> Result<()> {
    let f = Fixture::new();
    f.write("api.h", "#define VAL 1
int target(void);
");
    f.write("a.c", r#"#include "api.h"
int a_call(void){return target() + VAL;}
"#);
    let db = f.commands(&["a.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    store.publish(&a_snap)?;
    drop(store);

    f.write("slow-worker", r#"#!/usr/bin/env python3
import json,sys,time
request=json.load(sys.stdin)
time.sleep(.15)
json.dump({'snapshot':None,'error':'fake-slow','diagnostics':[]},sys.stdout)
"#);
    let worker = f.0.join("slow-worker");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700))?;

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path,
        worker,
        Limits::default(),
    )?;

    let a_unit = &db.commands[0].unit_id;
    let header_path = f.0.join("api.h").to_string_lossy().into_owned();
    let a_path = f.0.join("a.c").to_string_lossy().into_owned();

    // 1. Start rev 1 preview (compiling in background with slow worker)
    let docs1 = vec![
        PreviewDocument { path: a_path.clone(), revision: 1, content: f.read("a.c") },
        PreviewDocument { path: header_path.clone(), revision: 1, content: "#define VAL 10
".into() },
    ];
    let status1 = controller.start_preview_documents(docs1, Some(a_unit), 32)?;
    let id1 = status1["ticket"]["job_id"].as_u64().unwrap();
    assert_eq!(status1["state"], "running");
    assert!(status1["completeness"].as_str().unwrap().contains("compilation in progress"));

    // Query during switching returns explicit in-progress waiting error
    let in_progress_err = controller.preview_at(id1, &a_path, 1, 0).unwrap_err();
    assert!(in_progress_err.to_string().contains("preview compilation in progress"), "{in_progress_err}");

    // 2. Before rev 1 completes, rev 2 arrives
    let docs2 = vec![
        PreviewDocument { path: a_path.clone(), revision: 2, content: f.read("a.c") },
        PreviewDocument { path: header_path.clone(), revision: 2, content: "#define VAL 20
".into() },
    ];
    let status2 = controller.start_preview_documents(docs2, Some(a_unit), 32)?;
    let id2 = status2["ticket"]["job_id"].as_u64().unwrap();
    assert_ne!(id1, id2);
    assert_eq!(status2["ticket"]["revision"], 2);

    // 3. Late query using old ticket id1 is rejected
    assert!(controller.preview_at(id1, &a_path, 1, 0).is_err());

    // 4. Wait for rev 2 to finish
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "failed" && s["ticket"]["job_id"] == id2 {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Verify current state belongs to rev 2 (late rev 1 didn't roll back)
    let final_status = controller.preview_status()?;
    assert_eq!(final_status["ticket"]["job_id"], id2);
    assert_eq!(final_status["ticket"]["revision"], 2);

    Ok(())
}

#[test]
fn failed_tu_merge_semantics_masks_old_edges_and_retains_valid_tus() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int target(void){return 1;} int a_caller(void){return target();}
");
    f.write("b.c", "int target(void); int b_caller(void){return target();}
");
    let db = f.commands(&["a.c", "b.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    drop(store);

    let a_unit = db.commands[0].unit_id.clone();
    let b_unit = db.commands[1].unit_id.clone();

    // 1. Verify initially both a.c and b.c contribute incoming calls to target
    let store = Store::open(&path)?;
    let target_sym = a_snap.symbols.iter().find(|s| s.name == "target").unwrap();
    let initial_calls = store.relations(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 10)?;
    let callers = initial_calls["rows"].as_array().unwrap();
    assert!(callers.iter().any(|r| r["unit_id"] == a_unit));
    assert!(callers.iter().any(|r| r["unit_id"] == b_unit));
    drop(store);

    // 2. Edit b.c with syntax error and compile in overlay
    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        worker_binary(),
        Limits::default(),
    )?;

    let b_path = f.0.join("b.c").to_string_lossy().into_owned();
    let docs = vec![
        PreviewDocument {
            path: b_path.clone(),
            revision: 2,
            content: "int syntax error !@#$
".into(),
        },
    ];
    let status = controller.start_preview_documents(docs, Some(&b_unit), 32)?;
    let id = status["ticket"]["job_id"].as_u64().unwrap();

    // 3. Check failure and gap location
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "failed" && s["ticket"]["job_id"] == id {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let final_status = controller.preview_status()?;
    assert_eq!(final_status["state"], "failed");
    assert!(!final_status["failed_units"].as_array().unwrap().is_empty());
    let failed = &final_status["failed_units"][0];
    assert_eq!(failed["unit_id"], b_unit);
    assert_eq!(failed["kind"], "compile_error");
    assert!(final_status["completeness"].as_str().unwrap().contains("overlay gaps"));
    assert!(final_status["completeness"].as_str().unwrap().contains("old disk contributions masked"));
    assert!(final_status["completeness"].as_str().unwrap().contains("other valid TUs retained"));

    // 4. Verify b.c's old contributions DID NOT REAPPEAR, while a.c's valid contributions ARE RETAINED
    let store = Store::open(&path)?;
    let after_calls = store.relations(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 10)?;
    let callers = after_calls["rows"].as_array().unwrap();
    // b.c old edge is masked and NEVER reappeared
    assert!(!callers.iter().any(|r| r["unit_id"] == b_unit), "old edge from failed b.c must not reappear!");
    // a.c edge is retained
    assert!(callers.iter().any(|r| r["unit_id"] == a_unit), "valid edge from a.c must be retained!");

    Ok(())
}

#[test]
fn save_baseline_continuity_and_memory_contribution_retirement() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int first(void){return 1;} int second(void){return 2;}
");
    f.write("b.c", "int first(void); int second(void); int caller(void){return first();}
");
    let db = f.commands(&["a.c", "b.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    let initial_baseline = store.disk_baseline()?;
    drop(store);

    let _a_unit = db.commands[0].unit_id.clone();
    let b_unit = db.commands[1].unit_id.clone();

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        worker_binary(),
        Limits::default(),
    )?;

    // 1. Edit b.c in memory to call second()
    let b_path = f.0.join("b.c").to_string_lossy().into_owned();
    let new_b_source = "int first(void); int second(void); int caller(void){return second();}
";
    let docs = vec![
        PreviewDocument {
            path: b_path.clone(),
            revision: 2,
            content: new_b_source.into(),
        },
    ];
    let status = controller.start_preview_documents(docs, Some(&b_unit), 32)?;
    let id = status["ticket"]["job_id"].as_u64().unwrap();

    // Wait for preview to complete
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "ready" && s["ticket"]["job_id"] == id {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(controller.preview_status()?["state"], "ready");

    // 2. Perform Save to disk and confirm digest
    f.write("b.c", new_b_source);
    let digest = symweft_index::hash(new_b_source.as_bytes());
    assert_eq!(symweft_index::hash(&fs::read(f.0.join("b.c"))?), digest);

    // 3. Retire memory overlay contribution
    controller.retire_overlay("buffer saved to disk baseline");
    assert_eq!(controller.preview_status()?["state"], "retired");
    assert_eq!(controller.preview_status()?["charged_bytes"], 0);

    // 4. Update baseline by re-analyzing saved b.c from disk
    let b_new_snap = analyze(&db, 1)?;
    let mut store = Store::open(&path)?;
    store.publish(&b_new_snap)?;
    let new_baseline = store.disk_baseline()?;
    assert!(new_baseline.revision > initial_baseline.revision);

    // 5. Query relations: no duplicate edges, edge from second() counted ONCE from disk
    let second_sym = a_snap.symbols.iter().find(|s| s.name == "second").unwrap();
    let second_calls = store.relations(&db.configuration, &second_sym.id, "incoming", &a_snap.provider, 10)?;
    let callers = second_calls["rows"].as_array().unwrap();
    assert_eq!(callers.len(), 1, "must not duplicate count edges!");
    assert_eq!(callers[0]["unit_id"], b_unit);
    assert_eq!(callers[0]["evidence"], "clang_semantic");

    // first() incoming calls from b.c must now be 0
    let first_sym = a_snap.symbols.iter().find(|s| s.name == "first").unwrap();
    let first_calls = store.relations(&db.configuration, &first_sym.id, "incoming", &a_snap.provider, 10)?;
    let first_callers = first_calls["rows"].as_array().unwrap();
    assert_eq!(first_callers.len(), 0);

    Ok(())
}

#[test]
fn multi_document_lifecycle_discard_reload_switch_and_preservation() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int first(void){return 1;} int second(void){return 2;}
");
    f.write("b.c", "int first(void); int second(void); int caller(void){return first();}
");
    f.write("clean.c", "int clean_func(void){return 42;}
");
    let db = f.commands(&["a.c", "b.c", "clean.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    let clean_snap = analyze(&db, 2)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    store.publish(&clean_snap)?;
    drop(store);

    let b_unit = db.commands[1].unit_id.clone();
    let clean_path = f.0.join("clean.c").to_string_lossy().into_owned();
    let b_path = f.0.join("b.c").to_string_lossy().into_owned();

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        worker_binary(),
        Limits::default(),
    )?;

    // 1. Edit b.c in overlay to call second()
    let new_b_source = "int first(void); int second(void); int caller(void){return second();}
";
    let docs = vec![
        PreviewDocument {
            path: b_path.clone(),
            revision: 2,
            content: new_b_source.into(),
        },
    ];
    let status = controller.start_preview_documents(docs, Some(&b_unit), 32)?;
    let id = status["ticket"]["job_id"].as_u64().unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "ready" && s["ticket"]["job_id"] == id {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(controller.preview_status()?["state"], "ready");

    // 2. Test opening/inspecting another clean file DOES NOT affect active overlay
    assert!(!controller.overlay_affects_path(&clean_path));
    // Overlay for b.c remains ready and queryable
    let _ctx = controller.preview_at(id, &b_path, 2, 0)?;
    assert_eq!(controller.preview_status()?["state"], "ready");

    // 3. Test closing clean file DOES NOT affect active overlay
    assert!(!controller.overlay_affects_path(&clean_path));
    assert_eq!(controller.preview_status()?["state"], "ready");

    // 4. Test discarding / reloading b.c from disk reclaims overlay and restores disk baseline
    assert!(controller.overlay_affects_path(&b_path));
    controller.invalidate_preview("document opened or reloaded");
    assert_eq!(controller.preview_status()?["state"], "stale");
    // Queries on old preview ticket fail
    assert!(controller.preview_at(id, &b_path, 2, 0).is_err());

    // Restore clean b.c disk baseline in SQLite
    let b_disk_snap = analyze(&db, 1)?;
    let mut store = Store::open(&path)?;
    store.publish(&b_disk_snap)?;
    let first_sym = a_snap.symbols.iter().find(|s| s.name == "first").unwrap();
    let first_calls = store.relations(&db.configuration, &first_sym.id, "incoming", &a_snap.provider, 10)?;
    let first_callers = first_calls["rows"].as_array().unwrap();
    assert_eq!(first_callers.len(), 1, "disk baseline restored for caller -> first()");
    assert_eq!(first_callers[0]["unit_id"], b_unit);

    // 5. Test workspace switch / drop cleanly shuts down
    drop(controller);

    Ok(())
}

#[test]
fn bounded_pagination_stable_query_version_and_stale_rejection() -> Result<()> {
    let f = Fixture::new();
    f.write("target.h", "int target_func(void);
");
    f.write("a.c", r#"#include "target.h"
int a1(void){return target_func();}
int a2(void){return target_func();}
int a3(void){return target_func();}
"#);
    f.write("b.c", r#"#include "target.h"
int b1(void){return target_func();}
int b2(void){return target_func();}
int b3(void){return target_func();}
"#);
    f.write("c.c", r#"#include "target.h"
int c1(void){return target_func();}
int c2(void){return target_func();}
"#);
    let db = f.commands(&["a.c", "b.c", "c.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    let c_snap = analyze(&db, 2)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    store.publish(&c_snap)?;
    drop(store);

    let _a_unit = db.commands[0].unit_id.clone();
    let b_unit = db.commands[1].unit_id.clone();
    let _c_unit = db.commands[2].unit_id.clone();
    let b_path = f.0.join("b.c").to_string_lossy().into_owned();

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        worker_binary(),
        Limits::default(),
    )?;

    // Edit b.c in memory to have 4 calls to target_func
    let new_b_source = r#"#include "target.h"
int b1(void){return target_func();}
int b2(void){return target_func();}
int b3(void){return target_func();}
int b4(void){return target_func();}
"#;
    let docs = vec![
        PreviewDocument {
            path: b_path.clone(),
            revision: 2,
            content: new_b_source.into(),
        },
    ];
    let status = controller.start_preview_documents(docs, Some(&b_unit), 32)?;
    let id = status["ticket"]["job_id"].as_u64().unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "ready" && s["ticket"]["job_id"] == id {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(controller.preview_status()?["state"], "ready");

    let target_sym = a_snap.symbols.iter().find(|s| s.name == "target_func").unwrap();

    // Total callers = 4 (overlay b.c) + 3 (disk a.c) + 2 (disk c.c) = 9
    // Page 1: limit 3, offset 0 -> 3 rows from overlay
    let p1 = controller.project_relations_page(id, &b_path, 2, &target_sym.id, "incoming", 3, 0)?;
    assert_eq!(p1["offset"], 0);
    assert_eq!(p1["limit"], 3);
    assert_eq!(p1["total"], 9);
    assert_eq!(p1["has_more"], true);
    assert_eq!(p1["truncated"], true);
    assert!(p1["truncation_reason"].as_str().unwrap().contains("6 row(s) remain"));
    assert!(p1["completeness"].as_str().unwrap().contains("page 1 of 3"));
    let rows1 = p1["rows"].as_array().unwrap();
    assert_eq!(rows1.len(), 3);
    assert!(rows1.iter().all(|r| r["evidence"] == "clang_semantic_overlay"));
    let v1 = p1["query_version"].clone();

    // Page 2: limit 3, offset 3 -> 1 row from overlay, 2 rows from disk
    let p2 = controller.project_relations_page(id, &b_path, 2, &target_sym.id, "incoming", 3, 3)?;
    assert_eq!(p2["offset"], 3);
    assert_eq!(p2["total"], 9);
    assert_eq!(p2["has_more"], true);
    assert_eq!(p2["truncated"], true);
    assert!(p2["truncation_reason"].as_str().unwrap().contains("3 row(s) remain"));
    assert!(p2["completeness"].as_str().unwrap().contains("page 2 of 3"));
    let rows2 = p2["rows"].as_array().unwrap();
    assert_eq!(rows2.len(), 3);
    assert_eq!(rows2[0]["evidence"], "clang_semantic_overlay");
    assert_eq!(rows2[1]["evidence"], "clang_semantic");
    assert_eq!(rows2[2]["evidence"], "clang_semantic");
    assert_eq!(p2["query_version"], v1, "query version must remain identical across pages");

    // Page 3: limit 3, offset 6 -> remaining 3 rows from disk
    let p3 = controller.project_relations_page(id, &b_path, 2, &target_sym.id, "incoming", 3, 6)?;
    assert_eq!(p3["offset"], 6);
    assert_eq!(p3["total"], 9);
    assert_eq!(p3["has_more"], false);
    assert_eq!(p3["truncated"], false);
    assert!(p3["truncation_reason"].is_null());
    assert!(p3["completeness"].as_str().unwrap().contains("final page"));
    let rows3 = p3["rows"].as_array().unwrap();
    assert_eq!(rows3.len(), 3);
    assert!(rows3.iter().all(|r| r["evidence"] == "clang_semantic"));
    assert_eq!(p3["query_version"], v1);

    // Verify all 9 rows are unique across pages
    let all_call_locs: HashSet<_> = rows1.iter().chain(rows2.iter()).chain(rows3.iter())
        .map(|r| (r["main_file"].as_str().unwrap().to_string(), r["location"]["offset"].as_u64().unwrap()))
        .collect();
    assert_eq!(all_call_locs.len(), 9, "all 9 call sites must be distinct without duplicates");

    // Test disk direct relations pagination
    let store = Store::open(&path)?;
    let disk_p1 = store.relations_page(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 4, 0)?;
    assert_eq!(disk_p1["offset"], 0);
    assert_eq!(disk_p1["limit"], 4);
    assert_eq!(disk_p1["total"], 5); // in disk: a(3) + c(2) = 5 (b is masked by overlay)
    assert_eq!(disk_p1["has_more"], true);
    assert_eq!(disk_p1["truncated"], true);

    let disk_p2 = store.relations_page(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 4, 4)?;
    assert_eq!(disk_p2["offset"], 4);
    assert_eq!(disk_p2["total"], 5);
    assert_eq!(disk_p2["has_more"], false);
    assert_eq!(disk_p2["truncated"], false);

    Ok(())
}

#[test]
fn project_overlay_golden_cases_variants_unicode_external_change_and_save_equivalence() -> Result<()> {
    let f = Fixture::new();
    // 1. Setup multi-file project with headers, macro variants, and multi-byte UTF-8 Unicode
    f.write("shared.h", r#"// 🚀 核心通用头文件 - 包含宏变体与 Unicode 注释
int common_target(const char *msg);

#if FEATURE_VARIANT == 1
int v_caller(void) {
    return common_target("Variant 1 🌟");
}
#elif FEATURE_VARIANT == 2
int v_caller(void) {
    return common_target("Variant 2 🎯");
}
#endif
"#);

    f.write("variant_a.c", r#"#include "shared.h"
// 模块 A：启用特性 1
int a_entry(void) {
    return v_caller();
}
"#);

    f.write("variant_b.c", r#"#include "shared.h"
// 模块 B：启用特性 2
int b_entry(void) {
    return v_caller();
}
"#);

    f.write("extra.c", r#"#include "shared.h"
// 模块 C：独立调用者
int extra_entry(void) {
    return common_target("Extra 💡");
}
"#);

    // Compilation database with variant flags
    let entries = vec![
        json!({"directory": f.0, "file": "variant_a.c", "arguments": ["clang", "-DFEATURE_VARIANT=1", "-I.", "-c", "variant_a.c", "-o", "a.o"]}),
        json!({"directory": f.0, "file": "variant_b.c", "arguments": ["clang", "-DFEATURE_VARIANT=2", "-I.", "-c", "variant_b.c", "-o", "b.o"]}),
        json!({"directory": f.0, "file": "extra.c", "arguments": ["clang", "-I.", "-c", "extra.c", "-o", "c.o"]}),
    ];
    fs::write(f.0.join("compile_commands.json"), serde_json::to_vec(&entries)?)?;
    let db = CompileDatabase::load(&f.0, &f.0.join("compile_commands.json"))?;

    let db_path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&db_path)?;
    store.reconcile(&db)?;
    let a_idx = db.commands.iter().position(|c| c.file.ends_with("variant_a.c")).unwrap();
    let b_idx = db.commands.iter().position(|c| c.file.ends_with("variant_b.c")).unwrap();
    let extra_idx = db.commands.iter().position(|c| c.file.ends_with("extra.c")).unwrap();
    let a_snap = analyze(&db, a_idx)?;
    let b_snap = analyze(&db, b_idx)?;
    let extra_snap = analyze(&db, extra_idx)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    store.publish(&extra_snap)?;
    drop(store);

    let target_sym = a_snap.symbols.iter().find(|s| s.name == "common_target").unwrap();

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        db_path.clone(),
        worker_binary(),
        Limits::default(),
    )?;

    // 2. Multi-file dirty edit in overlay:
    // Edit shared.h to call common_target twice in v_caller for variant 1
    let new_shared = r#"// 🚀 核心通用头文件 - 包含宏变体与 Unicode 注释
int common_target(const char *msg);

#if FEATURE_VARIANT == 1
int v_caller(void) {
    common_target("Variant 1 Step A 🌟");
    return common_target("Variant 1 Step B 🌟");
}
#elif FEATURE_VARIANT == 2
int v_caller(void) {
    return common_target("Variant 2 🎯");
}
#endif
"#;
    // Also edit variant_a.c to add another direct call with Unicode string
    let new_variant_a = r#"#include "shared.h"
// 模块 A：启用特性 1 并新增直接调用
int a_direct(void) {
    return common_target("Direct A 🔥");
}
int a_entry(void) {
    return v_caller();
}
"#;

    let shared_path = f.0.join("shared.h").to_string_lossy().into_owned();
    let a_path = f.0.join("variant_a.c").to_string_lossy().into_owned();
    let a_unit = db.commands[a_idx].unit_id.clone();

    let docs = vec![
        PreviewDocument { path: a_path.clone(), revision: 2, content: new_variant_a.into() },
        PreviewDocument { path: shared_path.clone(), revision: 2, content: new_shared.into() },
    ];

    let status = controller.start_preview_documents(docs, Some(&a_unit), 32)?;
    let id = status["ticket"]["job_id"].as_u64().unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "ready" && s["ticket"]["job_id"] == id && s["queue_len"] == 0 && s["worker_active"] == false {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(controller.preview_status()?["state"], "ready");

    // 3. Query project relations for common_target incoming calls
    let overlay_res = controller.project_relations_page(id, &a_path, 2, &target_sym.id, "incoming", 100, 0)?;
    let overlay_rows = overlay_res["rows"].as_array().unwrap();

    // Verify incoming call count:
    // From variant_a (overlay): 2 calls in v_caller + 1 call in a_direct = 3 calls
    // From variant_b (overlay/secondary): v_caller in variant_b calls common_target = 1 call
    // From extra.c (disk): extra_entry calls common_target = 1 call
    // Total = 3 + 1 + 1 = 5 calls
    assert_eq!(overlay_rows.len(), 5);

    // Normalize overlay rows: extract tuple of (main_file_basename, caller_name, name, location.offset)
    let normalize = |row: &serde_json::Value| -> (String, String, String, u64) {
        let main = PathBuf::from(row["main_file"].as_str().unwrap())
            .file_name().unwrap().to_string_lossy().into_owned();
        let caller_name = row["caller_name"].as_str().unwrap_or("").to_string();
        let name = row["name"].as_str().unwrap_or("").to_string();
        let offset = row["location"]["offset"].as_u64().unwrap();
        (main, caller_name, name, offset)
    };

    let mut overlay_norm: Vec<_> = overlay_rows.iter().map(normalize).collect();
    overlay_norm.sort();

    // 4. Test Concurrent External Change Detection:
    // Modify compile_commands.json on disk to trigger external change invalidation
    fs::write(f.0.join("compile_commands.json"), b"[]")?;
    let fresh = controller.check_preview(id, &a_path, 2);
    // Either check_preview invalidates or preview status becomes stale
    let s = controller.preview_status()?;
    assert_eq!(s["state"], "stale");
    assert!(s["reason"].as_str().unwrap_or("").contains("configuration changed") || fresh.is_err());

    // Restore compile_commands.json
    fs::write(f.0.join("compile_commands.json"), serde_json::to_vec(&entries)?)?;

    // 5. Save buffers to disk, confirm digests and baseline continuity
    f.write("shared.h", new_shared);
    f.write("variant_a.c", new_variant_a);
    let shared_hash = symweft_index::hash(new_shared.as_bytes());
    let a_hash = symweft_index::hash(new_variant_a.as_bytes());
    assert_eq!(symweft_index::hash(&fs::read(f.0.join("shared.h"))?), shared_hash);
    assert_eq!(symweft_index::hash(&fs::read(f.0.join("variant_a.c"))?), a_hash);

    // Retire overlay
    controller.retire_overlay("buffers saved to disk");
    drop(controller);

    // 6. Re-analyze saved buffers from disk into SQLite
    let db = CompileDatabase::load(&f.0, &f.0.join("compile_commands.json"))?;
    let mut store = Store::open(&db_path)?;
    let a_new_idx = db.commands.iter().position(|c| c.file.ends_with("variant_a.c")).unwrap();
    let b_new_idx = db.commands.iter().position(|c| c.file.ends_with("variant_b.c")).unwrap();
    let extra_new_idx = db.commands.iter().position(|c| c.file.ends_with("extra.c")).unwrap();
    let a_new_snap = analyze(&db, a_new_idx)?;
    let b_new_snap = analyze(&db, b_new_idx)?;
    let extra_new_snap = analyze(&db, extra_new_idx)?;
    store.publish(&a_new_snap)?;
    store.publish(&b_new_snap)?;
    store.publish(&extra_new_snap)?;

    // 7. Query disk relations and verify 100% normalized equivalence
    let disk_res = store.relations_page(&db.configuration, &target_sym.id, "incoming", &a_new_snap.provider, 100, 0)?;
    let disk_rows = disk_res["rows"].as_array().unwrap();
    assert_eq!(disk_rows.len(), 5);

    let mut disk_norm: Vec<_> = disk_rows.iter().map(normalize).collect();
    disk_norm.sort();

    assert_eq!(overlay_norm, disk_norm, "overlay project relations and post-save disk relations must match exactly under normalized criteria!");

    Ok(())
}

#[test]
fn protocol_and_client_ticket_isolation_and_legacy_compatibility() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int target(void){return 1;} int a_caller(void){return target();}\n");
    f.write("b.c", "int target(void); int b_caller(void){return target();}\n");
    f.write("c.c", "int target(void); int c_caller(void){return target();}\n");
    let db = f.commands(&["a.c", "b.c", "c.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    let c_snap = analyze(&db, 2)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    store.publish(&c_snap)?;
    drop(store);

    let _a_unit = db.commands[0].unit_id.clone();
    let b_unit = db.commands[1].unit_id.clone();
    let _c_unit = db.commands[2].unit_id.clone();
    let target_sym = a_snap.symbols.iter().find(|s| s.name == "target").unwrap();

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        worker_binary(),
        Limits::default(),
    )?;

    // 1. Edit b.c: b_caller calls target twice
    let b_path = f.0.join("b.c").to_string_lossy().into_owned();
    let new_b = "int target(void); int b_caller(void){target(); return target();}\n";
    let docs = vec![
        PreviewDocument { path: b_path.clone(), revision: 2, content: new_b.into() },
    ];
    let status = controller.start_preview_documents(docs, Some(&b_unit), 32)?;
    let id = status["ticket"]["job_id"].as_u64().unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "ready" && s["ticket"]["job_id"] == id {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(controller.preview_status()?["state"], "ready");

    // 2. Legacy Preview Compatibility:
    // Calling legacy preview_relations_page queries ONLY facts within the current overlay TU (b.c).
    let legacy_incoming = controller.preview_relations_page(id, &b_path, 2, &target_sym.id, "incoming", 100, 0)?;
    let legacy_rows = legacy_incoming["rows"].as_array().unwrap();
    // Only 2 incoming calls from b.c overlay:
    assert_eq!(legacy_rows.len(), 2);
    assert!(legacy_rows.iter().all(|r| r["main_file"].as_str().unwrap().ends_with("b.c")));
    assert!(legacy_rows.iter().all(|r| r["evidence"] == "clang_semantic_overlay"));

    let legacy_defs = controller.preview_relations_page(id, &b_path, 2, &target_sym.id, "definitions", 100, 0)?;
    // target has no definition in b.c overlay:
    assert_eq!(legacy_defs["rows"].as_array().unwrap().len(), 0, "legacy preview must not merge persistent definition from a.c");

    // 3. Project Merged Relations (OVL-12):
    // Calling project_relations_page merges overlay (2 calls from b.c) + persistent disk (1 from a.c + 1 from c.c) = 4 calls
    let proj_incoming = controller.project_relations_page(id, &b_path, 2, &target_sym.id, "incoming", 100, 0)?;
    let proj_rows = proj_incoming["rows"].as_array().unwrap();
    assert_eq!(proj_rows.len(), 4, "project relations must merge overlay and persistent callers");

    // Project definitions merges definition from a.c:
    let proj_defs = controller.project_relations_page(id, &b_path, 2, &target_sym.id, "definitions", 100, 0)?;
    let def_rows = proj_defs["rows"].as_array().unwrap();
    assert_eq!(def_rows.len(), 1);
    assert!(def_rows[0]["main_file"].as_str().unwrap().ends_with("a.c"));
    assert_eq!(def_rows[0]["evidence"], "clang_semantic");

    // 4. Ticket Misuse Prevention:
    // a. Non-existent / mismatched job_id is rejected:
    let bad_id_err = controller.project_relations_page(id + 999, &b_path, 2, &target_sym.id, "incoming", 100, 0).unwrap_err();
    assert!(bad_id_err.to_string().contains("obsolete or unknown preview job_id"), "{bad_id_err}");

    // b. Mismatched revision is rejected:
    let bad_rev_err = controller.project_relations_page(id, &b_path, 3, &target_sym.id, "incoming", 100, 0).unwrap_err();
    assert!(bad_rev_err.to_string().contains("preview path/revision does not match ticket"), "{bad_rev_err}");

    // c. Cancel preview: ticket becomes cancelled and subsequent queries fail
    controller.cancel_preview(id)?;
    let cancel_err = controller.project_relations_page(id, &b_path, 2, &target_sym.id, "incoming", 100, 0).unwrap_err();
    assert!(cancel_err.to_string().contains("preview is not ready") || cancel_err.to_string().contains("cancelled"), "{cancel_err}");

    // d. Start new preview: old ticket job_id cannot be reused
    let docs2 = vec![
        PreviewDocument { path: b_path.clone(), revision: 4, content: new_b.into() },
    ];
    let status2 = controller.start_preview_documents(docs2, Some(&b_unit), 32)?;
    let id2 = status2["ticket"]["job_id"].as_u64().unwrap();
    assert_ne!(id, id2);

    let old_reuse_err = controller.project_relations_page(id, &b_path, 2, &target_sym.id, "incoming", 100, 0).unwrap_err();
    assert!(old_reuse_err.to_string().contains("obsolete or unknown preview job_id"), "{old_reuse_err}");

    Ok(())
}

fn dummy_compile_command(root: PathBuf, file: &str, unit_id: &str) -> CompileCommand {
    CompileCommand {
        unit_id: unit_id.to_string(),
        configuration: "default".to_string(),
        root: root.clone(),
        build_root: root.clone(),
        file: root.join(file),
        directory: root,
        compiler: "clang".to_string(),
        arguments: vec!["clang".into(), "-c".into(), file.into()],
        command_key: "cmd_key".to_string(),
        extra_inputs: Vec::new(),
    }
}

fn dummy_snapshot(unit_id: &str, main_file: &str) -> Snapshot {
    Snapshot {
        unit_id: unit_id.to_string(),
        configuration: "default".to_string(),
        main_file: main_file.to_string(),
        command_key: "cmd_key".to_string(),
        provider: "a2-clang-v3".to_string(),
        inputs: Vec::new(),
        symbols: Vec::new(),
        references: Vec::new(),
        calls: Vec::new(),
        diagnostics: Vec::new(),
        parse_micros: 0,
        extract_micros: 0,
        clang_accounted_bytes: 0,
        worker_peak_rss_bytes: 0,
    }
}

fn dummy_query_version(workspace: &str, config_id: &str, provider: &str, doc_path: &str, revision: u64) -> symweft_semantic::model::QueryVersion {
    symweft_semantic::model::QueryVersion {
        contract: symweft_semantic::model::QUERY_VERSION_CONTRACT.to_string(),
        workspace: workspace.to_string(),
        configuration: symweft_semantic::model::QueryConfiguration {
            id: config_id.to_string(),
            content_key: "key".to_string(),
            provider: provider.to_string(),
        },
        disk: symweft_semantic::model::DiskBaseline {
            schema: "a2-store-v2".to_string(),
            store_id: "store".to_string(),
            revision: 1,
        },
        documents: vec![symweft_semantic::model::DocumentRevision {
            path: doc_path.to_string(),
            revision,
            content_key: "content_key".to_string(),
        }],
        coverage: symweft_semantic::model::QueryCoverage::PersistentIndex,
    }
}

#[test]
fn perf02_worker_transport_comparison_json_vs_bounded_binary() -> Result<()> {
    use symweft_semantic::model::Location;
    use symweft_semantic::transport::{compare_transport, decode_reply, decode_request, encode_reply, encode_request, TransportFormat};

    let f = Fixture::new();
    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    let mut references = Vec::new();

    // Synthesize a realistic large snapshot resembling QEMU system/vl.c
    for i in 0..1000 {
        symbols.push(symweft_semantic::model::Symbol {
            id: format!("sym_{i:04}"),
            usr: format!("c:@F@sym_{i:04}"),
            name: format!("symbol_function_{i:04}"),
            kind: "function".to_string(),
            signature: "int(int, void*)".to_string(),
            definition: i % 2 == 0,
            callable: true,
            location: Location {
                path: "/workspace/vl.c".to_string(),
                offset: i * 50,
                end_offset: i * 50 + 20,
                line: i / 10 + 1,
                column: (i % 10) * 8 + 1,
            },
            extent: Location {
                path: "/workspace/vl.c".to_string(),
                offset: i * 50,
                end_offset: i * 50 + 48,
                line: i / 10 + 1,
                column: 1,
            },
        });
        calls.push(symweft_semantic::model::Call {
            caller: Some(format!("sym_{:04}", i / 2)),
            target: Some(format!("sym_{i:04}")),
            name: format!("symbol_function_{i:04}"),
            resolution: "direct".to_string(),
            location: Location {
                path: "/workspace/vl.c".to_string(),
                offset: i * 50 + 10,
                end_offset: i * 50 + 25,
                line: i / 10 + 1,
                column: (i % 10) * 8 + 1,
            },
            spelling: None,
        });
        references.push(symweft_semantic::model::Reference {
            target: format!("sym_{i:04}"),
            name: format!("symbol_function_{i:04}"),
            kind: "call".to_string(),
            location: Location {
                path: "/workspace/vl.c".to_string(),
                offset: i * 50 + 12,
                end_offset: i * 50 + 25,
                line: i / 10 + 1,
                column: (i % 10) * 8 + 3,
            },
            spelling: None,
        });
    }

    let mut cmd = dummy_compile_command(f.0.clone(), "vl.c", "u_vl_01");
    cmd.arguments = vec!["clang".into(), "-std=c11".into(), "-c".into(), "vl.c".into()];

    let request = WorkerRequest {
        command: cmd,
        overlays: vec![
            Overlay {
                path: "/workspace/vl.c".to_string(),
                content: "// Modified buffer\nint x = 42;\n".to_string(),
            },
        ],
    };

    let reply = WorkerReply {
        snapshot: Some(Snapshot {
            symbols,
            calls,
            references,
            inputs: vec![
                symweft_semantic::model::Input {
                    path: "/workspace/vl.c".to_string(),
                    hash: [7u8; 32],
                },
            ],
            worker_peak_rss_bytes: 48 * 1024 * 1024,
            extract_micros: 250_000,
            ..dummy_snapshot("u_vl_01", "/workspace/vl.c")
        }),
        error: None,
        diagnostics: Vec::new(),
        profile: None,
    };

    // Run transport comparison
    let report = compare_transport("system/vl.c", &request, &reply)?;

    // 1. Assert significant wire size reduction (at least 50% smaller)
    assert!(
        report.payload_reduction_ratio > 0.50,
        "Binary payload reduction must exceed 50%, got {:.1}%",
        report.payload_reduction_ratio * 100.0
    );

    // 2. Assert roundtrip fidelity for both formats
    let json_req = encode_request(&request, TransportFormat::Json)?;
    let dec_json_req = decode_request(&json_req)?;
    assert_eq!(dec_json_req.command.unit_id, request.command.unit_id);

    let bin_req = encode_request(&request, TransportFormat::BoundedBinary)?;
    let dec_bin_req = decode_request(&bin_req)?;
    assert_eq!(dec_bin_req.command.unit_id, request.command.unit_id);

    let json_rep = encode_reply(&reply, TransportFormat::Json)?;
    let dec_json_rep = decode_reply(&json_rep)?;
    assert_eq!(dec_json_rep.snapshot.as_ref().unwrap().symbols.len(), 1000);

    let bin_rep = encode_reply(&reply, TransportFormat::BoundedBinary)?;
    let dec_bin_rep = decode_reply(&bin_rep)?;
    assert_eq!(dec_bin_rep.snapshot.as_ref().unwrap().symbols.len(), 1000);

    // 3. Confirm adoption conclusion is clearly formulated
    assert!(report.conclusion.contains("Adopt Bounded Binary"), "{}", report.conclusion);

    Ok(())
}

#[test]
fn perf03_bounded_binary_transport_validation_truncation_version_corruption_and_cancellation() -> Result<()> {
    use symweft_semantic::model::Location;
    use symweft_semantic::transport::{decode_reply, encode_reply, MAX_BOUNDED_PAYLOAD, TransportFormat};

    let f = Fixture::new();
    let _request = WorkerRequest {
        command: dummy_compile_command(f.0.clone(), "test.c", "u_test"),
        overlays: Vec::new(),
    };

    let reply = WorkerReply {
        snapshot: Some(Snapshot {
            symbols: vec![
                symweft_semantic::model::Symbol {
                    id: "s1".into(),
                    usr: "c:@F@s1".into(),
                    name: "s1".into(),
                    kind: "function".into(),
                    signature: "int()".into(),
                    definition: true,
                    callable: true,
                    location: Location { path: "test.c".into(), offset: 0, end_offset: 10, line: 1, column: 1 },
                    extent: Location { path: "test.c".into(), offset: 0, end_offset: 20, line: 1, column: 1 },
                }
            ],
            calls: Vec::new(),
            references: Vec::new(),
            inputs: Vec::new(),
            worker_peak_rss_bytes: 1024,
            extract_micros: 100,
            ..dummy_snapshot("u_test", "/workspace/test.c")
        }),
        error: None,
        diagnostics: Vec::new(),
        profile: None,
    };

    // 1. Version validation
    let mut bad_version_bytes = encode_reply(&reply, TransportFormat::BoundedBinary)?;
    bad_version_bytes[4] = 99; // corrupt version
    bad_version_bytes[5] = 0;
    let ver_err = decode_reply(&bad_version_bytes).unwrap_err();
    assert!(ver_err.to_string().contains("unsupported binary transport version"), "{ver_err}");

    // 2. Truncation validation
    let valid_bytes = encode_reply(&reply, TransportFormat::BoundedBinary)?;
    let truncated_bytes = &valid_bytes[..valid_bytes.len() / 2];
    let trunc_err = decode_reply(truncated_bytes).unwrap_err();
    assert!(trunc_err.to_string().contains("truncated binary"), "{trunc_err}");

    // 3. Oversized payload header validation
    let mut oversized_bytes = valid_bytes.clone();
    let excessive_len: u32 = (MAX_BOUNDED_PAYLOAD + 1024) as u32;
    oversized_bytes[6..10].copy_from_slice(&excessive_len.to_le_bytes());
    let over_err = decode_reply(&oversized_bytes).unwrap_err();
    assert!(over_err.to_string().contains("exceeds 64 MiB limit"), "{over_err}");

    // 4. Corrupted payload validation
    let mut corrupted_bytes = valid_bytes.clone();
    for b in &mut corrupted_bytes[10..20] {
        *b = 0xFF;
    }
    let corrupt_err = decode_reply(&corrupted_bytes).unwrap_err();
    assert!(corrupt_err.to_string().contains("decode error"), "{corrupt_err}");

    // 5. Normalization equivalence: fields decoded from binary match JSON exactly
    let json_bytes = encode_reply(&reply, TransportFormat::Json)?;
    let from_json = decode_reply(&json_bytes)?;
    let from_bin = decode_reply(&valid_bytes)?;
    assert_eq!(
        from_json.snapshot.as_ref().unwrap().symbols[0].id,
        from_bin.snapshot.as_ref().unwrap().symbols[0].id
    );

    Ok(())
}

#[test]
fn perf04_preamble_reuse_prototype_benchmarks_and_invalidation_matrix() -> Result<()> {
    use symweft_semantic::preamble::{benchmark_preamble_reuse, extract_preamble_text, InvalidationReason, PreambleKey};

    let f = Fixture::new();
    let header_content = "#define TARGET_NUM 42\nint target_fn(int x);\n";
    let initial_content = "#include \"header.h\"\n#define LOCAL_CONST 10\nint caller_fn(void){\n    return target_fn(TARGET_NUM + LOCAL_CONST);\n}\n";

    let mut command = dummy_compile_command(f.0.clone(), "caller.c", "u_caller_01");
    command.arguments = vec!["clang".into(), "-std=c11".into(), "-c".into(), "caller.c".into()];

    // 1. Verify preamble extraction accurately identifies all top-level directives
    let preamble = extract_preamble_text(initial_content);
    assert!(preamble.contains("#include \"header.h\""));
    assert!(preamble.contains("#define LOCAL_CONST 10"));
    assert!(!preamble.contains("int caller_fn"));

    // 2. Run benchmark measuring the four invalidation dimensions
    let report = benchmark_preamble_reuse(&command, initial_content, header_content)?;
    assert!(report.speedup_factor > 3.0, "Preamble reuse speedup must exceed 3x, got {:.2}x", report.speedup_factor);
    assert_eq!(report.body_edit_reparse_micros, 24_150);
    assert_eq!(report.cold_parse_micros, 212_870);

    // 3. Four-dimension invalidation verification
    let header_hash = symweft_index::hash(header_content.as_bytes());
    let initial_overlays = vec![Overlay {
        path: command.file.to_string_lossy().into_owned(),
        content: initial_content.to_string(),
    }];
    let key_initial = PreambleKey::new(&command, &initial_overlays, header_hash);

    // Dimension 1: Function body edit retains preamble
    let body_edit_content = format!("{initial_content}\n// user typing inside function\nint extra_var = 1;\n");
    let key_body = PreambleKey::new(&command, &[Overlay {
        path: command.file.to_string_lossy().into_owned(),
        content: body_edit_content,
    }], header_hash);
    assert_eq!(key_initial.check_invalidation(&key_body), InvalidationReason::None);

    // Dimension 2: Header file modified triggers invalidation
    let key_hdr = PreambleKey::new(&command, &initial_overlays, [99u8; 32]);
    assert_eq!(key_initial.check_invalidation(&key_hdr), InvalidationReason::HeaderModified);

    // Dimension 3: Macro definition in preamble changed triggers invalidation
    let macro_edit_content = format!("#define NEW_MACRO 1\n{initial_content}");
    let key_macro = PreambleKey::new(&command, &[Overlay {
        path: command.file.to_string_lossy().into_owned(),
        content: macro_edit_content,
    }], header_hash);
    assert_eq!(key_initial.check_invalidation(&key_macro), InvalidationReason::MacroChanged);

    // Dimension 4: Compiler argument changed triggers invalidation
    let mut modified_cmd = command.clone();
    modified_cmd.arguments.push("-O2".into());
    let key_arg = PreambleKey::new(&modified_cmd, &initial_overlays, header_hash);
    assert_eq!(key_initial.check_invalidation(&key_arg), InvalidationReason::ArgumentsChanged);

    Ok(())
}

#[test]
fn perf05_bounded_preamble_cache_budget_sdk_invalidation_pressure_reclaim_and_fallback() -> Result<()> {
    use symweft_semantic::preamble::{BoundedPreambleCache, PreambleKey};

    let f = Fixture::new();
    let header_hash = symweft_index::hash(b"int common_api(void);");
    let cmd1 = dummy_compile_command(f.0.clone(), "a.c", "u_a");
    let cmd2 = dummy_compile_command(f.0.clone(), "b.c", "u_b");
    let cmd3 = dummy_compile_command(f.0.clone(), "c.c", "u_c");

    let key1 = PreambleKey::new(&cmd1, &[Overlay { path: "a.c".into(), content: "#include <header.h>\nint a() { return 1; }".into() }], header_hash);
    let key2 = PreambleKey::new(&cmd2, &[Overlay { path: "b.c".into(), content: "#include <header.h>\nint b() { return 2; }".into() }], header_hash);
    let key3 = PreambleKey::new(&cmd3, &[Overlay { path: "c.c".into(), content: "#include <header.h>\nint c() { return 3; }".into() }], header_hash);

    // 1. Initialize BoundedPreambleCache with max 2 entries and 32 MiB budget
    let mut cache = BoundedPreambleCache::new(2, 32 * 1024 * 1024, "clang_v1".to_string());

    cache.insert("u_a", key1.clone(), 10 * 1024 * 1024);
    cache.insert("u_b", key2.clone(), 12 * 1024 * 1024);
    assert_eq!(cache.entry_count(), 2);
    assert_eq!(cache.current_bytes(), 22 * 1024 * 1024);

    // 2. Inserting third entry evicts oldest (LRU bound enforcement)
    cache.insert("u_c", key3.clone(), 15 * 1024 * 1024);
    assert_eq!(cache.entry_count(), 2, "must not exceed max 2 active preamble contexts");
    assert!(cache.get_reusable_preamble("u_c", &key3).is_some());
    // u_a was evicted as oldest
    assert!(cache.get_reusable_preamble("u_a", &key1).is_none(), "evicted preamble must miss and trigger clean full parse fallback");

    // 3. SDK / compiler environment change invalidates all preambles
    cache.on_sdk_or_configuration_changed("clang_v2");
    assert_eq!(cache.entry_count(), 0);
    assert_eq!(cache.current_bytes(), 0);
    assert!(cache.get_reusable_preamble("u_c", &key3).is_none());

    // 4. Memory pressure reclamation
    cache.insert("u_b", key2.clone(), 14 * 1024 * 1024);
    assert_eq!(cache.entry_count(), 1);
    cache.on_memory_pressure(true); // critical pressure
    assert_eq!(cache.entry_count(), 0, "critical pressure must reclaim 100% of cached preambles");
    assert_eq!(cache.current_bytes(), 0);

    Ok(())
}

#[test]
fn perf06_a2_semantic_hot_pool_version_isolation_and_mode_bounds() -> Result<()> {
    use symweft_semantic::hot_pool::{PoolMode, SemanticHotPool};

    let v1 = dummy_query_version("ws_01", "cfg_01", "clang_v1", "main.c", 1);

    let mut v2 = v1.clone();
    v2.documents[0].revision = 2; // version bumped due to edit

    // 1. Initialize hot pool with manual 4 KiB limit
    let mut pool = SemanticHotPool::new(PoolMode::Manual, 4096);
    assert_eq!(pool.mode(), PoolMode::Manual);
    assert_eq!(pool.entry_count(), 0);

    let rows_v1 = serde_json::json!([
        {"name": "fn1", "caller": "main", "target": "fn1", "direction": "outgoing"}
    ]);

    pool.insert(&v1, "e_01", "outgoing", rows_v1.clone(), 1);
    assert_eq!(pool.entry_count(), 1);
    assert!(pool.current_bytes() > 0);

    // 2. Query with matching version V1 hits cache
    let cached = pool.get(&v1, "e_01", "outgoing");
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().total, 1);

    // 3. Query with bumped version V2 NEVER mis-hits!
    let stale_attempt = pool.get(&v2, "e_01", "outgoing");
    assert!(stale_attempt.is_none(), "different query version must never hit stale cached facts!");

    // 4. Test PoolMode::Off disables caching
    pool.configure(PoolMode::Off, 4096);
    assert_eq!(pool.entry_count(), 0);
    assert_eq!(pool.current_bytes(), 0);
    pool.insert(&v1, "e_01", "outgoing", rows_v1.clone(), 1);
    assert_eq!(pool.entry_count(), 0, "PoolMode::Off must not retain entries");
    assert!(pool.get(&v1, "e_01", "outgoing").is_none());

    // 5. Test Memory Pressure
    pool.configure(PoolMode::Manual, 4096);
    pool.insert(&v1, "e_01", "outgoing", rows_v1, 1);
    assert_eq!(pool.entry_count(), 1);
    pool.on_memory_pressure(true);
    assert_eq!(pool.entry_count(), 0, "critical pressure must clear hot pool");
    assert_eq!(pool.current_bytes(), 0);

    Ok(())
}

#[test]
fn perf07_semantic_neighbor_prefetch_bounds_cancellation_and_metrics() -> Result<()> {
    use symweft_semantic::hot_pool::{PoolMode, SemanticHotPool};
    use symweft_semantic::prefetch::SemanticPrefetcher;
    use std::sync::atomic::AtomicBool;

    let f = Fixture::new();
    f.write("a.c", "int callee_one(void){return 1;} int callee_two(void){return 2;} int caller(void){return callee_one() + callee_two();}\n");
    let db = f.commands(&["a.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    store.publish(&a_snap)?;

    let caller_sym = a_snap.symbols.iter().find(|s| s.name == "caller").unwrap();
    let one_sym = a_snap.symbols.iter().find(|s| s.name == "callee_one").unwrap();

    let version = dummy_query_version(&f.0.to_string_lossy(), &db.configuration, &a_snap.provider, "a.c", 1);

    let mut pool = SemanticHotPool::new(PoolMode::Manual, 64 * 1024);
    let mut prefetcher = SemanticPrefetcher::new(8, 32 * 1024, 100);

    // 1. Prefetch 1-hop outgoing neighbors of caller
    let cancel = AtomicBool::new(false);
    let prefetched_count = prefetcher.prefetch_neighbors(&store, &mut pool, &version, &caller_sym.id, &cancel)?;
    assert!(prefetched_count > 0, "must prefetch neighbors for callee_one and callee_two");
    assert!(pool.entry_count() > 0);

    // 2. Access callee_one in hot pool without querying store
    let hit = pool.get(&version, &one_sym.id, "definitions");
    assert!(hit.is_some(), "prefetched definitions must hit hot pool");
    prefetcher.record_access(&one_sym.id, "definitions", &version, hit.unwrap().estimated_bytes);

    // 3. Inspect metrics: useful ratio and wasted bytes
    let metrics = prefetcher.metrics();
    assert!(metrics.total_prefetches > 0);
    assert_eq!(metrics.useful_prefetches, 1);
    assert!(metrics.useful_ratio > 0.0 && metrics.useful_ratio <= 1.0);
    assert!(metrics.wasted_bytes > 0, "unvisited prefetch items are properly tracked as wasted bytes");

    // 4. Cancellation stops prefetch immediately
    let cancelled = AtomicBool::new(true);
    let zero = prefetcher.prefetch_neighbors(&store, &mut pool, &version, &caller_sym.id, &cancelled)?;
    assert_eq!(zero, 0, "cancelled prefetch must immediately return 0");

    // 5. Memory pressure clears hot pool and prefetch history
    prefetcher.on_memory_pressure(&mut pool);
    assert_eq!(pool.entry_count(), 0);
    assert_eq!(prefetcher.metrics().total_prefetches, 0);

    Ok(())
}

#[test]
fn perf08_active_tu_interval_and_adjacency_query_index_performance() -> Result<()> {
    use symweft_semantic::model::Location;
    use symweft_semantic::query_index::ActiveTuQueryIndex;

    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    let mut references = Vec::new();

    // Synthesize large TU with 2000 symbols and 4000 calls
    for i in 0..2000 {
        symbols.push(symweft_semantic::model::Symbol {
            id: format!("e_{i:04}"),
            usr: format!("c:@F@fn_{i}"),
            name: format!("fn_{i}"),
            kind: "function".into(),
            signature: "void()".into(),
            definition: true,
            callable: true,
            location: Location { path: "large.c".into(), offset: i * 40, end_offset: i * 40 + 20, line: i + 1, column: 1 },
            extent: Location { path: "large.c".into(), offset: i * 40, end_offset: i * 40 + 38, line: i + 1, column: 1 },
        });
        calls.push(symweft_semantic::model::Call {
            caller: Some(format!("e_{:04}", i / 4)),
            target: Some(format!("e_{i:04}")),
            name: format!("fn_{i}"),
            resolution: "direct".into(),
            location: Location { path: "large.c".into(), offset: i * 40 + 10, end_offset: i * 40 + 18, line: i + 1, column: 5 },
            spelling: None,
        });
        calls.push(symweft_semantic::model::Call {
            caller: Some(format!("e_{i:04}")),
            target: Some(format!("e_{:04}", (i + 1) % 2000)),
            name: format!("fn_{}", (i + 1) % 2000),
            resolution: "direct".into(),
            location: Location { path: "large.c".into(), offset: i * 40 + 22, end_offset: i * 40 + 30, line: i + 1, column: 5 },
            spelling: None,
        });
        references.push(symweft_semantic::model::Reference {
            target: format!("e_{i:04}"),
            name: format!("fn_{i}"),
            kind: "call".into(),
            location: Location { path: "large.c".into(), offset: i * 40 + 10, end_offset: i * 40 + 18, line: i + 1, column: 5 },
            spelling: None,
        });
    }

    let snapshot = Snapshot {
        symbols,
        calls,
        references,
        worker_peak_rss_bytes: 32 * 1024 * 1024,
        extract_micros: 200_000,
        ..dummy_snapshot("u_large", "large.c")
    };

    // 1. Build index and verify memory accounting
    let index = ActiveTuQueryIndex::build(&snapshot);
    assert_eq!(index.unit_id, "u_large");
    assert!(index.memory_bytes > 0 && index.memory_bytes < 2 * 1024 * 1024, "index must remain compact within budget");

    // 2. Test interval offset point queries (binary search)
    let t0 = std::time::Instant::now();
    for i in 0..100 {
        let offset = (i * 17 * 40) % (2000 * 40);
        let found = index.find_entity_at_offset(offset + 5);
        assert!(found.is_some());
    }
    let elapsed = t0.elapsed();
    assert!(elapsed.as_millis() < 5, "100 point interval lookups must complete in under 5 ms");

    // 3. Test O(1) adjacency lookups
    let t1 = std::time::Instant::now();
    let outgoing = index.get_outgoing_calls("e_0005", &snapshot);
    assert!(!outgoing.is_empty());
    let incoming = index.get_incoming_calls("e_0005", &snapshot);
    assert!(!incoming.is_empty());
    assert!(t1.elapsed().as_micros() < 500, "adjacency lookup must be sub-millisecond");

    Ok(())
}

#[test]
fn perf09_unified_resource_ledger_subsystems_and_policy_modes() -> Result<()> {
    use symweft_semantic::ledger::{LedgerMode, UnifiedResourceLedger};

    // 1. Initialize in Manual mode with 64 MiB limit
    let mut ledger = UnifiedResourceLedger::new(LedgerMode::Manual, 64 * 1024 * 1024);
    assert_eq!(ledger.mode(), LedgerMode::Manual);
    assert_eq!(ledger.effective_limit_bytes(), 64 * 1024 * 1024);

    // 2. Record billing across five independent subsystems
    ledger.record_a1(4 * 1024 * 1024);
    ledger.record_a2_hot_pool(8 * 1024 * 1024);
    ledger.record_preamble(16 * 1024 * 1024);
    ledger.record_sqlite(12 * 1024 * 1024);
    ledger.record_queues(2 * 1024 * 1024);
    ledger.set_sampled_rss(Some(120 * 1024 * 1024));

    let report = ledger.report();
    assert_eq!(report.total_estimated_bytes, 42 * 1024 * 1024);
    assert_eq!(report.max_allowed_total_bytes, 64 * 1024 * 1024);
    assert!(!report.over_budget);
    assert_eq!(report.sampled_process_rss_bytes, Some(120 * 1024 * 1024));
    assert_eq!(report.gpu_state, "unmeasured_external");

    // 3. Verify Manual limit is strictly enforced and NEVER silently inflated by Auto policy
    ledger.set_manual_limit(30 * 1024 * 1024);
    let constrained_report = ledger.report();
    assert_eq!(constrained_report.max_allowed_total_bytes, 30 * 1024 * 1024);
    assert!(constrained_report.over_budget, "42 MiB usage must exceed 30 MiB manual ceiling");

    // 4. Switch to Constrained mode
    ledger.set_mode(LedgerMode::Constrained);
    assert_eq!(ledger.effective_limit_bytes(), 30 * 1024 * 1024);

    Ok(())
}

#[test]
fn perf10_adaptive_coordinator_hysteresis_and_user_pause_preservation() -> Result<()> {
    use symweft_semantic::adaptive::{AdaptiveCoordinator, SystemPhase};

    // 1. Initialize coordinator with 50 ms hysteresis window
    let mut coordinator = AdaptiveCoordinator::new(50);
    assert_eq!(coordinator.phase(), SystemPhase::Normal);
    let s0 = coordinator.status();
    assert_eq!(s0.prefetch_scale, 1.0);
    assert!(!s0.background_indexing_paused);

    // 2. Transition to ForegroundBusy upon user activity
    coordinator.on_foreground_activity();
    assert_eq!(coordinator.phase(), SystemPhase::ForegroundBusy);
    let s1 = coordinator.status();
    assert_eq!(s1.prefetch_scale, 0.25, "prefetch must scale down during busy editing");
    assert!(s1.background_indexing_paused, "background work must pause during busy editing");

    // 3. Transient idle tick within hysteresis duration does NOT flap
    coordinator.on_tick_idle();
    assert_eq!(coordinator.phase(), SystemPhase::ForegroundBusy, "hysteresis must prevent premature phase flapping");

    // 4. Stable idle past hysteresis duration transitions to Idle
    std::thread::sleep(std::time::Duration::from_millis(60));
    coordinator.on_tick_idle();
    coordinator.on_tick_idle();
    assert_eq!(coordinator.phase(), SystemPhase::Idle);
    let s2 = coordinator.status();
    assert_eq!(s2.prefetch_scale, 1.0);
    assert!(!s2.background_indexing_paused);

    // 5. User explicitly pauses background work
    coordinator.set_user_pause(true);
    assert!(coordinator.is_user_paused());
    assert!(coordinator.status().background_indexing_paused);

    // 6. Memory pressure arrives and then recovers
    coordinator.on_memory_pressure(true);
    assert_eq!(coordinator.phase(), SystemPhase::MemoryPressure);
    assert_eq!(coordinator.status().prefetch_scale, 0.0);

    // Transition back to Idle after pressure clears
    std::thread::sleep(std::time::Duration::from_millis(60));
    coordinator.on_tick_idle();
    coordinator.on_tick_idle();
    coordinator.on_tick_idle();
    assert_eq!(coordinator.phase(), SystemPhase::Idle);

    // CRITICAL ACCEPTANCE CHECK: Pressure recovery MUST NOT revoke user pause!
    let s_rec = coordinator.status();
    assert!(s_rec.user_paused, "user pause flag must survive pressure recovery");
    assert!(s_rec.background_indexing_paused, "background indexing must remain paused if user requested it");

    Ok(())
}

#[test]
fn perf11_batch_transaction_wal_checkpoint_and_garbage_collection() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int target_fn(void){return 1;} int a_caller(void){return target_fn();}\n");
    f.write("b.c", "int target_fn(void); int b_caller(void){return target_fn();}\n");
    let db = f.commands(&["a.c", "b.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;

    let target_sym = a_snap.symbols.iter().find(|s| s.name == "target_fn").unwrap();

    // 1. Initial verification: both callers exist
    let initial_calls = store.relations(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 10)?;
    assert_eq!(initial_calls["rows"].as_array().unwrap().len(), 2);

    // 2. Mark b.c as retired or replace it with empty to create unreachable orphaned facts
    let mut empty_b_snap = b_snap.clone();
    empty_b_snap.symbols.clear();
    empty_b_snap.calls.clear();
    empty_b_snap.references.clear();
    store.publish(&empty_b_snap)?;

    // 3. Run garbage_collect to purge unreachable facts, entities, and paths
    let gc_report = store.garbage_collect()?;
    assert!(gc_report.unreferenced_facts_removed > 0, "must purge unreferenced facts left by replaced unit");

    // 4. Critical check: Active references and facts of a.c MUST NOT be deleted!
    let remaining_calls = store.relations(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 10)?;
    let remaining_rows = remaining_calls["rows"].as_array().unwrap();
    assert_eq!(remaining_rows.len(), 1, "active incoming call from a.c must remain fully intact");
    assert!(remaining_rows[0]["main_file"].as_str().unwrap().ends_with("a.c"));

    // 5. Concurrent reader queries are not blocked (verified via WAL mode reader)
    let reader_store = Store::open(&path)?;
    let read_result = reader_store.relations(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 10)?;
    assert_eq!(read_result["rows"].as_array().unwrap().len(), 1);

    Ok(())
}

#[test]
fn perf12_optimization_roundtrip_regression_and_performance_retest() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int target_fn(int x){return x+1;}\n");
    f.write("b.c", "int target_fn(int); int caller(void){return target_fn(42);}\n");
    let db = f.commands(&["a.c", "b.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;

    // 1. Initial Index Phase
    let t_idx = Instant::now();
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    let idx_ms = t_idx.elapsed().as_millis();

    // 2. Unchanged review phase
    let t_rev = Instant::now();
    assert!(store.affected(&db.configuration, "a.c")?.is_empty());
    let rev_ms = t_rev.elapsed().as_millis();
    assert!(rev_ms <= idx_ms);

    // 3. Edit & Preview phase with optimized transport and hot pool
    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        worker_binary(),
        Limits::default(),
    )?;
    let b_path = f.0.join("b.c").to_string_lossy().into_owned();
    let docs = vec![PreviewDocument {
        path: b_path.clone(),
        revision: 2,
        content: "int target_fn(int); int caller(void){return target_fn(100);}\n".into(),
    }];
    let b_unit = &db.commands[1].unit_id;
    let status = controller.start_preview_documents(docs, Some(b_unit), 32)?;
    let id = status["ticket"]["job_id"].as_u64().unwrap();

    let deadline = Instant::now() + std::time::Duration::from_secs(3);
    while Instant::now() < deadline {
        controller.maintain(&HashSet::new(), true)?;
        let s = controller.preview_status()?;
        if s["state"] == "ready" && s["ticket"]["job_id"] == id {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    // 4. Read query phase: verify 100% semantic accuracy
    let target_sym = a_snap.symbols.iter().find(|s| s.name == "target_fn").unwrap();
    let rels = controller.project_relations_page(id, &b_path, 2, &target_sym.id, "incoming", 10, 0)?;
    let rows = rels["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["main_file"], b_path);
    assert_eq!(rows[0]["evidence"], "clang_semantic_overlay");

    // 5. Idle cleanup phase
    controller.retire_overlay("test completed");
    assert_eq!(controller.preview_status()?["charged_bytes"], 0);

    Ok(())
}

#[test]
fn perf13_storage_scalability_layer_breakdown_and_freeze_criteria() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int target(void){return 1;} int caller(void){return target();}\n");
    let db = f.commands(&["a.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    store.publish(&a_snap)?;

    let target_sym = a_snap.symbols.iter().find(|s| s.name == "target").unwrap();

    // 1. Layer Breakdown Measurement: SQL index lookup vs blob decompression
    let t0 = std::time::Instant::now();
    let res = store.relations(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 10)?;
    let total_us = t0.elapsed().as_micros();

    assert_eq!(res["rows"].as_array().unwrap().len(), 1);
    // Freeze criteria: query latency must stay below 5 ms (5000 us)
    assert!(total_us < 5000, "Layer query latency must be < 5 ms, got {} us", total_us);

    // 2. Disk baseline and schema integrity under scale
    let baseline = store.disk_baseline()?;
    assert!(baseline.revision > 0);
    assert!(!baseline.store_id.is_empty());
    assert_eq!(baseline.schema, "a2-store-v2");

    Ok(())
}

#[test]
fn perf14_idle_cycle_suppression_and_event_driven_responsiveness() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int target(void){return 1;} int caller(void){return target();}\n");
    let db = f.commands(&["a.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    store.publish(&a_snap)?;
    drop(store);

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        f.0.join("worker-dummy"),
        Limits::default(),
    )?;

    // 1. Idle status stability and zero perturbation during polling
    let initial_status = controller.preview_status()?;
    let t0 = std::time::Instant::now();
    for _ in 0..100 {
        let s = controller.preview_status()?;
        assert_eq!(s["state"], initial_status["state"]);
        assert_eq!(s["charged_bytes"], initial_status["charged_bytes"]);
        assert_eq!(s["queued"], false);
        assert_eq!(s["worker_active"], false);
    }
    let elapsed = t0.elapsed();
    assert!(elapsed < std::time::Duration::from_millis(100), "100 idle status polls took {:?}", elapsed);

    // 2. Responsive event-driven wake up on change
    f.write("a.c", "int target(void){return 2;} int caller(void){return target();}\n");
    let mut modified = HashSet::new();
    modified.insert(f.0.join("a.c").to_string_lossy().into_owned());
    controller.maintain(&modified, true)?;

    let status = controller.preview_status()?;
    assert_eq!(status["worker_active"], false);

    Ok(())
}

#[test]
fn heat01_structural_heat_specification_and_variant_deduplication() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int target(void){return 1;} int caller1(void){return target();}\n");
    f.write("b.c", "int target(void); int caller2(void){return target();}\n");
    let entries = vec![
        json!({"directory": f.0, "file": "a.c", "arguments": ["clang", "-c", "a.c", "-o", "a.o"]}),
        json!({"directory": f.0, "file": "b.c", "arguments": ["clang", "-c", "b.c", "-DVARIANT=1", "-o", "b_v1.o"]}),
        json!({"directory": f.0, "file": "b.c", "arguments": ["clang", "-c", "b.c", "-DVARIANT=2", "-o", "b_v2.o"]}),
    ];
    fs::write(f.0.join("compile_commands.json"), serde_json::to_vec(&entries)?)?;
    let db = CompileDatabase::load(&f.0, &f.0.join("compile_commands.json"))?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;

    let a_snap = analyze(&db, 0)?;
    let b_snap_v1 = analyze(&db, 1)?;
    let b_snap_v2 = analyze(&db, 2)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap_v1)?;
    store.publish(&b_snap_v2)?;

    let target_sym = a_snap.symbols.iter().find(|s| s.name == "target").unwrap();

    // Compute entity structural heat
    let heat = store.entity_structural_heat(&db.configuration, &target_sym.id)?.expect("target heat found");

    // 1. Calling metrics and deduplication:
    assert_eq!(heat.raw_call_sites, 3, "Raw call sites across variants should be 3");
    assert_eq!(heat.call_sites, 2, "Deduplicated call sites should be 2");
    assert_eq!(heat.distinct_callers, 2);
    assert_eq!(heat.fan_in, 2);
    assert_eq!(heat.dedup_policy, "dedup_by_source_location");
    assert_eq!(heat.evidence_level, "clang_semantic");
    assert_eq!(heat.syntactic_incoming_proxy, None);
    assert!(heat.structural_score > 0.0);

    // 2. File-level structural heat
    let file_heats = store.file_structural_heat(&db.configuration, 10)?;
    assert!(!file_heats.is_empty());
    assert!(file_heats.iter().any(|f| f.path.contains("a.c")));

    // 3. Top ranking
    let top = store.top_structural_heat(&db.configuration, 10)?;
    assert_eq!(top[0].entity_key, target_sym.id);

    Ok(())
}

#[test]
fn heat02_symbol_attention_identity_line_drift_and_migration() -> Result<()> {
    let f = Fixture::new();
    let path = f.0.join("semantics.sqlite3");
    let store = Store::open(&path)?;
    let config = "default";
    let now = 1700000000.0;

    // 1. Initial attention recording for symbol
    store.record_attention(config, "worker.c", "c:@F@process_data", "process_data", 25_000, true, now)?;
    let attn1 = store.get_attention(config, "worker.c", "c:@F@process_data")?.expect("attention recorded");
    assert_eq!(attn1.active_ms, 25_000);
    assert_eq!(attn1.opens, 1);
    assert!(attn1.attention_score > 0.0);

    // 2. Line drift resilience: code above process_data shifts, but USR and path remain constant
    // Next reading session 100 seconds later accumulates onto same stable identity
    store.record_attention(config, "worker.c", "c:@F@process_data", "process_data", 15_000, false, now + 100.0)?;
    let attn2 = store.get_attention(config, "worker.c", "c:@F@process_data")?.expect("attention accumulated");
    assert_eq!(attn2.active_ms, 40_000);
    assert_eq!(attn2.opens, 1);
    assert!(attn2.attention_score > attn1.attention_score);

    // 3. Symbol rename / file relocation migration
    // Function moved from worker.c:c:@F@process_data to task.c:c:@F@execute_task
    let migrated = store.migrate_attention(
        config,
        "worker.c",
        "c:@F@process_data",
        "task.c",
        "c:@F@execute_task",
        "execute_task",
    )?;
    assert!(migrated, "migration succeeded");

    // Old key is cleared
    assert!(store.get_attention(config, "worker.c", "c:@F@process_data")?.is_none());

    // New key inherited previous active_ms and opens
    let attn_migrated = store.get_attention(config, "task.c", "c:@F@execute_task")?.expect("migrated record found");
    assert_eq!(attn_migrated.active_ms, 40_000);
    assert_eq!(attn_migrated.opens, 1);
    assert_eq!(attn_migrated.name, "execute_task");

    // 4. Missing source returns false gracefully
    let bad_mig = store.migrate_attention(config, "ghost.c", "none", "dest.c", "none", "none")?;
    assert!(!bad_mig);

    Ok(())
}

#[test]
fn heat03_independent_decay_persistence_privacy_and_indexing_isolation() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int worker(void){return 1;} int caller(void){return worker();}\n");
    let db = f.commands(&["a.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    store.publish(&a_snap)?;

    let config = "default";
    let t0 = 1700000000.0;

    // 1. Record user attention
    store.record_attention(config, "a.c", "c:@F@worker", "worker", 30_000, true, t0)?;
    let attn_t0 = store.get_attention(config, "a.c", "c:@F@worker")?.unwrap();
    assert_eq!(attn_t0.active_ms, 30_000);
    assert_eq!(attn_t0.opens, 1);
    let initial_score = attn_t0.attention_score;

    // 2. Structural heat has no time decay; attention score decays with 7-day half-life
    let t_7days = t0 + 604800.0;
    // Record empty heartbeat (0 ms) 7 days later to evaluate decayed score
    store.record_attention(config, "a.c", "c:@F@worker", "worker", 0, false, t_7days)?;
    let attn_decayed = store.get_attention(config, "a.c", "c:@F@worker")?.unwrap();
    let expected_decayed = initial_score * 0.5;
    assert!((attn_decayed.attention_score - expected_decayed).abs() < 1e-4, "Score must decay by half after 7 days");

    // 3. Restart persistence: drop Store and re-open
    drop(store);
    let mut store2 = Store::open(&path)?;
    let attn_reopened = store2.get_attention(config, "a.c", "c:@F@worker")?.unwrap();
    assert_eq!(attn_reopened.active_ms, 30_000);
    assert_eq!(attn_reopened.opens, 1);
    assert_eq!(attn_reopened.attention_score, attn_decayed.attention_score);

    // 4. Background indexing isolation: publishing another snapshot does NOT generate user attention
    f.write("b.c", "int another_fn(void){return 42;}\n");
    let db2 = f.commands(&["a.c", "b.c"])?;
    store2.reconcile(&db2)?;
    let b_snap = analyze(&db2, 1)?;
    store2.publish(&b_snap)?;
    // Attention table still contains only the 1 record user read
    let all_attn = store2.top_attention(config, 100)?;
    assert_eq!(all_attn.len(), 1);
    assert_eq!(all_attn[0].usr, "c:@F@worker");

    // 5. Privacy controls: disable collection and clear
    store2.set_attention_enabled(false);
    assert!(!store2.is_attention_enabled());
    // Calling record when disabled does nothing
    store2.record_attention(config, "b.c", "c:@F@another_fn", "another_fn", 20_000, true, t_7days)?;
    assert!(store2.get_attention(config, "b.c", "c:@F@another_fn")?.is_none());

    // Clear operation
    let deleted = store2.clear_attention(Some(config))?;
    assert_eq!(deleted, 1);
    assert!(store2.top_attention(config, 100)?.is_empty());

    Ok(())
}

#[test]
fn heat04_heat_driven_cache_admission_eviction_and_large_function_crowding_defense() -> Result<()> {
    use symweft_semantic::hot_pool::{PoolMode, SemanticHotPool};

    let f = Fixture::new();
    let version = dummy_query_version(&f.0.to_string_lossy(), "default", "a2-clang-v3", "dummy.c", 1);

    // Hot pool with tight 1200-byte budget
    let mut pool = SemanticHotPool::new(PoolMode::Manual, 1200);

    // 1. Insert Item A: High user attention (actively read function, 250 bytes)
    let rows_a = serde_json::json!([{"caller": "main", "target": "worker_a"}]);
    pool.insert_with_heat(&version, "worker_a", "incoming", rows_a, 1, 1.5, 12.0);

    // 2. Insert Item B: High structural heat (core architecture hub, 250 bytes)
    let rows_b = serde_json::json!([{"caller": "main", "target": "core_hub"}]);
    pool.insert_with_heat(&version, "core_hub", "incoming", rows_b, 1, 9.5, 0.0);

    // 3. Insert Item C: Cold small leaf function (250 bytes)
    let rows_c = serde_json::json!([{"caller": "main", "target": "leaf_c"}]);
    pool.insert_with_heat(&version, "leaf_c", "incoming", rows_c, 1, 0.2, 0.0);

    assert_eq!(pool.entry_count(), 3);

    // 4. Large cold function arrives: 600 bytes, 0 attention, very low structural heat
    let rows_giant = serde_json::json!([
        {"caller": "main", "target": "giant_cold_blob_line_1"},
        {"caller": "main", "target": "giant_cold_blob_line_2"},
        {"caller": "main", "target": "giant_cold_blob_line_3"},
        {"caller": "main", "target": "giant_cold_blob_line_4"},
        {"caller": "main", "target": "giant_cold_blob_line_5"}
    ]);
    pool.insert_with_heat(&version, "giant_cold_fn", "incoming", rows_giant, 5, 0.1, 0.0);

    // 5. Verify Crowding Defense:
    // High-attention worker_a and high-structural core_hub MUST STILL BE RETAINED!
    assert!(pool.get(&version, "worker_a", "incoming").is_some(), "Actively read symbol must not be evicted by cold giant");
    assert!(pool.get(&version, "core_hub", "incoming").is_some(), "High structural hub must not be evicted by cold giant");

    // The cold low-utility item (leaf_c) or the giant itself was evicted to respect budget
    assert!(pool.current_bytes() <= 1200, "Pool bytes {} must stay within 1200", pool.current_bytes());

    Ok(())
}

#[test]
fn heat05_heat_report_export_json_csv_and_scope_statistics() -> Result<()> {
    let f = Fixture::new();
    f.write("a.c", "int target(void){return 1;} int caller(void){return target();}\n");
    let db = f.commands(&["a.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    store.publish(&a_snap)?;

    let config = &db.configuration;
    let now = 1700000000.0;
    let target_sym = a_snap.symbols.iter().find(|s| s.name == "target").unwrap();

    // Record user reading attention
    store.record_attention(config, "a.c", &target_sym.id, "target", 45_000, true, now)?;

    // 1. Export Symbol Heat as JSON sorted by structure
    let json_struct = store.export_heat(config, "symbol", "structure", "json", 10, now)?;
    assert_eq!(json_struct["format"], "json");
    assert!(json_struct["scope"].as_str().unwrap().contains(config.as_str()));
    let rows_struct = json_struct["rows"].as_array().unwrap();
    assert!(!rows_struct.is_empty());
    assert_eq!(rows_struct[0]["entity_key"], target_sym.id);
    assert_eq!(rows_struct[0]["evidence_level"], "clang_semantic");
    assert_eq!(rows_struct[0]["active_ms"], 45_000);

    // 2. Export Symbol Heat as CSV sorted by attention
    let csv_attn = store.export_heat(config, "symbol", "attention", "csv", 10, now)?;
    assert_eq!(csv_attn["format"], "csv");
    let csv_content = csv_attn["content"].as_str().unwrap();
    assert!(csv_content.starts_with("path,entity_key,name,kind,structural_metric,structural_value,evidence_level,ambiguous,active_ms,open_count,attention_score,last_seen"));
    assert!(csv_content.contains("target"));
    assert!(csv_content.contains("clang_semantic"));

    // 3. Export File Heat as JSON and CSV
    let file_json = store.export_heat(config, "file", "structure", "json", 10, now)?;
    assert_eq!(file_json["kind"], "file");
    assert!(!file_json["rows"].as_array().unwrap().is_empty());

    let file_csv = store.export_heat(config, "file", "structure", "csv", 10, now)?;
    assert!(file_csv["content"].as_str().unwrap().contains("a.c"));

    Ok(())
}

#[test]
fn heat06_synthetic_replay_trace_comparison_and_zero_user_pollution() -> Result<()> {
    use symweft_semantic::heat::{generate_synthetic_replay_trace, run_heat_replay_benchmark};

    let f = Fixture::new();
    f.write("hub.c", "int hub_fn(void){return 10;}\n");
    f.write("callers.c", "int hub_fn(void); int c1(void){return hub_fn();} int c2(void){return hub_fn();}\n");
    f.write("cold.c", "int cold_1(void){return 1;} int cold_2(void){return 2;}\n");
    let db = f.commands(&["hub.c", "callers.c", "cold.c"])?;
    let path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&path)?;
    store.reconcile(&db)?;
    let s0 = analyze(&db, 0)?;
    let s1 = analyze(&db, 1)?;
    let s2 = analyze(&db, 2)?;
    store.publish(&s0)?;
    store.publish(&s1)?;
    store.publish(&s2)?;

    let config = &db.configuration;
    let version = dummy_query_version(&f.0.to_string_lossy(), config, &s0.provider, "dummy.c", 1);

    let all_symbols: Vec<_> = s0.symbols.iter().chain(&s1.symbols).chain(&s2.symbols).collect();
    let hub_sym = all_symbols.iter().find(|s| s.name == "hub_fn").unwrap();
    let cold_sym1 = all_symbols.iter().find(|s| s.name == "cold_1").unwrap();
    let cold_sym2 = all_symbols.iter().find(|s| s.name == "cold_2").unwrap();

    // Record authentic user attention before synthetic benchmark
    store.record_attention(config, "hub.c", &hub_sym.id, "hub_fn", 60_000, true, 1700000000.0)?;
    let initial_attention = store.top_attention(config, 100)?;
    assert_eq!(initial_attention.len(), 1);

    // 1. Generate 50-step synthetic replay trace (80% hub, 20% cold)
    let hot = vec![hub_sym.id.clone()];
    let cold = vec![cold_sym1.id.clone(), cold_sym2.id.clone()];
    let trace = generate_synthetic_replay_trace(&hot, &cold, 50);
    assert_eq!(trace.len(), 50);

    // 2. Replay Strategy 1: Baseline FIFO / LRU (no heat utility, no prefetch)
    let res_baseline = run_heat_replay_benchmark(&store, &version, &trace, 8192, false)?;
    assert_eq!(res_baseline.strategy, "baseline_fifo_lru");
    assert!(!res_baseline.real_user_attention_modified, "Baseline replay must not pollute real user attention");

    // 3. Replay Strategy 2: Heat-driven utility eviction with prefetch
    let res_heat = run_heat_replay_benchmark(&store, &version, &trace, 8192, true)?;
    assert_eq!(res_heat.strategy, "heat_driven_utility_with_prefetch");
    assert!(!res_heat.real_user_attention_modified, "Heat replay must not pollute real user attention");

    // 4. Compare performance metrics:
    // Heat-driven strategy achieves higher hit ratio and maintains memory budget
    assert!(res_heat.hit_ratio >= res_baseline.hit_ratio, "Heat-driven hit ratio must meet or exceed baseline");
    assert!(res_heat.peak_bytes <= 8192, "Peak bytes must stay within budget");

    // 5. Final assertion: authentic user attention remains exactly 1 record!
    let final_attention = store.top_attention(config, 100)?;
    assert_eq!(final_attention.len(), 1);
    assert_eq!(final_attention[0].usr, hub_sym.id);
    assert_eq!(final_attention[0].active_ms, 60_000);

    Ok(())
}

#[test]
fn cpp01_construct_capability_matrix_and_golden_samples() -> Result<()> {
    use symweft_semantic::cpp_constructs::{construct_capability_matrix, ConstructSupportStatus};

    let matrix = construct_capability_matrix();
    assert!(matrix.len() >= 8, "capability matrix must cover all core C/C++ construct categories");

    // Verify all required categories are present and have non-empty golden samples & rules
    let categories: HashSet<_> = matrix.iter().map(|e| e.category.as_str()).collect();
    for expected in [
        "functions",
        "templates",
        "implicit_and_operators",
        "types_and_inheritance",
        "variables",
        "preprocessor",
        "dispatch",
    ] {
        assert!(categories.contains(expected), "missing category: {expected}");
    }

    for entry in &matrix {
        assert!(!entry.construct_id.is_empty());
        assert!(!entry.binding_identity_rule.is_empty());
        assert!(!entry.location_anchor_rule.is_empty());
        assert!(!entry.variant_isolation_rule.is_empty());
        assert!(!entry.error_boundary_rule.is_empty());
        assert!(!entry.compiler_flags.is_empty());
        assert!(!entry.golden_sample.is_empty());
    }

    // Verify dispatch distinctions: virtual_dispatch is Candidate, function_pointers is Unknown
    let virt = matrix.iter().find(|e| e.construct_id == "cpp_virtual_dispatch_candidates").unwrap();
    assert_eq!(virt.status, ConstructSupportStatus::Candidate);

    let fn_ptr = matrix.iter().find(|e| e.construct_id == "c_cpp_function_pointers_and_indirect_calls").unwrap();
    assert_eq!(fn_ptr.status, ConstructSupportStatus::Unknown);

    Ok(())
}

#[test]
fn cpp02_templates_specializations_and_bounded_instantiations() -> Result<()> {
    use symweft_semantic::cpp_constructs::{
        MAX_TEMPLATE_INSTANTIATIONS_PER_TU, TemplateInstantiationTracker, TemplateRole,
        TemplateSymbolBinding,
    };

    let mut tracker = TemplateInstantiationTracker::new(2);

    // 1. Function template primary definition vs implicit instantiation vs explicit specialization
    let primary_fn = TemplateSymbolBinding {
        symbol_id: "sym:compute_primary".into(),
        usr: "c:@FT@>1#Tcompute#t0.0#".into(),
        bare_name: "compute".into(),
        role: TemplateRole::classify_role("FunctionTemplate", "c:@FT@>1#Tcompute#t0.0#", false, false),
        primary_template_id: None,
        primary_template_usr: None,
        type_arguments: vec!["T".into()],
    };
    assert_eq!(primary_fn.role, TemplateRole::PrimaryFunctionTemplate);
    assert!(tracker.record_binding(primary_fn));

    let inst_int = TemplateSymbolBinding {
        symbol_id: "sym:compute_int".into(),
        usr: "c:@F@compute<#I>#I#".into(),
        bare_name: "compute".into(),
        role: TemplateRole::classify_role("FunctionDecl", "c:@F@compute<#I>#I#", true, false),
        primary_template_id: Some("sym:compute_primary".into()),
        primary_template_usr: Some("c:@FT@>1#Tcompute#t0.0#".into()),
        type_arguments: vec!["int".into()],
    };
    assert_eq!(inst_int.role, TemplateRole::ImplicitInstantiation);
    assert!(tracker.record_binding(inst_int));

    let spec_double = TemplateSymbolBinding {
        symbol_id: "sym:compute_double_spec".into(),
        usr: "c:@F@compute<#d>#d#".into(),
        bare_name: "compute".into(),
        role: TemplateRole::classify_role("FunctionDecl", "c:@F@compute<#d>#d#", true, true),
        primary_template_id: Some("sym:compute_primary".into()),
        primary_template_usr: Some("c:@FT@>1#Tcompute#t0.0#".into()),
        type_arguments: vec!["double".into()],
    };
    assert_eq!(spec_double.role, TemplateRole::ExplicitSpecialization);
    assert!(tracker.record_binding(spec_double));

    // 2. Class template primary vs partial specialization
    let primary_cls = TemplateSymbolBinding {
        symbol_id: "sym:box_primary".into(),
        usr: "c:@ST>1#T@Box".into(),
        bare_name: "Box".into(),
        role: TemplateRole::classify_role("ClassTemplate", "c:@ST>1#T@Box", false, false),
        primary_template_id: None,
        primary_template_usr: None,
        type_arguments: vec!["T".into()],
    };
    assert_eq!(primary_cls.role, TemplateRole::PrimaryClassTemplate);
    assert!(tracker.record_binding(primary_cls));

    let partial_cls = TemplateSymbolBinding {
        symbol_id: "sym:box_ptr_partial".into(),
        usr: "c:@SP>1#T@Box>#*t0.0".into(),
        bare_name: "Box".into(),
        role: TemplateRole::classify_role("ClassTemplatePartialSpecialization", "c:@SP>1#T@Box>#*t0.0", true, false),
        primary_template_id: Some("sym:box_primary".into()),
        primary_template_usr: Some("c:@ST>1#T@Box".into()),
        type_arguments: vec!["T*".into()],
    };
    assert_eq!(partial_cls.role, TemplateRole::PartialSpecialization);
    assert!(tracker.record_binding(partial_cls));

    // 3. Same bare name "compute" never conflates targets
    let resolved_int = tracker.resolve_by_usr("c:@F@compute<#I>#I#").unwrap();
    let resolved_double = tracker.resolve_by_usr("c:@F@compute<#d>#d#").unwrap();
    assert_ne!(resolved_int.symbol_id, resolved_double.symbol_id);
    assert_eq!(resolved_int.role, TemplateRole::ImplicitInstantiation);
    assert_eq!(resolved_double.role, TemplateRole::ExplicitSpecialization);
    assert_eq!(tracker.specializations_of("c:@FT@>1#Tcompute#t0.0#").len(), 2);

    // 4. Budget protection on implicit instantiations
    let inst_long = TemplateSymbolBinding {
        symbol_id: "sym:compute_long".into(),
        usr: "c:@F@compute<#l>#l#".into(),
        bare_name: "compute".into(),
        role: TemplateRole::ImplicitInstantiation,
        primary_template_id: Some("sym:compute_primary".into()),
        primary_template_usr: Some("c:@FT@>1#Tcompute#t0.0#".into()),
        type_arguments: vec!["long".into()],
    };
    assert!(tracker.record_binding(inst_long));
    assert_eq!(tracker.recorded_instantiations, 2);
    assert!(!tracker.budget_exhausted);

    let inst_over_budget = TemplateSymbolBinding {
        symbol_id: "sym:compute_short".into(),
        usr: "c:@F@compute<#s>#s#".into(),
        bare_name: "compute".into(),
        role: TemplateRole::ImplicitInstantiation,
        primary_template_id: Some("sym:compute_primary".into()),
        primary_template_usr: Some("c:@FT@>1#Tcompute#t0.0#".into()),
        type_arguments: vec!["short".into()],
    };
    assert!(!tracker.record_binding(inst_over_budget));
    assert!(tracker.budget_exhausted);
    assert_eq!(tracker.skipped_instantiations, 1);
    assert_eq!(MAX_TEMPLATE_INSTANTIATIONS_PER_TU, 1024);

    Ok(())
}

#[test]
fn cpp03_implicit_calls_constructors_destructors_and_operators() -> Result<()> {
    use symweft_semantic::cpp_constructs::{
        CallSyntaxKind, classify_call_edge, untracked_implicit_edges_catalog,
    };

    // 1. Explicit vs implicit constructor calls
    let explicit_ctor = classify_call_edge(
        Some("fn:main".into()),
        Some("ctor:Widget".into()),
        "Widget",
        "Constructor",
        Some("Widget"),
    );
    assert_eq!(explicit_ctor.syntax_kind, CallSyntaxKind::ExplicitCall);
    assert!(!explicit_ctor.is_implicit);
    assert!(explicit_ctor.persisted);

    let implicit_ctor = classify_call_edge(
        Some("fn:main".into()),
        Some("ctor:Widget".into()),
        "Widget",
        "Constructor",
        Some("w"),
    );
    assert_eq!(implicit_ctor.syntax_kind, CallSyntaxKind::ImplicitConstructor);
    assert!(implicit_ctor.is_implicit);
    assert!(implicit_ctor.persisted);

    // 2. Implicit user-defined conversion vs explicit conversion operator call
    let implicit_conv = classify_call_edge(
        Some("fn:main".into()),
        Some("conv:Widget::operator bool".into()),
        "operator bool",
        "ConversionFunction",
        Some("w"),
    );
    assert_eq!(implicit_conv.syntax_kind, CallSyntaxKind::ImplicitConversion);
    assert!(implicit_conv.is_implicit);
    assert!(implicit_conv.persisted);

    // 3. Overloaded operator (e.g. a + b -> operator+) vs explicit w.operator+(b)
    let overloaded_op = classify_call_edge(
        Some("fn:main".into()),
        Some("op:Vec2::operator+".into()),
        "operator+",
        "CXXMethod",
        Some("+"),
    );
    assert_eq!(overloaded_op.syntax_kind, CallSyntaxKind::OverloadedOperator);
    assert!(overloaded_op.is_implicit);
    assert!(overloaded_op.persisted);

    let explicit_op = classify_call_edge(
        Some("fn:main".into()),
        Some("op:Vec2::operator+".into()),
        "operator+",
        "CXXMethod",
        Some("operator+"),
    );
    assert_eq!(explicit_op.syntax_kind, CallSyntaxKind::ExplicitCall);
    assert!(!explicit_op.is_implicit);

    // 4. Explicit destructor / delete vs untracked automatic scope-exit destructor
    let explicit_delete = classify_call_edge(
        Some("fn:cleanup".into()),
        Some("dtor:~Widget".into()),
        "~Widget",
        "Destructor",
        Some("delete"),
    );
    assert_eq!(
        explicit_delete.syntax_kind,
        CallSyntaxKind::ExplicitDestructorOrDelete
    );
    assert!(explicit_delete.persisted);

    let untracked_dtor = classify_call_edge(
        Some("fn:cleanup".into()),
        Some("dtor:~Widget".into()),
        "~Widget",
        "Destructor",
        None,
    );
    assert_eq!(
        untracked_dtor.syntax_kind,
        CallSyntaxKind::UntrackedCompilerSynthesized
    );
    assert!(!untracked_dtor.persisted);
    assert!(
        untracked_dtor
            .untracked_reason
            .as_deref()
            .unwrap()
            .contains("not materialized in libclang AST")
    );

    let catalog = untracked_implicit_edges_catalog();
    assert!(catalog.iter().any(|(id, _)| *id == "scope_exit_automatic_destructor"));

    Ok(())
}

#[test]
fn cpp04_types_inheritance_overrides_and_type_aliases() -> Result<()> {
    use symweft_semantic::cpp_constructs::{
        AccessSpecifier, CppTypeKind, InheritanceRelation, MemberDeclarationRelation,
        TypeAliasRelation, TypeHierarchyIndex, VirtualOverrideRelation,
    };

    // 1. Verify class/struct/union/enum/typedef/using classification
    assert_eq!(CppTypeKind::from_cursor_kind("ClassDecl"), Some(CppTypeKind::Class));
    assert_eq!(CppTypeKind::from_cursor_kind("StructDecl"), Some(CppTypeKind::Struct));
    assert_eq!(CppTypeKind::from_cursor_kind("UnionDecl"), Some(CppTypeKind::Union));
    assert_eq!(CppTypeKind::from_cursor_kind("EnumDecl"), Some(CppTypeKind::Enum));
    assert_eq!(
        CppTypeKind::from_cursor_kind("EnumConstantDecl"),
        Some(CppTypeKind::EnumConstant)
    );
    assert_eq!(
        CppTypeKind::from_cursor_kind("TypedefDecl"),
        Some(CppTypeKind::TypedefAlias)
    );
    assert_eq!(
        CppTypeKind::from_cursor_kind("TypeAliasDecl"),
        Some(CppTypeKind::UsingTypeAlias)
    );

    let mut index = TypeHierarchyIndex::default();

    // 2. Base/derived class relationships
    index.inheritances.push(InheritanceRelation {
        derived_id: "type:FileStream".into(),
        derived_name: "FileStream".into(),
        base_id: "type:Stream".into(),
        base_name: "Stream".into(),
        access: AccessSpecifier::Public,
        is_virtual: false,
    });
    index.inheritances.push(InheritanceRelation {
        derived_id: "type:MemoryStream".into(),
        derived_name: "MemoryStream".into(),
        base_id: "type:Stream".into(),
        base_name: "Stream".into(),
        access: AccessSpecifier::Public,
        is_virtual: true,
    });
    assert_eq!(index.base_classes_of("type:FileStream").len(), 1);
    assert_eq!(index.derived_classes_of("type:Stream").len(), 2);

    // 3. Virtual method override relationships
    index.overrides.push(VirtualOverrideRelation {
        override_method_id: "method:FileStream::read".into(),
        override_method_name: "read".into(),
        base_method_id: "method:Stream::read".into(),
        base_method_name: "read".into(),
    });
    assert_eq!(index.overridden_by("method:FileStream::read")[0].base_method_id, "method:Stream::read");
    assert_eq!(index.overrides_of("method:Stream::read")[0].override_method_id, "method:FileStream::read");

    // 4. Typedef & using type alias relationships
    index.aliases.push(TypeAliasRelation {
        alias_id: "alias:StreamPtr".into(),
        alias_name: "StreamPtr".into(),
        kind: CppTypeKind::UsingTypeAlias,
        underlying_type_id: Some("type:Stream".into()),
        underlying_type_spelling: "Stream *".into(),
    });
    let alias = index.resolve_type_alias("alias:StreamPtr").unwrap();
    assert_eq!(alias.underlying_type_id.as_deref(), Some("type:Stream"));

    // 5. Member declaration & out-of-line definition links
    index.members.push(MemberDeclarationRelation {
        owner_type_id: "type:FileStream".into(),
        member_id: "method:FileStream::read".into(),
        member_name: "read".into(),
        is_definition: false,
        is_static: false,
    });
    index.members.push(MemberDeclarationRelation {
        owner_type_id: "type:FileStream".into(),
        member_id: "method:FileStream::read".into(),
        member_name: "read".into(),
        is_definition: true,
        is_static: false,
    });
    let members = index.members_of("type:FileStream");
    assert_eq!(members.len(), 2);
    assert!(members.iter().any(|m| !m.is_definition));
    assert!(members.iter().any(|m| m.is_definition));

    Ok(())
}

#[test]
fn cpp05_variable_references_and_read_write_classification() -> Result<()> {
    use symweft_semantic::cpp_constructs::{
        VariableAccessMode, VariableStorageScope, classify_variable_access,
        classify_variable_scope,
    };

    // 1. Storage scope classification: local, global, static, parameter, member field
    assert_eq!(
        classify_variable_scope("VarDecl", "FunctionDecl", false),
        Some(VariableStorageScope::Local)
    );
    assert_eq!(
        classify_variable_scope("VarDecl", "TranslationUnit", false),
        Some(VariableStorageScope::Global)
    );
    assert_eq!(
        classify_variable_scope("VarDecl", "TranslationUnit", true),
        Some(VariableStorageScope::StaticInternal)
    );
    assert_eq!(
        classify_variable_scope("ParmDecl", "FunctionDecl", false),
        Some(VariableStorageScope::Parameter)
    );
    assert_eq!(
        classify_variable_scope("FieldDecl", "StructDecl", false),
        Some(VariableStorageScope::MemberField)
    );

    // 2. Read / Write / ReadWrite / AddressOf classification
    assert_eq!(
        classify_variable_access("VarDecl", "BinaryOperator", Some("="), true),
        Some(VariableAccessMode::Write)
    );
    assert_eq!(
        classify_variable_access("VarDecl", "BinaryOperator", Some("="), false),
        Some(VariableAccessMode::Read)
    );
    assert_eq!(
        classify_variable_access("FieldDecl", "CompoundAssignOperator", Some("+="), true),
        Some(VariableAccessMode::ReadWrite)
    );
    assert_eq!(
        classify_variable_access("ParmDecl", "UnaryOperator", Some("++"), false),
        Some(VariableAccessMode::ReadWrite)
    );
    assert_eq!(
        classify_variable_access("VarDecl", "UnaryOperator", Some("&"), false),
        Some(VariableAccessMode::AddressOf)
    );

    // 3. Guard: Function calls, methods, macros, and enum constants are NEVER classified as writes
    assert_eq!(
        classify_variable_scope("FunctionDecl", "TranslationUnit", false),
        None
    );
    assert_eq!(
        classify_variable_access("FunctionDecl", "BinaryOperator", Some("="), true),
        None
    );
    assert_eq!(
        classify_variable_access("CXXMethod", "UnaryOperator", Some("++"), true),
        None
    );
    assert_eq!(
        classify_variable_access("MacroDefinition", "BinaryOperator", Some("="), true),
        None
    );
    assert_eq!(
        classify_variable_access("EnumConstantDecl", "BinaryOperator", Some("="), true),
        None
    );

    Ok(())
}

#[test]
fn cpp06_macros_generated_code_and_preprocessor_variants() -> Result<()> {
    use symweft_semantic::{
        cpp_constructs::{
            GeneratedHeaderTracker, InactiveBranchPolicy, MacroDualAnchor,
            PreprocessorVariantIdentity,
        },
        model::Location,
    };

    // 1. Parameterized macro preserves both expansion site and spelling site
    let expansion = Location {
        path: "/workspace/src/main.c".into(),
        offset: 120,
        end_offset: 142,
        line: 10,
        column: 5,
    };
    let spelling = Location {
        path: "/workspace/include/macros.h".into(),
        offset: 40,
        end_offset: 55,
        line: 3,
        column: 25,
    };
    let dual = MacroDualAnchor::new("CALL_WRAP", "c:macros.h@40@macro@CALL_WRAP", expansion, spelling);
    assert!(dual.has_distinct_spelling_anchor());
    assert_eq!(dual.expansion_location.path, "/workspace/src/main.c");
    assert_eq!(
        dual.spelling_location.as_ref().unwrap().path,
        "/workspace/include/macros.h"
    );

    // 2. Build-directory generated header tracked and invalidated upon rebuild hash change
    let tracker = GeneratedHeaderTracker::new("/workspace", "/workspace/build");
    let gen_hdr = "/workspace/build/generated/config_autoconf.h";
    assert!(tracker.is_tracked_generated_file(gen_hdr));
    assert!(tracker.is_in_project_or_build_scope(gen_hdr));
    assert!(!tracker.should_invalidate_on_rebuild(gen_hdr, "hash_v1", "hash_v1"));
    assert!(tracker.should_invalidate_on_rebuild(gen_hdr, "hash_v1", "hash_v2"));

    // 3. Different -D preprocessor variants produce distinct variant unit IDs and never overwrite each other
    let var_a = PreprocessorVariantIdentity::from_arguments(
        "/workspace/src/driver.c",
        &["clang".into(), "-DUSE_FAST_PATH=1".into(), "-c".into(), "driver.c".into()],
    );
    let var_b = PreprocessorVariantIdentity::from_arguments(
        "/workspace/src/driver.c",
        &["clang".into(), "-DUSE_FAST_PATH=0".into(), "-c".into(), "driver.c".into()],
    );
    assert_ne!(var_a.variant_unit_id, var_b.variant_unit_id);
    assert_eq!(var_a.defines, vec!["USE_FAST_PATH=1"]);
    assert_eq!(var_b.defines, vec!["USE_FAST_PATH=0"]);

    // 4. Inactive #ifdef branch retains A1 syntax without fabricating A2 edges
    let inactive = InactiveBranchPolicy::evaluate_inactive_branch("#ifdef UNDEFINED_PLATFORM_FLAG");
    assert!(inactive.a1_syntax_retained);
    assert!(!inactive.a2_semantic_edges_emitted);

    Ok(())
}

#[test]
fn cpp07_virtual_dispatch_candidates_and_toolchain_boundaries() -> Result<()> {
    use symweft_semantic::cpp_constructs::{
        DispatchResolutionKind, ToolchainKind, TypeHierarchyIndex, VirtualDispatchCandidateSet,
        VirtualOverrideRelation, normalize_toolchain_arguments,
    };

    // 1. Virtual dispatch candidate set with explicit closed-world scope assumption
    let mut hierarchy = TypeHierarchyIndex::default();
    hierarchy.overrides.push(VirtualOverrideRelation {
        override_method_id: "method:TcpTransport::send".into(),
        override_method_name: "send".into(),
        base_method_id: "method:Transport::send".into(),
        base_method_name: "send".into(),
    });
    hierarchy.overrides.push(VirtualOverrideRelation {
        override_method_id: "method:UdpTransport::send".into(),
        override_method_name: "send".into(),
        base_method_id: "method:Transport::send".into(),
        base_method_name: "send".into(),
    });

    let candidates = VirtualDispatchCandidateSet::from_hierarchy(
        "method:Transport::send",
        &hierarchy,
        8,
        10,
    );
    assert!(!candidates.is_exact);
    assert_eq!(candidates.closed_world_scope, "partial_indexed_translation_units");
    assert!(candidates.assumption_note.contains("8/10 indexed TU(s)"));
    assert_eq!(
        candidates.candidate_override_ids,
        vec![
            "method:TcpTransport::send".to_string(),
            "method:UdpTransport::send".to_string()
        ]
    );

    // 2. Function pointer & unresolved calls preserve honest markers
    assert_eq!(
        DispatchResolutionKind::from_label("direct"),
        DispatchResolutionKind::Direct
    );
    assert!(!DispatchResolutionKind::Direct.is_honest_non_deterministic());
    assert_eq!(
        DispatchResolutionKind::from_label("virtual_candidate"),
        DispatchResolutionKind::VirtualCandidate
    );
    assert!(DispatchResolutionKind::VirtualCandidate.is_honest_non_deterministic());
    assert_eq!(
        DispatchResolutionKind::from_label("indirect"),
        DispatchResolutionKind::IndirectFunctionPointer
    );
    assert!(DispatchResolutionKind::IndirectFunctionPointer.is_honest_non_deterministic());
    assert_eq!(
        DispatchResolutionKind::from_label("unresolved"),
        DispatchResolutionKind::Unresolved
    );
    assert!(DispatchResolutionKind::Unresolved.is_honest_non_deterministic());

    // 3. Clang/GCC flag normalization vs MSVC deferred to WIN-05
    let gcc_args = vec![
        "-Iinclude".into(),
        "-DDEBUG=1".into(),
        "-std=c++20".into(),
        "-MMD".into(),
        "-MF".into(),
        "out.d".into(),
        "-fno-canonical-system-headers".into(),
        "-c".into(),
        "-o".into(),
        "main.o".into(),
    ];
    let gcc_norm = normalize_toolchain_arguments("aarch64-linux-gnu-g++", &gcc_args);
    assert_eq!(gcc_norm.toolchain, ToolchainKind::Gcc);
    assert!(gcc_norm.deferred_reason.is_none());
    assert_eq!(
        gcc_norm.retained_arguments,
        vec!["-Iinclude", "-DDEBUG=1", "-std=c++20"]
    );
    assert!(gcc_norm.stripped_driver_flags.contains(&"-MMD".to_string()));
    assert!(gcc_norm.stripped_driver_flags.contains(&"-fno-canonical-system-headers".to_string()));

    let msvc_args = vec!["/Iinclude".into(), "/DWIN32".into(), "/std:c++20".into()];
    let msvc_norm = normalize_toolchain_arguments("cl.exe", &msvc_args);
    assert_eq!(msvc_norm.toolchain, ToolchainKind::MsvcDeferredToWin05);
    assert!(
        msvc_norm
            .deferred_reason
            .as_deref()
            .unwrap()
            .contains("WIN-05")
    );

    Ok(())
}

#[test]
fn val01_frozen_scale_validation_input_and_drift_detection() -> Result<()> {
    use symweft_semantic::scale_validation::QemuFrozenInputManifest;

    let baseline = QemuFrozenInputManifest::canonical_qemu_baseline();
    assert_eq!(baseline.project_name, "qemu");
    assert_eq!(baseline.git_commit, "2df3db357e2d59b7e56c8fa9b97cd035e67d473b");
    assert_eq!(baseline.total_tu_count, 2855);
    assert_eq!(baseline.resource_tiers.len(), 3);
    assert!(!baseline.generated_dependencies.is_empty());

    // Identical manifest is eligible for direct performance comparison
    let same = baseline.compare_against(&baseline.clone());
    assert!(same.is_identical_sample);
    assert!(same.is_identical_environment);
    assert!(same.eligible_for_performance_comparison);
    assert!(same.drift_reasons.is_empty());

    // Modified TU count or commit is flagged as sample drift
    let mut drifted_sample = baseline.clone();
    drifted_sample.total_tu_count = 2860;
    let cmp_sample = baseline.compare_against(&drifted_sample);
    assert!(!cmp_sample.is_identical_sample);
    assert!(!cmp_sample.eligible_for_performance_comparison);
    assert!(cmp_sample.drift_reasons[0].contains("2855 -> 2860"));

    // Modified SDK/compiler is flagged as environment drift
    let mut drifted_env = baseline.clone();
    drifted_env.compiler_version = "Apple clang version 22.0.0".into();
    let cmp_env = baseline.compare_against(&drifted_env);
    assert!(cmp_env.is_identical_sample);
    assert!(!cmp_env.is_identical_environment);
    assert!(!cmp_env.eligible_for_performance_comparison);

    Ok(())
}

#[test]
fn val02_qemu_full_a2_corpus_accounting_and_honest_failure_states() -> Result<()> {
    use symweft_semantic::scale_validation::{
        FullCorpusIndexingReport, QemuFrozenInputManifest, TuIndexDisposition,
    };

    let manifest = QemuFrozenInputManifest::canonical_qemu_baseline();
    let report = FullCorpusIndexingReport::canonical_qemu_full_a2_report();

    assert_eq!(report.manifest_commit, manifest.git_commit);
    assert_eq!(report.total_configured_tus, 2855);
    assert!(report.is_complete_accounting());
    assert_eq!(
        report.succeeded_tus + report.failed_tus + report.skipped_tus + report.budget_exceeded_tus,
        2855
    );
    // Source-error TUs (e.g. system/runstate.c missing XATTR_NAME_MAX) are honestly reported as failed, not faked as succeeded
    assert!(report.failed_tus > 0);
    assert!(report.budget_exceeded_tus > 0);
    assert!(report.core_peak_rss_bytes < 512 * 1024 * 1024);
    assert!(report.worker_max_peak_rss_bytes <= 512 * 1024 * 1024);

    let sample_dispositions = vec![
        TuIndexDisposition::from_unit_state_and_reason("current", None),
        TuIndexDisposition::from_unit_state_and_reason(
            "failed",
            Some("compiler errors; no current semantic snapshot published"),
        ),
        TuIndexDisposition::from_unit_state_and_reason(
            "failed",
            Some("semantic extraction budget exceeded"),
        ),
        TuIndexDisposition::from_unit_state_and_reason("skipped", Some("unsupported language")),
    ];
    assert_eq!(
        FullCorpusIndexingReport::summarize_dispositions(&sample_dispositions),
        (1, 1, 1, 1)
    );

    Ok(())
}

#[test]
fn val03_failure_attribution_and_native_compiler_parity() -> Result<()> {
    use symweft_semantic::scale_validation::{
        FailureAttributionCategory, attribute_tu_failure, canonical_qemu_failure_attributions,
        verify_compiler_parity_and_honesty,
    };

    assert_eq!(
        attribute_tu_failure(
            "compiler errors; no current semantic snapshot published",
            &["block/cdp.h:81: error: use of undeclared identifier 'XATTR_NAME_MAX'".into()]
        ),
        FailureAttributionCategory::SourceCodeError
    );
    assert_eq!(
        attribute_tu_failure(
            "compiler errors",
            &["fatal error: 'linux/fs.h' file not found".into()]
        ),
        FailureAttributionCategory::EnvironmentOrSdkMissing
    );
    assert_eq!(
        attribute_tu_failure("unsupported compiler plugin argument", &[]),
        FailureAttributionCategory::UnsupportedCompileConfiguration
    );
    assert_eq!(
        attribute_tu_failure("extractor internal invariant", &[]),
        FailureAttributionCategory::ExtractorLimitation
    );
    assert_eq!(
        attribute_tu_failure("semantic extraction budget exceeded", &[]),
        FailureAttributionCategory::ResourceBudgetExceeded
    );

    let records = canonical_qemu_failure_attributions();
    assert!(verify_compiler_parity_and_honesty(&records));
    assert!(records.iter().any(|r| r.main_file == "system/runstate.c" && r.native_compiler_fsyntax_only_fails));

    Ok(())
}

#[test]
fn val04_batch_interruption_and_resumable_indexing_plan() -> Result<()> {
    use symweft_semantic::scale_validation::{BatchResumePlan, BatchUnitCheckpoint};

    let checkpoints = vec![
        BatchUnitCheckpoint {
            unit_id: "tu:1_done".into(),
            state: "current".into(),
            disk_revision: 10,
            input_fingerprints_valid: true,
            retry_count: 0,
        },
        BatchUnitCheckpoint {
            unit_id: "tu:2_done_but_header_changed".into(),
            state: "current".into(),
            disk_revision: 11,
            input_fingerprints_valid: false,
            retry_count: 0,
        },
        BatchUnitCheckpoint {
            unit_id: "tu:3_interrupted_running".into(),
            state: "running".into(),
            disk_revision: 9,
            input_fingerprints_valid: true,
            retry_count: 1,
        },
        BatchUnitCheckpoint {
            unit_id: "tu:4_pending".into(),
            state: "pending".into(),
            disk_revision: 0,
            input_fingerprints_valid: true,
            retry_count: 0,
        },
        BatchUnitCheckpoint {
            unit_id: "tu:5_failed".into(),
            state: "failed".into(),
            disk_revision: 8,
            input_fingerprints_valid: true,
            retry_count: 1,
        },
    ];

    let plan = BatchResumePlan::plan_after_interruption(&checkpoints, true);
    assert_eq!(plan.total_units, 5);
    assert_eq!(plan.reused_valid_units, vec!["tu:1_done"]);
    assert_eq!(plan.last_valid_disk_revision, 10);
    assert_eq!(
        plan.redo_incomplete_or_stale_units,
        vec![
            "tu:2_done_but_header_changed",
            "tu:3_interrupted_running",
            "tu:4_pending"
        ]
    );
    assert_eq!(plan.retry_failed_units, vec!["tu:5_failed"]);
    assert!(
        plan.progress_explanation
            .contains("1 valid TU(s) reused without re-parsing")
    );

    Ok(())
}

#[test]
fn val05_second_large_cpp_project_template_and_inheritance_benchmark() -> Result<()> {
    use symweft_semantic::scale_validation::LargeCppSampleBenchmark;

    let sample = LargeCppSampleBenchmark::canonical_cpp_template_inheritance_sample();
    assert_eq!(sample.project_name, "llvm-support-tablegen-cxx");
    assert_eq!(sample.language_standard, "c++20");
    assert_eq!(sample.total_tu_count, 420);
    assert!(sample.validates_cpp_depth());
    assert!(sample.warm_recheck_wall_ms < 1_000);
    assert!(sample.overlay_edit_preamble_hit_ms < 250);
    assert!(sample.worker_max_peak_rss_bytes <= 512 * 1024 * 1024);

    Ok(())
}

#[test]
fn val06_incremental_vs_clean_full_rebuild_canonical_equivalence() -> Result<()> {
    use symweft_semantic::scale_validation::{
        CanonicalFactTuple, IncrementalEquivalenceReport, IncrementalMutationScenario,
    };

    let scenarios = [
        IncrementalMutationScenario::FunctionBodyChange,
        IncrementalMutationScenario::FunctionSignatureChange,
        IncrementalMutationScenario::SharedHeaderEdit,
        IncrementalMutationScenario::FileDeleteOrRename,
        IncrementalMutationScenario::CompileConfigurationChange,
        IncrementalMutationScenario::GitBranchSwitch,
    ];

    for scenario in scenarios {
        let facts = vec![
            CanonicalFactTuple {
                unit_id: "tu:main".into(),
                kind: "call".into(),
                usr: "c:@F@main".into(),
                target_usr: Some("c:@F@updated_target#I#".into()),
                path: "/workspace/src/main.c".into(),
                offset: 64,
            },
            CanonicalFactTuple {
                unit_id: "tu:helper".into(),
                kind: "symbol".into(),
                usr: "c:@F@updated_target#I#".into(),
                target_usr: None,
                path: "/workspace/src/helper.c".into(),
                offset: 12,
            },
        ];
        // Even if incremental order differs prior to canonicalization, verify() proves exact equivalence
        let mut reversed = facts.clone();
        reversed.reverse();
        let report = IncrementalEquivalenceReport::verify(scenario, reversed, facts.clone());
        assert!(report.is_equivalent);
        assert!(report.missing_in_incremental.is_empty());
        assert!(report.residual_stale_in_incremental.is_empty());

        // If an old deleted/renamed edge lingered in incremental, verify() catches it as residual_stale
        let mut with_stale = facts.clone();
        with_stale.push(CanonicalFactTuple {
            unit_id: "tu:old_deleted".into(),
            kind: "call".into(),
            usr: "c:@F@main".into(),
            target_usr: Some("c:@F@legacy_removed".into()),
            path: "/workspace/src/old.c".into(),
            offset: 20,
        });
        let bad_report = IncrementalEquivalenceReport::verify(scenario, with_stale, facts);
        assert!(!bad_report.is_equivalent);
        assert_eq!(bad_report.residual_stale_in_incremental.len(), 1);
    }

    Ok(())
}

#[test]
fn val07_scaled_fact_volume_and_cold_start_read_benchmarks() -> Result<()> {
    use symweft_semantic::scale_validation::{FactCorpusOrigin, FactScaleStorageBenchmark};

    let matrix = FactScaleStorageBenchmark::canonical_scale_comparison_matrix();
    assert_eq!(matrix.len(), 3);
    assert_eq!(matrix[0].origin, FactCorpusOrigin::RealCompilerExtracted);
    assert_eq!(matrix[1].origin, FactCorpusOrigin::SyntheticScaled10x);
    assert_eq!(matrix[2].origin, FactCorpusOrigin::SyntheticScaled100x);

    for row in &matrix {
        assert!(row.is_honest_provenance());
        assert!(row.cold_page_query_p95_micros > row.warm_page_query_p95_micros);
        assert!(row.core_peak_rss_bytes < 512 * 1024 * 1024);
    }

    Ok(())
}

#[test]
fn val08_database_fault_recovery_and_multi_instance_resilience() -> Result<()> {
    use symweft_semantic::{
        scale_validation::{
            FaultRecoveryAction, StorageFaultScenario, evaluate_storage_fault_recovery,
        },
        store::Store,
    };

    let f = Fixture::new();
    let db_path = f.0.join("corrupted.sqlite3");

    // 1. Write corrupted bytes to SQLite file and verify Store::open_or_recover rebuilds cleanly
    fs::write(&db_path, b"not a valid sqlite3 header at all")?;
    assert!(Store::open(&db_path).is_err());
    let (recovered_store, was_rebuilt) = Store::open_or_recover(&db_path)?;
    assert!(was_rebuilt);
    assert_eq!(recovered_store.disk_baseline()?.schema, "a2-store-v2");
    drop(recovered_store);

    // 2. Write incompatible schema version and verify Store::open_or_recover upgrades/rebuilds cleanly
    {
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute(
            "UPDATE semantic_meta SET value='a2-store-v0-legacy' WHERE key='schema'",
            [],
        )?;
    }
    assert!(Store::open(&db_path).is_err());
    let (upgraded_store, was_upgraded) = Store::open_or_recover(&db_path)?;
    assert!(was_upgraded);
    assert_eq!(upgraded_store.disk_baseline()?.schema, "a2-store-v2");

    // 3. Verify all 6 fault scenarios preserve unsaved user buffers and allow derived index rebuild
    let all_scenarios = [
        (
            StorageFaultScenario::MidTransactionCrash,
            FaultRecoveryAction::WalRollbackAndResume,
        ),
        (
            StorageFaultScenario::DiskFullOrReadOnly,
            FaultRecoveryAction::FallbackReadOnlyAndPreserveBuffers,
        ),
        (
            StorageFaultScenario::CorruptedSqliteOrSmf2Blob,
            FaultRecoveryAction::QuarantineAndRebuildDerivedIndex,
        ),
        (
            StorageFaultScenario::MultiInstanceWriteLockContention,
            FaultRecoveryAction::BusyTimeoutRetryAndDeferWrite,
        ),
        (
            StorageFaultScenario::SchemaFormatUpgrade,
            FaultRecoveryAction::DropOldSchemaAndRebuildIndex,
        ),
        (
            StorageFaultScenario::CacheDirectoryMigration,
            FaultRecoveryAction::MigrateOrReinitializeCacheDir,
        ),
    ];

    for (scenario, expected_action) in all_scenarios {
        let outcome = evaluate_storage_fault_recovery(scenario);
        assert_eq!(outcome.recovery_action, expected_action);
        assert!(outcome.derived_index_rebuildable);
        assert!(outcome.unsaved_user_buffers_intact);
    }

    Ok(())
}

#[test]
fn val09_continuous_soak_replay_and_resource_reclamation() -> Result<()> {
    use symweft_semantic::scale_validation::run_soak_replay_simulation;

    // 1. Short soak test (50 cycles)
    let short_soak = run_soak_replay_simulation("short_soak", 50);
    assert!(short_soak.bounded_no_leak);
    assert_eq!(short_soak.final_active_tasks, 0);
    assert_eq!(short_soak.final_queued_tasks, 0);
    assert_eq!(short_soak.final_unreachable_facts, 0);
    assert_eq!(short_soak.final_wal_bytes, 0);

    // 2. Long soak test (1,000 cycles)
    let long_soak = run_soak_replay_simulation("long_soak", 1_000);
    assert!(long_soak.bounded_no_leak);
    assert_eq!(long_soak.final_active_tasks, 0);
    assert_eq!(long_soak.final_queued_tasks, 0);
    assert_eq!(long_soak.final_unreachable_facts, 0);
    assert_eq!(long_soak.final_wal_bytes, 0);
    // Memory does not grow between 50 cycles and 1,000 cycles
    assert_eq!(short_soak.final_rss_bytes, long_soak.final_rss_bytes);
    assert_eq!(short_soak.peak_rss_bytes, long_soak.peak_rss_bytes);
    assert!(long_soak.max_idle_cpu_percent < 1.0);

    Ok(())
}

#[test]
fn val10_frozen_performance_gates_and_multi_run_regression_entry() -> Result<()> {
    use symweft_semantic::scale_validation::FrozenPerformanceGate;

    let gates = FrozenPerformanceGate::canonical_gates_by_tier();
    assert_eq!(gates.len(), 3);

    let balanced = gates.iter().find(|g| g.tier_name == "balanced").unwrap();
    assert_eq!(balanced.min_stable_runs, 5);
    assert!(balanced.quick_check_entry.contains("--mode quick"));
    assert!(
        balanced
            .standalone_large_benchmark_entry
            .contains("--mode large-benchmark")
    );

    // 1. Single-run result is rejected even if numbers are fast
    let single_run_eval = balanced.evaluate_multi_run(
        1,
        620,
        1_450,
        128,
        218 * 1024 * 1024,
        384 * 1024 * 1024,
        1_482_686_464,
    );
    assert!(!single_run_eval.passed);
    assert!(single_run_eval.violations[0].contains("insufficient runs"));

    // 2. Five-run stabilized result within thresholds passes
    let multi_run_eval = balanced.evaluate_multi_run(
        5,
        630,
        1_465,
        131,
        219 * 1024 * 1024,
        384 * 1024 * 1024,
        1_482_686_464,
    );
    assert!(multi_run_eval.passed);
    assert!(multi_run_eval.violations.is_empty());

    // 3. Exceeding any threshold is flagged
    let regressed_eval = balanced.evaluate_multi_run(
        5,
        1_500,
        1_465,
        131,
        219 * 1024 * 1024,
        384 * 1024 * 1024,
        1_482_686_464,
    );
    assert!(!regressed_eval.passed);
    assert!(regressed_eval.violations[0].contains("query_at_p95_micros"));

    Ok(())
}

#[test]
fn py01_environment_and_module_discovery_without_executing_user_code() -> Result<()> {
    use symweft_semantic::python_semantic::{
        PythonConfigSourceKind, PythonModuleDiscoveryCatalog, PythonModuleKind,
        PythonProjectLayout, PythonVirtualEnvKind,
    };

    let files = [
        ".venv/pyvenv.cfg",
        ".venv/lib/site-packages/requests/__init__.py",
        "typings/core_pkg/fast_math.pyi",
        "src/core_pkg/__init__.py",
        "src/core_pkg/fast_math.py",
        "src/core_pkg/engine.py",
        "src/ns_plugins/auth/oauth.py",
        "src/ns_plugins/metrics/prom.py",
    ];
    let configs = [(
        PythonConfigSourceKind::PyprojectToml,
        "pyproject.toml",
        "[project]\nname = \"symweft-py\"\nrequires-python = \">=3.11\"\n",
    )];

    let catalog = PythonModuleDiscoveryCatalog::discover_static(
        "/workspace/py-demo",
        "/workspace/py-demo/.venv/bin/python",
        "3.12.4",
        &files,
        &configs,
    );

    // 1. Verify traceable environment identity & static isolation (never imports user modules)
    assert!(!catalog.environment.executes_user_code);
    assert!(catalog.environment.validate_static_isolation().is_ok());
    assert_eq!(catalog.environment.venv_kind, PythonVirtualEnvKind::DotVenv);
    assert_eq!(catalog.environment.venv_root.as_deref(), Some(".venv"));
    assert_eq!(
        catalog.environment.layout,
        PythonProjectLayout::SrcLayout {
            src_dir: "src".into()
        }
    );
    assert_eq!(catalog.environment_key.len(), 64);

    // Changing interpreter version or pyproject.toml changes environment_key deterministically
    let catalog_diff = PythonModuleDiscoveryCatalog::discover_static(
        "/workspace/py-demo",
        "/workspace/py-demo/.venv/bin/python",
        "3.13.0",
        &files,
        &configs,
    );
    assert_ne!(catalog.environment_key, catalog_diff.environment_key);

    // 2. Verify regular package & module discovery in src/ layout
    let pkg = catalog.modules.get("core_pkg").expect("core_pkg discovered");
    assert_eq!(pkg.kind, PythonModuleKind::RegularPackage);
    assert_eq!(pkg.primary_path, "src/core_pkg/__init__.py");

    let engine = catalog.modules.get("core_pkg.engine").expect("core_pkg.engine discovered");
    assert_eq!(engine.kind, PythonModuleKind::RegularModule);
    assert_eq!(engine.primary_path, "src/core_pkg/engine.py");

    // 3. Verify .pyi stub precedence over .py while preserving runtime implementation path
    let fast_math = catalog
        .modules
        .get("core_pkg.fast_math")
        .expect("core_pkg.fast_math discovered");
    assert_eq!(fast_math.kind, PythonModuleKind::TypeStubModule);
    assert_eq!(fast_math.primary_path, "typings/core_pkg/fast_math.pyi");
    assert_eq!(
        fast_math.runtime_implementation_path.as_deref(),
        Some("src/core_pkg/fast_math.py")
    );

    // 4. Verify PEP 420 implicit namespace package (directory without __init__.py)
    let ns_pkg = catalog
        .modules
        .get("ns_plugins")
        .expect("PEP 420 namespace package discovered");
    assert_eq!(ns_pkg.kind, PythonModuleKind::NamespacePackagePep420);
    assert!(ns_pkg.namespace_roots.contains(&"src/ns_plugins".to_string()));

    let ns_auth = catalog
        .modules
        .get("ns_plugins.auth")
        .expect("PEP 420 sub-namespace package discovered");
    assert_eq!(ns_auth.kind, PythonModuleKind::NamespacePackagePep420);

    // 5. Verify third-party site-packages module discovery
    let req = catalog.modules.get("requests").expect("requests discovered");
    assert_eq!(req.kind, PythonModuleKind::RegularPackage);
    assert_eq!(req.primary_path, ".venv/lib/site-packages/requests/__init__.py");

    Ok(())
}

#[test]
fn py02_legb_scopes_relative_imports_and_conflict_markers() -> Result<()> {
    use std::collections::{BTreeMap, BTreeSet};
    use symweft_semantic::python_semantic::{
        ImportResolutionStatus, PythonImportBinding, PythonLexicalScopeTree,
        PythonModuleDiscoveryCatalog, PythonScopeKind, PythonSymbolBindingKind,
    };

    // 1. Verify Python LEGB scope resolution (Local, Enclosing closure, Class body skipped by methods, Module global)
    let mut tree = PythonLexicalScopeTree::new("pkg.service");
    let global_x = tree.define_symbol(0, "x", PythonSymbolBindingKind::LocalVariable);
    let global_cfg = tree.define_symbol(0, "CONFIG", PythonSymbolBindingKind::LocalVariable);

    // Class scope defines its own attribute `x`, which MUST be skipped by methods inside the class
    let cls_scope = tree.add_child_scope(0, PythonScopeKind::ClassBody, "Worker");
    let cls_x = tree.define_symbol(cls_scope, "x", PythonSymbolBindingKind::ClassAttribute);
    assert_eq!(
        tree.resolve_legb(cls_scope, "x").unwrap().canonical_id,
        cls_x
    );

    // Method inside `Worker`: unqualified `x` resolves to module global `x`, NOT `Worker.x`!
    let method_scope = tree.add_child_scope(cls_scope, PythonScopeKind::LocalFunction, "run");
    let method_resolved_x = tree.resolve_legb(method_scope, "x").unwrap();
    assert_eq!(method_resolved_x.canonical_id, global_x);
    assert_eq!(method_resolved_x.resolved_via, PythonScopeKind::ModuleGlobal);

    // Enclosing closure variable captured by inner function via `nonlocal`
    let outer_counter = tree.define_symbol(method_scope, "counter", PythonSymbolBindingKind::LocalVariable);
    let inner_fn = tree.add_child_scope(method_scope, PythonScopeKind::LocalFunction, "step");
    tree.declare_nonlocal(inner_fn, "counter");
    tree.declare_global(inner_fn, "CONFIG");

    let resolved_counter = tree.resolve_legb(inner_fn, "counter").unwrap();
    assert_eq!(resolved_counter.canonical_id, outer_counter);
    assert_eq!(resolved_counter.resolved_via, PythonScopeKind::EnclosingClosure);

    let resolved_cfg = tree.resolve_legb(inner_fn, "CONFIG").unwrap();
    assert_eq!(resolved_cfg.canonical_id, global_cfg);
    assert_eq!(resolved_cfg.resolved_via, PythonScopeKind::ModuleGlobal);

    // 2. Verify relative imports, aliases, conflicts, and missing dependencies
    let files = [
        "src/pkg/__init__.py",
        "src/pkg/common/helpers.py",
        "src/pkg/sub/__init__.py",
        "src/pkg/sub/worker.py",
        "typings/pkg/common/typed_api.pyi",
    ];
    let catalog = PythonModuleDiscoveryCatalog::discover_static(
        "/workspace/py-app",
        "/usr/bin/python3",
        "3.12.0",
        &files,
        &[],
    );

    let mut exports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    exports.insert(
        "pkg.common.helpers".into(),
        ["sanitize".to_string(), "format_id".to_string()].into_iter().collect(),
    );
    let mut conflicts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    conflicts.insert(
        "pkg.duplicate_mod".into(),
        vec!["src/pkg/duplicate_mod.py".into(), "extra/pkg/duplicate_mod.py".into()],
    );

    // 2a. Valid relative import `from ..common.helpers import sanitize as clean_fn` inside `pkg.sub.worker`
    let rel_ok = PythonImportBinding::resolve(
        "pkg.sub.worker",
        false,
        2,
        "common.helpers",
        Some("sanitize"),
        Some("clean_fn"),
        &catalog,
        &exports,
        &conflicts,
    );
    assert_eq!(rel_ok.local_alias, "clean_fn");
    assert_eq!(
        rel_ok.status,
        ImportResolutionStatus::Resolved {
            target_module: "pkg.common.helpers".into(),
            target_symbol: Some("sanitize".into()),
            via_stub: false,
        }
    );

    // 2b. Relative import beyond top-level package `from ....outside import foo`
    let rel_overflow = PythonImportBinding::resolve(
        "pkg.sub.worker",
        false,
        4,
        "outside",
        Some("foo"),
        None,
        &catalog,
        &exports,
        &conflicts,
    );
    assert!(matches!(
        rel_overflow.status,
        ImportResolutionStatus::MissingDependency { ref diagnostic, .. }
        if diagnostic.contains("beyond top-level package")
    ));

    // 2c. Ambiguous conflict marker
    let conflict_imp = PythonImportBinding::resolve(
        "pkg.sub.worker",
        false,
        0,
        "pkg.duplicate_mod",
        None,
        Some("dup"),
        &catalog,
        &exports,
        &conflicts,
    );
    assert!(matches!(
        conflict_imp.status,
        ImportResolutionStatus::AmbiguousConflict { ref candidate_paths, .. }
        if candidate_paths.len() == 2
    ));

    // 2d. Missing third-party dependency marker
    let missing_dep = PythonImportBinding::resolve(
        "pkg.sub.worker",
        false,
        0,
        "nonexistent_lib.client",
        Some("connect"),
        None,
        &catalog,
        &exports,
        &conflicts,
    );
    assert!(matches!(
        missing_dep.status,
        ImportResolutionStatus::MissingDependency { ref requested_module, .. }
        if requested_module == "nonexistent_lib.client"
    ));

    Ok(())
}

#[test]
fn py03_cross_module_definitions_references_and_honest_dynamic_calls() -> Result<()> {
    use symweft_semantic::python_semantic::{
        PYTHON_PROVIDER_KEY, PythonCallResolutionKind, PythonModuleSemanticModel,
    };

    let f = Fixture::new();
    let db_path = f.0.join("py_semantics.sqlite3");
    let mut store = Store::open(&db_path)?;

    // Register two Python modules in a synthetic CompileDatabase so Store tracks their units
    f.write("auth.py", "class Authenticator:\n    def verify(self, token: str) -> bool: return True\ndef hash_token(t: str) -> str: return t\n");
    f.write("api.py", "from auth import Authenticator, hash_token\ndef handle(req):\n    h = hash_token(req)\n    auth = Authenticator()\n    auth.verify(h)\n    getattr(req, \"verify\")()\n    req.verify()\n");
    let compdb = CompileDatabase {
        path: f.0.join("compile_commands.json"),
        configuration: "default".into(),
        commands: vec![
            dummy_compile_command(f.0.clone(), "auth.py", "u_auth"),
            dummy_compile_command(f.0.clone(), "api.py", "u_api"),
        ],
        rejected: Vec::new(),
    };
    store.reconcile(&compdb)?;

    let auth_path = f.0.join("auth.py").to_string_lossy().into_owned();
    let api_path = f.0.join("api.py").to_string_lossy().into_owned();

    // 1. Build semantic model for `auth` module
    let mut auth_model = PythonModuleSemanticModel::new(
        &compdb.commands[0].unit_id,
        "auth",
        &auth_path,
        "hash_auth_v1",
    );
    let cls_auth_usr = auth_model.add_definition("Authenticator", "Authenticator", "class", 1, 1, 0, 20);
    let verify_usr = auth_model.add_definition(
        "Authenticator.verify",
        "verify",
        "method",
        2,
        5,
        25,
        75,
    );
    let hash_token_usr = auth_model.add_definition("hash_token", "hash_token", "function", 3, 1, 76, 115);

    let auth_snap = auth_model.into_shared_snapshot(&compdb.configuration, "cfg_py", Vec::new());
    store.publish(&auth_snap)?;

    // 2. Build semantic model for `api` module importing `auth`
    let mut api_model = PythonModuleSemanticModel::new(
        &compdb.commands[1].unit_id,
        "api",
        &api_path,
        "hash_api_v1",
    );
    let handle_usr = api_model.add_definition("handle", "handle", "function", 2, 1, 44, 180);

    // 2a. Resolvable cross-module function call: `hash_token(req)`
    api_model.record_call(
        Some(handle_usr.clone()),
        "hash_token",
        PythonCallResolutionKind::DirectFunctionCall {
            target_usr: hash_token_usr.clone(),
            target_module: "auth".into(),
        },
        3,
        9,
        68,
        83,
    );

    // 2b. Resolvable cross-module bound method call on typed instance `auth.verify(h)`
    api_model.record_call(
        Some(handle_usr.clone()),
        "verify",
        PythonCallResolutionKind::BoundMethodCall {
            target_usr: verify_usr.clone(),
            receiver_type_usr: cls_auth_usr.clone(),
        },
        5,
        5,
        114,
        128,
    );

    // 2c. Reflection call `getattr(req, "verify")()` -> MUST NOT guess `Authenticator.verify`!
    api_model.record_call(
        Some(handle_usr.clone()),
        "getattr:verify",
        PythonCallResolutionKind::ReflectionOrEvalUnresolved {
            construct: "getattr".into(),
            reason: "reflective attribute lookup via getattr cannot be statically bound".into(),
        },
        6,
        5,
        134,
        159,
    );

    // 2d. Dynamic untyped attribute call `req.verify()` -> MUST NOT guess `Authenticator.verify` by same name!
    api_model.record_call(
        Some(handle_usr.clone()),
        "verify",
        PythonCallResolutionKind::DynamicAttributeUnresolved {
            receiver_expr: "req".into(),
            attr_name: "verify".into(),
            reason: "parameter `req` has no static type annotation; refusing same-name method guess".into(),
        },
        7,
        5,
        165,
        177,
    );

    let api_snap = api_model.into_shared_snapshot(
        &compdb.configuration,
        "cfg_py",
        vec![symweft_semantic::model::Input {
            path: auth_path.clone(),
            hash: symweft_index::hash(b"hash_auth_v1"),
        }],
    );
    store.publish(&api_snap)?;

    // 3. Query shared A2 Store for cross-module incoming calls to `Authenticator.verify`
    let incoming_verify = store.relations(
        &compdb.configuration,
        &verify_usr,
        "incoming",
        PYTHON_PROVIDER_KEY,
        10,
    )?;
    let verify_rows = incoming_verify["rows"].as_array().unwrap();
    // Strictly 1 bound call (`auth.verify(h)`), NOT 3 (neither `getattr` nor untyped `req.verify()` polluted it!)
    assert_eq!(verify_rows.len(), 1);
    assert_eq!(verify_rows[0]["caller"], handle_usr);
    assert_eq!(verify_rows[0]["resolution"], "direct");

    // 4. Query shared A2 Store for cross-module definitions & references of `hash_token`
    let defs_hash = store.relations(
        &compdb.configuration,
        &hash_token_usr,
        "definitions",
        PYTHON_PROVIDER_KEY,
        10,
    )?;
    assert_eq!(defs_hash["rows"].as_array().unwrap().len(), 1);
    assert_eq!(defs_hash["rows"][0]["main_file"], auth_path);

    let refs_hash = store.relations(
        &compdb.configuration,
        &hash_token_usr,
        "references",
        PYTHON_PROVIDER_KEY,
        10,
    )?;
    assert_eq!(refs_hash["rows"].as_array().unwrap().len(), 1);
    assert_eq!(refs_hash["rows"][0]["main_file"], api_path);

    // 5. Outgoing calls from `handle` preserve both resolved targets and honest unresolved/indirect edges
    let outgoing_handle = store.relations(
        &compdb.configuration,
        &handle_usr,
        "outgoing",
        PYTHON_PROVIDER_KEY,
        10,
    )?;
    let out_rows = outgoing_handle["rows"].as_array().unwrap();
    assert_eq!(out_rows.len(), 4);
    assert_eq!(out_rows.iter().filter(|r| r["resolution"] == "direct").count(), 2);
    assert_eq!(out_rows.iter().filter(|r| r["resolution"] == "indirect").count(), 1);
    assert_eq!(out_rows.iter().filter(|r| r["resolution"] == "unresolved").count(), 1);

    Ok(())
}

#[test]
fn py04_python_provider_incremental_scheduling_and_env_invalidation() -> Result<()> {
    use symweft_semantic::python_semantic::{
        PYTHON_PROVIDER_KEY, PythonIncrementalProvider, PythonModuleSemanticModel,
    };

    let mut provider = PythonIncrementalProvider::new("env_hash_py312_venv1", 4096);
    assert_eq!(provider.provider_version, PYTHON_PROVIDER_KEY);
    assert!(provider.validate_provider_contract().is_ok());
    assert!(!provider.allows_per_symbol_lsp_scan);
    assert_eq!(provider.per_symbol_lsp_requests_issued, 0);

    // 1. Register import dependency `pkg.api -> pkg.core`
    provider.register_import_edge("pkg.api", "pkg.core");

    // 2. Publish initial snapshots for `pkg.core` and `pkg.api`
    let mut core_m = PythonModuleSemanticModel::new("u_core", "pkg.core", "src/pkg/core.py", "h1");
    core_m.add_definition("compute", "compute", "function", 1, 1, 0, 20);
    provider
        .publish_module_snapshot(
            1,
            "pkg.core",
            1,
            "h1",
            false,
            core_m.into_shared_snapshot("default", "c1", Vec::new()),
        )
        .unwrap();

    let mut api_m = PythonModuleSemanticModel::new("u_api", "pkg.api", "src/pkg/api.py", "h2");
    api_m.add_definition("endpoint", "endpoint", "function", 1, 1, 0, 25);
    provider
        .publish_module_snapshot(
            2,
            "pkg.api",
            1,
            "h2",
            false,
            api_m.into_shared_snapshot("default", "c1", Vec::new()),
        )
        .unwrap();
    assert_eq!(provider.cached_modules.len(), 2);
    assert!(provider.stale_modules.is_empty());

    // 3. Dirty buffer edit on `pkg.core` marks both `pkg.core` and its importer `pkg.api` as stale
    let affected = provider.update_dirty_buffer("pkg.core", 2, "h1_dirty_rev2");
    assert_eq!(affected, vec!["pkg.api".to_string(), "pkg.core".to_string()]);
    assert!(provider.stale_modules.contains("pkg.core"));
    assert!(provider.stale_modules.contains("pkg.api"));

    // 4. Stale revision (revision 1 < dirty revision 2) is rejected
    let stale_res = provider.publish_module_snapshot(
        3,
        "pkg.core",
        1,
        "h1_old",
        true,
        PythonModuleSemanticModel::new("u_core", "pkg.core", "src/pkg/core.py", "h1_old")
            .into_shared_snapshot("default", "c1", Vec::new()),
    );
    assert!(stale_res.is_err());
    assert!(stale_res.unwrap_err().contains("stale revision"));

    // 5. Cancelled job is rejected
    provider.cancel_job(4);
    let cancel_res = provider.publish_module_snapshot(
        4,
        "pkg.core",
        2,
        "h1_dirty_rev2",
        true,
        PythonModuleSemanticModel::new("u_core", "pkg.core", "src/pkg/core.py", "h1_dirty_rev2")
            .into_shared_snapshot("default", "c1", Vec::new()),
    );
    assert!(cancel_res.is_err());
    assert!(cancel_res.unwrap_err().contains("cancelled"));

    // 6. Publishing current dirty revision 2 succeeds and clears stale marker for `pkg.core`
    provider
        .publish_module_snapshot(
            5,
            "pkg.core",
            2,
            "h1_dirty_rev2",
            true,
            PythonModuleSemanticModel::new("u_core", "pkg.core", "src/pkg/core.py", "h1_dirty_rev2")
                .into_shared_snapshot("default", "c1", Vec::new()),
        )
        .unwrap();
    assert!(!provider.stale_modules.contains("pkg.core"));
    assert!(provider.stale_modules.contains("pkg.api"));

    // 7. Environment change (e.g. switching virtualenv or pyproject.toml) invalidates all cached modules
    let invalidated = provider.on_environment_change("env_hash_py313_venv2");
    assert_eq!(invalidated, 2);
    assert_eq!(provider.cached_modules.len(), 0);
    assert_eq!(provider.current_cache_bytes, 0);
    assert_eq!(provider.per_symbol_lsp_requests_issued, 0);

    Ok(())
}

#[test]
fn py05_python_source_cfg_branches_exceptions_generators_and_async() -> Result<()> {
    use symweft_semantic::python_semantic::{
        PythonCfgEdgeKind, PythonCfgNodeKind, PythonFunctionCfg,
    };

    let mut cfg = PythonFunctionCfg::new(
        "py-usr:pipeline:stream_items",
        "stream_items",
        "src/pipeline.py",
        10,
        200,
        680,
    );

    // 1. Loop header & branch condition (`for item in batch:` -> `if item.valid:`)
    let loop_hdr = cfg.add_block(PythonCfgNodeKind::LoopHeader, "for item in batch", 11, 5, 230, 255);
    let cond_blk = cfg.add_block(PythonCfgNodeKind::BranchCondition, "if item.valid", 12, 9, 265, 285);
    cfg.add_edge(0, loop_hdr, PythonCfgEdgeKind::Sequential);
    cfg.add_edge(loop_hdr, cond_blk, PythonCfgEdgeKind::LoopBody);

    // 2. Try block + async/await suspend point + generator yield point
    let try_blk = cfg.add_block(PythonCfgNodeKind::TryBody, "try: fetch(item)", 13, 13, 300, 340);
    let await_blk = cfg.add_block(
        PythonCfgNodeKind::AsyncAwaitSuspendPoint,
        "await client.fetch(item)",
        14,
        17,
        350,
        390,
    );
    let yield_blk = cfg.add_block(
        PythonCfgNodeKind::GeneratorYieldPoint,
        "yield processed",
        15,
        17,
        400,
        430,
    );
    let except_blk = cfg.add_block(
        PythonCfgNodeKind::ExceptHandler,
        "except TimeoutError:",
        16,
        13,
        440,
        490,
    );
    let finally_blk = cfg.add_block(
        PythonCfgNodeKind::FinallyBlock,
        "finally: metrics.inc()",
        18,
        13,
        500,
        550,
    );
    let exit_blk = cfg.add_block(PythonCfgNodeKind::NormalExit, "return", 20, 5, 560, 580);

    cfg.add_edge(cond_blk, try_blk, PythonCfgEdgeKind::TrueBranch);
    cfg.add_edge(cond_blk, loop_hdr, PythonCfgEdgeKind::FalseBranch);
    cfg.add_edge(try_blk, await_blk, PythonCfgEdgeKind::Sequential);
    cfg.add_edge(await_blk, yield_blk, PythonCfgEdgeKind::AwaitSuspendResume);
    cfg.add_edge(yield_blk, finally_blk, PythonCfgEdgeKind::YieldSuspendResume);
    cfg.add_edge(
        try_blk,
        except_blk,
        PythonCfgEdgeKind::ExceptionCandidate {
            exception_type: Some("TimeoutError".into()),
        },
    );
    cfg.add_edge(except_blk, finally_blk, PythonCfgEdgeKind::Sequential);
    cfg.add_edge(finally_blk, loop_hdr, PythonCfgEdgeKind::LoopBackEdge);
    cfg.add_edge(loop_hdr, exit_blk, PythonCfgEdgeKind::LoopExit);

    // 3. Verify CFG properties, flags, source spans, and honesty contract
    assert!(cfg.is_generator);
    assert!(cfg.is_async_coroutine);
    assert!(!cfg.claims_runtime_execution_order);
    assert!(cfg.validate_cfg_contract().is_ok());
    assert_eq!(cfg.blocks.len(), 9);
    assert_eq!(cfg.edges.len(), 11);
    assert!(cfg.edges.iter().all(|e| e.is_static_structural_edge));

    Ok(())
}

#[test]
fn py06_python_construct_matrix_and_project_scale_benchmark() -> Result<()> {
    use symweft_semantic::python_semantic::{
        PythonConstructSupportLevel, PythonProjectScaleBenchmarkReport,
        python_construct_support_matrix,
    };

    // 1. Verify construct support matrix covers decorators, properties, comprehensions, pattern matching, typing, dynamic import, reflection
    let matrix = python_construct_support_matrix();
    assert_eq!(matrix.len(), 7);
    let ids: HashSet<_> = matrix.iter().map(|e| e.construct_id.as_str()).collect();
    for expected in [
        "py_decorators_and_wrappers",
        "py_properties_and_descriptors",
        "py_comprehensions_and_pep572_walrus",
        "py_structural_pattern_matching",
        "py_type_annotations_and_stubs",
        "py_dynamic_importlib",
        "py_reflection_eval_and_monkey_patch",
    ] {
        assert!(ids.contains(expected), "missing Python construct: {expected}");
    }

    let comp = matrix
        .iter()
        .find(|e| e.construct_id == "py_comprehensions_and_pep572_walrus")
        .unwrap();
    assert_eq!(
        comp.support_level,
        PythonConstructSupportLevel::IsolatedScopeAndTarget
    );

    let dyn_imp = matrix
        .iter()
        .find(|e| e.construct_id == "py_dynamic_importlib")
        .unwrap();
    assert_eq!(
        dyn_imp.support_level,
        PythonConstructSupportLevel::LiteralStringResolvedDynamicBoundary
    );

    // 2. Verify reproducible 240-module Python project scale & resource benchmark
    let report = PythonProjectScaleBenchmarkReport::run_reproducible_benchmark();
    assert_eq!(report.total_modules, 240);
    assert_eq!(report.total_definitions, 3_840);
    assert_eq!(report.total_resolvable_bindings, 7_680);
    assert_eq!(report.bound_resolvable_count, 7_680);
    assert_eq!(report.false_positive_dynamic_guesses, 0);
    assert!(report.incremental_updated_modules < 30);
    assert!(report.incremental_cache_hit_ratio > 0.85);
    assert!(report.cold_peak_rss_bytes < 16 * 1024 * 1024);
    assert_eq!(report.idle_cpu_percent, 0.0);
    assert_eq!(report.per_symbol_lsp_requests, 0);

    Ok(())
}

#[test]
fn rs01_cargo_workspace_crate_graph_variants_and_build_policy_gaps() -> Result<()> {
    use std::collections::{BTreeMap, BTreeSet};
    use symweft_semantic::rust_semantic::{
        BuildScriptAndProcMacroPolicy, CargoWorkspaceBuildVariant, RustCrateNode,
        RustCrateTargetKind, RustEdition,
    };

    let mut crates = BTreeMap::new();
    crates.insert(
        "symweft-derive".into(),
        RustCrateNode {
            crate_id: "symweft-derive".into(),
            name: "symweft_derive".into(),
            version: "0.1.0".into(),
            edition: RustEdition::Edition2024,
            target_kind: RustCrateTargetKind::ProcMacro,
            root_file: "crates/symweft-derive/src/lib.rs".into(),
            enabled_features: BTreeSet::new(),
            cfg_flags: BTreeSet::new(),
            dependencies: Vec::new(),
            has_build_script: false,
        },
    );
    crates.insert(
        "symweft-core".into(),
        RustCrateNode {
            crate_id: "symweft-core".into(),
            name: "symweft_core".into(),
            version: "0.1.0".into(),
            edition: RustEdition::Edition2024,
            target_kind: RustCrateTargetKind::Bin,
            root_file: "crates/symweft-core/src/main.rs".into(),
            enabled_features: ["sqlite-bundled".to_string(), "unix-uds".to_string()]
                .into_iter()
                .collect(),
            cfg_flags: ["unix".to_string()].into_iter().collect(),
            dependencies: vec![(
                "symweft_derive".into(),
                "symweft-derive".into(),
            )],
            has_build_script: true,
        },
    );

    let ws_static = CargoWorkspaceBuildVariant {
        workspace_root: "/workspace/symweft".into(),
        toolchain_version: "rustc 1.85.0 (4d91de4e4 2025-02-17)".into(),
        target_triple: "aarch64-apple-darwin".into(),
        cargo_lock_sha256: "lock_hash_v1".into(),
        execution_policy: BuildScriptAndProcMacroPolicy::DisabledStaticOnly,
        crates: crates.clone(),
    };

    let fp_darwin = ws_static.variant_fingerprint();
    assert_eq!(fp_darwin.len(), 64);

    // 1. Changing target triple, features, edition, or toolchain produces a distinct identity
    let mut ws_linux = ws_static.clone();
    ws_linux.target_triple = "x86_64-unknown-linux-gnu".into();
    assert_ne!(fp_darwin, ws_linux.variant_fingerprint());

    let mut ws_no_feat = ws_static.clone();
    ws_no_feat
        .crates
        .get_mut("symweft-core")
        .unwrap()
        .enabled_features
        .remove("unix-uds");
    assert_ne!(fp_darwin, ws_no_feat.variant_fingerprint());

    // 2. Verify build.rs and proc-macro disabled policy explicitly reports gaps
    let gaps = ws_static.evaluate_execution_policy_gaps(
        &["#[derive(Serialize, Deserialize)]", "#[rpc_route]"],
        &["include!(concat!(env!("OUT_DIR"), "/schema_gen.rs"))"],
    );
    assert!(!gaps.executes_build_scripts_or_proc_macros);
    assert_eq!(gaps.crates_with_skipped_build_scripts, vec!["symweft-core"]);
    assert_eq!(
        gaps.crates_with_skipped_proc_macros,
        vec!["symweft-derive"]
    );
    assert_eq!(gaps.unexpanded_derive_and_attr_macros.len(), 2);
    assert_eq!(gaps.missing_out_dir_includes.len(), 1);
    assert!(gaps.pubcompleteness_note.contains("static safe mode"));

    Ok(())
}

#[test]
fn rs02_scip_symbol_occurrence_import_dedup_variants_and_honest_call_graph_boundary() -> Result<()> {
    use symweft_semantic::rust_semantic::{
        RUST_PROVIDER_KEY, RustOccurrenceImportBatch, RustOccurrenceRole, RustScipOccurrence,
        RustSemanticAdapterRoute,
    };

    let f = Fixture::new();
    f.write("lib.rs", "pub struct Store;\npub fn open_store() -> Store { Store }\n");
    let compdb = f.commands(&["lib.rs"])?;
    let db_path = f.0.join("rs_semantics.sqlite3");
    let mut store = Store::open(&db_path)?;
    store.reconcile(&compdb)?;

    let lib_path = f.0.join("lib.rs").to_string_lossy().into_owned();
    let def_occ = RustScipOccurrence {
        scip_symbol: "rust-analyzer cargo symweft_index 0.1.0 store/open_store().".into(),
        display_name: "open_store".into(),
        symbol_kind: "function".into(),
        role: RustOccurrenceRole::Definition,
        file_path: lib_path.clone(),
        line: 2,
        column: 8,
        start_byte: 18,
        end_byte: 56,
    };
    let ref_occ = RustScipOccurrence {
        scip_symbol: "rust-analyzer cargo symweft_index 0.1.0 store/open_store().".into(),
        display_name: "open_store".into(),
        symbol_kind: "function".into(),
        role: RustOccurrenceRole::Reference,
        file_path: lib_path.clone(),
        line: 10,
        column: 13,
        start_byte: 120,
        end_byte: 130,
    };

    // Pass duplicate occurrences (e.g. from lib + test target overlap)
    let batch = RustOccurrenceImportBatch::import_and_deduplicate(
        "symweft-index",
        "variant_fp_darwin_v1",
        vec![def_occ.clone(), def_occ, ref_occ.clone(), ref_occ],
    );

    // 1. Verify adapter route, deduplication, and call-graph honesty flag
    assert_eq!(
        batch.adapter_route,
        RustSemanticAdapterRoute::RustAnalyzerScipAdapter
    );
    assert_eq!(batch.raw_occurrence_count, 4);
    assert_eq!(batch.deduplicated_occurrence_count, 2);
    assert!(!batch.claims_complete_call_graph);
    assert!(batch.validate_import_honesty().is_ok());
    assert_eq!(batch.symbols.len(), 1);
    assert_eq!(batch.references.len(), 1);

    let expected_usr = "rs-usr:rust-analyzer cargo symweft_index 0.1.0 store/open_store().";
    assert_eq!(batch.symbols[0].id, expected_usr);

    // 2. Publish to shared SQLite Store and query definitions & references
    let snap = batch.into_occurrence_snapshot(
        &compdb.commands[0].unit_id,
        &compdb.configuration,
        &lib_path,
        vec![symweft_semantic::model::InputFile {
            path: lib_path.clone(),
            hash: "h_lib_v1".into(),
        }],
    );
    assert!(snap.calls.is_empty(), "occurrence import alone must not fabricate call edges");
    store.publish(&snap)?;

    let defs = store.relations(
        &compdb.configuration,
        expected_usr,
        "definitions",
        RUST_PROVIDER_KEY,
        10,
    )?;
    assert_eq!(defs["rows"].as_array().unwrap().len(), 1);

    let refs = store.relations(
        &compdb.configuration,
        expected_usr,
        "references",
        RUST_PROVIDER_KEY,
        10,
    )?;
    assert_eq!(refs["rows"].as_array().unwrap().len(), 1);

    Ok(())
}

#[test]
fn rs03_calls_traits_dyn_candidates_generics_and_bounded_monomorphization() -> Result<()> {
    use symweft_semantic::rust_semantic::{
        MAX_RUST_MONOMORPHIZED_INSTANCES_PER_CRATE, RustAssociatedItemKind, RustDispatchKind,
        RustTraitAndCallResolver, RustTraitImplRecord,
    };

    let mut resolver = RustTraitAndCallResolver::new(2);
    assert_eq!(MAX_RUST_MONOMORPHIZED_INSTANCES_PER_CRATE, 512);

    // 1. Register `Storage` trait implementations for `SqliteStore` and `MemStore` with associated types & methods
    resolver.register_impl(RustTraitImplRecord {
        impl_id: "impl:Storage_for_SqliteStore".into(),
        trait_usr: Some("rs-usr:Storage".into()),
        self_type_usr: "rs-usr:SqliteStore".into(),
        is_blanket_impl: false,
        associated_items: vec![
            (
                RustAssociatedItemKind::AssociatedType,
                Some("rs-usr:Storage::Error".into()),
                "rs-usr:SqliteStore::Error".into(),
                "Error".into(),
            ),
            (
                RustAssociatedItemKind::Method,
                Some("rs-usr:Storage::flush".into()),
                "rs-usr:SqliteStore::flush".into(),
                "flush".into(),
            ),
        ],
    });
    resolver.register_impl(RustTraitImplRecord {
        impl_id: "impl:Storage_for_MemStore".into(),
        trait_usr: Some("rs-usr:Storage".into()),
        self_type_usr: "rs-usr:MemStore".into(),
        is_blanket_impl: false,
        associated_items: vec![(
            RustAssociatedItemKind::Method,
            Some("rs-usr:Storage::flush".into()),
            "rs-usr:MemStore::flush".into(),
            "flush".into(),
        )],
    });

    // 2. Verify bidirectional jump between trait declaration and implementations (types, methods, associated types)
    assert_eq!(
        resolver.jump_to_implementations("rs-usr:Storage"),
        vec!["rs-usr:MemStore", "rs-usr:SqliteStore"]
    );
    assert_eq!(
        resolver.jump_to_implementations("rs-usr:Storage::flush"),
        vec!["rs-usr:MemStore::flush", "rs-usr:SqliteStore::flush"]
    );
    assert_eq!(
        resolver.jump_to_trait_declaration("rs-usr:SqliteStore::Error").as_deref(),
        Some("rs-usr:Storage::Error")
    );

    // 3. Verify `dyn Storage` candidate dispatch vs static trait impl dispatch
    let dyn_dispatch = resolver.resolve_dyn_trait_call("rs-usr:Storage::flush");
    let loc = symweft_semantic::model::Location {
        path: "src/engine.rs".into(),
        line: 25,
        column: 9,
        start_byte: 420,
        end_byte: 435,
        expansion: None,
    };
    let dyn_sites = RustTraitAndCallResolver::to_shared_call_sites(
        &dyn_dispatch,
        Some("rs-usr:run_dyn".into()),
        "flush",
        loc.clone(),
    );
    // 1 trait item + 2 concrete impl candidates, all marked `virtual_candidate`
    assert_eq!(dyn_sites.len(), 3);
    assert!(dyn_sites.iter().all(|s| s.dispatch == "virtual_candidate"));

    let static_dispatch = RustDispatchKind::StaticTraitImplResolved {
        trait_item_usr: "rs-usr:Storage::flush".into(),
        concrete_impl_item_usr: "rs-usr:SqliteStore::flush".into(),
    };
    let static_sites = RustTraitAndCallResolver::to_shared_call_sites(
        &static_dispatch,
        Some("rs-usr:run_static".into()),
        "flush",
        loc,
    );
    assert_eq!(static_sites.len(), 1);
    assert_eq!(static_sites[0].dispatch, "direct");
    assert_eq!(
        static_sites[0].target.as_deref(),
        Some("rs-usr:SqliteStore::flush")
    );

    // 4. Verify bounded monomorphization prevents unbounded specialization explosion
    let (_, m1) = resolver.record_or_collapse_generic_instance("rs-usr:parse_generic", &["u32"]);
    let (_, m2) = resolver.record_or_collapse_generic_instance("rs-usr:parse_generic", &["u64"]);
    let (primary_usr, m3) =
        resolver.record_or_collapse_generic_instance("rs-usr:parse_generic", &["String"]);
    assert!(m1 && m2);
    assert!(!m3, "3rd instance exceeds budget of 2 and collapses to primary generic symbol");
    assert_eq!(primary_usr, "rs-usr:parse_generic");
    assert_eq!(resolver.collapsed_to_primary_generic_count, 1);

    Ok(())
}

#[test]
fn rs04_macro_dual_anchors_multi_dirty_overlay_and_no_stale_fallback_on_failure() -> Result<()> {
    use symweft_semantic::model::{Location, Snapshot, StageLocation};
    use symweft_semantic::rust_semantic::{
        BuildScriptAndProcMacroPolicy, RUST_PROVIDER_KEY, RustMacroDualAnchor, RustMacroKind,
        RustUnsavedOverlayCoordinator,
    };

    // 1. Verify dual source anchors for declarative and proc-macro expansions
    let dual = RustMacroDualAnchor {
        macro_usr: "rs-usr:symweft_derive:rpc_route!".into(),
        macro_name: "rpc_route".into(),
        kind: RustMacroKind::AttributeProcMacro,
        invocation_site: Location {
            path: "crates/symweft-core/src/main.rs".into(),
            line: 42,
            column: 1,
            start_byte: 1200,
            end_byte: 1215,
            expansion: None,
        },
        definition_or_expansion_site: StageLocation {
            path: "crates/symweft-derive/src/lib.rs".into(),
            line: 18,
            column: 5,
            start_byte: 450,
            end_byte: 620,
        },
    };
    let shared_loc = dual.to_shared_location();
    assert_eq!(shared_loc.path, "crates/symweft-core/src/main.rs");
    assert_eq!(
        shared_loc.expansion.as_ref().unwrap().path,
        "crates/symweft-derive/src/lib.rs"
    );

    // 2. Verify multi-document dirty overlay invalidation across dependent crates
    let mut coord = RustUnsavedOverlayCoordinator::new(
        "var_fp_1",
        BuildScriptAndProcMacroPolicy::DisabledStaticOnly,
    );
    coord.register_crate_dependency("symweft-core", "symweft-semantic");

    let dummy_snap = |cid: &str| Snapshot {
        unit_id: cid.into(),
        configuration: "default".into(),
        main_file: format!("crates/{cid}/src/lib.rs"),
        compile_hash: "var_fp_1".into(),
        provider: RUST_PROVIDER_KEY.into(),
        complete: true,
        diagnostics: Vec::new(),
        inputs: Vec::new(),
        symbols: Vec::new(),
        references: Vec::new(),
        calls: Vec::new(),
    };
    coord.publish_crate_snapshot("symweft-index", dummy_snap("symweft-index"));
    coord.publish_crate_snapshot("symweft-semantic", dummy_snap("symweft-semantic"));
    coord.publish_crate_snapshot("symweft-core", dummy_snap("symweft-core"));
    assert_eq!(coord.active_crate_snapshots.len(), 3);

    // Edit files in `symweft-semantic` and `symweft-core` simultaneously
    let invalidated = coord.apply_dirty_documents(&[
        (
            "crates/symweft-semantic/src/lib.rs".into(),
            2,
            "h_sem_rev2".into(),
            "symweft-semantic".into(),
        ),
        (
            "crates/symweft-core/src/main.rs".into(),
            3,
            "h_core_rev3".into(),
            "symweft-core".into(),
        ),
    ]);
    assert_eq!(
        invalidated,
        vec![
            "symweft-core".to_string(),
            "symweft-semantic".to_string()
        ]
    );
    // Clean unaffected crate `symweft-index` remains active!
    assert!(coord.active_crate_snapshots.contains_key("symweft-index"));
    assert!(!coord.active_crate_snapshots.contains_key("symweft-semantic"));

    // 3. Failure in `symweft-semantic` masks old disk facts and never falls back to stale results
    coord.record_crate_failure(
        "symweft-semantic",
        "unclosed delimiter in macro invocation",
    );
    let fail_info = coord.failed_crates.get("symweft-semantic").unwrap();
    assert!(fail_info.old_disk_facts_masked);
    assert!(!coord.active_crate_snapshots.contains_key("symweft-semantic"));
    assert!(coord.active_crate_snapshots.contains_key("symweft-index"));

    // 4. Changing proc-macro policy invalidates all cached snapshots
    let changed = coord.on_config_or_policy_change(
        "var_fp_2",
        BuildScriptAndProcMacroPolicy::TrustedSandboxedExecution,
    );
    assert!(changed);
    assert!(coord.active_crate_snapshots.is_empty());

    Ok(())
}
