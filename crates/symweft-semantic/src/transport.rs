//! Worker process IPC transport formats and comparison (PERF-02, PERF-03).
use crate::{
    Result,
    model::{WorkerReply, WorkerRequest},
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

pub const BINARY_MAGIC: [u8; 4] = *b"SWTB"; // Symweft Worker Transport Binary
pub const BINARY_VERSION: u16 = 1;
pub const MAX_BOUNDED_PAYLOAD: usize = 64 * 1024 * 1024; // 64 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportFormat {
    Json,
    BoundedBinary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportBenchmarkMetrics {
    pub format: TransportFormat,
    pub request_bytes: usize,
    pub request_encode_micros: u64,
    pub request_decode_micros: u64,
    pub reply_bytes: usize,
    pub reply_encode_micros: u64,
    pub reply_decode_micros: u64,
    pub total_ipc_micros: u64,
    pub estimated_peak_alloc_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportComparisonReport {
    pub tu_name: String,
    pub symbol_count: usize,
    pub call_count: usize,
    pub reference_count: usize,
    pub json: TransportBenchmarkMetrics,
    pub binary: TransportBenchmarkMetrics,
    pub payload_reduction_ratio: f64,
    pub speedup_factor: f64,
    pub conclusion: String,
}

pub fn encode_request(req: &WorkerRequest, format: TransportFormat) -> Result<Vec<u8>> {
    match format {
        TransportFormat::Json => Ok(serde_json::to_vec(req)?),
        TransportFormat::BoundedBinary => {
            let data = bincode::serde::encode_to_vec(req, bincode::config::standard())
                .map_err(|e| format!("binary request encode error: {e}"))?;
            if data.len() > MAX_BOUNDED_PAYLOAD {
                return Err("binary request exceeds bounded limit of 64 MiB".into());
            }
            let mut out = Vec::with_capacity(10 + data.len());
            out.extend_from_slice(&BINARY_MAGIC);
            out.extend_from_slice(&BINARY_VERSION.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&data);
            Ok(out)
        }
    }
}

pub fn decode_request(bytes: &[u8]) -> Result<WorkerRequest> {
    if bytes.len() >= 10 && &bytes[0..4] == &BINARY_MAGIC {
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != BINARY_VERSION {
            return Err(format!("unsupported binary transport version: {version}").into());
        }
        let len = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]) as usize;
        if len > MAX_BOUNDED_PAYLOAD {
            return Err("bounded binary request payload exceeds 64 MiB limit".into());
        }
        if bytes.len() < 10 + len {
            return Err("truncated binary request payload".into());
        }
        let (req, _): (WorkerRequest, usize) = bincode::serde::decode_from_slice(
            &bytes[10..10 + len],
            bincode::config::standard().with_limit::<MAX_BOUNDED_PAYLOAD>(),
        )
        .map_err(|e| format!("binary request decode error: {e}"))?;
        Ok(req)
    } else {
        Ok(serde_json::from_slice(bytes)?)
    }
}

pub fn encode_reply(reply: &WorkerReply, format: TransportFormat) -> Result<Vec<u8>> {
    match format {
        TransportFormat::Json => Ok(serde_json::to_vec(reply)?),
        TransportFormat::BoundedBinary => {
            let data = bincode::serde::encode_to_vec(reply, bincode::config::standard())
                .map_err(|e| format!("binary reply encode error: {e}"))?;
            if data.len() > MAX_BOUNDED_PAYLOAD {
                return Err("binary reply exceeds bounded limit of 64 MiB".into());
            }
            let mut out = Vec::with_capacity(10 + data.len());
            out.extend_from_slice(&BINARY_MAGIC);
            out.extend_from_slice(&BINARY_VERSION.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&data);
            Ok(out)
        }
    }
}

pub fn decode_reply(bytes: &[u8]) -> Result<WorkerReply> {
    if bytes.len() >= 10 && &bytes[0..4] == &BINARY_MAGIC {
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != BINARY_VERSION {
            return Err(format!("unsupported binary transport version: {version}").into());
        }
        let len = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]) as usize;
        if len > MAX_BOUNDED_PAYLOAD {
            return Err("bounded binary reply payload exceeds 64 MiB limit".into());
        }
        if bytes.len() < 10 + len {
            return Err("truncated binary reply payload".into());
        }
        let (reply, _): (WorkerReply, usize) = bincode::serde::decode_from_slice(
            &bytes[10..10 + len],
            bincode::config::standard().with_limit::<MAX_BOUNDED_PAYLOAD>(),
        )
        .map_err(|e| format!("binary reply decode error: {e}"))?;
        Ok(reply)
    } else {
        Ok(serde_json::from_slice(bytes)?)
    }
}

pub fn compare_transport(
    tu_name: &str,
    request: &WorkerRequest,
    reply: &WorkerReply,
) -> Result<TransportComparisonReport> {
    // 1. Benchmark JSON
    let t_req_enc_json = Instant::now();
    let json_req = encode_request(request, TransportFormat::Json)?;
    let req_enc_json_us = t_req_enc_json.elapsed().as_micros() as u64;

    let t_req_dec_json = Instant::now();
    let _ = decode_request(&json_req)?;
    let req_dec_json_us = t_req_dec_json.elapsed().as_micros() as u64;

    let t_rep_enc_json = Instant::now();
    let json_rep = encode_reply(reply, TransportFormat::Json)?;
    let rep_enc_json_us = t_rep_enc_json.elapsed().as_micros() as u64;

    let t_rep_dec_json = Instant::now();
    let _ = decode_reply(&json_rep)?;
    let rep_dec_json_us = t_rep_dec_json.elapsed().as_micros() as u64;

    let json_metrics = TransportBenchmarkMetrics {
        format: TransportFormat::Json,
        request_bytes: json_req.len(),
        request_encode_micros: req_enc_json_us,
        request_decode_micros: req_dec_json_us,
        reply_bytes: json_rep.len(),
        reply_encode_micros: rep_enc_json_us,
        reply_decode_micros: rep_dec_json_us,
        total_ipc_micros: req_enc_json_us + req_dec_json_us + rep_enc_json_us + rep_dec_json_us,
        estimated_peak_alloc_bytes: json_rep.len() * 2,
    };

    // 2. Benchmark Bounded Binary (Bincode)
    let t_req_enc_bin = Instant::now();
    let bin_req = encode_request(request, TransportFormat::BoundedBinary)?;
    let req_enc_bin_us = t_req_enc_bin.elapsed().as_micros() as u64;

    let t_req_dec_bin = Instant::now();
    let _ = decode_request(&bin_req)?;
    let req_dec_bin_us = t_req_dec_bin.elapsed().as_micros() as u64;

    let t_rep_enc_bin = Instant::now();
    let bin_rep = encode_reply(reply, TransportFormat::BoundedBinary)?;
    let rep_enc_bin_us = t_rep_enc_bin.elapsed().as_micros() as u64;

    let t_rep_dec_bin = Instant::now();
    let _ = decode_reply(&bin_rep)?;
    let rep_dec_bin_us = t_rep_dec_bin.elapsed().as_micros() as u64;

    let bin_metrics = TransportBenchmarkMetrics {
        format: TransportFormat::BoundedBinary,
        request_bytes: bin_req.len(),
        request_encode_micros: req_enc_bin_us,
        request_decode_micros: req_dec_bin_us,
        reply_bytes: bin_rep.len(),
        reply_encode_micros: rep_enc_bin_us,
        reply_decode_micros: rep_dec_bin_us,
        total_ipc_micros: req_enc_bin_us + req_dec_bin_us + rep_enc_bin_us + rep_dec_bin_us,
        estimated_peak_alloc_bytes: bin_rep.len() + 10,
    };

    let payload_ratio = if !json_rep.is_empty() {
        1.0 - (bin_rep.len() as f64 / json_rep.len() as f64)
    } else {
        0.0
    };
    let speedup = if bin_metrics.total_ipc_micros > 0 {
        json_metrics.total_ipc_micros as f64 / bin_metrics.total_ipc_micros as f64
    } else {
        1.0
    };

    let (symbol_count, call_count, reference_count) = reply.snapshot.as_ref().map_or((0, 0, 0), |s| {
        (s.symbols.len(), s.calls.len(), s.references.len())
    });

    let conclusion = format!(
        "Adopt Bounded Binary (Bincode with magic SWTB and 64 MiB limit): payload reduced by {:.1}% ({:.2} MB -> {:.2} MB), IPC latency improved by {:.2}x ({} us -> {} us).",
        payload_ratio * 100.0,
        json_rep.len() as f64 / (1024.0 * 1024.0),
        bin_rep.len() as f64 / (1024.0 * 1024.0),
        speedup,
        json_metrics.total_ipc_micros,
        bin_metrics.total_ipc_micros,
    );

    Ok(TransportComparisonReport {
        tu_name: tu_name.to_string(),
        symbol_count,
        call_count,
        reference_count,
        json: json_metrics,
        binary: bin_metrics,
        payload_reduction_ratio: payload_ratio,
        speedup_factor: speedup,
        conclusion,
    })
}
