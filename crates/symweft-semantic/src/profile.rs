//! Opt-in diagnostic measurements. Never part of semantic identity or storage.
use serde::{Deserialize, Serialize};
use std::{io::Write, sync::OnceLock, time::Instant};

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("SYMWEFT_PROFILE").as_deref() == Ok("1"))
}
pub(crate) struct Clock(Option<Instant>);
impl Clock {
    pub(crate) fn start(enabled: bool) -> Self {
        Self(enabled.then(Instant::now))
    }
    pub(crate) fn micros(&self) -> u64 {
        self.0.map_or(0, |s| s.elapsed().as_micros() as u64)
    }
    pub(crate) fn nanos(&self) -> u64 {
        self.0.map_or(0, |s| s.elapsed().as_nanos() as u64)
    }
}
/// Preserve the streaming serializer and its buffering. Writer time includes
/// buffer copies and pipe backpressure, not just kernel transfer time.
#[allow(dead_code)]
pub(crate) struct TimedWriter<W> {
    pub inner: W,
    pub nanos: u64,
    pub bytes: usize,
}
impl<W: Write> Write for TimedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let start = Instant::now();
        let result = self.inner.write(bytes);
        self.nanos += start.elapsed().as_nanos() as u64;
        if let Ok(n) = &result {
            self.bytes += n;
        }
        result
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let start = Instant::now();
        let result = self.inner.flush();
        self.nanos += start.elapsed().as_nanos() as u64;
        result
    }
}
pub const PREFIX: &str = "SYMWEFT_WORKER_PROFILE ";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerProfile {
    pub pid: u32,
    pub request_decode_micros: u64,
    pub prepare_micros: u64,
    pub parse_micros: u64,
    pub diagnostics_micros: u64,
    pub fact_extract_micros: u64,
    pub input_validation_micros: u64,
    pub provider_wall_micros: u64,
    pub reply_encode_micros: u64,
    pub stdout_write_micros: u64,
    pub worker_wall_micros: u64,
    pub reply_bytes: usize,
    pub cpu_user_micros: Option<u64>,
    pub cpu_system_micros: Option<u64>,
    pub self_peak_rss_bytes: Option<u64>,
}
impl WorkerProfile {
    pub(crate) fn usage(&mut self) {
        self.pid = std::process::id();
        #[cfg(unix)]
        {
            let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
            if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
                self.cpu_user_micros = Some(
                    (usage.ru_utime.tv_sec as u64) * 1_000_000 + usage.ru_utime.tv_usec as u64,
                );
                self.cpu_system_micros = Some(
                    (usage.ru_stime.tv_sec as u64) * 1_000_000 + usage.ru_stime.tv_usec as u64,
                );
                self.self_peak_rss_bytes = Some(if cfg!(target_os = "macos") {
                    usage.ru_maxrss as u64
                } else {
                    usage.ru_maxrss as u64 * 1024
                });
            }
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunProfile {
    pub request_encode_micros: u64,
    pub spawn_micros: u64,
    pub stdin_write_micros: u64,
    /// Includes execution and concurrent stdout drain. Not a pure transfer time.
    pub child_wait_micros: u64,
    pub reply_decode_micros: u64,
    pub controller_wall_micros: u64,
    pub request_bytes: usize,
    pub reply_bytes: usize,
    pub worker: Option<WorkerProfile>,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct StoreProfile {
    #[serde(skip)]
    pub enabled: bool,
    pub publish_wall_micros: u64,
    pub fact_encode_nanos: u64,
    pub fact_hash_nanos: u64,
    pub fact_compress_nanos: u64,
    pub reused_fact_verify_nanos: u64,
    pub commit_micros: u64,
    pub sqlite_and_other_micros: u64,
    pub new_facts: u64,
    pub reused_facts: u64,
}
