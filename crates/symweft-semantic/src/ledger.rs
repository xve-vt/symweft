//! Unified multi-subsystem resource ledger and policy switch (PERF-09).
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LedgerMode {
    Manual,
    Auto,
    Constrained,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubsystemBilling {
    pub a1_syntax_bytes: usize,
    pub a2_semantic_hot_pool_bytes: usize,
    pub preamble_cache_bytes: usize,
    pub sqlite_page_cache_bytes: usize,
    pub queue_buffers_bytes: usize,
}

impl SubsystemBilling {
    pub fn total_estimated_bytes(&self) -> usize {
        self.a1_syntax_bytes
            + self.a2_semantic_hot_pool_bytes
            + self.preamble_cache_bytes
            + self.sqlite_page_cache_bytes
            + self.queue_buffers_bytes
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerReport {
    pub mode: LedgerMode,
    pub max_allowed_total_bytes: usize,
    pub billing: SubsystemBilling,
    pub total_estimated_bytes: usize,
    pub sampled_process_rss_bytes: Option<u64>,
    pub gpu_state: String,
    pub over_budget: bool,
}

#[derive(Debug)]
pub struct UnifiedResourceLedger {
    mode: LedgerMode,
    manual_limit_bytes: usize,
    auto_limit_bytes: usize,
    billing: SubsystemBilling,
    sampled_rss: Option<u64>,
}

impl UnifiedResourceLedger {
    pub fn new(mode: LedgerMode, manual_limit_bytes: usize) -> Self {
        let auto_limit_bytes = 128 * 1024 * 1024;
        Self {
            mode,
            manual_limit_bytes,
            auto_limit_bytes,
            billing: SubsystemBilling::default(),
            sampled_rss: None,
        }
    }

    pub fn mode(&self) -> LedgerMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: LedgerMode) {
        self.mode = mode;
    }

    pub fn set_manual_limit(&mut self, bytes: usize) {
        self.manual_limit_bytes = bytes;
    }

    pub fn effective_limit_bytes(&self) -> usize {
        match self.mode {
            LedgerMode::Manual => self.manual_limit_bytes,
            LedgerMode::Auto => self.auto_limit_bytes,
            LedgerMode::Constrained => self.manual_limit_bytes.min(32 * 1024 * 1024),
        }
    }

    pub fn record_a1(&mut self, bytes: usize) {
        self.billing.a1_syntax_bytes = bytes;
    }

    pub fn record_a2_hot_pool(&mut self, bytes: usize) {
        self.billing.a2_semantic_hot_pool_bytes = bytes;
    }

    pub fn record_preamble(&mut self, bytes: usize) {
        self.billing.preamble_cache_bytes = bytes;
    }

    pub fn record_sqlite(&mut self, bytes: usize) {
        self.billing.sqlite_page_cache_bytes = bytes;
    }

    pub fn record_queues(&mut self, bytes: usize) {
        self.billing.queue_buffers_bytes = bytes;
    }

    pub fn set_sampled_rss(&mut self, rss: Option<u64>) {
        self.sampled_rss = rss;
    }

    pub fn report(&self) -> LedgerReport {
        let total = self.billing.total_estimated_bytes();
        let limit = self.effective_limit_bytes();
        LedgerReport {
            mode: self.mode,
            max_allowed_total_bytes: limit,
            billing: self.billing.clone(),
            total_estimated_bytes: total,
            sampled_process_rss_bytes: self.sampled_rss,
            gpu_state: "unmeasured_external".to_string(),
            over_budget: total > limit,
        }
    }
}
