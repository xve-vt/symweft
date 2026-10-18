#[path = "overlay.rs"]
mod overlay;
#[path = "realtime.rs"]
mod realtime;
use crate::{
    Result,
    compdb::CompileDatabase,
    model::{
        DiskBaseline, DocumentRevision, Overlay, QUERY_VERSION_CONTRACT, QueryConfiguration,
        QueryCoverage, QueryVersion, WorkerReply, WorkerRequest,
    },
    provider,
    store::Store,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub timeout_seconds: u64,
    pub worker_memory_mib: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_seconds: 30,
            worker_memory_mib: 512,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        if !(1..=120).contains(&self.timeout_seconds)
            || !(32..=4096).contains(&self.worker_memory_mib)
        {
            return Err("semantic budget must be 1..120 seconds and 32..4096 MiB".into());
        }
        Ok(())
    }
}
fn read_limited(mut pipe: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let n = pipe.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if output.len() + n > limit {
            return Err("semantic worker output exceeds limit".into());
        }
        output.extend_from_slice(&chunk[..n]);
    }
    Ok(output)
}
struct WorkerChild(std::process::Child);
impl Drop for WorkerChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn process_rss(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        text.lines()
            .find(|l| l.starts_with("VmRSS:"))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|n| n * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("/bin/ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()
            .map(|n| n * 1024)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}
pub fn run_worker(
    executable: &Path,
    request: &WorkerRequest,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<WorkerReply> {
    let profiling = crate::profile::enabled();
    let total = crate::profile::Clock::start(profiling);
    let encode = crate::profile::Clock::start(profiling);
    let mut measured = crate::profile::RunProfile::default();
    limits.validate()?;
    let bytes = serde_json::to_vec(request)?;
    measured.request_encode_micros = encode.micros();
    measured.request_bytes = bytes.len();
    if bytes.len() > 12 * 1024 * 1024 {
        return Err("semantic request exceeds 12 MiB".into());
    }
    let spawn = crate::profile::Clock::start(profiling);
    let mut managed = WorkerChild(
        Command::new(executable)
            .arg("semantic-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    measured.spawn_micros = spawn.micros();
    let child = &mut managed.0;
    let stdout = child.stdout.take().ok_or("worker stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("worker stderr unavailable")?;
    let output = thread::spawn(move || read_limited(stdout, 64 * 1024 * 1024));
    let errors = thread::spawn(move || read_limited(stderr, 256 * 1024));
    let write = crate::profile::Clock::start(profiling);
    if let Err(e) = child
        .stdin
        .take()
        .ok_or("worker stdin unavailable")?
        .write_all(&bytes)
    {
        let _ = child.kill();
        let _ = child.wait();
        let _ = output.join();
        let _ = errors.join();
        return Err(e.into());
    }
    measured.stdin_write_micros = write.micros();
    let wait = crate::profile::Clock::start(profiling);
    let start = Instant::now();
    let mut sampled = Instant::now();
    let mut failure = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.load(Ordering::Relaxed) {
            failure = Some("cancelled".to_owned());
        } else if start.elapsed() > Duration::from_secs(limits.timeout_seconds) {
            failure = Some("semantic worker timeout".to_owned());
        } else if sampled.elapsed() > Duration::from_millis(250) {
            sampled = Instant::now();
            if process_rss(child.id()).is_some_and(|n| n > limits.worker_memory_mib * 1024 * 1024) {
                failure = Some("semantic worker memory budget exceeded (sampled RSS)".to_owned());
            }
        }
        if failure.is_some() {
            let _ = child.kill();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(15));
    };
    let output = output.join().map_err(|_| "worker output thread panicked")?;
    let errors = errors
        .join()
        .map_err(|_| "worker stderr thread panicked")??;
    measured.child_wait_micros = wait.micros();
    if let Some(failure) = failure {
        return Err(failure.into());
    }
    if !status.success() {
        return Err(format!(
            "semantic worker exited {status}: {}",
            String::from_utf8_lossy(&errors)
        )
        .into());
    }
    let output = output?;
    measured.reply_bytes = output.len();
    let decode = crate::profile::Clock::start(profiling);
    let mut reply: WorkerReply = serde_json::from_slice(&output)?;
    measured.reply_decode_micros = decode.micros();
    if profiling {
        measured.worker = String::from_utf8_lossy(&errors)
            .lines()
            .filter_map(|line| line.strip_prefix(crate::profile::PREFIX))
            .filter_map(|line| serde_json::from_str::<crate::profile::WorkerProfile>(line).ok())
            .rfind(|p| p.pid == child.id());
        if let (Some(snapshot), Some(peak)) = (
            &mut reply.snapshot,
            measured.worker.as_ref().and_then(|p| p.self_peak_rss_bytes),
        ) {
            snapshot.worker_peak_rss_bytes = snapshot.worker_peak_rss_bytes.max(peak);
        }
        measured.controller_wall_micros = total.micros();
        reply.profile = Some(measured);
    }
    Ok(reply)
}
fn unchanged(snapshot: &crate::model::Snapshot) -> bool {
    snapshot
        .inputs
        .iter()
        .all(|i| fs::read(&i.path).is_ok_and(|b| symweft_index::hash(&b) == i.hash))
}
pub struct Controller {
    pub database: CompileDatabase,
    pub storage: PathBuf,
    pub provider: String,
    pub limits: Limits,
    executable: PathBuf,
    database_hash: [u8; 32],
    workspace_root: PathBuf,
    configuration_key: String,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<Value>>,
    job: Option<JoinHandle<()>>,
    query_store: Store,
    validation: RefCell<crate::fingerprint::Fingerprints>,
    runtime: realtime::State,
    source_epoch: Arc<std::sync::atomic::AtomicU64>,
    overlay: overlay::State,
}
impl Controller {
    pub fn open(
        root: &Path,
        commands: &Path,
        storage: PathBuf,
        executable: PathBuf,
        limits: Limits,
    ) -> Result<Self> {
        limits.validate()?;
        let workspace_root = root.canonicalize()?;
        let database = CompileDatabase::load(&workspace_root, commands)?;
        let configuration_key = crate::compdb::key(&(&database.commands, &database.rejected));
        let provider = provider::identity_key()?;
        let mut query_store = Store::open(&storage)?;
        query_store.reconcile(&database)?;
        let database_hash = symweft_index::hash(&fs::read(&database.path)?);
        Ok(Self {
            database,
            storage,
            provider,
            limits,
            executable,
            database_hash,
            workspace_root,
            configuration_key,
            cancel: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(json!({"state":"idle"}))),
            job: None,
            query_store,
            validation: RefCell::new(crate::fingerprint::Fingerprints::default()),
            runtime: realtime::State::default(),
            source_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            overlay: overlay::State::default(),
        })
    }
    pub fn ensure_configuration(&self) -> Result<()> {
        if self.validation.borrow_mut().digest(&self.database.path) != Some(self.database_hash) {
            return Err("compilation database changed; configure semantics again".into());
        }
        for command in &self.database.commands {
            for input in &command.extra_inputs {
                if self.validation.borrow_mut().digest(Path::new(&input.path)) != Some(input.hash) {
                    return Err("response file changed; configure semantics again".into());
                }
            }
        }
        Ok(())
    }
    pub fn store(&self) -> Result<&Store> {
        self.ensure_configuration()?;
        Ok(&self.query_store)
    }
    pub fn query_version(
        &self,
        disk: DiskBaseline,
        mut documents: Vec<DocumentRevision>,
        coverage: QueryCoverage,
    ) -> QueryVersion {
        documents.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.revision.cmp(&right.revision))
        });
        QueryVersion {
            contract: QUERY_VERSION_CONTRACT.into(),
            workspace: self.workspace_root.to_string_lossy().into_owned(),
            configuration: QueryConfiguration {
                id: self.database.configuration.clone(),
                content_key: self.configuration_key.clone(),
                provider: self.provider.clone(),
            },
            disk,
            documents,
            coverage,
        }
    }
    pub fn disk_query_version(&self) -> Result<QueryVersion> {
        Ok(self.query_version(
            self.query_store.disk_baseline()?,
            Vec::new(),
            QueryCoverage::PersistentIndex,
        ))
    }
    pub fn status(&self) -> Result<Value> {
        let store = &self.query_store;
        let disk = store.disk_baseline()?;
        let query_version =
            self.query_version(disk.clone(), Vec::new(), QueryCoverage::PersistentIndex);
        let preview = self.preview_status_with_disk(disk)?;
        let mut result = json!({"configured":true,"compile_commands":self.database.path,"configuration":self.database.configuration,"configured_units":self.database.commands.len(),"rejected":self.database.rejected,"provider":self.provider,"limits":self.limits,"job":self.state.lock().map_err(|_|"semantic status poisoned")?.clone(),"index":store.status(&self.database.configuration)?,"configuration_matches":fs::read(&self.database.path).is_ok_and(|b|symweft_index::hash(&b)==self.database_hash),"query_version":query_version,"preview":preview});
        result["incompatible_provider_units"] = store
            .incompatible_provider_units(&self.database.configuration, &self.provider)?
            .into();
        result["watch"] = self.watch_status();
        Ok(result)
    }
    pub fn progress(&self) -> Result<Value> {
        let disk = self.query_store.disk_baseline()?;
        let query_version =
            self.query_version(disk.clone(), Vec::new(), QueryCoverage::PersistentIndex);
        let preview = self.preview_status_with_disk(disk)?;
        Ok(
            json!({"configured":true,"configured_units":self.database.commands.len(),"rejected_units":self.database.rejected.len(),"job":self.state.lock().map_err(|_|"semantic status poisoned")?.clone(),"watch":self.watch_status(),"preview":preview,"query_version":query_version}),
        )
    }
    pub fn index(&mut self, paths: &[String], limit: usize) -> Result<Value> {
        self.ensure_configuration()?;
        if self.job.as_ref().is_some_and(|h| !h.is_finished()) {
            return Err("semantic job already running".into());
        }
        if let Some(job) = self.job.take() {
            let _ = job.join();
        }
        let store = self.store()?;
        let mut affected = HashSet::new();
        if paths.len() > 128 {
            return Err("at most 128 changed paths per request".into());
        }
        let selected_paths: HashSet<String> = paths
            .iter()
            .map(|path| self.absolute_path(path))
            .collect::<Result<_>>()?;
        for path in paths {
            let full = self.absolute_path(path)?;
            affected.extend(store.affected(&self.database.configuration, &full)?);
        }
        let selected: Vec<_> = self
            .database
            .commands
            .iter()
            .filter(|c| {
                paths.is_empty()
                    || affected.contains(&c.unit_id)
                    || selected_paths.contains(c.file.to_string_lossy().as_ref())
            })
            .take(limit.clamp(1, 100_000))
            .cloned()
            .collect();
        if selected.is_empty() {
            return Err(
                "no configured translation units match; headers require an indexed including TU"
                    .into(),
            );
        }
        self.runtime.paused = false;
        self.start_selected(selected, false)
    }
    fn start_selected(
        &mut self,
        mut selected: Vec<crate::compdb::CompileCommand>,
        automatic: bool,
    ) -> Result<Value> {
        self.ensure_configuration()?;
        if self.overlay.busy() {
            return Err("semantic preview active; cancel or finish it before indexing".into());
        }
        if self.job.as_ref().is_some_and(|h| !h.is_finished()) {
            return Err("semantic job already running".into());
        }
        if let Some(job) = self.job.take() {
            job.join().map_err(|_| "semantic job panicked")?;
        }
        if let Some(active) = &self.runtime.active {
            selected.sort_by_key(|c| c.file.to_string_lossy() != active.as_str());
        }
        self.runtime.inflight = selected.iter().map(|c| c.unit_id.clone()).collect();
        self.runtime
            .pending
            .retain(|id| !self.runtime.inflight.contains(id));
        self.query_store.clear_validation();
        let epoch = Arc::clone(&self.source_epoch);
        let trigger = if automatic { "watch" } else { "manual" };
        let total = selected.len();
        self.cancel.store(false, Ordering::Relaxed);
        *self.state.lock().map_err(|_| "semantic status poisoned")? = json!({"state":"running","trigger":trigger,"total":total,"completed":0,"analyzed":0,"reused":0,"failures":[]});
        let (storage, exe, provider, limits, config_path, config_hash) = (
            self.storage.clone(),
            self.executable.clone(),
            self.provider.clone(),
            self.limits.clone(),
            self.database.path.clone(),
            self.database_hash,
        );
        let cancel = Arc::clone(&self.cancel);
        let state = Arc::clone(&self.state);
        self.job = Some(thread::spawn(move || {
            let started = Instant::now();
            let profiling = crate::profile::enabled();
            let mut profiles = Vec::new();
            let mut recorded = 0u64;
            let mut checkpoint_micros = 0;
            let (mut analyzed, mut reused, mut failures) = (0, 0, Vec::new());
            let result = (|| -> Result<()> {
                let mut store = Store::open(&storage)?;
                for (n, command) in selected.iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    if fs::read(&config_path).map(|b| symweft_index::hash(&b))? != config_hash {
                        return Err("compilation database changed during job".into());
                    }
                    let unit_epoch = epoch.load(Ordering::SeqCst);
                    let validation = crate::profile::Clock::start(profiling);
                    let reusable = store.reusable(command, &provider)?;
                    let mut unit_profile = if profiling {
                        json!({"file":command.file,"reused":reusable,"reuse_validation_micros":validation.micros()})
                    } else {
                        Value::Null
                    };
                    if reusable {
                        reused += 1;
                    } else {
                        store.mark(command, &provider, "indexing", &json!({}))?;
                        let request = WorkerRequest {
                            command: command.clone(),
                            overlays: Vec::new(),
                        };
                        let result =
                            run_worker(&exe, &request, &limits, &cancel).and_then(|reply| {
                                if profiling {
                                    unit_profile["run"] = serde_json::to_value(&reply.profile)?;
                                }
                                if let Some(snapshot) = reply.snapshot {
                                    let validation = crate::profile::Clock::start(profiling);
                                    let current = snapshot.provider == provider
                                        && unchanged(&snapshot)
                                        && fs::read(&config_path)
                                            .map(|b| symweft_index::hash(&b))?
                                            == config_hash
                                        && !cancel.load(Ordering::Relaxed)
                                        && epoch.load(Ordering::SeqCst) == unit_epoch;
                                    if profiling {
                                        unit_profile["result_validation_micros"] =
                                            validation.micros().into();
                                    }
                                    if !current {
                                        return Err("obsolete semantic result discarded".into());
                                    }
                                    let publication = store.publish_profiled(&snapshot)?;
                                    if profiling {
                                        unit_profile["publish"] =
                                            serde_json::to_value(publication)?;
                                    }
                                    Ok(())
                                } else {
                                    Err(format!(
                                        "{}: {}",
                                        reply.error.unwrap_or_else(|| {
                                            "compiler rejected translation unit".into()
                                        }),
                                        reply
                                            .diagnostics
                                            .iter()
                                            .filter(|d| d.severity >= 3)
                                            .take(3)
                                            .map(|d| d.message.as_str())
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    )
                                    .into())
                                }
                            });
                        match result {
                            Ok(()) => analyzed += 1,
                            Err(error) => {
                                let failure =
                                    json!({"file":command.file,"error":error.to_string()});
                                store.mark(command, &provider, "failed", &failure)?;
                                failures.push(failure);
                            }
                        }
                    }
                    if profiling {
                        recorded += 1;
                        if profiles.len() == 64 {
                            profiles.remove(0);
                        }
                        profiles.push(unit_profile);
                    }
                    if let Ok(mut output) = state.lock() {
                        *output = json!({"state":"running","trigger":trigger,"total":total,"completed":n+1,"analyzed":analyzed,"reused":reused,"failures":failures,"wall_ms":started.elapsed().as_millis()});
                        if profiling {
                            output["profile"] = json!({"units":profiles,"recorded_units":recorded,"truncated":recorded > 64});
                        }
                    }
                }
                let checkpoint = crate::profile::Clock::start(profiling);
                store.checkpoint()?;
                checkpoint_micros = checkpoint.micros();
                Ok(())
            })();
            if let Err(error) = result {
                failures.push(json!({"error":error.to_string()}));
            }
            if let Ok(mut output) = state.lock() {
                *output = json!({"state":if cancel.load(Ordering::Relaxed){"cancelled"}else if failures.is_empty(){"complete"}else{"complete_with_errors"},"trigger":trigger,"total":total,"analyzed":analyzed,"reused":reused,"failures":failures,"wall_ms":started.elapsed().as_millis()});
                if profiling {
                    output["profile"] = json!({"units":profiles,"recorded_units":recorded,"truncated":recorded > 64,"checkpoint_micros":checkpoint_micros});
                }
            }
        }));
        Ok(json!({"started":true,"total":total}))
    }
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.pause_automatic();
        self.overlay
            .invalidate("cancelled", "semantic work cancelled by client");
        self.pump_preview();
    }
    pub fn wait(&mut self) -> Result<Value> {
        if let Some(job) = self.job.take() {
            job.join().map_err(|_| "semantic job panicked")?;
        }
        self.status()
    }
    pub fn absolute_path(&self, path: &str) -> Result<String> {
        let root = &self.database.commands.first().ok_or("no commands")?.root;
        let path = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            root.join(path)
        };
        Ok(path
            .canonicalize()
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned())
    }
    pub fn preview(&self, path: &str, content: &str, unit_id: Option<&str>) -> Result<Value> {
        if self.overlay.busy() || self.job.as_ref().is_some_and(|h| !h.is_finished()) {
            return Err("semantic worker busy; cancel or finish indexing before preview".into());
        }
        self.ensure_configuration()?;
        let full = self.absolute_path(path)?;
        let command = self.preview_command(&full, unit_id)?;
        let reply = run_worker(
            &self.executable,
            &WorkerRequest {
                command,
                overlays: vec![Overlay {
                    path: full,
                    content: content.into(),
                }],
            },
            &self.limits,
            &AtomicBool::new(false),
        )?;
        Ok(
            json!({"overlay":true,"persisted":false,"snapshot":reply.snapshot,"error":reply.error,"diagnostics":reply.diagnostics}),
        )
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.cancel();
        self.overlay.shutdown();
        if let Some(job) = self.job.take() {
            let _ = job.join();
        }
    }
}

pub fn worker_main() -> Result<()> {
    let profiling = crate::profile::enabled();
    let total = crate::profile::Clock::start(profiling);
    let bytes = read_limited(std::io::stdin().lock(), 12 * 1024 * 1024)?;
    let decode = crate::profile::Clock::start(profiling);
    let request: WorkerRequest = serde_json::from_slice(&bytes)?;
    let request_decode_micros = decode.micros();
    let mut reply = match provider::analyze(&request) {
        Ok(reply) => reply,
        Err(e) => WorkerReply {
            snapshot: None,
            error: Some(e.to_string()),
            diagnostics: Vec::new(),
            profile: None,
        },
    };
    if profiling {
        // The measured encoding duration cannot be embedded in the bytes that
        // were just encoded. Use the bounded diagnostic stderr channel instead.
        let mut profile = reply
            .profile
            .take()
            .and_then(|p| p.worker)
            .unwrap_or_default();
        profile.request_decode_micros = request_decode_micros;
        let encode = crate::profile::Clock::start(true);
        let mut output = crate::profile::TimedWriter {
            inner: std::io::stdout().lock(),
            nanos: 0,
            bytes: 0,
        };
        serde_json::to_writer(&mut output, &reply)?;
        output.flush()?;
        profile.stdout_write_micros = output.nanos / 1000;
        profile.reply_encode_micros = encode.micros().saturating_sub(profile.stdout_write_micros);
        profile.reply_bytes = output.bytes;
        profile.worker_wall_micros = total.micros();
        profile.usage();
        eprintln!(
            "{}{}",
            crate::profile::PREFIX,
            serde_json::to_string(&profile)?
        );
    } else {
        serde_json::to_writer(std::io::stdout().lock(), &reply)?;
    }
    Ok(())
}
