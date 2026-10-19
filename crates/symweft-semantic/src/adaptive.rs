//! Load-adaptive scaling, hysteresis control, and user pause preservation (PERF-10).
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemPhase {
    Normal,
    ForegroundBusy,
    MemoryPressure,
    Idle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveScalingStatus {
    pub phase: SystemPhase,
    pub prefetch_scale: f64,
    pub hot_pool_scale: f64,
    pub background_indexing_paused: bool,
    pub user_paused: bool,
    pub hysteresis_active: bool,
}

#[derive(Debug)]
pub struct AdaptiveCoordinator {
    phase: SystemPhase,
    user_paused: bool,
    last_phase_change: Instant,
    hysteresis_duration: Duration,
    consecutive_idle_ticks: usize,
}

impl AdaptiveCoordinator {
    pub fn new(hysteresis_ms: u64) -> Self {
        Self {
            phase: SystemPhase::Normal,
            user_paused: false,
            last_phase_change: Instant::now(),
            hysteresis_duration: Duration::from_millis(hysteresis_ms),
            consecutive_idle_ticks: 0,
        }
    }

    pub fn phase(&self) -> SystemPhase {
        self.phase
    }

    pub fn is_user_paused(&self) -> bool {
        self.user_paused
    }

    pub fn set_user_pause(&mut self, paused: bool) {
        self.user_paused = paused;
    }

    pub fn on_foreground_activity(&mut self) {
        if self.phase != SystemPhase::ForegroundBusy {
            self.phase = SystemPhase::ForegroundBusy;
            self.last_phase_change = Instant::now();
            self.consecutive_idle_ticks = 0;
        }
    }

    pub fn on_memory_pressure(&mut self, _critical: bool) {
        self.phase = SystemPhase::MemoryPressure;
        self.last_phase_change = Instant::now();
        self.consecutive_idle_ticks = 0;
    }

    pub fn on_tick_idle(&mut self) {
        self.consecutive_idle_ticks += 1;
        if self.last_phase_change.elapsed() >= self.hysteresis_duration && self.consecutive_idle_ticks >= 3 {
            if self.phase != SystemPhase::Idle {
                self.phase = SystemPhase::Idle;
                self.last_phase_change = Instant::now();
            }
        }
    }

    pub fn status(&self) -> AdaptiveScalingStatus {
        let (prefetch_scale, hot_pool_scale, bg_paused) = match self.phase {
            SystemPhase::Normal => (1.0, 1.0, self.user_paused),
            SystemPhase::ForegroundBusy => (0.25, 0.5, true),
            SystemPhase::MemoryPressure => (0.0, 0.25, true),
            SystemPhase::Idle => (1.0, 1.0, self.user_paused),
        };

        let hysteresis_active = self.last_phase_change.elapsed() < self.hysteresis_duration;

        AdaptiveScalingStatus {
            phase: self.phase,
            prefetch_scale,
            hot_pool_scale,
            background_indexing_paused: bg_paused,
            user_paused: self.user_paused,
            hysteresis_active,
        }
    }
}
