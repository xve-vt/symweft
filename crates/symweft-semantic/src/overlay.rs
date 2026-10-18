//! One transient TU, separate from the persistent disk index. No AST is retained.
use super::*;
use crate::model::{
    Diagnostic, DiskBaseline, DocumentRevision, Location, MAX_OVERLAY_DOCUMENTS,
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
}
struct Completed {
    reply: WorkerReply,
    charged: usize,
    wall_ms: u128,
    validation_micros: u64,
}
#[derive(Default)]
pub(super) struct State {
    ticket: Option<Ticket>,
    pending: Option<Pending>,
    running: Option<Running>,
    result: Option<Completed>,
    state: &'static str,
    reason: Option<String>,
    budget: usize,
}
impl State {
    pub(super) fn busy(&self) -> bool {
        self.pending.is_some() || self.running.is_some()
    }
    pub(super) fn invalidate(&mut self, state: &'static str, reason: &str) {
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
        self.pending = None;
        self.result = None;
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
    pub(super) fn preview_status_with_disk(&self, disk: DiskBaseline) -> Result<Value> {
        let query_version = self.overlay_query_version(disk.clone());
        let project_query_version = self.project_overlay_query_version(disk);
        let overlay = &self.overlay;
        let result = overlay.result.as_ref();
        Ok(
            json!({"state":if overlay.state.is_empty(){"idle"}else{overlay.state},
            "ticket":overlay.ticket,"reason":overlay.reason,
            "worker_active":overlay.running.is_some(),"queued":overlay.pending.is_some(),
            "overlay":true,"persisted":false,"memory_budget_bytes":overlay.budget,
            "charged_bytes":result.map_or(0,|r|r.charged),
            "wall_ms":result.map(|r|r.wall_ms),
            "parse_micros":result.and_then(|r|r.reply.snapshot.as_ref().map(|s|s.parse_micros)),
            "extract_micros":result.and_then(|r|r.reply.snapshot.as_ref().map(|s|s.extract_micros)),
            "clang_accounted_bytes":result.and_then(|r|r.reply.snapshot.as_ref().map(|s|s.clang_accounted_bytes)),
            "profile":result.and_then(|r|r.reply.profile.as_ref()).map(|run|json!({"run":run,"result_validation_micros":result.map(|r|r.validation_micros)})),
            "diagnostics":result.map(|r|r.reply.diagnostics.iter().take(64).map(|d|
                json!({"severity":d.severity,"message":excerpt(&d.message,1024),"location":d.location})).collect::<Vec<_>>()),
            "completeness":COMPLETENESS,"query_version":query_version,"project_query_version":project_query_version}),
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
        };
        self.overlay.ticket = Some(ticket.clone());
        self.overlay.budget = memory_mib as usize * 1024 * 1024;
        self.overlay.pending = Some(Pending {
            ticket,
            request: WorkerRequest {
                command,
                overlays: full_documents
                    .into_iter()
                    .map(|document| Overlay {
                        path: document.path,
                        content: document.content,
                    })
                    .collect(),
            },
            budget: self.overlay.budget,
        });
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
        let mut candidates = self
            .database
            .commands
            .iter()
            .filter(|c| c.file.to_string_lossy() == full || affected.contains(&c.unit_id))
            .filter(|c| unit_id.is_none_or(|id| id == c.unit_id));
        let command = candidates
            .next()
            .ok_or("no matching TU; headers need an indexed owner")?;
        if candidates.next().is_some() {
            return Err(
                "preview requires exactly one matching TU; supply unit_id for headers or variants"
                    .into(),
            );
        }
        Ok(command.clone())
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
            if self
                .overlay
                .ticket
                .as_ref()
                .is_some_and(|t| t.job_id == running.job_id)
                && self.overlay.state == "running"
            {
                match result {
                    Ok(result) => {
                        self.overlay.state = if result.reply.snapshot.is_some() {
                            "ready"
                        } else {
                            "failed"
                        };
                        self.overlay.reason = result.reply.error.as_ref().map(|s| excerpt(s, 2048));
                        self.overlay.result = Some(result);
                    }
                    Err(error) => {
                        self.overlay.state = "failed";
                        self.overlay.reason = Some(excerpt(&error.to_string(), 2048));
                    }
                }
            }
        }
        if self.overlay.running.is_some() || self.job.as_ref().is_some_and(|j| !j.is_finished()) {
            return;
        }
        let Some(pending) = self.overlay.pending.take() else {
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
        });
        self.overlay.state = "running";
    }
    fn overlay_snapshot(&self) -> Result<&Snapshot> {
        if self.overlay.state != "ready" {
            return Err("preview is not ready; inspect semantic.preview.progress".into());
        }
        self.overlay
            .result
            .as_ref()
            .and_then(|r| r.reply.snapshot.as_ref())
            .ok_or_else(|| "preview snapshot unavailable".into())
    }
    fn overlay_row(&self, value: impl Serialize) -> Value {
        let mut row = serde_json::to_value(value).expect("semantic fact serializes");
        if let Some(ticket) = &self.overlay.ticket {
            row["unit_id"] = ticket.unit_id.clone().into();
            row["revision"] = ticket.revision.into();
            row["preview_id"] = ticket.job_id.into();
        }
        row["evidence"] = "clang_semantic_overlay".into();
        row["freshness"] = "overlay".into();
        if let Some(snapshot) = self
            .overlay
            .result
            .as_ref()
            .and_then(|r| r.reply.snapshot.as_ref())
        {
            row["main_file"] = snapshot.main_file.clone().into();
        }
        row
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
