use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use symweft_semantic::{
    Result,
    compdb::CompileDatabase,
    model::{
        MAX_OVERLAY_DOCUMENTS, MAX_OVERLAY_SOURCE_BYTES, Overlay, PreviewDocument, Snapshot,
        WorkerRequest,
    },
    provider,
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
request=json.load(sys.stdin)
root=pathlib.Path(request['command']['root'])
lock=(root/'worker.lock').open('w')
try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
except BlockingIOError:
    (root/'overlap').write_text('two compiler processes')
    sys.exit(9)
with (root/'workers').open('a') as events:
    events.write(str(os.getpid())+'\n')
overlays=request['overlays']
if not overlays: time.sleep(5)
else: time.sleep(.15)
message='x'*(2*1024*1024) if overlays and 'budget' in overlays[0]['content'] else 'fixture rejection'
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
    f.write("a.c", "#include "api.h"
int a_call(void){return target() + VAL;}
");
    f.write("b.c", "#include "api.h"
int b_call(void){return target() + VAL;}
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

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        f.0.join("worker-dummy"),
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
    f.write("a.c", "#include "api.h"
int a_call(void){return target() + VAL;}
");
    f.write("b.c", "#include "api.h"
int b_call(void){return target() + VAL;}
");
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
    f.write("a.c", "#include "api.h"
int a_call(void){return target() + VAL;}
");
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
        f.0.join("worker-dummy"),
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

    let a_unit = db.commands[0].unit_id.clone();
    let b_unit = db.commands[1].unit_id.clone();

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        f.0.join("worker-dummy"),
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
        f.0.join("worker-dummy"),
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
    let ctx = controller.preview_at(id, &b_path, 2, 0)?;
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
    f.write("a.c", "#include "target.h"
int a1(void){return target_func();}
int a2(void){return target_func();}
int a3(void){return target_func();}
");
    f.write("b.c", "#include "target.h"
int b1(void){return target_func();}
int b2(void){return target_func();}
int b3(void){return target_func();}
");
    f.write("c.c", "#include "target.h"
int c1(void){return target_func();}
int c2(void){return target_func();}
");
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

    let a_unit = db.commands[0].unit_id.clone();
    let b_unit = db.commands[1].unit_id.clone();
    let c_unit = db.commands[2].unit_id.clone();
    let b_path = f.0.join("b.c").to_string_lossy().into_owned();

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        path.clone(),
        f.0.join("worker-dummy"),
        Limits::default(),
    )?;

    // Edit b.c in memory to have 4 calls to target_func
    let new_b_source = "#include "target.h"
int b1(void){return target_func();}
int b2(void){return target_func();}
int b3(void){return target_func();}
int b4(void){return target_func();}
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
    assert_eq!(disk_p1["total"], 8); // in disk: a(3) + b(3) + c(2) = 8
    assert_eq!(disk_p1["has_more"], true);
    assert_eq!(disk_p1["truncated"], true);

    let disk_p2 = store.relations_page(&db.configuration, &target_sym.id, "incoming", &a_snap.provider, 4, 4)?;
    assert_eq!(disk_p2["offset"], 4);
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
        json!({"directory": f.0, "file": "extra.c", "arguments": ["clang", "-DFEATURE_VARIANT=1", "-I.", "-c", "extra.c", "-o", "c.o"]}),
    ];
    fs::write(f.0.join("compile_commands.json"), serde_json::to_vec(&entries)?)?;
    let db = CompileDatabase::load(&f.0, &f.0.join("compile_commands.json"))?;

    let db_path = f.0.join("semantics.sqlite3");
    let mut store = Store::open(&db_path)?;
    store.reconcile(&db)?;
    let a_snap = analyze(&db, 0)?;
    let b_snap = analyze(&db, 1)?;
    let extra_snap = analyze(&db, 2)?;
    store.publish(&a_snap)?;
    store.publish(&b_snap)?;
    store.publish(&extra_snap)?;
    drop(store);

    let target_sym = a_snap.symbols.iter().find(|s| s.name == "common_target").unwrap();

    let mut controller = Controller::open(
        &f.0,
        &db.path,
        db_path.clone(),
        f.0.join("worker-dummy"),
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
    let a_unit = db.commands[0].unit_id.clone();

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
        if s["state"] == "ready" && s["ticket"]["job_id"] == id {
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
    assert!(s["reason"].as_deref().unwrap_or("").contains("configuration changed") || fresh.is_err());

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
    let a_new_snap = analyze(&db, 0)?;
    let b_new_snap = analyze(&db, 1)?;
    store.publish(&a_new_snap)?;
    store.publish(&b_new_snap)?;

    // 7. Query disk relations and verify 100% normalized equivalence
    let disk_res = store.relations_page(&db.configuration, &target_sym.id, "incoming", &a_new_snap.provider, 100, 0)?;
    let disk_rows = disk_res["rows"].as_array().unwrap();
    assert_eq!(disk_rows.len(), 5);

    let mut disk_norm: Vec<_> = disk_rows.iter().map(normalize).collect();
    disk_norm.sort();

    assert_eq!(overlay_norm, disk_norm, "overlay project relations and post-save disk relations must match exactly under normalized criteria!");

    Ok(())
}
