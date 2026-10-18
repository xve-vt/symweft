//! A1 snapshot persistence and bounded caches. No name-based semantic call edges.
pub mod cache;
pub mod hybrid;
pub mod pack;
pub mod store;
pub mod worker;

use serde::{Deserialize, Serialize};
use std::{error::Error, sync::Arc};
use symweft_analysis::Facts;

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
pub const ANALYZER_KEY: &str =
    "a1-v2/ts-0.25.10/py-0.25.0/c-0.24.1/cpp-0.23.4/rs-0.24.0/java-0.23.5/bincode-2";
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub path: String,
    pub source_hash: [u8; 32],
    pub checksum: [u8; 32],
    pub source_bytes: u64,
    pub payload: Vec<u8>,
    pub dependencies: Vec<String>,
}
impl Snapshot {
    pub fn new(path: &str, source: &str, facts: &Facts) -> Result<Self> {
        let mut payload = b"SWF1".to_vec();
        payload.extend(bincode::serde::encode_to_vec(
            facts,
            bincode::config::standard(),
        )?);
        if payload.len() > MAX_PAYLOAD {
            return Err("fact payload limit".into());
        }
        Ok(Self {
            path: path.into(),
            source_hash: hash(source.as_bytes()),
            checksum: hash(&payload),
            source_bytes: source.len() as u64,
            payload,
            dependencies: Vec::new(),
        })
    }
    pub fn decode(&self) -> Result<Arc<Facts>> {
        if self.payload.len() > MAX_PAYLOAD
            || !self.payload.starts_with(b"SWF1")
            || hash(&self.payload) != self.checksum
        {
            return Err("invalid snapshot checksum/format/length".into());
        }
        let (facts, used): (Facts, usize) = bincode::serde::decode_from_slice(
            &self.payload[4..],
            bincode::config::standard().with_limit::<MAX_PAYLOAD>(),
        )?;
        if used + 4 != self.payload.len() {
            return Err("trailing snapshot bytes".into());
        }
        Ok(Arc::new(facts))
    }
}
pub fn hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

pub fn symbol_key(path: &str, facts: &Facts, id: u32) -> String {
    let mut parts = Vec::new();
    let mut current = Some(id);
    let mut depth = 0;
    while let Some(id) = current {
        let Some(s) = facts.symbols.get(id as usize) else {
            break;
        };
        let ordinal = facts.symbols[..id as usize]
            .iter()
            .filter(|p| p.name == s.name && p.kind == s.kind && p.parent_id == s.parent_id)
            .count();
        parts.push((s.kind.as_str(), s.name.as_str(), ordinal));
        current = s.parent_id;
        depth += 1;
        if depth > 128 {
            break;
        }
    }
    parts.reverse();
    let mut key = Vec::new();
    key.extend((path.len() as u64).to_le_bytes());
    key.extend(path.as_bytes());
    for (kind, name, ordinal) in parts {
        for field in [kind, name] {
            key.extend((field.len() as u64).to_le_bytes());
            key.extend(field.as_bytes());
        }
        key.extend((ordinal as u64).to_le_bytes());
    }
    blake3::hash(&key).to_hex().to_string()
}

pub fn simple_name(callee: &str) -> Option<&str> {
    let mut chars = callee.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_alphabetic()) || !chars.all(|c| c == '_' || c.is_alphanumeric()) {
        return None;
    }
    Some(callee)
}
