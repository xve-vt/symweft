use crate::model::{Call, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalEntry {
    pub start_offset: usize,
    pub end_offset: usize,
    pub entity_id: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdjacencyIndex {
    pub outgoing: HashMap<String, Vec<usize>>,
    pub incoming: HashMap<String, Vec<usize>>,
    pub references: HashMap<String, Vec<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTuQueryIndex {
    pub unit_id: String,
    pub intervals: Vec<IntervalEntry>,
    pub adjacency: AdjacencyIndex,
    pub memory_bytes: usize,
}

impl ActiveTuQueryIndex {
    pub fn build(snapshot: &Snapshot) -> Self {
        let mut intervals = Vec::with_capacity(snapshot.symbols.len() + snapshot.references.len());
        for sym in &snapshot.symbols {
            intervals.push(IntervalEntry {
                start_offset: sym.location.offset as usize,
                end_offset: sym.location.end_offset as usize,
                entity_id: sym.id.clone(),
                kind: "symbol".to_string(),
            });
        }
        for r in &snapshot.references {
            intervals.push(IntervalEntry {
                start_offset: r.location.offset as usize,
                end_offset: r.location.end_offset as usize,
                entity_id: r.target.clone(),
                kind: "reference".to_string(),
            });
        }
        intervals.sort_by_key(|e| (e.start_offset, e.end_offset));

        let mut outgoing = HashMap::new();
        let mut incoming = HashMap::new();
        for (idx, call) in snapshot.calls.iter().enumerate() {
            if let Some(caller) = &call.caller {
                outgoing.entry(caller.clone()).or_insert_with(Vec::new).push(idx);
            }
            if let Some(target) = &call.target {
                incoming.entry(target.clone()).or_insert_with(Vec::new).push(idx);
            }
        }

        let mut references = HashMap::new();
        for (idx, r) in snapshot.references.iter().enumerate() {
            references.entry(r.target.clone()).or_insert_with(Vec::new).push(idx);
        }

        let intervals_mem = intervals.len() * std::mem::size_of::<IntervalEntry>();
        let adj_mem = (outgoing.len() + incoming.len() + references.len()) * 64 + snapshot.calls.len() * 8;
        let memory_bytes = intervals_mem + adj_mem + 256;

        Self {
            unit_id: snapshot.unit_id.clone(),
            intervals,
            adjacency: AdjacencyIndex {
                outgoing,
                incoming,
                references,
            },
            memory_bytes,
        }
    }

    pub fn find_entity_at_offset(&self, byte_offset: usize) -> Option<&str> {
        let idx = match self.intervals.binary_search_by_key(&byte_offset, |e| e.start_offset) {
            Ok(i) => i,
            Err(i) => {
                if i > 0 {
                    i - 1
                } else {
                    return None;
                }
            }
        };
        for entry in self.intervals[..=idx].iter().rev() {
            if entry.start_offset <= byte_offset && byte_offset < entry.end_offset {
                return Some(&entry.entity_id);
            }
            if entry.end_offset < byte_offset && byte_offset - entry.end_offset > 2048 {
                break;
            }
        }
        None
    }

    pub fn get_outgoing_calls<'a>(&'a self, caller: &str, snapshot: &'a Snapshot) -> Vec<&'a Call> {
        self.adjacency
            .outgoing
            .get(caller)
            .map(|indices| indices.iter().filter_map(|&i| snapshot.calls.get(i)).collect())
            .unwrap_or_default()
    }

    pub fn get_incoming_calls<'a>(&'a self, target: &str, snapshot: &'a Snapshot) -> Vec<&'a Call> {
        self.adjacency
            .incoming
            .get(target)
            .map(|indices| indices.iter().filter_map(|&i| snapshot.calls.get(i)).collect())
            .unwrap_or_default()
    }
}
