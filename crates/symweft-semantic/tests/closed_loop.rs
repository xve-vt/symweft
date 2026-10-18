use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use symweft_semantic::{
    Result,
    compdb::CompileDatabase,
    model::{Overlay, Snapshot, WorkerRequest},
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
    store.publish(&a)?;
    store.publish(&b)?;
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
        if controller.preview_status()["state"] == "failed" {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(controller.preview_status()["state"], "failed");
    assert_eq!(controller.preview_status()["ticket"]["job_id"], id);
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
        if controller.preview_status()["state"] == "failed" {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let status = controller.preview_status();
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
