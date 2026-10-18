//! Bounded content fingerprints. Strict mode never trusts cached metadata.
use serde::Serialize;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::Path,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}
fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(Stamp {
        len: m.len(),
        modified: m.modified().ok()?,
        #[cfg(unix)]
        identity: (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()),
        #[cfg(not(unix))]
        created: m.created().ok(),
    })
}
struct Entry {
    stamp: Stamp,
    digest: [u8; 32],
    verified: Instant,
    used: u64,
    bytes: usize,
}
#[derive(Default, Clone, Serialize)]
pub struct Stats {
    pub hits: u64,
    pub hashes: u64,
    pub bytes_read: u64,
    pub evictions: u64,
    pub entries: usize,
    pub charged_bytes: usize,
    pub enabled: bool,
}
pub struct Fingerprints {
    entries: HashMap<String, Entry>,
    budget: usize,
    used: usize,
    tick: u64,
    enabled: bool,
    stats: Stats,
}
impl Default for Fingerprints {
    fn default() -> Self {
        Self::new(1024 * 1024)
    }
}
impl Fingerprints {
    pub fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget,
            used: 0,
            tick: 0,
            enabled: false,
            stats: Stats::default(),
        }
    }
    pub fn enable(&mut self, value: bool) {
        if self.enabled != value {
            self.clear();
        }
        self.enabled = value;
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.used = 0;
    }
    pub fn stats(&self) -> Stats {
        Stats {
            entries: self.entries.len(),
            charged_bytes: self.used,
            enabled: self.enabled,
            ..self.stats.clone()
        }
    }
    pub fn digest(&mut self, path: &Path) -> Option<[u8; 32]> {
        let before = stamp(path)?;
        self.tick = self.tick.wrapping_add(1);
        let key = path.to_string_lossy();
        #[cfg(unix)]
        let lifetime = Duration::from_secs(2);
        #[cfg(not(unix))]
        let lifetime = Duration::from_millis(100);
        if self.enabled {
            if let Some(entry) = self.entries.get_mut(key.as_ref()) {
                if entry.stamp == before && entry.verified.elapsed() < lifetime {
                    entry.used = self.tick;
                    self.stats.hits += 1;
                    return Some(entry.digest);
                }
            }
        }
        let mut file = File::open(path).ok()?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buffer).ok()?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            self.stats.bytes_read += n as u64;
        }
        self.stats.hashes += 1;
        if stamp(path)? != before {
            return None;
        }
        let digest = *hasher.finalize().as_bytes();
        if self.enabled {
            if let Some(old) = self.entries.remove(key.as_ref()) {
                self.used -= old.bytes;
            }
            let bytes = key.len() + std::mem::size_of::<Entry>() + 96;
            if bytes <= self.budget {
                while self.used + bytes > self.budget || self.entries.len() >= 8192 {
                    let oldest = self
                        .entries
                        .iter()
                        .min_by_key(|(_, e)| e.used)
                        .map(|(k, _)| k.clone());
                    if let Some(oldest) = oldest {
                        if let Some(e) = self.entries.remove(&oldest) {
                            self.used -= e.bytes;
                            self.stats.evictions += 1;
                        }
                    } else {
                        break;
                    }
                }
                self.used += bytes;
                self.entries.insert(
                    key.into_owned(),
                    Entry {
                        stamp: before,
                        digest,
                        verified: Instant::now(),
                        used: self.tick,
                        bytes,
                    },
                );
            }
        }
        Some(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn folder() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "symweft-hash-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
    #[test]
    fn strict_hashes_every_time_and_cached_hashes_stay_bounded() {
        let root = folder();
        let mut cache = Fingerprints::new(1500);
        fs::write(root.join("file"), b"first").unwrap();
        let first = cache.digest(&root.join("file")).unwrap();
        cache.digest(&root.join("file"));
        assert_eq!(cache.stats().hashes, 2);
        cache.enable(true);
        cache.digest(&root.join("file"));
        cache.digest(&root.join("file"));
        assert_eq!(cache.stats().hits, 1);
        fs::write(root.join("file"), b"other").unwrap();
        assert_ne!(cache.digest(&root.join("file")), Some(first));
        for n in 0..30 {
            let path = root.join(n.to_string());
            fs::write(&path, b"data").unwrap();
            cache.digest(&path);
        }
        assert!(cache.stats().charged_bytes <= 1500 && cache.stats().evictions > 0);
        cache.clear();
        assert_eq!(cache.stats().entries, 0);
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn same_length_and_restored_mtime_and_atomic_replacement_are_detected() {
        let root = folder();
        let path = root.join("file");
        fs::write(&path, b"first").unwrap();
        let mut cache = Fingerprints::default();
        cache.enable(true);
        let first = cache.digest(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, b"other").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_ne!(cache.digest(&path), Some(first));
        let second = cache.digest(&path);
        fs::write(root.join("new"), b"third").unwrap();
        fs::rename(root.join("new"), &path).unwrap();
        assert_ne!(cache.digest(&path), second);
        fs::remove_dir_all(root).unwrap();
    }
}
