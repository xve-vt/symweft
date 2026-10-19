//! Rust accurate semantic analysis, Cargo workspace variants, SCIP/rust-analyzer fact adaptation,
//! trait/generic dispatch, macro expansion dual anchors, and source CFG (RS-01..RS-06).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Rust language edition for a crate (RS-01).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustEdition {
    Edition2015,
    Edition2018,
    Edition2021,
    Edition2024,
}

/// Target kind of a Cargo crate compilation unit (RS-01).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustCrateTargetKind {
    Lib,
    Bin,
    ProcMacro,
    Test,
    Bench,
    CustomBuild,
}

/// Execution policy for `build.rs` build scripts and `proc-macro` dynamic libraries (RS-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildScriptAndProcMacroPolicy {
    /// Default safe mode: never executes `build.rs` or `proc-macro` code; explicitly reports resulting gaps.
    DisabledStaticOnly,
    /// Trusted workspace mode: allows sandboxed execution of `build.rs` (`OUT_DIR`) and `proc-macro` expansion.
    TrustedSandboxedExecution,
}

/// Explicit report of semantic gaps when `build.rs` or `proc-macro` execution is disabled (RS-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisabledExecutionGapReport {
    pub policy: BuildScriptAndProcMacroPolicy,
    pub executes_build_scripts_or_proc_macros: bool,
    pub crates_with_skipped_build_scripts: Vec<String>,
    pub crates_with_skipped_proc_macros: Vec<String>,
    pub unexpanded_derive_and_attr_macros: Vec<String>,
    pub missing_out_dir_includes: Vec<String>,
    pubcompleteness_note: String,
}

/// A crate node in the Cargo workspace crate graph (RS-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustCrateNode {
    pub crate_id: String,
    pub name: String,
    pub version: String,
    pub edition: RustEdition,
    pub target_kind: RustCrateTargetKind,
    pub root_file: String,
    pub enabled_features: BTreeSet<String>,
    pub cfg_flags: BTreeSet<String>,
    /// Directed dependency edges `(extern_crate_alias, target_crate_id)`.
    pub dependencies: Vec<(String, String)>,
    pub has_build_script: bool,
}

/// Traceable Cargo workspace and build variant identity (RS-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoWorkspaceBuildVariant {
    pub workspace_root: String,
    pub toolchain_version: String,
    pub target_triple: String,
    pub cargo_lock_sha256: String,
    pub execution_policy: BuildScriptAndProcMacroPolicy,
    pub crates: BTreeMap<String, RustCrateNode>,
}

impl CargoWorkspaceBuildVariant {
    /// Computes a deterministic SHA-256 identity covering the crate graph, features, target triple,
    /// edition, toolchain version, Cargo.lock digest, and `build.rs`/`proc-macro` execution policy.
    pub fn variant_fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"rs-workspace-v1:");
        hasher.update(self.workspace_root.as_bytes());
        hasher.update(b"|");
        hasher.update(self.toolchain_version.as_bytes());
        hasher.update(b"|");
        hasher.update(self.target_triple.as_bytes());
        hasher.update(b"|");
        hasher.update(self.cargo_lock_sha256.as_bytes());
        hasher.update(b"|");
        hasher.update(format!("{:?}", self.execution_policy).as_bytes());
        hasher.update(b"|");
        for (cid, node) in &self.crates {
            hasher.update(cid.as_bytes());
            hasher.update(b":");
            hasher.update(node.name.as_bytes());
            hasher.update(b"@");
            hasher.update(node.version.as_bytes());
            hasher.update(format!(":{:?}:{:?}:", node.edition, node.target_kind).as_bytes());
            hasher.update(node.root_file.as_bytes());
            hasher.update(b":features=");
            for feat in &node.enabled_features {
                hasher.update(feat.as_bytes());
                hasher.update(b",");
            }
            hasher.update(b":cfg=");
            for cfg in &node.cfg_flags {
                hasher.update(cfg.as_bytes());
                hasher.update(b",");
            }
            hasher.update(b":deps=");
            for (alias, dep_id) in &node.dependencies {
                hasher.update(alias.as_bytes());
                hasher.update(b"->");
                hasher.update(dep_id.as_bytes());
                hasher.update(b",");
            }
            hasher.update(if node.has_build_script { b":build=1;" } else { b":build=0;" });
        }
        format!("{:x}", hasher.finalize())
    }

    /// Evaluates the `build.rs` and `proc-macro` execution policy and reports any resulting semantic gaps.
    pub fn evaluate_execution_policy_gaps(
        &self,
        observed_derive_macros: &[&str],
        observed_out_dir_includes: &[&str],
    ) -> DisabledExecutionGapReport {
        match self.execution_policy {
            BuildScriptAndProcMacroPolicy::DisabledStaticOnly => {
                let skipped_build: Vec<String> = self
                    .crates
                    .values()
                    .filter(|c| c.has_build_script)
                    .map(|c| c.crate_id.clone())
                    .collect();
                let skipped_pm: Vec<String> = self
                    .crates
                    .values()
                    .filter(|c| c.target_kind == RustCrateTargetKind::ProcMacro)
                    .map(|c| c.crate_id.clone())
                    .collect();
                let unexpanded: Vec<String> = observed_derive_macros
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect();
                let missing_out: Vec<String> = observed_out_dir_includes
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect();
                DisabledExecutionGapReport {
                    policy: BuildScriptAndProcMacroPolicy::DisabledStaticOnly,
                    executes_build_scripts_or_proc_macros: false,
                    crates_with_skipped_build_scripts: skipped_build.clone(),
                    crates_with_skipped_proc_macros: skipped_pm.clone(),
                    unexpanded_derive_and_attr_macros: unexpanded.clone(),
                    missing_out_dir_includes: missing_out.clone(),
                    pubcompleteness_note: format!(
                        "build.rs and proc-macro execution disabled (static safe mode): {} build script(s), {} proc-macro crate(s), {} derive/attr macro invocation(s), and {} OUT_DIR include(s) are unexpanded",
                        skipped_build.len(),
                        skipped_pm.len(),
                        unexpanded.len(),
                        missing_out.len()
                    ),
                }
            }
            BuildScriptAndProcMacroPolicy::TrustedSandboxedExecution => DisabledExecutionGapReport {
                policy: BuildScriptAndProcMacroPolicy::TrustedSandboxedExecution,
                executes_build_scripts_or_proc_macros: true,
                crates_with_skipped_build_scripts: Vec::new(),
                crates_with_skipped_proc_macros: Vec::new(),
                unexpanded_derive_and_attr_macros: Vec::new(),
                missing_out_dir_includes: Vec::new(),
                pubcompleteness_note:
                    "build.rs and proc-macro execution enabled under trusted sandbox policy".into(),
            },
        }
    }
}

/// Provider key for Rust A2 semantic snapshots in the shared store (RS-02..RS-06).
pub const RUST_PROVIDER_KEY: &str = "a2-rust-scip-v1";

/// Maintainable Rust semantic adapter route (RS-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustSemanticAdapterRoute {
    /// Frozen production route: rust-analyzer SCIP / HIR occurrence & call adapter.
    RustAnalyzerScipAdapter,
}

/// Role of an imported Rust symbol occurrence (RS-02).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustOccurrenceRole {
    Definition,
    Declaration,
    Reference,
    TypeReference,
    WriteAccess,
}

/// Raw symbol occurrence imported from rust-analyzer / SCIP index (RS-02).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RustScipOccurrence {
    pub scip_symbol: String,
    pub display_name: String,
    pub symbol_kind: String,
    pub role: RustOccurrenceRole,
    pub file_path: String,
    pub line: u32,
    pub column: u32,
    pub start_byte: u32,
    pub end_byte: u32,
}

/// Deduplicated, variant-bound Rust symbol & occurrence importer (RS-02).
/// Explicitly marks occurrence-only imports as `claims_complete_call_graph = false` so raw
/// reference occurrences are never misrepresented as a complete caller->callee call graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustOccurrenceImportBatch {
    pub adapter_route: RustSemanticAdapterRoute,
    pub crate_id: String,
    pub variant_fingerprint: String,
    pub raw_occurrence_count: usize,
    pub deduplicated_occurrence_count: usize,
    /// Hard honesty flag: symbol/occurrence index import alone is NOT a complete call graph.
    pub claims_complete_call_graph: bool,
    pub symbols: Vec<crate::model::Symbol>,
    pub references: Vec<crate::model::Reference>,
}

impl RustOccurrenceImportBatch {
    /// Normalizes a SCIP descriptor into a canonical Symweft Rust USR (`rs-usr:<scip>`).
    pub fn canonical_rust_usr(scip_symbol: &str) -> String {
        if scip_symbol.starts_with("rs-usr:") {
            scip_symbol.to_string()
        } else {
            format!("rs-usr:{}", scip_symbol.trim())
        }
    }

    /// Imports and deduplicates raw SCIP occurrences for a crate target and build variant.
    pub fn import_and_deduplicate(
        crate_id: &str,
        variant_fingerprint: &str,
        raw_occurrences: Vec<RustScipOccurrence>,
    ) -> Self {
        let raw_occurrence_count = raw_occurrences.len();
        // Deduplicate identical occurrences emitted across overlapping lib/test/bin targets
        let mut unique_set: BTreeSet<RustScipOccurrence> = BTreeSet::new();
        for occ in raw_occurrences {
            unique_set.insert(occ);
        }
        let deduplicated_occurrence_count = unique_set.len();

        let mut symbols = Vec::new();
        let mut references = Vec::new();

        for occ in unique_set {
            let usr = Self::canonical_rust_usr(&occ.scip_symbol);
            let loc = crate::model::Location {
                path: occ.file_path.clone(),
                line: occ.line,
                column: occ.column,
                start_byte: occ.start_byte,
                end_byte: occ.end_byte,
                expansion: None,
            };
            match occ.role {
                RustOccurrenceRole::Definition | RustOccurrenceRole::Declaration => {
                    symbols.push(crate::model::Symbol {
                        id: usr,
                        name: occ.display_name,
                        kind: occ.symbol_kind,
                        definition: occ.role == RustOccurrenceRole::Definition,
                        location: loc,
                    });
                }
                RustOccurrenceRole::Reference
                | RustOccurrenceRole::TypeReference
                | RustOccurrenceRole::WriteAccess => {
                    references.push(crate::model::Reference {
                        target: usr,
                        name: occ.display_name,
                        kind: match occ.role {
                            RustOccurrenceRole::TypeReference => "type_ref".into(),
                            RustOccurrenceRole::WriteAccess => "write_ref".into(),
                            _ => "ref".into(),
                        },
                        location: loc,
                    });
                }
            }
        }

        Self {
            adapter_route: RustSemanticAdapterRoute::RustAnalyzerScipAdapter,
            crate_id: crate_id.to_string(),
            variant_fingerprint: variant_fingerprint.to_string(),
            raw_occurrence_count,
            deduplicated_occurrence_count,
            claims_complete_call_graph: false,
            symbols,
            references,
        }
    }

    /// Validates that occurrence imports are never misrepresented as a full call graph.
    pub fn validate_import_honesty(&self) -> std::result::Result<(), String> {
        if self.claims_complete_call_graph {
            return Err(
                "RS-02 honesty violation: SCIP occurrence import alone must not be claimed as a complete call graph"
                    .into(),
            );
        }
        Ok(())
    }

    /// Converts the occurrence import batch into a shared A2 `Snapshot` (with empty `calls` until RS-03 call resolution).
    pub fn into_occurrence_snapshot(
        self,
        unit_id: &str,
        configuration: &str,
        main_file: &str,
        inputs: Vec<crate::model::InputFile>,
    ) -> crate::model::Snapshot {
        crate::model::Snapshot {
            unit_id: unit_id.to_string(),
            configuration: configuration.to_string(),
            main_file: main_file.to_string(),
            compile_hash: self.variant_fingerprint,
            provider: RUST_PROVIDER_KEY.to_string(),
            complete: true,
            diagnostics: Vec::new(),
            inputs,
            symbols: self.symbols,
            references: self.references,
            calls: Vec::new(),
        }
    }
}
