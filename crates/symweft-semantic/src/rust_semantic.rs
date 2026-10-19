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
