//! Python accurate static semantic analysis, module resolution, and source CFG (PY-01..PY-06).
//! Strictly static: never imports or executes user Python modules.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Virtual environment kind discovered for a Python workspace (PY-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonVirtualEnvKind {
    None,
    DotVenv,
    Venv,
    Conda,
    Poetry,
    Pipenv,
    Custom,
}

/// Project source layout classification (PY-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonProjectLayout {
    FlatLayout,
    SrcLayout { src_dir: String },
    MonorepoMultiRoot { roots: Vec<String> },
}

/// Static project configuration file kind (PY-01).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonConfigSourceKind {
    PyprojectToml,
    SetupCfg,
    PyrightConfigJson,
    MypyIni,
    Default,
}

/// Discovered Python module or package classification (PY-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonModuleKind {
    /// Standard `.py` module file (e.g. `pkg/mod.py`).
    RegularModule,
    /// Standard package directory containing `__init__.py`.
    RegularPackage,
    /// PEP 420 implicit namespace package (directory without `__init__.py`, potentially spanning multiple roots).
    NamespacePackagePep420,
    /// Type stub `.pyi` file (`mod.pyi` or `__init__.pyi`), taking static type precedence over `.py`.
    TypeStubModule,
}

/// Traceable identity of the Python interpreter, virtual environment, layout, and config (PY-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PythonEnvironmentIdentity {
    pub workspace_root: String,
    pub interpreter_path: String,
    pub interpreter_version: String,
    pub venv_kind: PythonVirtualEnvKind,
    pub venv_root: Option<String>,
    pub layout: PythonProjectLayout,
    /// Tuple of `(config_kind, relative_path, content_sha256)`.
    pub config_sources: Vec<(PythonConfigSourceKind, String, String)>,
    /// Ordered module search roots (`typings`/`stubs`, `src/` or flat root, extra paths, `site-packages`).
    pub search_paths: Vec<String>,
    /// Hard safety invariant: static analysis never imports or executes user modules.
    pub executes_user_code: bool,
}

impl PythonEnvironmentIdentity {
    /// Computes a deterministic SHA-256 fingerprint over all environment identity dimensions.
    pub fn environment_fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"py-env-v1:");
        hasher.update(self.workspace_root.as_bytes());
        hasher.update(b"|");
        hasher.update(self.interpreter_path.as_bytes());
        hasher.update(b"|");
        hasher.update(self.interpreter_version.as_bytes());
        hasher.update(b"|");
        hasher.update(format!("{:?}", self.venv_kind).as_bytes());
        hasher.update(b"|");
        if let Some(venv) = &self.venv_root {
            hasher.update(venv.as_bytes());
        }
        hasher.update(b"|");
        hasher.update(format!("{:?}", self.layout).as_bytes());
        hasher.update(b"|");
        for (kind, path, digest) in &self.config_sources {
            hasher.update(format!("{:?}:{}:{},", kind, path, digest).as_bytes());
        }
        hasher.update(b"|");
        for sp in &self.search_paths {
            hasher.update(sp.as_bytes());
            hasher.update(b";");
        }
        hasher.update(if self.executes_user_code {
            b"|exec:1"
        } else {
            b"|exec:0"
        });
        format!("{:x}", hasher.finalize())
    }

    /// Validates that the environment configuration strictly forbids importing or running user code.
    pub fn validate_static_isolation(&self) -> std::result::Result<(), String> {
        if self.executes_user_code {
            return Err(
                "static analysis violation: Python environment must never import or execute user modules"
                    .into(),
            );
        }
        Ok(())
    }
}

/// A discovered module or package entry in the Python workspace catalog (PY-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredPythonModule {
    pub module_name: String,
    pub kind: PythonModuleKind,
    /// Primary file or directory resolved for static analysis (`.pyi` if stub exists, else `.py` or dir).
    pub primary_path: String,
    /// If `kind == TypeStubModule` and a corresponding `.py` implementation exists, records its path.
    pub runtime_implementation_path: Option<String>,
    /// All contributing directories for PEP 420 namespace packages or regular packages.
    pub namespace_roots: Vec<String>,
    /// Index into `PythonEnvironmentIdentity::search_paths` of the highest-priority root.
    pub search_root_index: usize,
}

/// Complete result of static environment & module discovery (PY-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PythonModuleDiscoveryCatalog {
    pub environment: PythonEnvironmentIdentity,
    pub environment_key: String,
    pub modules: BTreeMap<String, DiscoveredPythonModule>,
}

impl PythonModuleDiscoveryCatalog {
    /// Statically discovers environment identity and module catalog from virtual relative file paths
    /// and static configuration files without executing any Python interpreter or importing user code.
    pub fn discover_static(
        workspace_root: &str,
        interpreter_path: &str,
        interpreter_version: &str,
        relative_files: &[&str],
        config_files: &[(PythonConfigSourceKind, &str, &str)],
    ) -> Self {
        let file_set: BTreeSet<&str> = relative_files.iter().copied().collect();

        // 1. Detect virtual environment from file tree without running binaries
        let (venv_kind, venv_root) = if file_set.iter().any(|p| p.starts_with(".venv/")) {
            (PythonVirtualEnvKind::DotVenv, Some(".venv".to_string()))
        } else if file_set.iter().any(|p| p.starts_with("venv/")) {
            (PythonVirtualEnvKind::Venv, Some("venv".to_string()))
        } else if file_set.iter().any(|p| p.starts_with(".conda/")) {
            (PythonVirtualEnvKind::Conda, Some(".conda".to_string()))
        } else {
            (PythonVirtualEnvKind::None, None)
        };

        // 2. Detect src/ layout vs flat layout
        let has_src_layout = file_set.iter().any(|p| {
            p.starts_with("src/") && (p.ends_with(".py") || p.ends_with(".pyi"))
        });
        let layout = if has_src_layout {
            PythonProjectLayout::SrcLayout {
                src_dir: "src".into(),
            }
        } else {
            PythonProjectLayout::FlatLayout
        };

        // 3. Hash config files deterministically
        let mut config_sources = Vec::new();
        for (kind, path, content) in config_files {
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            config_sources.push((kind.clone(), (*path).to_string(), format!("{:x}", h.finalize())));
        }
        config_sources.sort();

        // 4. Construct ordered search roots: typings/stubs -> src (if present) -> root -> site-packages
        let mut search_paths = Vec::new();
        if file_set.iter().any(|p| p.starts_with("typings/")) {
            search_paths.push("typings".to_string());
        }
        if file_set.iter().any(|p| p.starts_with("stubs/")) {
            search_paths.push("stubs".to_string());
        }
        if has_src_layout {
            search_paths.push("src".to_string());
        }
        search_paths.push(".".to_string());
        if let Some(vr) = &venv_root {
            let sp_prefix = format!("{vr}/lib/site-packages");
            if file_set.iter().any(|p| p.starts_with(&sp_prefix)) {
                search_paths.push(sp_prefix);
            }
        }

        let environment = PythonEnvironmentIdentity {
            workspace_root: workspace_root.to_string(),
            interpreter_path: interpreter_path.to_string(),
            interpreter_version: interpreter_version.to_string(),
            venv_kind,
            venv_root,
            layout,
            config_sources,
            search_paths: search_paths.clone(),
            executes_user_code: false,
        };
        let environment_key = environment.environment_fingerprint();

        // 5. Discover modules, packages, PEP 420 namespace packages, and .pyi stubs
        let mut modules: BTreeMap<String, DiscoveredPythonModule> = BTreeMap::new();
        let mut package_init_dirs: BTreeMap<String, (usize, String, bool)> = BTreeMap::new();
        let mut candidate_namespace_dirs: BTreeMap<String, (usize, BTreeSet<String>)> =
            BTreeMap::new();

        for (root_idx, root) in search_paths.iter().enumerate() {
            for &path in &file_set {
                // Skip venv files unless scanning the explicit site-packages search root
                if (path.starts_with(".venv/")
                    || path.starts_with("venv/")
                    || path.starts_with(".conda/"))
                    && !path.starts_with(root.as_str())
                {
                    continue;
                }
                // If scanning "." root, skip paths already owned by higher-priority roots ("src", "typings", "stubs")
                if root == "."
                    && (path.starts_with("src/")
                        || path.starts_with("typings/")
                        || path.starts_with("stubs/"))
                {
                    continue;
                }

                let rel = if root == "." {
                    Some(path)
                } else {
                    let prefix = format!("{root}/");
                    path.strip_prefix(&prefix)
                };
                let Some(rel_path) = rel else { continue };

                let (no_ext, is_stub) = if let Some(s) = rel_path.strip_suffix(".pyi") {
                    (s, true)
                } else if let Some(s) = rel_path.strip_suffix(".py") {
                    (s, false)
                } else {
                    continue;
                };

                let parts: Vec<&str> = no_ext.split('/').filter(|s| !s.is_empty()).collect();
                if parts.is_empty() {
                    continue;
                }

                // Record ancestor directories as candidate namespace packages
                for depth in 1..parts.len() {
                    let pkg_name = parts[..depth].join(".");
                    let dir_rel = parts[..depth].join("/");
                    let full_dir = if root == "." {
                        dir_rel
                    } else {
                        format!("{root}/{dir_rel}")
                    };
                    let entry = candidate_namespace_dirs
                        .entry(pkg_name)
                        .or_insert_with(|| (root_idx, BTreeSet::new()));
                    if root_idx < entry.0 {
                        entry.0 = root_idx;
                    }
                    entry.1.insert(full_dir);
                }

                if *parts.last().unwrap() == "__init__" {
                    if parts.len() == 1 {
                        continue;
                    }
                    let pkg_name = parts[..parts.len() - 1].join(".");
                    package_init_dirs
                        .entry(pkg_name.clone())
                        .and_modify(|existing| {
                            if is_stub && !existing.2 {
                                *existing = (root_idx, path.to_string(), true);
                            }
                        })
                        .or_insert((root_idx, path.to_string(), is_stub));
                } else {
                    let mod_name = parts.join(".");
                    if let Some(existing) = modules.get_mut(&mod_name) {
                        if is_stub && existing.kind != PythonModuleKind::TypeStubModule {
                            let prev_py = existing.primary_path.clone();
                            existing.kind = PythonModuleKind::TypeStubModule;
                            existing.primary_path = path.to_string();
                            existing.runtime_implementation_path = Some(prev_py);
                            existing.search_root_index = root_idx;
                        } else if !is_stub
                            && existing.kind == PythonModuleKind::TypeStubModule
                            && existing.runtime_implementation_path.is_none()
                        {
                            existing.runtime_implementation_path = Some(path.to_string());
                        }
                    } else {
                        modules.insert(
                            mod_name.clone(),
                            DiscoveredPythonModule {
                                module_name: mod_name,
                                kind: if is_stub {
                                    PythonModuleKind::TypeStubModule
                                } else {
                                    PythonModuleKind::RegularModule
                                },
                                primary_path: path.to_string(),
                                runtime_implementation_path: None,
                                namespace_roots: Vec::new(),
                                search_root_index: root_idx,
                            },
                        );
                    }
                }
            }
        }

        // Materialize RegularPackage vs PEP 420 NamespacePackagePep420
        for (pkg_name, (min_root_idx, dir_set)) in candidate_namespace_dirs {
            let roots: Vec<String> = dir_set.into_iter().collect();
            if let Some((init_root_idx, init_path, is_stub)) = package_init_dirs.get(&pkg_name) {
                modules.insert(
                    pkg_name.clone(),
                    DiscoveredPythonModule {
                        module_name: pkg_name,
                        kind: if *is_stub {
                            PythonModuleKind::TypeStubModule
                        } else {
                            PythonModuleKind::RegularPackage
                        },
                        primary_path: init_path.clone(),
                        runtime_implementation_path: None,
                        namespace_roots: roots,
                        search_root_index: *init_root_idx,
                    },
                );
            } else {
                let primary_dir = roots.first().cloned().unwrap_or_default();
                modules.insert(
                    pkg_name.clone(),
                    DiscoveredPythonModule {
                        module_name: pkg_name,
                        kind: PythonModuleKind::NamespacePackagePep420,
                        primary_path: primary_dir,
                        runtime_implementation_path: None,
                        namespace_roots: roots,
                        search_root_index: min_root_idx,
                    },
                );
            }
        }

        Self {
            environment,
            environment_key,
            modules,
        }
    }
}
