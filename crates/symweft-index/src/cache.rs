use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use symweft_analysis::Facts;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: String,
    pub max_bytes: usize,
    pub prefetch: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            max_bytes: 64 * 1024 * 1024,
            prefetch: true,
        }
    }
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheStats {
    pub effective_bytes: usize,
    pub fact_limit_bytes: usize,
    pub used_bytes: usize,
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub bypasses: u64,
    pub prefetch_admitted: u64,
    pub prefetch_useful: u64,
    pub prefetch_wasted_bytes: u64,
    pub metadata_estimated_bytes: usize,
    pub pressure: String,
}
struct Entry {
    hash: [u8; 32],
    facts: Arc<Facts>,
    bytes: usize,
    touched: u64,
    frequency: f64,
    prior: f64,
    speculative: bool,
}
pub struct Cache {
    pub settings: Settings,
    pub stats: CacheStats,
    entries: HashMap<String, Entry>,
    order: VecDeque<(String, u64)>,
    tick: u64,
    prefetch_bytes: usize,
    last_adjust_ms: u64,
    last_misses: u64,
    busy: bool,
    pub lru_only: bool,
}
impl Cache {
    pub fn new(settings: Settings) -> Self {
        let effective = if settings.mode == "off" {
            0
        } else if settings.mode == "auto" {
            settings.max_bytes / 2
        } else {
            settings.max_bytes
        };
        Self {
            settings,
            stats: CacheStats {
                effective_bytes: effective,
                fact_limit_bytes: effective * 3 / 5,
                pressure: "normal".into(),
                ..CacheStats::default()
            },
            entries: HashMap::new(),
            order: VecDeque::new(),
            tick: 0,
            prefetch_bytes: 0,
            last_adjust_ms: 0,
            last_misses: 0,
            busy: false,
            lru_only: false,
        }
    }
    pub fn configure(&mut self, settings: Settings) -> Result<(), String> {
        if !matches!(settings.mode.as_str(), "auto" | "manual" | "off")
            || !(1024..=1024 * 1024 * 1024).contains(&settings.max_bytes)
        {
            return Err("invalid cache mode or byte limit".into());
        }
        let old_effective = self.stats.effective_bytes;
        self.settings = settings;
        let cap = if self.settings.mode == "off" {
            0
        } else if self.stats.pressure == "critical" {
            self.settings.max_bytes / 8
        } else if self.stats.pressure == "warning" {
            self.settings.max_bytes / 4
        } else if self.settings.mode == "auto" {
            old_effective
                .min(self.settings.max_bytes)
                .max(self.settings.max_bytes / 2)
        } else {
            self.settings.max_bytes
        };
        self.resize(cap);
        Ok(())
    }
    pub fn observe(&mut self, pressure: &str, busy: bool, now_ms: u64) {
        self.busy = busy;
        self.stats.pressure = match pressure {
            "critical" => "critical",
            "warning" => "warning",
            _ => "normal",
        }
        .into();
        let max = self.settings.max_bytes;
        let target = if self.settings.mode == "off" {
            0
        } else if pressure == "critical" {
            max / 8
        } else if pressure == "warning" {
            max / 4
        } else if self.settings.mode == "manual" {
            max
        } else if now_ms.saturating_sub(self.last_adjust_ms) >= 30_000
            && self.stats.misses > self.last_misses + 4
            && !busy
        {
            (self.stats.effective_bytes + self.stats.effective_bytes.max(8) / 8).min(max)
        } else {
            self.stats.effective_bytes
        };
        if target != self.stats.effective_bytes {
            self.last_adjust_ms = now_ms;
            self.last_misses = self.stats.misses;
            self.resize(target);
        }
    }
    pub fn prefetch_enabled(&self) -> bool {
        self.settings.prefetch
            && self.settings.mode != "off"
            && self.stats.pressure == "normal"
            && !self.busy
            && self.stats.fact_limit_bytes > 0
    }
    pub fn get(&mut self, path: &str, hash: &[u8; 32]) -> Option<Arc<Facts>> {
        if self.entries.get(path).is_some_and(|e| &e.hash != hash) {
            self.remove(path);
        }
        let Some(entry) = self.entries.get_mut(path) else {
            self.stats.misses += 1;
            return None;
        };
        self.tick += 1;
        entry.frequency =
            entry.frequency * 2f64.powf(-((self.tick - entry.touched) as f64) / 512.0) + 1.0;
        entry.touched = self.tick;
        if entry.speculative {
            entry.speculative = false;
            self.prefetch_bytes -= entry.bytes;
            self.stats.prefetch_useful += 1;
        }
        self.order.push_back((path.into(), self.tick));
        self.stats.hits += 1;
        let result = Arc::clone(&entry.facts);
        self.compact();
        Some(result)
    }
    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }
    pub fn insert(
        &mut self,
        path: &str,
        hash: [u8; 32],
        facts: Arc<Facts>,
        speculative: bool,
        prior: f64,
    ) -> bool {
        let bytes = facts.estimated_bytes() + path.len() + std::mem::size_of::<Entry>();
        let limit = self.stats.fact_limit_bytes;
        if limit == 0
            || bytes > limit / 2
            || (speculative
                && (!self.prefetch_enabled()
                    || bytes > limit / 10
                    || self.prefetch_bytes + bytes > limit / 10))
        {
            self.stats.bypasses += 1;
            return false;
        }
        self.remove(path);
        while self.stats.used_bytes + bytes > limit || self.entries.len() >= 4096 {
            if !self.evict() {
                break;
            }
        }
        self.tick += 1;
        self.entries.insert(
            path.into(),
            Entry {
                hash,
                facts,
                bytes,
                touched: self.tick,
                frequency: if speculative { 0.0 } else { 1.0 },
                prior: prior.clamp(0.0, 2.0),
                speculative,
            },
        );
        self.order.push_back((path.into(), self.tick));
        self.stats.used_bytes += bytes;
        if speculative {
            self.prefetch_bytes += bytes;
            self.stats.prefetch_admitted += 1;
        }
        self.compact();
        true
    }
    fn resize(&mut self, bytes: usize) {
        self.stats.effective_bytes = bytes;
        self.stats.fact_limit_bytes = bytes * 3 / 5;
        while self.stats.used_bytes > self.stats.fact_limit_bytes {
            if !self.evict() {
                break;
            }
        }
        self.compact();
    }
    fn remove(&mut self, path: &str) {
        if let Some(entry) = self.entries.remove(path) {
            self.stats.used_bytes -= entry.bytes;
            if entry.speculative {
                self.prefetch_bytes -= entry.bytes;
                self.stats.prefetch_wasted_bytes += entry.bytes as u64;
            }
        }
    }
    fn evict(&mut self) -> bool {
        let mut candidates = Vec::new();
        while let Some((path, tick)) = self.order.pop_front() {
            if self.entries.get(&path).is_some_and(|e| e.touched == tick) {
                candidates.push((path, tick));
                if self.lru_only || candidates.len() >= 16 {
                    break;
                }
            }
        }
        if candidates.is_empty() {
            return false;
        }
        let victim = candidates
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                let score = |(path, _): &(String, u64)| {
                    let e = &self.entries[path];
                    if e.speculative {
                        -1.0
                    } else {
                        (e.frequency * 2f64.powf(-((self.tick - e.touched) as f64) / 512.0)
                            + e.prior)
                            / (e.bytes.max(1) as f64).sqrt()
                    }
                };
                score(a).total_cmp(&score(b))
            })
            .map(|(i, _)| i)
            .unwrap();
        let (path, _) = candidates.remove(victim);
        self.remove(&path);
        self.stats.evictions += 1;
        for ticket in candidates {
            self.order.push_back(ticket);
        }
        true
    }
    fn compact(&mut self) {
        if self.order.len() > self.entries.len().max(1) * 4 + 32 {
            self.order
                .retain(|(k, t)| self.entries.get(k).is_some_and(|e| e.touched == *t));
        }
        self.stats.entries = self.entries.len();
        self.stats.metadata_estimated_bytes =
            self.entries.len() * (std::mem::size_of::<Entry>() + 128) + self.order.len() * 96;
    }
}
