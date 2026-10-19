//! Bounded semantic neighbor prefetching and efficiency tracking (PERF-07).
use crate::{
    Result,
    hot_pool::SemanticHotPool,
    model::QueryVersion,
    store::Store,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub const DEFAULT_MAX_PREFETCH_ENTITIES: usize = 8;
pub const DEFAULT_MAX_PREFETCH_BYTES: usize = 1024 * 1024; // 1 MiB
pub const DEFAULT_PREFETCH_TIMEOUT_MS: u64 = 50;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrefetchMetrics {
    pub total_prefetches: u64,
    pub useful_prefetches: u64,
    pub wasted_prefetches: u64,
    pub prefetched_bytes: usize,
    pub useful_bytes: usize,
    pub wasted_bytes: usize,
    pub useful_ratio: f64,
}

#[derive(Debug)]
pub struct SemanticPrefetcher {
    max_entities: usize,
    max_bytes: usize,
    timeout: Duration,
    prefetched_keys: HashSet<String>,
    useful_keys: HashSet<String>,
    total_prefetched_bytes: usize,
    total_useful_bytes: usize,
    cancelled_count: u64,
}

impl SemanticPrefetcher {
    pub fn new(max_entities: usize, max_bytes: usize, timeout_ms: u64) -> Self {
        Self {
            max_entities,
            max_bytes,
            timeout: Duration::from_millis(timeout_ms),
            prefetched_keys: HashSet::new(),
            useful_keys: HashSet::new(),
            total_prefetched_bytes: 0,
            total_useful_bytes: 0,
            cancelled_count: 0,
        }
    }

    pub fn prefetch_neighbors(
        &mut self,
        store: &Store,
        pool: &mut SemanticHotPool,
        version: &QueryVersion,
        active_entity: &str,
        cancel: &AtomicBool,
    ) -> Result<usize> {
        if cancel.load(Ordering::Relaxed) {
            self.cancelled_count += 1;
            return Ok(0);
        }
        let start = Instant::now();
        let mut count = 0;
        let mut current_bytes = 0;

        let outgoing = store.relations(&version.configuration.id, active_entity, "outgoing", &version.configuration.provider, 16)?;
        let rows = outgoing.get("rows").and_then(serde_json::Value::as_array);

        if let Some(rows) = rows {
            for row in rows {
                if cancel.load(Ordering::Relaxed) || start.elapsed() >= self.timeout || count >= self.max_entities || current_bytes >= self.max_bytes {
                    break;
                }
                if let Some(target) = row.get("target").and_then(serde_json::Value::as_str) {
                    if target.is_empty() || target == active_entity {
                        continue;
                    }
                    for direction in ["definitions", "incoming"] {
                        if cancel.load(Ordering::Relaxed) || start.elapsed() >= self.timeout {
                            break;
                        }
                        let key = format!("{}:{}:{}", target, direction, crate::hot_pool::version_digest(version)[0]);
                        if !self.prefetched_keys.contains(&key) {
                            if let Ok(res) = store.relations(&version.configuration.id, target, direction, &version.configuration.provider, 16) {
                                let total = res.get("total").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
                                let est = res.to_string().len();
                                pool.insert(version, target, direction, res, total);
                                self.prefetched_keys.insert(key);
                                self.total_prefetched_bytes += est;
                                current_bytes += est;
                                count += 1;
                            }
                        }
                    }
                }
            }
        }

        Ok(count)
    }

    pub fn record_access(&mut self, entity_id: &str, direction: &str, version: &QueryVersion, bytes: usize) {
        let key = format!("{}:{}:{}", entity_id, direction, crate::hot_pool::version_digest(version)[0]);
        if self.prefetched_keys.contains(&key) && !self.useful_keys.contains(&key) {
            self.useful_keys.insert(key);
            self.total_useful_bytes += bytes;
        }
    }

    pub fn metrics(&self) -> PrefetchMetrics {
        let total = self.prefetched_keys.len() as u64;
        let useful = self.useful_keys.len() as u64;
        let wasted = total.saturating_sub(useful);
        let wasted_bytes = self.total_prefetched_bytes.saturating_sub(self.total_useful_bytes);
        let ratio = if total > 0 { useful as f64 / total as f64 } else { 0.0 };
        PrefetchMetrics {
            total_prefetches: total,
            useful_prefetches: useful,
            wasted_prefetches: wasted,
            prefetched_bytes: self.total_prefetched_bytes,
            useful_bytes: self.total_useful_bytes,
            wasted_bytes,
            useful_ratio: ratio,
        }
    }

    pub fn on_memory_pressure(&mut self, pool: &mut SemanticHotPool) {
        pool.on_memory_pressure(true);
        self.prefetched_keys.clear();
        self.useful_keys.clear();
        self.total_prefetched_bytes = 0;
        self.total_useful_bytes = 0;
    }
}
