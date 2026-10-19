//! In-memory A2 semantic fact & adjacency hot pool (PERF-06 & HEAT-04).
use crate::{
    model::QueryVersion,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolMode {
    Off,
    Manual,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolKey {
    pub version_hash: [u8; 32],
    pub entity_id: String,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRelations {
    pub entity_id: String,
    pub direction: String,
    pub rows: serde_json::Value,
    pub total: usize,
    pub estimated_bytes: usize,
    pub structural_heat: f64,
    pub attention_score: f64,
    pub hit_count: u64,
}

impl CachedRelations {
    pub fn utility(&self, now_ns: u64, last_access_ns: u64) -> f64 {
        let heat_factor = 2.0 * self.attention_score + 1.0 * self.structural_heat + 0.5 * (self.hit_count as f64).ln_1p();
        let age_sec = (now_ns.saturating_sub(last_access_ns) as f64) / 1e9;
        let recency = 2.0_f64.powf(-age_sec / 3600.0);
        let size_kb = (self.estimated_bytes as f64 / 1024.0).max(0.1);
        (heat_factor * recency + 0.01) / size_kb.sqrt()
    }
}

#[derive(Debug)]
pub struct SemanticHotPool {
    mode: PoolMode,
    max_bytes: usize,
    current_bytes: usize,
    cache: HashMap<PoolKey, (CachedRelations, u64)>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl SemanticHotPool {
    pub fn new(mode: PoolMode, max_bytes: usize) -> Self {
        Self {
            mode,
            max_bytes,
            current_bytes: 0,
            cache: HashMap::new(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    pub fn mode(&self) -> PoolMode {
        self.mode
    }

    pub fn current_bytes(&self) -> usize {
        self.current_bytes
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn entry_count(&self) -> usize {
        self.cache.len()
    }

    pub fn configure(&mut self, mode: PoolMode, max_bytes: usize) {
        self.mode = mode;
        self.max_bytes = max_bytes;
        if self.mode == PoolMode::Off {
            self.clear();
        } else {
            self.shrink_to_fit();
        }
    }

    pub fn get(&mut self, version: &QueryVersion, entity_id: &str, direction: &str) -> Option<CachedRelations> {
        if self.mode == PoolMode::Off {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let key = PoolKey {
            version_hash: version_digest(version),
            entity_id: entity_id.to_string(),
            direction: direction.to_string(),
        };
        let now_ns = Instant::now().elapsed().as_nanos() as u64;
        if let Some((rel, ns)) = self.cache.get_mut(&key) {
            rel.hit_count += 1;
            *ns = now_ns;
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(rel.clone())
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    pub fn insert(
        &mut self,
        version: &QueryVersion,
        entity_id: &str,
        direction: &str,
        rows: serde_json::Value,
        total: usize,
    ) {
        self.insert_with_heat(version, entity_id, direction, rows, total, 1.0, 0.0);
    }

    pub fn insert_with_heat(
        &mut self,
        version: &QueryVersion,
        entity_id: &str,
        direction: &str,
        rows: serde_json::Value,
        total: usize,
        structural_heat: f64,
        attention_score: f64,
    ) {
        if self.mode == PoolMode::Off {
            return;
        }
        let rows_str = rows.to_string();
        let estimated_bytes = rows_str.len() + 128;
        if estimated_bytes > self.max_bytes {
            return;
        }

        let now_ns = Instant::now().elapsed().as_nanos() as u64;
        let incoming_item = CachedRelations {
            entity_id: entity_id.to_string(),
            direction: direction.to_string(),
            rows,
            total,
            estimated_bytes,
            structural_heat,
            attention_score,
            hit_count: 0,
        };

        // Admission control: if full and incoming utility is lower than all cached items, reject
        if self.current_bytes + estimated_bytes > self.max_bytes && !self.cache.is_empty() {
            let incoming_utility = incoming_item.utility(now_ns, now_ns);
            let min_cached_utility = self
                .cache
                .values()
                .map(|(c, ns)| c.utility(now_ns, *ns))
                .fold(f64::INFINITY, f64::min);
            if incoming_utility < min_cached_utility && self.current_bytes >= self.max_bytes {
                return;
            }
        }

        while self.current_bytes + estimated_bytes > self.max_bytes && !self.cache.is_empty() {
            self.evict_lowest_utility(now_ns);
        }

        let key = PoolKey {
            version_hash: version_digest(version),
            entity_id: entity_id.to_string(),
            direction: direction.to_string(),
        };
        if let Some((old, _)) = self.cache.insert(key, (incoming_item, now_ns)) {
            self.current_bytes = self.current_bytes.saturating_sub(old.estimated_bytes);
        }
        self.current_bytes += estimated_bytes;
    }

    fn evict_lowest_utility(&mut self, now_ns: u64) {
        if let Some(worst_key) = self
            .cache
            .iter()
            .min_by(|(_, (a, a_ns)), (_, (b, b_ns))| {
                let u_a = a.utility(now_ns, *a_ns);
                let u_b = b.utility(now_ns, *b_ns);
                u_a.total_cmp(&u_b)
            })
            .map(|(k, _)| k.clone())
        {
            if let Some((removed, _)) = self.cache.remove(&worst_key) {
                self.current_bytes = self.current_bytes.saturating_sub(removed.estimated_bytes);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn shrink_to_fit(&mut self) {
        let now_ns = Instant::now().elapsed().as_nanos() as u64;
        while self.current_bytes > self.max_bytes && !self.cache.is_empty() {
            self.evict_lowest_utility(now_ns);
        }
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.current_bytes = 0;
    }

    pub fn on_memory_pressure(&mut self, critical: bool) {
        if critical {
            self.clear();
        } else {
            self.max_bytes /= 2;
            self.shrink_to_fit();
        }
    }
}

pub fn version_digest(version: &QueryVersion) -> [u8; 32] {
    let s = format!(
        "{}:{}:{}:{}:{:?}:{:?}",
        version.disk.schema, version.workspace, version.configuration.id, version.configuration.provider, version.documents, version.coverage
    );
    symweft_index::hash(s.as_bytes())
}
