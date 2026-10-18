use super::*;
use crate::watch::{Monitor, Settings};
const MAX_AUTO_UNITS: usize = 256;
#[derive(Default)]
pub(super) struct State {
    pub settings: Settings,
    pub monitor: Option<Monitor>,
    pub pending: HashSet<String>,
    pub inflight: HashSet<String>,
    pub active: Option<String>,
    pub paused: bool,
    pub pressure_paused: bool,
    first: Option<Instant>,
    last: Option<Instant>,
    seen_epoch: u64,
    revision: u64,
    invalidated: u64,
    auto_jobs: u64,
    manual_required: bool,
    configuration_required: bool,
    error: Option<String>,
}
impl Controller {
    pub(super) fn defer_index_for_preview(&mut self) {
        if self.job.as_ref().is_some_and(|j| !j.is_finished()) {
            self.cancel.store(true, Ordering::Relaxed);
            self.runtime
                .pending
                .extend(self.runtime.inflight.iter().cloned());
            self.runtime.first.get_or_insert_with(Instant::now);
            self.runtime.last = Some(Instant::now());
            if self.runtime.pending.len() > MAX_AUTO_UNITS {
                self.runtime.pending.clear();
                self.runtime.manual_required = true;
            }
        }
    }
    pub(super) fn pause_automatic(&mut self) {
        self.runtime
            .pending
            .extend(self.runtime.inflight.iter().cloned());
        self.runtime.paused = true;
        if self.runtime.pending.len() > MAX_AUTO_UNITS {
            self.runtime.pending.clear();
            self.runtime.manual_required = true;
        }
    }
    pub fn watch_status(&self) -> Value {
        json!({"enabled":self.runtime.settings.enabled,"auto_refresh":self.runtime.settings.auto_refresh,"metadata_cache":self.runtime.settings.metadata_cache,"monitor":self.runtime.monitor.as_ref().map(Monitor::status),"pending_units":self.runtime.pending.len(),"paused":self.runtime.paused,"pressure_paused":self.runtime.pressure_paused,"revision":self.runtime.revision,"invalidated_units":self.runtime.invalidated,"auto_jobs":self.runtime.auto_jobs,"manual_refresh_required":self.runtime.manual_required,"configuration_required":self.runtime.configuration_required,"error":self.runtime.error,"fingerprints":self.query_store.fingerprint_stats(),"configuration_fingerprints":self.validation.borrow().stats(),"max_auto_units":MAX_AUTO_UNITS,"consistency":"native hints + per-query input stamps + bounded hash lifetime; strict when metadata cache disabled"})
    }
    pub fn configure_watch(&mut self, settings: Settings) -> Result<Value> {
        // Consume the old listener before replacing it so settings changes do
        // not silently drop saves that have already been queued.
        self.maintain(&HashSet::new(), false)?;
        if self.overlay.busy() || self.job.as_ref().is_some_and(|h| !h.is_finished()) {
            return Err("finish or cancel the semantic job before changing watch settings".into());
        }
        self.invalidate_preview("file watch settings changed");
        let active = self.runtime.active.take();
        let pending = std::mem::take(&mut self.runtime.pending);
        let (revision, invalidated, auto_jobs, manual_required, configuration_required) = (
            self.runtime.revision,
            self.runtime.invalidated,
            self.runtime.auto_jobs,
            self.runtime.manual_required,
            self.runtime.configuration_required,
        );
        self.runtime = State {
            settings: settings.clone(),
            active,
            pending,
            revision: revision + 1,
            invalidated,
            auto_jobs,
            manual_required,
            configuration_required,
            ..State::default()
        };
        if !self.runtime.pending.is_empty() {
            self.runtime.first = Some(Instant::now());
            self.runtime.last = Some(Instant::now());
        }
        self.query_store.clear_validation();
        self.validation.borrow_mut().clear();
        self.query_store.cache_validation(false);
        self.validation.borrow_mut().enable(false);
        if settings.enabled {
            let command = self
                .database
                .commands
                .first()
                .ok_or("no configured translation units")?;
            let excluded = self
                .storage
                .parent()
                .and_then(Path::parent)
                .ok_or("cache parent unavailable")?;
            match Monitor::open(&command.root, &command.build_root, excluded) {
                Ok(monitor) => {
                    self.source_epoch = Arc::clone(&monitor.publish_epoch);
                    self.runtime.monitor = Some(monitor);
                    self.query_store.cache_validation(settings.metadata_cache);
                    self.validation.borrow_mut().enable(settings.metadata_cache);
                }
                Err(error) => {
                    self.runtime.error = Some(format!(
                        "native file watch unavailable; strict queries remain available: {error}"
                    ));
                }
            }
        }
        Ok(self.watch_status())
    }
    pub fn focus(&mut self, path: &str) -> Result<()> {
        self.runtime.active = Some(self.absolute_path(path)?);
        Ok(())
    }
    pub fn invalidate_path(&self, path: &str) -> Result<usize> {
        self.query_store
            .invalidate(&self.database.configuration, &self.absolute_path(path)?)
    }
    pub fn refresh_recorded(&mut self) -> Result<Value> {
        self.ensure_configuration()?;
        if self.overlay.busy() || self.job.as_ref().is_some_and(|job| !job.is_finished()) {
            return Err("semantic job already running".into());
        }
        let ids: HashSet<_> = self
            .query_store
            .recorded_units(&self.database.configuration)?
            .into_iter()
            .collect();
        let selected: Vec<_> = self
            .database
            .commands
            .iter()
            .filter(|command| ids.contains(&command.unit_id))
            .cloned()
            .collect();
        if selected.is_empty() {
            return Err("no previously indexed translation units; analyze a file first".into());
        }
        self.query_store
            .invalidate_units(&self.database.configuration, &ids)?;
        self.runtime.manual_required = false;
        self.runtime.paused = false;
        self.start_selected(selected, false)
    }
    pub fn buffer_changed(&mut self, path: &str) -> Result<()> {
        self.invalidate_preview("document buffer changed");
        let full = self.absolute_path(path)?;
        let mut affected: HashSet<_> = self
            .query_store
            .affected(&self.database.configuration, &full)?
            .into_iter()
            .collect();
        if self.job.as_ref().is_some_and(|job| !job.is_finished()) {
            self.cancel.store(true, Ordering::Relaxed);
            affected.extend(self.runtime.inflight.iter().cloned());
        }
        self.query_store
            .invalidate_units(&self.database.configuration, &affected)?;
        self.runtime.pending.extend(affected);
        self.runtime.revision += 1;
        let now = Instant::now();
        self.runtime.first.get_or_insert(now);
        self.runtime.last = Some(now);
        if self.runtime.pending.len() > MAX_AUTO_UNITS {
            self.runtime.pending.clear();
            self.runtime.manual_required = true;
        }
        Ok(())
    }
    pub fn pressure(&mut self, critical: bool) {
        self.runtime.pressure_paused = critical;
        if critical {
            self.invalidate_preview("critical memory pressure");
            self.query_store.clear_validation();
            self.validation.borrow_mut().clear();
            if self.job.as_ref().is_some_and(|job| !job.is_finished()) {
                self.cancel.store(true, Ordering::Relaxed);
                self.runtime
                    .pending
                    .extend(self.runtime.inflight.iter().cloned());
                self.runtime.first.get_or_insert_with(Instant::now);
                self.runtime.last = Some(Instant::now());
                if self.runtime.pending.len() > MAX_AUTO_UNITS {
                    self.runtime.pending.clear();
                    self.runtime.manual_required = true;
                }
            }
        }
    }
    pub fn maintain(&mut self, dirty: &HashSet<String>, allow_auto: bool) -> Result<()> {
        self.pump_preview();
        let Some(monitor) = &self.runtime.monitor else {
            return Ok(());
        };
        let batch = monitor.drain();
        let epoch = monitor.epoch.load(Ordering::SeqCst);
        let healthy = monitor.healthy();
        if epoch != self.runtime.seen_epoch {
            self.runtime.seen_epoch = epoch;
            self.query_store.clear_validation();
            self.validation.borrow_mut().clear();
        }
        if !healthy {
            self.query_store.cache_validation(false);
            self.validation.borrow_mut().enable(false);
        }
        if !batch.paths.is_empty() || batch.rescan {
            // A running TU has not reported its include set yet. Conservatively
            // reject source/build events; README and unrelated assets are ignored.
            if batch.rescan
                || batch.paths.iter().any(|(p, broad)| {
                    *broad
                        || p == &self.database.path
                        || p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                            matches!(
                                e,
                                "c" | "C"
                                    | "cc"
                                    | "cpp"
                                    | "cxx"
                                    | "h"
                                    | "hh"
                                    | "hpp"
                                    | "hxx"
                                    | "inc"
                                    | "inl"
                                    | "def"
                                    | "rsp"
                            )
                        })
                })
            {
                self.invalidate_preview("source/build file event invalidated preview");
            }
            let mut affected = HashSet::new();
            let broad = batch.rescan || batch.paths.values().any(|b| *b);
            if broad {
                affected.extend(
                    self.query_store
                        .recorded_units(&self.database.configuration)?,
                );
            } else {
                for path in batch.paths.keys() {
                    let original = path.to_string_lossy();
                    affected.extend(
                        self.query_store
                            .affected(&self.database.configuration, &original)?,
                    );
                    if let Ok(real) = path.canonicalize() {
                        affected.extend(
                            self.query_store
                                .affected(&self.database.configuration, &real.to_string_lossy())?,
                        );
                    }
                }
            }
            if let Some(error) = batch.errors.first() {
                self.runtime.error = Some(error.clone());
            }
            if let Err(error) = self.ensure_configuration() {
                self.runtime.configuration_required = true;
                self.runtime.error = Some(error.to_string());
                affected.extend(
                    self.query_store
                        .recorded_units(&self.database.configuration)?,
                );
            }
            if !affected.is_empty() || self.runtime.configuration_required {
                if self.job.as_ref().is_some_and(|h| !h.is_finished()) {
                    self.cancel.store(true, Ordering::Relaxed);
                    affected.extend(self.runtime.inflight.iter().cloned());
                }
                self.query_store
                    .invalidate_units(&self.database.configuration, &affected)?;
                self.runtime.invalidated += affected.len() as u64;
                self.runtime.pending.extend(affected);
                let now = Instant::now();
                self.runtime.first.get_or_insert(now);
                self.runtime.last = Some(now);
                self.runtime.revision += 1;
                if self.runtime.pending.len() > MAX_AUTO_UNITS {
                    self.runtime.pending.clear();
                    self.runtime.manual_required = true;
                }
            }
        }
        if self.job.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(job) = self.job.take() {
                job.join().map_err(|_| "semantic job panicked")?;
            }
            self.runtime.inflight.clear();
            self.query_store.clear_validation();
            self.runtime.revision += 1;
        }
        let quiet = self
            .runtime
            .last
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(350))
            || self
                .runtime
                .first
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(2));
        if !quiet
            || !allow_auto
            || !healthy
            || !self.runtime.settings.auto_refresh
            || self.runtime.paused
            || self.runtime.pressure_paused
            || self.runtime.manual_required
            || self.runtime.configuration_required
            || self.job.is_some()
            || self.overlay.busy()
            || self.runtime.pending.is_empty()
        {
            return Ok(());
        }
        let mut blocked = HashSet::new();
        for path in dirty {
            let full = self.absolute_path(path)?;
            blocked.extend(
                self.query_store
                    .affected(&self.database.configuration, &full)?,
            );
            blocked.extend(
                self.database
                    .commands
                    .iter()
                    .filter(|c| c.file.to_string_lossy() == full)
                    .map(|c| c.unit_id.clone()),
            );
        }
        let selected: Vec<_> = self
            .database
            .commands
            .iter()
            .filter(|c| self.runtime.pending.contains(&c.unit_id) && !blocked.contains(&c.unit_id))
            .cloned()
            .collect();
        if selected.is_empty() {
            return Ok(());
        }
        self.start_selected(selected, true)?;
        self.runtime.auto_jobs += 1;
        if self.runtime.pending.is_empty() {
            self.runtime.first = None;
            self.runtime.last = None;
        }
        Ok(())
    }
}
