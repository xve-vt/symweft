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

/// Lexical scope category following Python LEGB rules (PY-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonScopeKind {
    LocalFunction,
    EnclosingClosure,
    ClassBody,
    ModuleGlobal,
    Builtin,
}

/// Kind of symbol binding within a Python lexical scope (PY-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonSymbolBindingKind {
    Parameter,
    LocalVariable,
    NonlocalDeclaration,
    GlobalDeclaration,
    ComprehensionTarget,
    ClassAttribute,
    MethodDef,
    FunctionDef,
    ClassDef,
    ImportedModule,
    ImportedSymbol,
}

/// Resolution outcome for an import statement or relative import (PY-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportResolutionStatus {
    Resolved {
        target_module: String,
        target_symbol: Option<String>,
        via_stub: bool,
    },
    AmbiguousConflict {
        candidate_paths: Vec<String>,
        reason: String,
    },
    MissingDependency {
        requested_module: String,
        requested_symbol: Option<String>,
        diagnostic: String,
    },
}

/// Import binding record with alias, relative level, and resolution status (PY-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PythonImportBinding {
    pub local_alias: String,
    pub source_module_spec: String,
    pub relative_level: usize,
    pub imported_symbol: Option<String>,
    pub status: ImportResolutionStatus,
}

impl PythonImportBinding {
    /// Resolves an absolute or relative import against a `PythonModuleDiscoveryCatalog`,
    /// exported module symbol tables, and known conflicting module roots.
    pub fn resolve(
        current_module: &str,
        is_package_init: bool,
        relative_level: usize,
        module_spec: &str,
        imported_symbol: Option<&str>,
        alias: Option<&str>,
        catalog: &PythonModuleDiscoveryCatalog,
        exported_symbols: &BTreeMap<String, BTreeSet<String>>,
        conflicting_modules: &BTreeMap<String, Vec<String>>,
    ) -> Self {
        let default_alias = if let Some(sym) = imported_symbol {
            sym.to_string()
        } else {
            module_spec
                .split('.')
                .next()
                .unwrap_or(module_spec)
                .to_string()
        };
        let local_alias = alias.map(str::to_string).unwrap_or(default_alias);

        // Compute target module path for relative or absolute import
        let resolved_mod_name = if relative_level > 0 {
            let mut parts: Vec<&str> = current_module
                .split('.')
                .filter(|s| !s.is_empty())
                .collect();
            // For a regular module `pkg.sub.mod`, level 1 (`.`) anchors at `pkg.sub` (pop 1).
            // For `__init__.py` of `pkg.sub`, level 1 (`.`) anchors at `pkg.sub` (pop 0).
            let pops_needed = if is_package_init {
                relative_level.saturating_sub(1)
            } else {
                relative_level
            };
            if pops_needed > parts.len() {
                return Self {
                    local_alias,
                    source_module_spec: module_spec.to_string(),
                    relative_level,
                    imported_symbol: imported_symbol.map(str::to_string),
                    status: ImportResolutionStatus::MissingDependency {
                        requested_module: format!("{}{}", ".".repeat(relative_level), module_spec),
                        requested_symbol: imported_symbol.map(str::to_string),
                        diagnostic: "attempted relative import beyond top-level package".into(),
                    },
                };
            }
            parts.truncate(parts.len() - pops_needed);
            if !module_spec.is_empty() {
                for seg in module_spec.split('.').filter(|s| !s.is_empty()) {
                    parts.push(seg);
                }
            }
            parts.join(".")
        } else {
            module_spec.to_string()
        };

        // Check if target module has an explicit conflict across roots
        if let Some(candidates) = conflicting_modules.get(&resolved_mod_name)
            && candidates.len() > 1
        {
            return Self {
                local_alias,
                source_module_spec: module_spec.to_string(),
                relative_level,
                imported_symbol: imported_symbol.map(str::to_string),
                status: ImportResolutionStatus::AmbiguousConflict {
                    candidate_paths: candidates.clone(),
                    reason: format!(
                        "module `{resolved_mod_name}` has conflicting definitions across multiple roots"
                    ),
                },
            };
        }

        // Lookup module in catalog
        if let Some(found_mod) = catalog.modules.get(&resolved_mod_name) {
            let via_stub = found_mod.kind == PythonModuleKind::TypeStubModule;
            if let Some(sym) = imported_symbol {
                // Check if `from pkg import submod` actually imports a submodule `pkg.submod`
                let candidate_submod = format!("{resolved_mod_name}.{sym}");
                if let Some(sub_entry) = catalog.modules.get(&candidate_submod) {
                    return Self {
                        local_alias,
                        source_module_spec: module_spec.to_string(),
                        relative_level,
                        imported_symbol: Some(sym.to_string()),
                        status: ImportResolutionStatus::Resolved {
                            target_module: candidate_submod,
                            target_symbol: None,
                            via_stub: sub_entry.kind == PythonModuleKind::TypeStubModule,
                        },
                    };
                }
                // Check exported symbol table for `resolved_mod_name`
                if let Some(exports) = exported_symbols.get(&resolved_mod_name) {
                    if exports.contains(sym) {
                        return Self {
                            local_alias,
                            source_module_spec: module_spec.to_string(),
                            relative_level,
                            imported_symbol: Some(sym.to_string()),
                            status: ImportResolutionStatus::Resolved {
                                target_module: resolved_mod_name,
                                target_symbol: Some(sym.to_string()),
                                via_stub,
                            },
                        };
                    } else {
                        return Self {
                            local_alias,
                            source_module_spec: module_spec.to_string(),
                            relative_level,
                            imported_symbol: Some(sym.to_string()),
                            status: ImportResolutionStatus::MissingDependency {
                                requested_module: resolved_mod_name.clone(),
                                requested_symbol: Some(sym.to_string()),
                                diagnostic: format!(
                                    "module `{resolved_mod_name}` has no exported symbol `{sym}`"
                                ),
                            },
                        };
                    }
                }
            }
            return Self {
                local_alias,
                source_module_spec: module_spec.to_string(),
                relative_level,
                imported_symbol: imported_symbol.map(str::to_string),
                status: ImportResolutionStatus::Resolved {
                    target_module: resolved_mod_name,
                    target_symbol: imported_symbol.map(str::to_string),
                    via_stub,
                },
            };
        }

        Self {
            local_alias,
            source_module_spec: module_spec.to_string(),
            relative_level,
            imported_symbol: imported_symbol.map(str::to_string),
            status: ImportResolutionStatus::MissingDependency {
                requested_module: resolved_mod_name.clone(),
                requested_symbol: imported_symbol.map(str::to_string),
                diagnostic: format!("unresolved module `{resolved_mod_name}` in workspace or environment"),
            },
        }
    }
}

/// Resolved lexical symbol entry in a Python scope (PY-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PythonScopedSymbol {
    pub name: String,
    pub canonical_id: String,
    pub defining_scope_id: usize,
    pub resolved_via: PythonScopeKind,
    pub binding_kind: PythonSymbolBindingKind,
}

/// Node in a Python lexical scope tree supporting LEGB and class-scope skipping (PY-02).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonScopeNode {
    pub scope_id: usize,
    pub parent_id: Option<usize>,
    pub kind: PythonScopeKind,
    pub qualname: String,
    pub bindings: BTreeMap<String, (String, PythonSymbolBindingKind)>,
    pub nonlocal_names: BTreeSet<String>,
    pub global_names: BTreeSet<String>,
}

/// Lexical scope tree for a single Python module (PY-02).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PythonLexicalScopeTree {
    pub module_name: String,
    pub nodes: Vec<PythonScopeNode>,
}

impl PythonLexicalScopeTree {
    pub fn new(module_name: &str) -> Self {
        let root = PythonScopeNode {
            scope_id: 0,
            parent_id: None,
            kind: PythonScopeKind::ModuleGlobal,
            qualname: module_name.to_string(),
            bindings: BTreeMap::new(),
            nonlocal_names: BTreeSet::new(),
            global_names: BTreeSet::new(),
        };
        Self {
            module_name: module_name.to_string(),
            nodes: vec![root],
        }
    }

    pub fn add_child_scope(&mut self, parent_id: usize, kind: PythonScopeKind, name: &str) -> usize {
        let id = self.nodes.len();
        let parent_qual = &self.nodes[parent_id].qualname;
        let qualname = format!("{parent_qual}.{name}");
        self.nodes.push(PythonScopeNode {
            scope_id: id,
            parent_id: Some(parent_id),
            kind,
            qualname,
            bindings: BTreeMap::new(),
            nonlocal_names: BTreeSet::new(),
            global_names: BTreeSet::new(),
        });
        id
    }

    pub fn define_symbol(
        &mut self,
        scope_id: usize,
        name: &str,
        kind: PythonSymbolBindingKind,
    ) -> String {
        let qual = &self.nodes[scope_id].qualname;
        let canonical_id = format!("py-usr:{qual}:{name}");
        self.nodes[scope_id]
            .bindings
            .insert(name.to_string(), (canonical_id.clone(), kind));
        canonical_id
    }

    pub fn declare_nonlocal(&mut self, scope_id: usize, name: &str) {
        self.nodes[scope_id].nonlocal_names.insert(name.to_string());
    }

    pub fn declare_global(&mut self, scope_id: usize, name: &str) {
        self.nodes[scope_id].global_names.insert(name.to_string());
    }

    /// Resolves `name` used in `start_scope_id` according to Python LEGB rules:
    /// - `global x` jumps directly to `ModuleGlobal` (scope 0).
    /// - `nonlocal x` searches enclosing `LocalFunction` scopes (skipping `ClassBody`).
    /// - Unqualified names in a `LocalFunction` check local bindings first, then enclosing
    ///   `LocalFunction` closures (skipping `ClassBody`!), then `ModuleGlobal`.
    pub fn resolve_legb(&self, start_scope_id: usize, name: &str) -> Option<PythonScopedSymbol> {
        let start_node = self.nodes.get(start_scope_id)?;

        // 1. Explicit `global` declaration
        if start_node.global_names.contains(name) {
            let global_node = &self.nodes[0];
            let (id, bkind) = global_node.bindings.get(name)?;
            return Some(PythonScopedSymbol {
                name: name.to_string(),
                canonical_id: id.clone(),
                defining_scope_id: 0,
                resolved_via: PythonScopeKind::ModuleGlobal,
                binding_kind: bkind.clone(),
            });
        }

        // 2. Explicit `nonlocal` declaration: search strictly enclosing function scopes
        if start_node.nonlocal_names.contains(name) {
            let mut cur = start_node.parent_id;
            while let Some(pid) = cur {
                let pnode = &self.nodes[pid];
                if pnode.kind == PythonScopeKind::LocalFunction
                    && let Some((id, bkind)) = pnode.bindings.get(name)
                {
                    return Some(PythonScopedSymbol {
                        name: name.to_string(),
                        canonical_id: id.clone(),
                        defining_scope_id: pid,
                        resolved_via: PythonScopeKind::EnclosingClosure,
                        binding_kind: bkind.clone(),
                    });
                }
                cur = pnode.parent_id;
            }
            return None;
        }

        // 3. Direct lookup in current scope
        if let Some((id, bkind)) = start_node.bindings.get(name) {
            return Some(PythonScopedSymbol {
                name: name.to_string(),
                canonical_id: id.clone(),
                defining_scope_id: start_scope_id,
                resolved_via: start_node.kind.clone(),
                binding_kind: bkind.clone(),
            });
        }

        // 4. Walk parent scopes: skips ClassBody when resolving from inner functions (Python LEGB rule)
        let mut cur = start_node.parent_id;
        while let Some(pid) = cur {
            let pnode = &self.nodes[pid];
            if pnode.kind != PythonScopeKind::ClassBody
                && let Some((id, bkind)) = pnode.bindings.get(name)
            {
                let resolved_via = if pnode.kind == PythonScopeKind::LocalFunction {
                    PythonScopeKind::EnclosingClosure
                } else {
                    pnode.kind.clone()
                };
                return Some(PythonScopedSymbol {
                    name: name.to_string(),
                    canonical_id: id.clone(),
                    defining_scope_id: pid,
                    resolved_via,
                    binding_kind: bkind.clone(),
                });
            }
            cur = pnode.parent_id;
        }

        None
    }
}

/// Provider key for Python A2 semantic snapshots in the shared store (PY-03, PY-04).
pub const PYTHON_PROVIDER_KEY: &str = "a2-python-v1";

/// Classification of a Python call site resolution (PY-03).
/// Strictly forbids guessing targets by bare method/function name when receiver type is dynamic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonCallResolutionKind {
    DirectFunctionCall {
        target_usr: String,
        target_module: String,
    },
    BoundMethodCall {
        target_usr: String,
        receiver_type_usr: String,
    },
    DynamicAttributeUnresolved {
        receiver_expr: String,
        attr_name: String,
        reason: String,
    },
    ReflectionOrEvalUnresolved {
        construct: String,
        reason: String,
    },
    RuntimeMonkeyPatchUnresolved {
        target_expr: String,
        reason: String,
    },
}

impl PythonCallResolutionKind {
    /// Maps the Python call resolution into the shared A2 `(Option<target_usr>, dispatch_kind)` pair.
    pub fn to_shared_call_target(&self) -> (Option<String>, String) {
        match self {
            PythonCallResolutionKind::DirectFunctionCall { target_usr, .. } => {
                (Some(target_usr.clone()), "direct".into())
            }
            PythonCallResolutionKind::BoundMethodCall { target_usr, .. } => {
                (Some(target_usr.clone()), "direct".into())
            }
            PythonCallResolutionKind::DynamicAttributeUnresolved { .. } => {
                (None, "unresolved".into())
            }
            PythonCallResolutionKind::ReflectionOrEvalUnresolved { .. } => {
                (None, "indirect".into())
            }
            PythonCallResolutionKind::RuntimeMonkeyPatchUnresolved { .. } => {
                (None, "unresolved".into())
            }
        }
    }
}

/// Recorded Python call site with caller, resolution status, and source location (PY-03).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonCallFact {
    pub caller_usr: Option<String>,
    pub callee_spelling: String,
    pub resolution: PythonCallResolutionKind,
    pub location: crate::model::Location,
}

/// Cross-module symbol, reference, and call resolver for a Python module (PY-03).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PythonModuleSemanticModel {
    pub unit_id: String,
    pub module_name: String,
    pub file_path: String,
    pub file_hash: String,
    pub symbols: Vec<crate::model::Symbol>,
    pub references: Vec<crate::model::Reference>,
    pub calls: Vec<PythonCallFact>,
}

impl PythonModuleSemanticModel {
    pub fn new(unit_id: &str, module_name: &str, file_path: &str, file_hash: &str) -> Self {
        Self {
            unit_id: unit_id.to_string(),
            module_name: module_name.to_string(),
            file_path: file_path.to_string(),
            file_hash: file_hash.to_string(),
            symbols: Vec::new(),
            references: Vec::new(),
            calls: Vec::new(),
        }
    }

    pub fn add_definition(
        &mut self,
        qualname: &str,
        name: &str,
        kind: &str,
        line: u32,
        column: u32,
        start_byte: u32,
        end_byte: u32,
    ) -> String {
        let usr = format!("py-usr:{}:{qualname}", self.module_name);
        self.symbols.push(crate::model::Symbol {
            id: usr.clone(),
            name: name.to_string(),
            kind: kind.to_string(),
            definition: true,
            location: crate::model::Location {
                path: self.file_path.clone(),
                line,
                column,
                start_byte,
                end_byte,
                expansion: None,
            },
        });
        usr
    }

    pub fn add_reference(
        &mut self,
        target_usr: &str,
        name: &str,
        kind: &str,
        line: u32,
        column: u32,
        start_byte: u32,
        end_byte: u32,
    ) {
        self.references.push(crate::model::Reference {
            target: target_usr.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            location: crate::model::Location {
                path: self.file_path.clone(),
                line,
                column,
                start_byte,
                end_byte,
                expansion: None,
            },
        });
    }

    /// Resolves a call site without ever guessing by bare method name when receiver type is unknown.
    pub fn record_call(
        &mut self,
        caller_usr: Option<String>,
        callee_spelling: &str,
        resolution: PythonCallResolutionKind,
        line: u32,
        column: u32,
        start_byte: u32,
        end_byte: u32,
    ) {
        let location = crate::model::Location {
            path: self.file_path.clone(),
            line,
            column,
            start_byte,
            end_byte,
            expansion: None,
        };
        if let (Some(target_usr), _) = resolution.to_shared_call_target() {
            self.references.push(crate::model::Reference {
                target: target_usr,
                name: callee_spelling.to_string(),
                kind: "call_ref".into(),
                location: location.clone(),
            });
        }
        self.calls.push(PythonCallFact {
            caller_usr,
            callee_spelling: callee_spelling.to_string(),
            resolution,
            location,
        });
    }

    /// Converts the Python semantic model into a shared A2 `Snapshot` ready for SQLite `Store::publish`.
    pub fn into_shared_snapshot(
        self,
        configuration: &str,
        compile_hash: &str,
        dependency_inputs: Vec<crate::model::InputFile>,
    ) -> crate::model::Snapshot {
        let mut inputs = vec![crate::model::InputFile {
            path: self.file_path.clone(),
            hash: self.file_hash.clone(),
        }];
        for dep in dependency_inputs {
            if !inputs.iter().any(|i| i.path == dep.path) {
                inputs.push(dep);
            }
        }
        let shared_calls = self
            .calls
            .into_iter()
            .map(|c| {
                let (target, dispatch) = c.resolution.to_shared_call_target();
                crate::model::CallSite {
                    caller: c.caller_usr,
                    target,
                    name: c.callee_spelling,
                    dispatch,
                    location: c.location,
                }
            })
            .collect();

        crate::model::Snapshot {
            unit_id: self.unit_id,
            configuration: configuration.to_string(),
            main_file: self.file_path,
            compile_hash: compile_hash.to_string(),
            provider: PYTHON_PROVIDER_KEY.to_string(),
            complete: true,
            diagnostics: Vec::new(),
            inputs,
            symbols: self.symbols,
            references: self.references,
            calls: shared_calls,
        }
    }
}

/// Cached module entry inside the Python incremental provider (PY-04).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPythonModuleEntry {
    pub module_name: String,
    pub revision: u64,
    pub content_hash: String,
    pub is_dirty_overlay: bool,
    pub estimated_bytes: usize,
    pub snapshot: crate::model::Snapshot,
}

/// Incremental scheduler and provider interface for Python semantic analysis (PY-04).
/// Enforces module-level batch extraction, dirty overlay tracking, cancellation,
/// environment-change invalidation, and memory budgeting without per-symbol LSP scans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonIncrementalProvider {
    pub provider_version: String,
    pub environment_key: String,
    pub allows_per_symbol_lsp_scan: bool,
    pub per_symbol_lsp_requests_issued: usize,
    pub max_cache_bytes: usize,
    pub current_cache_bytes: usize,
    pub cached_modules: BTreeMap<String, CachedPythonModuleEntry>,
    pub dirty_revisions: BTreeMap<String, (u64, String)>,
    pub reverse_importers: BTreeMap<String, BTreeSet<String>>,
    pub stale_modules: BTreeSet<String>,
    pub cancelled_jobs: BTreeSet<u64>,
}

impl PythonIncrementalProvider {
    pub fn new(environment_key: &str, max_cache_bytes: usize) -> Self {
        Self {
            provider_version: PYTHON_PROVIDER_KEY.to_string(),
            environment_key: environment_key.to_string(),
            allows_per_symbol_lsp_scan: false,
            per_symbol_lsp_requests_issued: 0,
            max_cache_bytes,
            current_cache_bytes: 0,
            cached_modules: BTreeMap::new(),
            dirty_revisions: BTreeMap::new(),
            reverse_importers: BTreeMap::new(),
            stale_modules: BTreeSet::new(),
            cancelled_jobs: BTreeSet::new(),
        }
    }

    /// Guard verifying that unbounded per-symbol LSP fan-out scanning is strictly disabled.
    pub fn validate_provider_contract(&self) -> std::result::Result<(), String> {
        if self.provider_version != PYTHON_PROVIDER_KEY {
            return Err(format!("unsupported Python provider version: {}", self.provider_version));
        }
        if self.allows_per_symbol_lsp_scan || self.per_symbol_lsp_requests_issued > 0 {
            return Err(
                "provider contract violation: unbounded per-symbol LSP scanning is forbidden"
                    .into(),
            );
        }
        Ok(())
    }

    /// Registers a dependency edge `importer_module -> imported_module` for incremental invalidation.
    pub fn register_import_edge(&mut self, importer_module: &str, imported_module: &str) {
        self.reverse_importers
            .entry(imported_module.to_string())
            .or_default()
            .insert(importer_module.to_string());
    }

    /// Marks a module as having an unsaved dirty buffer revision and invalidates its direct importers.
    pub fn update_dirty_buffer(
        &mut self,
        module_name: &str,
        revision: u64,
        content_hash: &str,
    ) -> Vec<String> {
        self.dirty_revisions
            .insert(module_name.to_string(), (revision, content_hash.to_string()));
        let mut affected = vec![module_name.to_string()];
        self.stale_modules.insert(module_name.to_string());
        if let Some(importers) = self.reverse_importers.get(module_name) {
            for imp in importers {
                self.stale_modules.insert(imp.clone());
                affected.push(imp.clone());
            }
        }
        affected.sort();
        affected.dedup();
        affected
    }

    /// Requests cancellation of an in-flight analysis job.
    pub fn cancel_job(&mut self, job_id: u64) {
        self.cancelled_jobs.insert(job_id);
    }

    /// Publishes a batch-extracted module snapshot if the job was not cancelled and revision is current.
    pub fn publish_module_snapshot(
        &mut self,
        job_id: u64,
        module_name: &str,
        revision: u64,
        content_hash: &str,
        is_dirty_overlay: bool,
        snapshot: crate::model::Snapshot,
    ) -> std::result::Result<(), String> {
        self.validate_provider_contract()?;
        if self.cancelled_jobs.contains(&job_id) {
            return Err(format!("python analysis job {job_id} was cancelled"));
        }
        if let Some((latest_rev, _)) = self.dirty_revisions.get(module_name)
            && revision < *latest_rev
        {
            return Err(format!(
                "stale revision {revision} for module `{module_name}` (latest dirty revision is {latest_rev})"
            ));
        }

        let estimated_bytes = 256
            + snapshot.symbols.len() * 128
            + snapshot.references.len() * 96
            + snapshot.calls.len() * 96;

        // Evict existing entry for same module first
        if let Some(prev) = self.cached_modules.remove(module_name) {
            self.current_cache_bytes = self
                .current_cache_bytes
                .saturating_sub(prev.estimated_bytes);
        }

        // Enforce memory budget by evicting clean cached modules first
        while self.current_cache_bytes + estimated_bytes > self.max_cache_bytes
            && !self.cached_modules.is_empty()
        {
            let victim_key = self
                .cached_modules
                .iter()
                .find(|(_, v)| !v.is_dirty_overlay)
                .or_else(|| self.cached_modules.iter().next())
                .map(|(k, _)| k.clone())
                .unwrap();
            if let Some(removed) = self.cached_modules.remove(&victim_key) {
                self.current_cache_bytes = self
                    .current_cache_bytes
                    .saturating_sub(removed.estimated_bytes);
            }
        }

        self.current_cache_bytes += estimated_bytes;
        self.stale_modules.remove(module_name);
        self.cached_modules.insert(
            module_name.to_string(),
            CachedPythonModuleEntry {
                module_name: module_name.to_string(),
                revision,
                content_hash: content_hash.to_string(),
                is_dirty_overlay,
                estimated_bytes,
                snapshot,
            },
        );
        Ok(())
    }

    /// Handles interpreter, virtualenv, or project config changes by invalidating cached snapshots.
    pub fn on_environment_change(&mut self, new_environment_key: &str) -> usize {
        if self.environment_key == new_environment_key {
            return 0;
        }
        let invalidated = self.cached_modules.len();
        for k in self.cached_modules.keys() {
            self.stale_modules.insert(k.clone());
        }
        self.cached_modules.clear();
        self.current_cache_bytes = 0;
        self.environment_key = new_environment_key.to_string();
        invalidated
    }
}
