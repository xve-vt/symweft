use crate::{
    Result, Snapshot,
    cache::{Cache, CacheStats, Settings},
    hash,
    store::{Database, HeatRow},
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use symweft_analysis::Facts;

pub struct Loaded {
    pub facts: Arc<Facts>,
    pub source: &'static str,
    pub load_micros: u64,
}
#[derive(Serialize)]
pub struct Status {
    pub settings: Settings,
    pub cache: CacheStats,
    pub sqlite_version: String,
    pub pending_prefetch: usize,
    pub queued_payload_bytes: usize,
    pub last_write_error: Option<String>,
}
type Reply<T> = mpsc::Sender<Result<T>>;
enum Command {
    Load(String, [u8; 32], Reply<Option<Loaded>>),
    Store(Snapshot, Vec<String>),
    Settings(Settings, Reply<Status>),
    Pressure(String, bool, u64),
    Status(Reply<Status>),
    Heat(String, String, usize, Reply<Vec<HeatRow>>),
    Usage(String, Arc<Facts>, Option<u32>, u64, bool),
    ClearUsage(Reply<()>),
    Flush(Reply<()>),
}
pub struct Worker {
    sender: Option<SyncSender<Command>>,
    thread: Option<JoinHandle<()>>,
    queued: Arc<AtomicUsize>,
}
impl Worker {
    pub fn open(path: PathBuf, settings: Settings) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(32);
        let (ready_send, ready) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let queued_worker = Arc::clone(&queued);
        let thread = thread::spawn(move || {
            let mut db = match Database::open(&path) {
                Ok(db) => {
                    let _ = ready_send.send(Ok(()));
                    db
                }
                Err(e) => {
                    let _ = ready_send.send(Err(e));
                    return;
                }
            };
            let mut cache = Cache::new(settings);
            let mut prefetch = VecDeque::<String>::new();
            let mut pending = None;
            let mut last_write_error = None;
            loop {
                let command = if let Some(cmd) = pending.take() {
                    cmd
                } else {
                    match receiver.recv_timeout(Duration::from_millis(if prefetch.is_empty() {
                        500
                    } else {
                        0
                    })) {
                        Ok(cmd) => cmd,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if cache.prefetch_enabled() {
                                if let Some(path) = prefetch.pop_front() {
                                    if !cache.contains(&path) {
                                        let size:i64=db.connection.query_row("SELECT length(payload) FROM snapshots WHERE path=?1",[&path],|r|r.get(0)).unwrap_or(i64::MAX);
                                        if size <= 256 * 1024 {
                                            if let Ok(Some(snapshot)) = db.get(&path) {
                                                if let Ok(facts) = snapshot.decode() {
                                                    cache.insert(
                                                        &path,
                                                        snapshot.source_hash,
                                                        facts,
                                                        true,
                                                        0.0,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                prefetch.clear();
                            }
                            continue;
                        }
                    }
                };
                match command {
                    Command::Load(path, source_hash, reply) => {
                        let started = Instant::now();
                        let result = if let Some(facts) = cache.get(&path, &source_hash) {
                            Ok(Some(Loaded {
                                facts,
                                source: "memory",
                                load_micros: started.elapsed().as_micros() as u64,
                            }))
                        } else {
                            (|| -> Result<Option<Loaded>> {
                                let Some(snapshot) = db.get(&path)? else {
                                    return Ok(None);
                                };
                                if snapshot.source_hash != source_hash {
                                    return Ok(None);
                                }
                                let facts = snapshot.decode()?;
                                let prior = db.file_priority(&path).unwrap_or(0.0);
                                cache.insert(&path, source_hash, Arc::clone(&facts), false, prior);
                                Ok(Some(Loaded {
                                    facts,
                                    source: "sqlite",
                                    load_micros: started.elapsed().as_micros() as u64,
                                }))
                            })()
                        };
                        let _ = reply.send(result);
                        prefetch.clear();
                        if cache.prefetch_enabled() {
                            if let Ok(paths) = db.neighbors(&path, 4) {
                                prefetch.extend(paths);
                            }
                        }
                    }
                    Command::Store(snapshot, deps) => {
                        let mut batch_bytes = snapshot.payload.len();
                        let mut values = BTreeMap::new();
                        let mut snapshot = snapshot;
                        snapshot.dependencies = deps;
                        values.insert(snapshot.path.clone(), snapshot);
                        while values.len() < 16 {
                            match receiver.try_recv() {
                                Ok(Command::Store(mut s, d)) => {
                                    batch_bytes += s.payload.len();
                                    s.dependencies = d;
                                    values.insert(s.path.clone(), s);
                                }
                                Ok(other) => {
                                    pending = Some(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        if let Err(e) =
                            db.put_batch(&values.into_values().collect::<Vec<_>>(), true)
                        {
                            eprintln!("index write: {e}");
                            last_write_error = Some(e.to_string());
                        }
                        queued_worker.fetch_sub(batch_bytes, Ordering::Relaxed);
                    }
                    Command::Settings(settings, reply) => {
                        let result =
                            cache
                                .configure(settings)
                                .map_err(|e| e.into())
                                .and_then(|_| {
                                    db.set_page_budget(
                                        (cache.stats.effective_bytes / 10 / 1024).max(256),
                                    )?;
                                    Ok(status(
                                        &db,
                                        &cache,
                                        prefetch.len(),
                                        queued_worker.load(Ordering::Relaxed),
                                        last_write_error.clone(),
                                    ))
                                });
                        let _ = reply.send(result);
                    }
                    Command::Pressure(level, busy, now) => {
                        cache.observe(&level, busy, now);
                        let _ =
                            db.set_page_budget((cache.stats.effective_bytes / 10 / 1024).max(256));
                    }
                    Command::Status(reply) => {
                        let _ = reply.send(Ok(status(
                            &db,
                            &cache,
                            prefetch.len(),
                            queued_worker.load(Ordering::Relaxed),
                            last_write_error.clone(),
                        )));
                    }
                    Command::Heat(kind, sort, limit, reply) => {
                        let _ = reply.send(db.heat(&kind, &sort, limit));
                    }
                    Command::Usage(path, facts, id, ms, open) => {
                        if let Err(e) = db.record_usage(&path, &facts, id, ms, open) {
                            eprintln!("usage: {e}");
                        }
                    }
                    Command::ClearUsage(reply) => {
                        let _ = reply.send(db.clear_usage());
                    }
                    Command::Flush(reply) => {
                        let result = if let Some(error) = &last_write_error {
                            Err(error.clone().into())
                        } else {
                            db.checkpoint()
                        };
                        let _ = reply.send(result);
                    }
                }
            }
            let _ = db.checkpoint();
        });
        ready.recv()??;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
            queued,
        })
    }
    fn rpc<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.sender.as_ref().ok_or("index closed")?.send(make(tx))?;
        rx.recv()?
    }
    pub fn load(&self, path: &str, source_hash: [u8; 32]) -> Result<Option<Loaded>> {
        self.rpc(|r| Command::Load(path.into(), source_hash, r))
    }
    pub fn store(&self, snapshot: Snapshot, deps: Vec<String>) -> Result<()> {
        let bytes = snapshot.payload.len();
        let sender = self.sender.as_ref().ok_or("index closed")?;
        // compare_exchange also supports the workspace's Rust 1.85 minimum.
        let mut current = self.queued.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_add(bytes)
                .filter(|&n| n <= 16 * 1024 * 1024)
                .ok_or("index write byte queue is full")?;
            match self.queued.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        if let Err(error) = sender.try_send(Command::Store(snapshot, deps)) {
            self.queued.fetch_sub(bytes, Ordering::Relaxed);
            return Err(error.to_string().into());
        }
        Ok(())
    }
    pub fn configure(&self, settings: Settings) -> Result<Status> {
        self.rpc(|r| Command::Settings(settings, r))
    }
    pub fn pressure(&self, level: &str, busy: bool, now_ms: u64) -> Result<()> {
        self.sender
            .as_ref()
            .ok_or("index closed")?
            .send(Command::Pressure(level.into(), busy, now_ms))?;
        Ok(())
    }
    pub fn status(&self) -> Result<Status> {
        self.rpc(Command::Status)
    }
    pub fn heat(&self, kind: &str, sort: &str, limit: usize) -> Result<Vec<HeatRow>> {
        self.rpc(|r| Command::Heat(kind.into(), sort.into(), limit, r))
    }
    pub fn usage(
        &self,
        path: &str,
        facts: Arc<Facts>,
        id: Option<u32>,
        ms: u64,
        opened: bool,
    ) -> Result<()> {
        self.sender
            .as_ref()
            .ok_or("index closed")?
            .try_send(Command::Usage(path.into(), facts, id, ms, opened))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn flush(&self) -> Result<()> {
        self.rpc(Command::Flush)
    }
    pub fn clear_usage(&self) -> Result<()> {
        self.rpc(Command::ClearUsage)
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn status(
    db: &Database,
    cache: &Cache,
    pending: usize,
    queued: usize,
    error: Option<String>,
) -> Status {
    Status {
        settings: cache.settings.clone(),
        cache: cache.stats.clone(),
        sqlite_version: db.sqlite_version().into(),
        pending_prefetch: pending,
        queued_payload_bytes: queued,
        last_write_error: error,
    }
}

pub fn local_includes(root: &Path, path: &str, source: &str) -> Vec<String> {
    let mut result = Vec::new();
    for line in source.lines() {
        let Some(rest) = line.trim_start().strip_prefix("#include") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else { continue };
        let include = &rest[..end];
        if include.len() > 256 {
            continue;
        }
        for candidate in [
            root.join(Path::new(path).parent().unwrap_or(Path::new("")))
                .join(include),
            root.join("include").join(include),
            root.join(include),
        ] {
            if let Ok(canonical) = candidate.canonicalize() {
                if let Ok(relative) = canonical.strip_prefix(root) {
                    if canonical.is_file() {
                        if let Some(path) = relative.to_str() {
                            result.push(path.replace('\\', "/"));
                            break;
                        }
                    }
                }
            }
        }
        if result.len() >= 64 {
            break;
        }
    }
    result.sort();
    result.dedup();
    result
}
pub fn cache_directory(root: &Path) -> PathBuf {
    let base = if let Some(base) = std::env::var_os("SYMWEFT_CACHE_DIR") {
        PathBuf::from(base)
    } else {
        #[cfg(target_os = "macos")]
        let base = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("Library/Caches/Symweft");
        #[cfg(target_os = "windows")]
        let base = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
            .join("Symweft/Cache");
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
            })
            .join("symweft");
        base
    };
    base.join(
        blake3::Hash::from(hash(root.to_string_lossy().as_bytes()))
            .to_hex()
            .as_str(),
    )
    .join("a1-v2")
}
