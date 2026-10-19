use serde::{Deserialize, Serialize};

pub const QUERY_VERSION_CONTRACT: &str = "a2-query-v1";
pub const MAX_OVERLAY_DOCUMENTS: usize = 16;
pub const MAX_OVERLAY_SOURCE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct PreviewDocument {
    pub path: String,
    pub revision: u64,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryVersion {
    pub contract: String,
    pub workspace: String,
    pub configuration: QueryConfiguration,
    pub disk: DiskBaseline,
    pub documents: Vec<DocumentRevision>,
    pub coverage: QueryCoverage,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryConfiguration {
    pub id: String,
    pub content_key: String,
    pub provider: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiskBaseline {
    pub schema: String,
    pub store_id: String,
    pub revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DocumentRevision {
    pub path: String,
    pub revision: u64,
    pub content_key: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryCoverage {
    PersistentIndex,
    OverlayTranslationUnit { unit_id: String },
    OverlayWithPersistentIndex { unit_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct IncluderCoverage {
    pub scope: String,
    pub known_includers: Vec<String>,
    pub updated_includers: Vec<String>,
    pub stale_includers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    pub offset: u32,
    pub end_offset: u32,
    pub line: u32,
    pub column: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub usr: String,
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub definition: bool,
    pub callable: bool,
    pub location: Location,
    pub extent: Location,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub target: String,
    pub name: String,
    pub kind: String,
    pub location: Location,
    pub spelling: Option<Location>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub caller: Option<String>,
    pub target: Option<String>,
    pub name: String,
    /// direct, virtual_candidate, indirect, unresolved. Never a name guess.
    pub resolution: String,
    pub location: Location,
    pub spelling: Option<Location>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub path: String,
    pub hash: [u8; 32],
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: i32,
    pub message: String,
    pub location: Option<Location>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub unit_id: String,
    pub configuration: String,
    pub main_file: String,
    pub command_key: String,
    pub provider: String,
    pub inputs: Vec<Input>,
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
    pub calls: Vec<Call>,
    pub diagnostics: Vec<Diagnostic>,
    pub parse_micros: u64,
    pub extract_micros: u64,
    pub clang_accounted_bytes: u64,
    pub worker_peak_rss_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overlay {
    pub path: String,
    pub content: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub command: crate::compdb::CompileCommand,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerReply {
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<crate::profile::RunProfile>,
}
