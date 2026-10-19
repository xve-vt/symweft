//! One transient TU, separate from the persistent disk index. No AST is retained.
use super::*;
use crate::model::{
    Diagnostic, DiskBaseline, DocumentRevision, FailedUnit, IncluderCoverage, Location, MAX_OVERLAY_DOCUMENTS,
    MAX_OVERLAY_SOURCE_BYTES, PreviewDocument, QueryCoverage, QueryVersion, Snapshot,
};
// A delayed cancel must not match a new controller after workspace/configure.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn next_id() -> Result<u64> {
    let mut id = NEXT_ID.load(Ordering::Relaxed);
    loop {
        let next = id.checked_add(1).ok_or("preview id exhausted")?;
        match NEXT_ID.compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(id),
            Err(current) => id = current,
        }
    }
}
const COMPLETENESS: &str =
    "current overlay translation unit only; disk contributions are queried separately";

#[derive(Clone)]
struct TicketDocument {
    path: String,
    revision: u64,
    hash: [u8; 32],
}
#[derive(Clone, Serialize)]
struct Ticket {
    job_id: u64,
    path: String,
    revision: u64,
    unit_id: String,
    #[serde(skip)]
    epoch: u64,
    #[serde(skip)]
    documents: Vec<TicketDocument>,
    includer_coverage: IncluderCoverage,
}
struct Pending {
    ticket: Ticket,
    request: WorkerRequest,
    budget: usize,
}
struct Running {
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<Result<Completed>>,
    job_id: u64,
    unit_id: String,
    main_file: String,
}
struct Completed {
    reply: WorkerReply,
    charged: usize,
    wall_ms: u128,
    validation_micros: u64,
}
pub const MAX_QUEUED_OVERLAYS: usize = 16;

#[derive(Default)]
pub(super) struct State {
    ticket: Option<Ticket>,
    queue: std::collections::VecDeque<Pending>,
    running: Option<Running>,
    result: Option<Completed>,
    results: std::collections::HashMap<String, Completed>,
    failed_units: Vec<FailedUnit>,
    state: &'static str,
    reason: Option<String>,
    budget: usize,
}
impl State {
    pub(super) fn busy(&self) -> bool {
        !self.queue.is_empty() || self.running.is_some()
    }
    pub(super) fn invalidate(&mut self, state: &'static str, reason: &str) {
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
        self.queue.clear();
        self.result = None;
        self.results.clear();
        self.failed_units.clear();
        self.state = state;
        self.reason = Some(reason.into());
    }
    pub(super) fn shutdown(&mut self) {
        self.invalidate("cancelled", "controller closed");
        if let Some(running) = self.running.take() {
            let _ = running.handle.join();
        }
    }
}
fn location_bytes(location: &Location) -> usize {
    location.path.capacity()
}
fn diagnostic_bytes(d: &Diagnostic) -> usize {
    d.message.capacity() + d.location.as_ref().map_or(0, location_bytes)
}
fn excerpt(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}
fn hex_key(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
// Retained Vec storage + String capacities; excludes allocator overhead and RSS.
fn retained_bytes(reply: &WorkerReply) -> usize {
    let mut bytes = std::mem::size_of::<Completed>()
        + reply.error.as_ref().map_or(0, String::capacity)
        + reply.diagnostics.capacity() * std::mem::size_of::<Diagnostic>()
        + reply
            .diagnostics
            .iter()
            .map(diagnostic_bytes)
            .sum::<usize>();
    if let Some(s) = &reply.snapshot {
        bytes += s.unit_id.capacity()
            + s.configuration.capacity()
            + s.main_file.capacity()
            + s.command_key.capacity()
            + s.provider.capacity()
            + s.inputs.capacity() * std::mem::size_of::<crate::model::Input>()
            + s.inputs.iter().map(|i| i.path.capacity()).sum::<usize>()
            + s.symbols.capacity() * std::mem::size_of::<crate::model::Symbol>()
            + s.references.capacity() * std::mem::size_of::<crate::model::Reference>()
            + s.calls.capacity() * std::mem::size_of::<crate::model::Call>()
            + s.diagnostics.capacity() * std::mem::size_of::<Diagnostic>();
        for v in &s.symbols {
            bytes += v.id.capacity()
                + v.usr.capacity()
                + v.name.capacity()
                + v.kind.capacity()
                + v.signature.capacity()
                + location_bytes(&v.location)
                + location_bytes(&v.extent);
        }
        for v in &s.references {
            bytes += v.target.capacity()
                + v.name.capacity()
                + v.kind.capacity()
                + location_bytes(&v.location)
                + v.spelling.as_ref().map_or(0, location_bytes);
        }
        for v in &s.calls {
            bytes += v.caller.as_ref().map_or(0, String::capacity)
                + v.target.as_ref().map_or(0, String::capacity)
                + v.name.capacity()
                + v.resolution.capacity()
                + location_bytes(&v.location)
                + v.spelling.as_ref().map_or(0, location_bytes);
        }
        bytes += s.diagnostics.iter().map(diagnostic_bytes).sum::<usize>();
    }
    bytes
}
impl Controller {
    fn overlay_query_version(&self, disk: DiskBaseline) -> Option<QueryVersion> {
        let ticket = self.overlay.ticket.as_ref()?;
        Some(
            self.query_version(
                disk,
                ticket
                    .documents
                    .iter()
                    .map(|document| DocumentRevision {
                        path: document.path.clone(),
                        revision: document.revision,
                        content_key: hex_key(&document.hash),
                    })
                    .collect(),
                QueryCoverage::OverlayTranslationUnit {
                    unit_id: ticket.unit_id.clone(),
                },
            ),
        )
    }
    fn current_overlay_query_version(&self) -> Result<QueryVersion> {
        self.overlay_query_version(self.query_store.disk_baseline()?)
            .ok_or_else(|| "preview ticket unavailable".into())
    }
    fn project_overlay_query_version(&self, disk: DiskBaseline) -> Option<QueryVersion> {
        let unit_id = self.overlay.ticket.as_ref()?.unit_id.clone();
        let mut version = self.overlay_query_version(disk)?;
        version.coverage = QueryCoverage::OverlayWithPersistentIndex { unit_id };
        Some(version)
    }
    pub fn completeness_summary(&self) -> String {
        let Some(ticket) = &self.overlay.ticket else {
            return COMPLETENESS.to_string();
        };
        if self.overlay.state == "queued" || self.overlay.state == "running" {
            return format!(
                "compilation in progress for revision {}; results incomplete (state: {})",
                ticket.revision, self.overlay.state
            );
        }
        if !self.overlay.failed_units.is_empty() {
            let desc = self
                .overlay
                .failed_units
                .iter()
                .map(|f| format!("{}: {} ({})", f.unit_id, f.reason, f.kind))
                .collect::<Vec<_>>()
                .join("; ");
            return format!(
                "overlay gaps: {} failed TU(s) [{desc}]; old disk contributions masked; other valid TUs retained; unindexed units/changes may exist and results are not stable pages",
                self.overlay.failed_units.len()
            );
        }
        match ticket.includer_coverage.scope.as_str() {
            "none" => "no known includers found for edited headers; 0 includer units affected; unindexed units/changes may exist and results are not stable pages".to_string(),
            "partial" => format!(
                "partial includer coverage: {} of {} known includer(s) updated in overlay; {} includer(s) invalidated by unsaved header changes; project not fully updated",
                ticket.includer_coverage.updated_includers.len(),
                ticket.includer_coverage.known_includers.len(),
                ticket.includer_coverage.stale_includers.len(),
            ),
            "full" => format!(
                "all {} known includer(s) updated in overlay; unindexed units/changes may exist and results are not stable pages",
                ticket.includer_coverage.known_includers.len(),
            ),
            _ => COMPLETENESS.to_string(),
        }
    }
    pub(super) fn preview_status_with_disk(&self, disk: DiskBaseline) -> Result<Value> {
        let query_version = self.overlay_query_version(disk.clone());
        let project_query_version = self.project_overlay_query_version(disk);
        let overlay = &self.overlay;
        let result = overlay.result.as_ref();
        let completeness = self.completeness_summary();
        let includer_coverage = overlay.ticket.as_ref().map(|t| &t.includer_coverage);
        let queued_units: Vec<_> = overlay.queue.iter().map(|p| p.ticket.unit_id.clone()).collect();
        let queue_len = overlay.queue.len();
        let failed_units = &self.overlay.failed_units;
        Ok(
            json!({"state":if overlay.state.is_empty(){"idle"}else{overlay.state},
            "ticket":overlay.ticket,"reason":overlay.reason,
            "worker_active":overlay.running.is_some(),"queued":!overlay.queue.is_empty(),
            "queue_len":queue_len,"queued_units":queued_units,
            "failed_units":failed_units,
            "overlay":true,"persisted":false,"memory_budget_bytes":overlay.budget,
            "charged_bytes":result.map_or(0,|r|r.charged),
            "wall_ms":result.map(|r|r.wall_ms),
            "parse_micros":result.and_then(|r|r.reply.snapshot.as_ref().map(|s|s.parse_micros)),
            "extract_micros":result.and_then(|r|r.reply.snapshot.as_ref().map(|s|s.extract_micros)),
            "clang_accounted_bytes":result.and_then(|r|r.reply.snapshot.as_ref().map(|s|s.clang_accounted_bytes)),
            "profile":result.and_then(|r|r.reply.profile.as_ref()).map(|run|json!({"run":run,"result_validation_micros":result.map(|r|r.validation_micros)})),
            "diagnostics":result.map(|r|r.reply.diagnostics.iter().take(64).map(|d|
                json!({"severity":d.severity,"message":excerpt(&d.message,1024),"location":d.location})).collect::<Vec<_>>()),
            "completeness":completeness,"includer_coverage":includer_coverage,"query_version":query_version,"project_query_version":project_query_version}),
        )
    }
    pub fn preview_status(&self) -> Result<Value> {
        self.preview_status_with_disk(self.query_store.disk_baseline()?)
    }
    pub fn start_preview(
        &mut self,
        path: &str,
        revision: u64,
        content: &str,
        unit_id: Option<&str>,
        memory_mib: u64,
    ) -> Result<Value> {
        self.start_preview_documents(
            vec![PreviewDocument {
                path: path.into(),
                revision,
                content: content.into(),
            }],
            unit_id,
            memory_mib,
        )
    }
    pub fn start_preview_documents(
        &mut self,
        documents: Vec<PreviewDocument>,
        unit_id: Option<&str>,
        memory_mib: u64,
    ) -> Result<Value> {
        if documents.is_empty() || documents.len() > MAX_OVERLAY_DOCUMENTS {
            return Err(
                format!("preview requires 1..={MAX_OVERLAY_DOCUMENTS} overlay documents").into(),
            );
        }
        if !(1..=128).contains(&memory_mib) {
            return Err("preview requires 1..128 MiB retained-result budget".into());
        }
        let mut total_bytes = 0usize;
        for document in &documents {
            total_bytes = total_bytes
                .checked_add(document.content.len())
                .ok_or("preview overlay source size overflow")?;
            if document.content.len() > MAX_OVERLAY_SOURCE_BYTES
                || total_bytes > MAX_OVERLAY_SOURCE_BYTES
            {
                return Err("preview overlay source budget exceeds 8 MiB".into());
            }
        }
        if self.runtime.pressure_paused {
            return Err("preview paused by critical memory pressure".into());
        }
        self.ensure_configuration()?;
        let mut full_documents = Vec::with_capacity(documents.len());
        let mut seen = HashSet::new();
        for document in documents {
            let full = self.absolute_path(&document.path)?;
            if !seen.insert(full.clone()) {
                return Err(format!("duplicate preview document: {full}").into());
            }
            full_documents.push(PreviewDocument {
                path: full,
                revision: document.revision,
                content: document.content,
            });
        }
        let primary = full_documents.first().ok_or("preview document missing")?;
        let primary_path = primary.path.clone();
        let primary_revision = primary.revision;
        let command = self.preview_command(&primary_path, unit_id)?;
        let mut ticket_documents: Vec<_> = full_documents
            .iter()
            .map(|document| TicketDocument {
                path: document.path.clone(),
                revision: document.revision,
                hash: symweft_index::hash(document.content.as_bytes()),
            })
            .collect();
        ticket_documents.sort_by(|left, right| left.path.cmp(&right.path));
        let mut all_affected = std::collections::BTreeSet::new();
        for doc in &full_documents {
            let affected = self
                .query_store
                .affected(&self.database.configuration, &doc.path)?;
            all_affected.extend(affected);
        }
        let (scope, updated_includers, stale_includers) = if all_affected.is_empty() {
            ("none".to_string(), Vec::new(), Vec::new())
        } else {
            let mut updated = Vec::new();
            let mut stale = Vec::new();
            for id in &all_affected {
                if id == &command.unit_id {
                    updated.push(id.clone());
                } else {
                    stale.push(id.clone());
                }
            }
            let scope = if stale.is_empty() {
                "full".to_string()
            } else {
                "partial".to_string()
            };
            (scope, updated, stale)
        };
        let mut all_set: HashSet<_> = all_affected.iter().cloned().collect();
        all_set.insert(command.unit_id.clone());
        self.query_store
            .invalidate_units(&self.database.configuration, &all_set)?;
        let includer_coverage = IncluderCoverage {
            scope,
            known_includers: all_affected.into_iter().collect(),
            updated_includers,
            stale_includers,
        };
        self.overlay
            .invalidate("superseded", "replaced by a newer preview request");
        self.defer_index_for_preview();
        let job_id = next_id()?;
        let ticket = Ticket {
            job_id,
            path: primary_path,
            revision: primary_revision,
            unit_id: command.unit_id.clone(),
            epoch: self.source_epoch.load(Ordering::SeqCst),
            documents: ticket_documents,
            includer_coverage,
        };
        self.overlay.ticket = Some(ticket.clone());
        self.overlay.budget = memory_mib as usize * 1024 * 1024;
        let overlays: Vec<_> = full_documents
            .iter()
            .map(|document| Overlay {
                path: document.path.clone(),
                content: document.content.clone(),
            })
            .collect();
        let primary_pending = Pending {
            ticket,
            request: WorkerRequest {
                command: command.clone(),
                overlays: overlays.clone(),
            },
            budget: self.overlay.budget,
        };
        let mut secondary_jobs = Vec::new();
        for stale_id in &stale_includers {
            if let Some(cmd) = self.database.commands.iter().find(|c| &c.unit_id == stale_id) {
                let sec_job_id = next_id()?;
                let sec_ticket = Ticket {
                    job_id: sec_job_id,
                    path: cmd.file.to_string_lossy().into_owned(),
                    revision: primary_revision,
                    unit_id: stale_id.clone(),
                    epoch: self.source_epoch.load(Ordering::SeqCst),
                    documents: ticket_documents.clone(),
                    includer_coverage: includer_coverage.clone(),
                };
                secondary_jobs.push(Pending {
                    ticket: sec_ticket,
                    request: WorkerRequest {
                        command: cmd.clone(),
                        overlays: overlays.clone(),
                    },
                    budget: self.overlay.budget,
                });
            }
        }
        // Active file priority: primary job at front
        self.overlay.queue.push_front(primary_pending);
        // Append bounded secondary jobs
        for sec in secondary_jobs {
            if self.overlay.queue.len() < MAX_QUEUED_OVERLAYS {
                self.overlay.queue.push_back(sec);
            }
        }
        self.overlay.state = "queued";
        self.overlay.reason = None;
        self.pump_preview();
        self.preview_status()
    }
    pub(super) fn preview_command(
        &self,
        full: &str,
        unit_id: Option<&str>,
    ) -> Result<crate::compdb::CompileCommand> {
        let affected = self
            .query_store
            .affected(&self.database.configuration, full)?;
        let mut candidates: Vec<_> = self
            .database
            .commands
            .iter()
            .filter(|c| c.file.to_string_lossy() == full || affected.contains(&c.unit_id))
            .filter(|c| unit_id.is_none_or(|id| id == c.unit_id))
            .collect();
        if candidates.is_empty() {
            if affected.is_empty()
                && !self
                    .database
                    .commands
                    .iter()
                    .any(|c| c.file.to_string_lossy() == full)
            {
                return Err(
                    "no matching TU; header has no known includer records; analyze an including source file first"
                        .into(),
                );
            }
            return Err("no matching TU; headers need an indexed owner".into());
        }
        if candidates.len() > 1 {
            return Err(
                "preview requires exactly one matching TU; supply unit_id for headers or variants"
                    .into(),
            );
        }
        Ok(candidates.remove(0).clone())
    }
    pub fn cancel_preview(&mut self, job_id: u64) -> Result<Value> {
        self.preview_ticket(job_id)?;
        self.overlay
            .invalidate("cancelled", "preview cancelled by client");
        self.pump_preview();
        self.preview_status()
    }
    pub fn invalidate_preview(&mut self, reason: &str) {
        if self.overlay.ticket.is_some() {
            self.overlay.invalidate("stale", reason);
            self.pump_preview();
        }
    }
    pub fn retire_overlay(&mut self, reason: &str) {
        self.overlay.ticket = None;
        self.overlay.invalidate("retired", reason);
        self.pump_preview();
    }
    pub fn overlay_affects_path(&self, path: &str) -> bool {
        let Ok(full) = self.absolute_path(path) else {
            return false;
        };
        if let Some(ticket) = &self.overlay.ticket {
            if ticket.path == full || ticket.documents.iter().any(|d| d.path == full) {
                return true;
            }
            if ticket.includer_coverage.known_includers.iter().any(|id| {
                self.database
                    .commands
                    .iter()
                    .any(|c| &c.unit_id == id && c.file.to_string_lossy() == full)
            }) {
                return true;
            }
            if let Ok(affected) = self.query_store.affected(&self.database.configuration, &full) {
                if affected.contains(&ticket.unit_id) {
                    return true;
                }
            }
        }
        if self
            .overlay
            .queue
            .iter()
            .any(|p| p.ticket.path == full || p.ticket.documents.iter().any(|d| d.path == full))
        {
            return true;
        }
        false
    }
    fn preview_ticket(&self, job_id: u64) -> Result<&Ticket> {
        self.overlay
            .ticket
            .as_ref()
            .filter(|t| t.job_id == job_id)
            .ok_or_else(|| "obsolete or unknown preview job_id".into())
    }
    pub fn check_preview(&mut self, job_id: u64, path: &str, revision: u64) -> Result<Value> {
        let path = self.absolute_path(path)?;
        let ticket = self.preview_ticket(job_id)?;
        if ticket.path != path || ticket.revision != revision {
            return Err("preview path/revision does not match ticket".into());
        }
        if self.overlay.state == "ready" && !self.preview_fresh() {
            self.invalidate_preview("preview dependency/configuration changed");
        }
        self.preview_status()
    }
    fn preview_fresh(&self) -> bool {
        let Some(ticket) = &self.overlay.ticket else {
            return false;
        };
        let Some(snapshot) = self
            .overlay
            .result
            .as_ref()
            .and_then(|r| r.reply.snapshot.as_ref())
        else {
            return false;
        };
        self.ensure_configuration().is_ok()
            && self.source_epoch.load(Ordering::SeqCst) == ticket.epoch
            && ticket.documents.iter().all(|document| {
                snapshot
                    .inputs
                    .iter()
                    .any(|input| input.path == document.path && input.hash == document.hash)
            })
            && snapshot.inputs.iter().all(|input| {
                if let Some(document) = ticket
                    .documents
                    .iter()
                    .find(|document| document.path == input.path)
                {
                    input.hash == document.hash
                } else {
                    self.query_store.input_digest(Path::new(&input.path)) == Some(input.hash)
                }
            })
    }
    pub(super) fn pump_preview(&mut self) {
        if self
            .overlay
            .running
            .as_ref()
            .is_some_and(|r| r.handle.is_finished())
        {
            let running = self.overlay.running.take().expect("finished worker");
            let result = running
                .handle
                .join()
                .unwrap_or_else(|_| Err("preview thread panicked".into()));
            let current_job_id = self.overlay.ticket.as_ref().map(|t| t.job_id);
            match result {
                Ok(res) => {
                    if let Some(snapshot) = &res.reply.snapshot {
                        let matches_current = self.overlay.ticket.as_ref().map_or(false, |t| {
                            t.epoch == self.source_epoch.load(Ordering::SeqCst)
                                && t.documents.iter().all(|doc| {
                                    snapshot.inputs.iter().any(|inp| inp.path == doc.path && inp.hash == doc.hash)
                                })
                        });
                        if matches_current {
                            self.overlay.results.insert(snapshot.unit_id.clone(), res.clone());
                            if let Some(ticket) = self.overlay.ticket.as_mut() {
                                if let Some(pos) = ticket.includer_coverage.stale_includers.iter().position(|id| id == &snapshot.unit_id) {
                                    let id = ticket.includer_coverage.stale_includers.remove(pos);
                                    if !ticket.includer_coverage.updated_includers.contains(&id) {
                                        ticket.includer_coverage.updated_includers.push(id);
                                    }
                                    if ticket.includer_coverage.stale_includers.is_empty() {
                                        ticket.includer_coverage.scope = "full".to_string();
                                    }
                                }
                            }
                        }
                        if current_job_id == Some(running.job_id) && self.overlay.state == "running" {
                            self.overlay.state = "ready";
                            self.overlay.reason = res.reply.error.as_ref().map(|s| excerpt(s, 2048));
                            self.overlay.result = Some(res);
                        }
                    } else {
                        let err_msg = res.reply.error.as_deref().unwrap_or("compiler errors; no current semantic snapshot published");
                        self.overlay.failed_units.push(FailedUnit {
                            unit_id: running.unit_id.clone(),
                            main_file: running.main_file.clone(),
                            reason: err_msg.to_string(),
                            kind: "compile_error".to_string(),
                            diagnostics: res.reply.diagnostics.clone(),
                        });
                        if current_job_id == Some(running.job_id) && self.overlay.state == "running" {
                            self.overlay.state = "failed";
                            self.overlay.reason = Some(excerpt(err_msg, 2048));
                            self.overlay.result = Some(res);
                        }
                    }
                }
                Err(error) => {
                    let err_str = error.to_string();
                    let kind = if err_str.contains("timeout") {
                        "timeout"
                    } else if err_str.contains("budget exceeded") {
                        "budget_exceeded"
                    } else {
                        "worker_error"
                    };
                    self.overlay.failed_units.push(FailedUnit {
                        unit_id: running.unit_id.clone(),
                        main_file: running.main_file.clone(),
                        reason: err_str.clone(),
                        kind: kind.to_string(),
                        diagnostics: Vec::new(),
                    });
                    if current_job_id == Some(running.job_id) && self.overlay.state == "running" {
                        self.overlay.state = "failed";
                        self.overlay.reason = Some(excerpt(&err_str, 2048));
                    }
                }
            }
        }
        if self.overlay.running.is_some() || self.job.as_ref().is_some_and(|j| !j.is_finished()) {
            return;
        }
        let Some(pending) = self.overlay.queue.pop_front() else {
            return;
        };
        let (exe, limits, provider, config_hash) = (
            self.executable.clone(),
            self.limits.clone(),
            self.provider.clone(),
            self.database_hash,
        );
        // Keep only the selected command for response-file validation, avoiding a
        // clone of a large compilation database on each keystroke.
        let config_path = self.database.path.clone();
        let epoch = Arc::clone(&self.source_epoch);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let job_id = pending.ticket.job_id;
        let unit_id = pending.ticket.unit_id.clone();
        let main_file = pending.request.command.file.to_string_lossy().into_owned();
        let handle = thread::spawn(move || {
            let started = Instant::now();
            let reply = run_worker(&exe, &pending.request, &limits, &worker_cancel)?;
            if worker_cancel.load(Ordering::Relaxed)
                || epoch.load(Ordering::SeqCst) != pending.ticket.epoch
            {
                return Err("obsolete preview discarded".into());
            }
            let mut fingerprints = crate::fingerprint::Fingerprints::default();
            let validation = crate::profile::Clock::start(crate::profile::enabled());
            if fingerprints.digest(&config_path) != Some(config_hash)
                || !pending
                    .request
                    .command
                    .extra_inputs
                    .iter()
                    .all(|i| fingerprints.digest(Path::new(&i.path)) == Some(i.hash))
            {
                return Err("preview compilation configuration changed".into());
            }
            if let Some(s) = &reply.snapshot {
                let command = &pending.request.command;
                if s.provider != provider
                    || s.unit_id != command.unit_id
                    || s.command_key != command.command_key
                    || s.configuration != command.configuration
                    || !pending.ticket.documents.iter().all(|document| {
                        s.inputs
                            .iter()
                            .any(|input| input.path == document.path && input.hash == document.hash)
                    })
                    || !s.inputs.iter().all(|input| {
                        if let Some(document) = pending
                            .ticket
                            .documents
                            .iter()
                            .find(|document| document.path == input.path)
                        {
                            input.hash == document.hash
                        } else {
                            fingerprints.digest(Path::new(&input.path)) == Some(input.hash)
                        }
                    })
                {
                    return Err("preview input/provider changed during analysis".into());
                }
            }
            let charged = retained_bytes(&reply);
            if charged > pending.budget {
                return Err(format!(
                    "preview retained-result budget exceeded: {charged} > {} bytes",
                    pending.budget
                )
                .into());
            }
            Ok(Completed {
                reply,
                charged,
                wall_ms: started.elapsed().as_millis(),
                validation_micros: validation.micros(),
            })
        });
        self.overlay.running = Some(Running {
            cancel,
            handle,
            job_id,
            unit_id,
            main_file,
        });
        self.overlay.state = "running";
    }
    fn overlay_snapshot(&self) -> Result<&Snapshot> {
        if self.overlay.state == "queued" || self.overlay.state == "running" {
            return Err(format!(
                "preview compilation in progress (state: {}); retry after completion or inspect semantic.preview.progress",
                self.overlay.state
            ).into());
        }
        if self.overlay.state != "ready" {
            return Err(format!(
                "preview is not ready (state: {}, reason: {:?}); inspect semantic.preview.progress",
                self.overlay.state, self.overlay.reason
            ).into());
        }
        self.overlay
            .result
            .as_ref()
            .and_then(|r| r.reply.snapshot.as_ref())
            .ok_or_else(|| "preview snapshot unavailable".into())
    }
    pub(super) fn overlay_row_for(
        &self,
        value: impl Serialize,
        unit_id: &str,
        revision: u64,
        job_id: u64,
        main_file: &str,
    ) -> Value {
        let mut row = serde_json::to_value(value).expect("semantic fact serializes");
        row["unit_id"] = unit_id.to_string().into();
        row["revision"] = revision.into();
        row["preview_id"] = job_id.into();
        row["evidence"] = "clang_semantic_overlay".into();
        row["freshness"] = "overlay".into();
        row["main_file"] = main_file.to_string().into();
        row
    }
    fn overlay_row(&self, value: impl Serialize) -> Value {
        let (unit_id, revision, job_id) = self.overlay.ticket.as_ref().map_or(
            ("".to_string(), 0, 0),
            |t| (t.unit_id.clone(), t.revision, t.job_id),
        );
        let main_file = self
            .overlay
            .result
            .as_ref()
            .and_then(|r| r.reply.snapshot.as_ref())
            .map_or("", |s| s.main_file.as_str());
        self.overlay_row_for(value, &unit_id, revision, job_id, main_file)
    }
    pub fn preview_at(
        &mut self,
        job_id: u64,
        path: &str,
        revision: u64,
        offset: u64,
    ) -> Result<Value> {
        self.check_preview(job_id, path, revision)?;
        let path = self.absolute_path(path)?;
        let snapshot = self.overlay_snapshot()?;
        let contains = |l: &Location| {
            l.path == path && u64::from(l.offset) <= offset && offset < u64::from(l.end_offset)
        };
        let mut refs: Vec<_> = snapshot
            .references
            .iter()
            .filter(|v| contains(&v.location))
            .collect();
        refs.sort_by_key(|v| v.location.end_offset - v.location.offset);
        let declarations: Vec<_> = snapshot
            .symbols
            .iter()
            .filter(|v| contains(&v.location))
            .take(32)
            .collect();
        let mut enclosing: Vec<_> = snapshot
            .symbols
            .iter()
            .filter(|v| v.callable && v.definition && contains(&v.extent))
            .collect();
        enclosing.sort_by_key(|v| v.extent.end_offset - v.extent.offset);
        let refs: Vec<_> = refs.into_iter().take(32).collect();
        let entities: HashSet<_> = refs
            .iter()
            .map(|v| v.target.as_str())
            .chain(declarations.iter().map(|v| v.id.as_str()))
            .collect();
        let entity = if entities.len() == 1 {
            entities.into_iter().next()
        } else {
            None
        };
        let query_version = self.current_overlay_query_version()?;
        Ok(
            json!({"evidence":"clang_semantic_overlay","completeness":COMPLETENESS,"entity_id":entity,
            "references":refs.into_iter().map(|v|self.overlay_row(v)).collect::<Vec<_>>(),
            "declarations":declarations.into_iter().map(|v|self.overlay_row(v)).collect::<Vec<_>>(),
            "enclosing_functions":enclosing.into_iter().take(32).map(|v|self.overlay_row(v)).collect::<Vec<_>>(),
            "query_version":query_version}),
        )
    }
    pub fn preview_relations(
        &mut self,
        job_id: u64,
        path: &str,
        revision: u64,
        entity: &str,
        direction: &str,
        limit: usize,
    ) -> Result<Value> {
        self.check_preview(job_id, path, revision)?;
        let snapshot = self.overlay_snapshot()?;
        let limit = limit.clamp(1, 1000);
        let rows: Vec<_> = match direction {
            "definitions" | "declarations" => snapshot
                .symbols
                .iter()
                .filter(|v| v.id == entity && (direction != "definitions" || v.definition))
                .take(limit)
                .map(|v| self.overlay_row(v))
                .collect(),
            "references" => snapshot
                .references
                .iter()
                .filter(|v| v.target == entity)
                .take(limit)
                .map(|v| self.overlay_row(v))
                .collect(),
            "incoming" | "outgoing" => snapshot
                .calls
                .iter()
                .filter(|v| {
                    if direction == "incoming" {
                        v.target.as_deref() == Some(entity)
                    } else {
                        v.caller.as_deref() == Some(entity)
                    }
                })
                .take(limit)
                .map(|v| self.overlay_row(v))
                .collect(),
            _ => {
                return Err(
                    "expected definitions/declarations/references/incoming/outgoing".into(),
                );
            }
        };
        let query_version = self.current_overlay_query_version()?;
        Ok(
            json!({"evidence":"clang_semantic_overlay","completeness":COMPLETENESS,"rows":rows,"limit":limit,"entity_id":entity,"direction":direction,"query_version":query_version}),
        )
    }
}
