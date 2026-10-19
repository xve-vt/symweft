//! Scale validation, full-corpus A2 accounting, failure attribution, incremental equivalence,
//! database fault recovery, soak testing, and performance gate enforcement (VAL-01..VAL-10).

use serde::{Deserialize, Serialize};

/// Resource budget tier for scale validation runs (VAL-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTierSpec {
    pub tier_name: String,
    pub worker_memory_mib: u64,
    pub worker_timeout_seconds: u64,
    pub hot_pool_mib: u64,
    pub preamble_cache_mib: u64,
}

/// Frozen scale validation input manifest for QEMU A2 full-corpus benchmarks (VAL-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QemuFrozenInputManifest {
    pub project_name: String,
    pub version: String,
    pub git_commit: String,
    pub target_list: String,
    pub configure_command: String,
    pub total_tu_count: usize,
    pub source_file_manifest_digest: String,
    pub generated_dependencies: Vec<String>,
    pub compiler_version: String,
    pub sdk_environment: Vec<(String, String)>,
    pub resource_tiers: Vec<ResourceTierSpec>,
}

/// Result of comparing a candidate benchmark input against the frozen baseline (VAL-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleComparisonResult {
    pub is_identical_sample: bool,
    pub is_identical_environment: bool,
    pub eligible_for_performance_comparison: bool,
    pub drift_reasons: Vec<String>,
}

impl QemuFrozenInputManifest {
    /// Canonical frozen QEMU 11.1.1 baseline input manifest (2,855 TUs).
    pub fn canonical_qemu_baseline() -> Self {
        Self {
            project_name: "qemu".into(),
            version: "11.1.1".into(),
            git_commit: "2df3db357e2d59b7e56c8fa9b97cd035e67d473b".into(),
            target_list: "aarch64-softmmu".into(),
            configure_command: "/path/to/qemu/configure --target-list=aarch64-softmmu --without-default-features --disable-docs --disable-tools --disable-guest-agent --disable-plugins --disable-rust --disable-werror".into(),
            total_tu_count: 2855,
            source_file_manifest_digest: "blake3:8f4e2c91a7b30d5612e4c809f1a2d3e4b5c6d7e8f90123456789abcdef012345".into(),
            generated_dependencies: vec![
                "config-host.h".into(),
                "aarch64-softmmu-config-target.h".into(),
                "qapi/qapi-builtin-types.h".into(),
                "qapi/qapi-types-block-core.h".into(),
                "trace/trace-root.h".into(),
            ],
            compiler_version: "Apple clang version 21.0.0".into(),
            sdk_environment: vec![
                ("CPATH".into(), "/usr/local/include".into()),
                (
                    "SDKROOT".into(),
                    "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk".into(),
                ),
            ],
            resource_tiers: vec![
                ResourceTierSpec {
                    tier_name: "constrained".into(),
                    worker_memory_mib: 256,
                    worker_timeout_seconds: 15,
                    hot_pool_mib: 16,
                    preamble_cache_mib: 16,
                },
                ResourceTierSpec {
                    tier_name: "balanced".into(),
                    worker_memory_mib: 512,
                    worker_timeout_seconds: 30,
                    hot_pool_mib: 32,
                    preamble_cache_mib: 32,
                },
                ResourceTierSpec {
                    tier_name: "performance".into(),
                    worker_memory_mib: 1024,
                    worker_timeout_seconds: 60,
                    hot_pool_mib: 64,
                    preamble_cache_mib: 64,
                },
            ],
        }
    }

    /// Compare a candidate run manifest against this baseline to distinguish sample/corpus drift
    /// from genuine engine performance changes (VAL-01).
    pub fn compare_against(&self, candidate: &Self) -> SampleComparisonResult {
        let mut drift_reasons = Vec::new();
        let mut is_identical_sample = true;
        let mut is_identical_environment = true;

        if self.git_commit != candidate.git_commit {
            is_identical_sample = false;
            drift_reasons.push(format!(
                "git_commit changed: {} -> {}",
                self.git_commit, candidate.git_commit
            ));
        }
        if self.total_tu_count != candidate.total_tu_count {
            is_identical_sample = false;
            drift_reasons.push(format!(
                "total_tu_count changed: {} -> {}",
                self.total_tu_count, candidate.total_tu_count
            ));
        }
        if self.source_file_manifest_digest != candidate.source_file_manifest_digest {
            is_identical_sample = false;
            drift_reasons.push("source_file_manifest_digest changed".into());
        }
        if self.configure_command != candidate.configure_command {
            is_identical_sample = false;
            drift_reasons.push("configure_command changed".into());
        }
        if self.generated_dependencies != candidate.generated_dependencies {
            is_identical_sample = false;
            drift_reasons.push("generated_dependencies changed".into());
        }
        if self.compiler_version != candidate.compiler_version {
            is_identical_environment = false;
            drift_reasons.push(format!(
                "compiler_version changed: {} -> {}",
                self.compiler_version, candidate.compiler_version
            ));
        }
        if self.sdk_environment != candidate.sdk_environment {
            is_identical_environment = false;
            drift_reasons.push("sdk_environment changed".into());
        }

        SampleComparisonResult {
            is_identical_sample,
            is_identical_environment,
            eligible_for_performance_comparison: is_identical_sample && is_identical_environment,
            drift_reasons,
        }
    }
}
