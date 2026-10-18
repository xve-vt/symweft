//! Native events are hints, not a replacement for input validation.
use crate::Result;
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{MetadataKind, ModifyKind},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
const MAX_PATHS: usize = 1024;
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub auto_refresh: bool,
    pub metadata_cache: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_refresh: true,
            metadata_cache: true,
        }
    }
}
#[derive(Default)]
pub struct Batch {
    pub paths: HashMap<PathBuf, bool>,
    pub rescan: bool,
    pub errors: Vec<String>,
}
struct Inbox {
    batch: Batch,
    first: Option<Instant>,
    last: Option<Instant>,
    events: u64,
    overflows: u64,
    degraded: bool,
}
impl Inbox {
    fn new() -> Self {
        Self {
            batch: Batch::default(),
            first: None,
            last: None,
            events: 0,
            overflows: 0,
            degraded: false,
        }
    }
}
fn ignored(path: &Path, excluded: &Path) -> bool {
    path.starts_with(excluded)
        || path.components().any(|c| c.as_os_str() == ".git")
        || matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("o" | "a" | "d" | "log" | "pyc" | "sqlite3" | "sqlite3-wal" | "sqlite3-shm")
        )
}
fn structural_path(path: &Path) -> bool {
    path.is_dir()
        || matches!(
            path.extension().and_then(|s| s.to_str()),
            None | Some(
                "h" | "hh"
                    | "hpp"
                    | "hxx"
                    | "inc"
                    | "inl"
                    | "def"
                    | "c"
                    | "cc"
                    | "cpp"
                    | "cxx"
                    | "C"
            )
        )
}
fn record(
    inbox: &Mutex<Inbox>,
    epoch: &AtomicU64,
    publish_epoch: &AtomicU64,
    excluded: &Path,
    event: notify::Result<Event>,
) {
    let Ok(mut state) = inbox.lock() else {
        epoch.fetch_add(1, Ordering::SeqCst);
        publish_epoch.fetch_add(1, Ordering::SeqCst);
        return;
    };
    let mut changed = false;
    let mut search_changed = false;
    match event {
        Err(e) => {
            search_changed = true;
            state.degraded = true;
            state.batch.rescan = true;
            if state.batch.errors.len() < 8 {
                state.batch.errors.push(e.to_string());
            }
            changed = true;
        }
        Ok(event) => {
            if matches!(
                event.kind,
                EventKind::Access(_)
                    | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
            ) && !event.need_rescan()
            {
                return;
            }
            if event.need_rescan() {
                state.batch.rescan = true;
                search_changed = true;
                changed = true;
            }
            let structural = matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(ModifyKind::Name(_))
                    | EventKind::Any
                    | EventKind::Other
            );
            if event.paths.len() > MAX_PATHS {
                state.batch.rescan = true;
                state.overflows += 1;
                search_changed = true;
                changed = true;
            }
            for path in event.paths.into_iter().take(MAX_PATHS) {
                if ignored(&path, excluded) {
                    continue;
                }
                changed = true;
                let broad = structural && structural_path(&path);
                search_changed |= broad;
                if state.batch.paths.len() >= MAX_PATHS && !state.batch.paths.contains_key(&path) {
                    state.batch.rescan = true;
                    state.overflows += 1;
                    search_changed = true;
                } else {
                    state
                        .batch
                        .paths
                        .entry(path)
                        .and_modify(|value| *value |= broad)
                        .or_insert(broad);
                }
            }
        }
    }
    if changed {
        let now = Instant::now();
        state.first.get_or_insert(now);
        state.last = Some(now);
        state.events += 1;
        epoch.fetch_add(1, Ordering::SeqCst);
        if search_changed {
            publish_epoch.fetch_add(1, Ordering::SeqCst);
        }
    }
}
pub struct Monitor {
    _watcher: RecommendedWatcher,
    inbox: Arc<Mutex<Inbox>>,
    pub epoch: Arc<AtomicU64>,
    pub publish_epoch: Arc<AtomicU64>,
    roots: Vec<PathBuf>,
}
impl Monitor {
    pub fn open(source: &Path, build: &Path, excluded: &Path) -> Result<Self> {
        let inbox = Arc::new(Mutex::new(Inbox::new()));
        let epoch = Arc::new(AtomicU64::new(0));
        let publish_epoch = Arc::new(AtomicU64::new(0));
        let (queue, generation, publish_generation, excluded) = (
            Arc::clone(&inbox),
            Arc::clone(&epoch),
            Arc::clone(&publish_epoch),
            excluded
                .canonicalize()
                .unwrap_or_else(|_| excluded.to_owned()),
        );
        let mut watcher = RecommendedWatcher::new(
            move |event| record(&queue, &generation, &publish_generation, &excluded, event),
            notify::Config::default().with_follow_symlinks(false),
        )?;
        let roots = if source.starts_with(build) {
            vec![build.to_owned()]
        } else if build.starts_with(source) {
            vec![source.to_owned()]
        } else {
            vec![source.to_owned(), build.to_owned()]
        };
        for root in &roots {
            watcher.watch(root, RecursiveMode::Recursive)?;
        }
        Ok(Self {
            _watcher: watcher,
            inbox,
            epoch,
            publish_epoch,
            roots,
        })
    }
    pub fn drain(&self) -> Batch {
        self.inbox
            .lock()
            .map(|mut q| std::mem::take(&mut q.batch))
            .unwrap_or(Batch {
                rescan: true,
                ..Batch::default()
            })
    }
    pub fn ready(&self) -> bool {
        self.inbox.lock().is_ok_and(|q| {
            q.last
                .is_some_and(|last| last.elapsed() >= Duration::from_millis(350))
                || q.first
                    .is_some_and(|first| first.elapsed() >= Duration::from_secs(2))
        })
    }
    pub fn settled(&self) {
        if let Ok(mut q) = self.inbox.lock() {
            q.first = None;
            q.last = None;
        }
    }
    pub fn healthy(&self) -> bool {
        self.inbox.lock().is_ok_and(|q| !q.degraded)
    }
    pub fn status(&self) -> Value {
        match self.inbox.lock() {
            Ok(q) => {
                json!({"backend":format!("{:?}",RecommendedWatcher::kind()),"roots":self.roots,"epoch":self.epoch.load(Ordering::SeqCst),"publish_epoch":self.publish_epoch.load(Ordering::SeqCst),"queued_paths":q.batch.paths.len(),"events":q.events,"overflows":q.overflows,"degraded":q.degraded})
            }
            Err(_) => json!({"degraded":true,"error":"watch inbox poisoned"}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn events_are_bounded_access_and_cache_writes_are_ignored_and_overflow_requests_rescan() {
        let inbox = Mutex::new(Inbox::new());
        let epoch = AtomicU64::new(0);
        let publish_epoch = AtomicU64::new(0);
        let excluded = Path::new("/workspace/cache");
        for (kind, path) in [
            (
                EventKind::Access(notify::event::AccessKind::Any),
                "/workspace/a.h",
            ),
            (
                EventKind::Create(notify::event::CreateKind::File),
                "/workspace/cache/index.sqlite3",
            ),
            (
                EventKind::Create(notify::event::CreateKind::File),
                "/workspace/.git/index",
            ),
        ] {
            record(
                &inbox,
                &epoch,
                &publish_epoch,
                excluded,
                Ok(Event::new(kind).add_path(path.into())),
            );
        }
        assert_eq!(epoch.load(Ordering::SeqCst), 0);
        let mut event = Event::new(EventKind::Create(notify::event::CreateKind::File));
        event.paths = (0..2000)
            .map(|n| PathBuf::from(format!("/workspace/{n}.h")))
            .collect();
        record(&inbox, &epoch, &publish_epoch, excluded, Ok(event));
        let state = inbox.lock().unwrap();
        assert!(state.batch.rescan && state.batch.paths.len() <= MAX_PATHS && state.overflows > 0);
    }
}
