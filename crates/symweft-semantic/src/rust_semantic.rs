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

/// Upper bound on materialized generic instantiations per crate to prevent monomorphization explosion (RS-03).
pub const MAX_RUST_MONOMORPHIZED_INSTANCES_PER_CRATE: usize = 512;

/// Associated item category inside a Rust trait or `impl` block (RS-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustAssociatedItemKind {
    Method,
    AssociatedFunction,
    AssociatedType,
    AssociatedConst,
}

/// Call and method dispatch classification in Rust (RS-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustDispatchKind {
    /// Direct call to a free function or inherent `impl Type` method.
    InherentOrFreeDirect { target_usr: String },
    /// Statically resolved trait method call where receiver concrete type is known (`<T as Trait>::method`).
    StaticTraitImplResolved {
        trait_item_usr: String,
        concrete_impl_item_usr: String,
    },
    /// Call on a generic type parameter (`T: Trait`) bound to the primary trait declaration without unbounded monomorphization.
    GenericTraitBoundPrimary {
        trait_item_usr: String,
        type_param_name: String,
    },
    /// Dynamic dispatch via `&dyn Trait` / `Box<dyn Trait>` with bounded workspace impl candidates.
    DynTraitCandidates {
        trait_item_usr: String,
        candidate_impl_usrs: Vec<String>,
        open_world_note: String,
    },
    /// Indirect call via `fn(...)` pointer or dynamic closure object where target cannot be statically resolved.
    IndirectFnPointerOrClosure { expr_spelling: String },
}

/// Record of a trait or inherent implementation block and its associated items (RS-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustTraitImplRecord {
    pub impl_id: String,
    pub trait_usr: Option<String>,
    pub self_type_usr: String,
    pub is_blanket_impl: bool,
    /// Pairs of `(associated_item_kind, trait_item_usr_opt, impl_item_usr, name)`.
    pub associated_items: Vec<(RustAssociatedItemKind, Option<String>, String, String)>,
}

/// Registry for Rust trait implementations, associated items, dispatch resolution, and monomorphization bounds (RS-03).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RustTraitAndCallResolver {
    pub impls: Vec<RustTraitImplRecord>,
    pub max_monomorphized_instances: usize,
    pub recorded_monomorphized_instances: usize,
    pub collapsed_to_primary_generic_count: usize,
}

impl RustTraitAndCallResolver {
    pub fn new(max_monomorphized_instances: usize) -> Self {
        Self {
            impls: Vec::new(),
            max_monomorphized_instances,
            recorded_monomorphized_instances: 0,
            collapsed_to_primary_generic_count: 0,
        }
    }

    pub fn register_impl(&mut self, record: RustTraitImplRecord) {
        self.impls.push(record);
    }

    /// Jump from a trait USR to all implementing types (`impl Trait for Type`), or from a trait method USR to all concrete impl methods.
    pub fn jump_to_implementations(&self, trait_or_item_usr: &str) -> Vec<String> {
        let mut out = Vec::new();
        for imp in &self.impls {
            if imp.trait_usr.as_deref() == Some(trait_or_item_usr) {
                out.push(imp.self_type_usr.clone());
            }
            for (_, trait_item_opt, impl_item_usr, _) in &imp.associated_items {
                if trait_item_opt.as_deref() == Some(trait_or_item_usr) {
                    out.push(impl_item_usr.clone());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Jump from a concrete `impl` associated item (`Method`, `AssociatedType`, `AssociatedConst`) back to its trait declaration item.
    pub fn jump_to_trait_declaration(&self, impl_item_usr: &str) -> Option<String> {
        for imp in &self.impls {
            for (_, trait_item_opt, candidate_impl_usr, _) in &imp.associated_items {
                if candidate_impl_usr == impl_item_usr {
                    return trait_item_opt.clone();
                }
            }
        }
        None
    }

    /// Resolves a `dyn Trait` method invocation into its primary trait method + bounded workspace candidate impls.
    pub fn resolve_dyn_trait_call(&self, trait_method_usr: &str) -> RustDispatchKind {
        let candidates = self.jump_to_implementations(trait_method_usr);
        RustDispatchKind::DynTraitCandidates {
            trait_item_usr: trait_method_usr.to_string(),
            candidate_impl_usrs: candidates,
            open_world_note:
                "closed over indexed workspace crates; downstream or unindexed crates may add impls"
                    .into(),
        }
    }

    /// Records a generic specialization or collapses to the primary generic definition when budget is reached.
    pub fn record_or_collapse_generic_instance(
        &mut self,
        primary_generic_usr: &str,
        _type_args: &[&str],
    ) -> (String, bool) {
        if self.recorded_monomorphized_instances < self.max_monomorphized_instances {
            self.recorded_monomorphized_instances += 1;
            (primary_generic_usr.to_string(), true)
        } else {
            self.collapsed_to_primary_generic_count += 1;
            (primary_generic_usr.to_string(), false)
        }
    }

    /// Converts a `RustDispatchKind` into shared A2 `CallSite` records for `Store::publish`.
    pub fn to_shared_call_sites(
        dispatch: &RustDispatchKind,
        caller_usr: Option<String>,
        callee_name: &str,
        location: crate::model::Location,
    ) -> Vec<crate::model::CallSite> {
        match dispatch {
            RustDispatchKind::InherentOrFreeDirect { target_usr } => {
                vec![crate::model::CallSite {
                    caller: caller_usr,
                    target: Some(target_usr.clone()),
                    name: callee_name.to_string(),
                    dispatch: "direct".into(),
                    location,
                }]
            }
            RustDispatchKind::StaticTraitImplResolved {
                concrete_impl_item_usr,
                ..
            } => {
                vec![crate::model::CallSite {
                    caller: caller_usr,
                    target: Some(concrete_impl_item_usr.clone()),
                    name: callee_name.to_string(),
                    dispatch: "direct".into(),
                    location,
                }]
            }
            RustDispatchKind::GenericTraitBoundPrimary { trait_item_usr, .. } => {
                vec![crate::model::CallSite {
                    caller: caller_usr,
                    target: Some(trait_item_usr.clone()),
                    name: callee_name.to_string(),
                    dispatch: "direct".into(),
                    location,
                }]
            }
            RustDispatchKind::DynTraitCandidates {
                trait_item_usr,
                candidate_impl_usrs,
                ..
            } => {
                let mut sites = vec![crate::model::CallSite {
                    caller: caller_usr.clone(),
                    target: Some(trait_item_usr.clone()),
                    name: callee_name.to_string(),
                    dispatch: "virtual_candidate".into(),
                    location: location.clone(),
                }];
                for cand in candidate_impl_usrs {
                    sites.push(crate::model::CallSite {
                        caller: caller_usr.clone(),
                        target: Some(cand.clone()),
                        name: callee_name.to_string(),
                        dispatch: "virtual_candidate".into(),
                        location: location.clone(),
                    });
                }
                sites
            }
            RustDispatchKind::IndirectFnPointerOrClosure { .. } => {
                vec![crate::model::CallSite {
                    caller: caller_usr,
                    target: None,
                    name: callee_name.to_string(),
                    dispatch: "indirect".into(),
                    location,
                }]
            }
        }
    }
}

/// Classification of a Rust macro invocation (RS-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustMacroKind {
    DeclarativeMacroRules,
    DeriveProcMacro,
    AttributeProcMacro,
    FunctionLikeProcMacro,
}

/// Dual source anchor for a symbol or call generated inside a Rust macro expansion (RS-04).
/// Preserves both the user-written macro invocation site and the macro definition/expansion span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustMacroDualAnchor {
    pub macro_usr: String,
    pub macro_name: String,
    pub kind: RustMacroKind,
    pub invocation_site: crate::model::Location,
    pub definition_or_expansion_site: crate::model::StageLocation,
}

impl RustMacroDualAnchor {
    /// Builds a shared `crate::model::Location` whose primary coordinates point to the user invocation
    /// site and whose `expansion` field points to the macro definition/expansion span.
    pub fn to_shared_location(&self) -> crate::model::Location {
        let mut loc = self.invocation_site.clone();
        loc.expansion = Some(self.definition_or_expansion_site.clone());
        loc
    }
}

/// Failure record for a crate whose unsaved overlay analysis failed (RS-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedRustCrateOverlay {
    pub crate_id: String,
    pub reason: String,
    pub old_disk_facts_masked: bool,
}

/// Coordinator for multi-document unsaved Rust analysis, macro invalidation, and failure masking (RS-04).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustUnsavedOverlayCoordinator {
    pub variant_fingerprint: String,
    pub execution_policy: BuildScriptAndProcMacroPolicy,
    /// Map of `file_path -> (revision, content_hash)`.
    pub dirty_documents: BTreeMap<String, (u64, String)>,
    /// Map of `crate_id -> reverse dependent crate_ids`.
    pub reverse_crate_deps: BTreeMap<String, BTreeSet<String>>,
    /// Map of `crate_id -> active valid Snapshot`.
    pub active_crate_snapshots: BTreeMap<String, crate::model::Snapshot>,
    /// Map of `crate_id -> FailedRustCrateOverlay` when analysis fails (masks old facts!).
    pub failed_crates: BTreeMap<String, FailedRustCrateOverlay>,
    pub stale_crates: BTreeSet<String>,
}

impl RustUnsavedOverlayCoordinator {
    pub fn new(
        variant_fingerprint: &str,
        execution_policy: BuildScriptAndProcMacroPolicy,
    ) -> Self {
        Self {
            variant_fingerprint: variant_fingerprint.to_string(),
            execution_policy,
            dirty_documents: BTreeMap::new(),
            reverse_crate_deps: BTreeMap::new(),
            active_crate_snapshots: BTreeMap::new(),
            failed_crates: BTreeMap::new(),
            stale_crates: BTreeSet::new(),
        }
    }

    pub fn register_crate_dependency(&mut self, dependent_crate: &str, dependency_crate: &str) {
        self.reverse_crate_deps
            .entry(dependency_crate.to_string())
            .or_default()
            .insert(dependent_crate.to_string());
    }

    /// Applies multiple unsaved dirty buffers simultaneously and invalidates the owning crates
    /// plus their downstream dependent crates.
    pub fn apply_dirty_documents(
        &mut self,
        docs: &[(String, u64, String, String)], // (file_path, revision, hash, owning_crate_id)
    ) -> Vec<String> {
        let mut invalidated = BTreeSet::new();
        for (path, rev, hash, crate_id) in docs {
            self.dirty_documents
                .insert(path.clone(), (*rev, hash.clone()));
            invalidated.insert(crate_id.clone());
            if let Some(dependents) = self.reverse_crate_deps.get(crate_id) {
                for dep in dependents {
                    invalidated.insert(dep.clone());
                }
            }
        }
        for cid in &invalidated {
            self.stale_crates.insert(cid.clone());
            // Mask old snapshot immediately so stale facts are never served while dirty
            self.active_crate_snapshots.remove(cid);
        }
        invalidated.into_iter().collect()
    }

    /// Invalidates all cached crate snapshots when Cargo features/target or `proc-macro` policy changes.
    pub fn on_config_or_policy_change(
        &mut self,
        new_variant_fingerprint: &str,
        new_policy: BuildScriptAndProcMacroPolicy,
    ) -> bool {
        if self.variant_fingerprint == new_variant_fingerprint
            && self.execution_policy == new_policy
        {
            return false;
        }
        for cid in self.active_crate_snapshots.keys() {
            self.stale_crates.insert(cid.clone());
        }
        self.active_crate_snapshots.clear();
        self.failed_crates.clear();
        self.variant_fingerprint = new_variant_fingerprint.to_string();
        self.execution_policy = new_policy;
        true
    }

    /// Publishes a valid crate snapshot for the current variant.
    pub fn publish_crate_snapshot(&mut self, crate_id: &str, snapshot: crate::model::Snapshot) {
        self.stale_crates.remove(crate_id);
        self.failed_crates.remove(crate_id);
        self.active_crate_snapshots
            .insert(crate_id.to_string(), snapshot);
    }

    /// Records a crate analysis/macro-expansion failure without falling back to stale disk facts
    /// or fabricating pseudo-accurate bindings.
    pub fn record_crate_failure(&mut self, crate_id: &str, reason: &str) {
        self.active_crate_snapshots.remove(crate_id);
        self.stale_crates.insert(crate_id.to_string());
        self.failed_crates.insert(
            crate_id.to_string(),
            FailedRustCrateOverlay {
                crate_id: crate_id.to_string(),
                reason: reason.to_string(),
                old_disk_facts_masked: true,
            },
        );
    }
}

/// Control-flow basic block category in a Rust source CFG (RS-05).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustCfgNodeKind {
    Entry,
    BasicBlock,
    MatchScrutinee,
    MatchArm,
    LoopHeader,
    TryQuestionOperator,
    EarlyReturn,
    ClosureCreationSite,
    AsyncAwaitSuspendPoint,
    NormalExit,
}

/// Directed control-flow edge category in a Rust source CFG (RS-05).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustCfgEdgeKind {
    Sequential,
    MatchArmBranch { pattern_label: String },
    LoopBody,
    LoopBackEdge,
    LoopBreakOrExit,
    TryOkContinue,
    TryErrEarlyReturn,
    ReturnJump,
    AwaitSuspendResume,
}

/// Basic block node with source location anchor in a Rust CFG (RS-05).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustCfgBlock {
    pub block_id: usize,
    pub kind: RustCfgNodeKind,
    pub label: String,
    pub location: crate::model::Location,
}

/// Directed edge between two basic blocks in a Rust CFG (RS-05).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustCfgEdge {
    pub from_block: usize,
    pub to_block: usize,
    pub kind: RustCfgEdgeKind,
}

/// Source-level Control Flow Graph for a Rust function, method, or closure body (RS-05).
/// Models surface control-flow constructs (`match`, loops, `?`, `return`, closure boundaries, `.await`)
/// and explicitly never claims full rustc MIR, implicit `Drop` glue, or panic unwind semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustFunctionCfg {
    pub function_usr: String,
    pub function_name: String,
    pub file_path: String,
    pub is_async_fn: bool,
    pub has_try_question_operator: bool,
    /// Separate CFG identifiers for nested closures created inside this function.
    pub nested_closure_cfgs: Vec<String>,
    /// Hard honesty flag: source CFG never claims full MIR / implicit Drop / panic unwind coverage.
    pub claims_mir_drop_or_unwind_semantics: bool,
    pub blocks: Vec<RustCfgBlock>,
    pub edges: Vec<RustCfgEdge>,
}

impl RustFunctionCfg {
    pub fn new(
        function_usr: &str,
        function_name: &str,
        file_path: &str,
        start_line: u32,
        start_byte: u32,
        end_byte: u32,
    ) -> Self {
        let entry = RustCfgBlock {
            block_id: 0,
            kind: RustCfgNodeKind::Entry,
            label: format!("entry:{function_name}"),
            location: crate::model::Location {
                path: file_path.to_string(),
                line: start_line,
                column: 1,
                start_byte,
                end_byte,
                expansion: None,
            },
        };
        Self {
            function_usr: function_usr.to_string(),
            function_name: function_name.to_string(),
            file_path: file_path.to_string(),
            is_async_fn: false,
            has_try_question_operator: false,
            nested_closure_cfgs: Vec::new(),
            claims_mir_drop_or_unwind_semantics: false,
            blocks: vec![entry],
            edges: Vec::new(),
        }
    }

    pub fn add_block(
        &mut self,
        kind: RustCfgNodeKind,
        label: &str,
        line: u32,
        column: u32,
        start_byte: u32,
        end_byte: u32,
    ) -> usize {
        if kind == RustCfgNodeKind::AsyncAwaitSuspendPoint {
            self.is_async_fn = true;
        }
        if kind == RustCfgNodeKind::TryQuestionOperator {
            self.has_try_question_operator = true;
        }
        let id = self.blocks.len();
        self.blocks.push(RustCfgBlock {
            block_id: id,
            kind,
            label: label.to_string(),
            location: crate::model::Location {
                path: self.file_path.clone(),
                line,
                column,
                start_byte,
                end_byte,
                expansion: None,
            },
        });
        id
    }

    pub fn add_edge(&mut self, from_block: usize, to_block: usize, kind: RustCfgEdgeKind) {
        self.edges.push(RustCfgEdge {
            from_block,
            to_block,
            kind,
        });
    }

    pub fn register_nested_closure_cfg(&mut self, closure_usr: &str) {
        self.nested_closure_cfgs.push(closure_usr.to_string());
    }

    /// Validates source spans and ensures the CFG does not claim MIR/Drop/unwind completeness.
    pub fn validate_rust_cfg_contract(&self) -> std::result::Result<(), String> {
        if self.claims_mir_drop_or_unwind_semantics {
            return Err(
                "RS-05 honesty violation: Rust source CFG must not claim full MIR, Drop, or unwind semantics"
                    .into(),
            );
        }
        for b in &self.blocks {
            if b.location.path.is_empty() || b.location.end_byte < b.location.start_byte {
                return Err(format!("invalid source span on Rust CFG block {}", b.block_id));
            }
        }
        for e in &self.edges {
            if e.from_block >= self.blocks.len() || e.to_block >= self.blocks.len() {
                return Err(format!("dangling Rust CFG edge {} -> {}", e.from_block, e.to_block));
            }
        }
        Ok(())
    }
}
