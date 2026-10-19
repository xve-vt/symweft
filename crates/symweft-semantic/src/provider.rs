//! All C handles stay on this thread/in a disposable worker process.
#![allow(non_upper_case_globals)]
use crate::{
    PROVIDER_KEY, Result,
    compdb::{CompileCommand, key},
    model::*,
};
use clang_sys::*;
use std::cell::RefCell;
use std::{
    collections::{BTreeMap, HashMap},
    ffi::{CStr, CString},
    fs,
    path::{Path, PathBuf},
    ptr,
    time::Instant,
};

fn string(value: CXString) -> String {
    // libclang owns this string until disposeString; never retain its pointer.
    unsafe {
        let p = clang_getCString(value);
        let result = if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        };
        clang_disposeString(value);
        result
    }
}
thread_local! { static FILE_NAMES: RefCell<HashMap<usize,Option<String>>> = RefCell::new(HashMap::new()); }
fn file_name(file: CXFile) -> Option<String> {
    if file.is_null() {
        return None;
    }
    if let Some(path) = FILE_NAMES.with(|names| names.borrow().get(&(file as usize)).cloned()) {
        return path;
    }
    let path = string(unsafe { clang_getFileName(file) });
    let path = PathBuf::from(path);
    let result = path
        .canonicalize()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    FILE_NAMES.with(|names| names.borrow_mut().insert(file as usize, result.clone()));
    result
}
fn location(loc: CXSourceLocation, spelling: bool) -> Option<Location> {
    unsafe {
        let (mut file, mut line, mut column, mut offset) = (ptr::null_mut(), 0, 0, 0);
        if spelling {
            clang_getSpellingLocation(loc, &mut file, &mut line, &mut column, &mut offset);
        } else {
            clang_getExpansionLocation(loc, &mut file, &mut line, &mut column, &mut offset);
        }
        Some(Location {
            path: file_name(file)?,
            offset,
            end_offset: offset.saturating_add(1),
            line,
            column,
        })
    }
}
fn extent(cursor: CXCursor, spelling: bool) -> Option<Location> {
    let range = unsafe { clang_getCursorExtent(cursor) };
    let mut start = location(unsafe { clang_getRangeStart(range) }, spelling)?;
    if let Some(end) = location(unsafe { clang_getRangeEnd(range) }, spelling) {
        if start.path == end.path {
            start.end_offset = end.offset.max(start.offset.saturating_add(1));
        }
    }
    Some(start)
}
fn callable(kind: CXCursorKind) -> bool {
    matches!(
        kind,
        CXCursor_FunctionDecl
            | CXCursor_CXXMethod
            | CXCursor_Constructor
            | CXCursor_Destructor
            | CXCursor_ConversionFunction
            | CXCursor_FunctionTemplate
    )
}
fn identity(cursor: CXCursor, command: &CompileCommand) -> Option<(String, String)> {
    unsafe {
        if clang_Cursor_isNull(cursor) != 0 {
            return None;
        }
        let canonical = clang_getCanonicalCursor(cursor);
        let usr = string(clang_getCursorUSR(canonical));
        if usr.is_empty() {
            return None;
        }
        let linkage = clang_getCursorLinkage(canonical);
        // Clang USRs for internal-linkage functions can only contain basenames.
        // Qualify with TU/command, including static inline header instances.
        let source_domain;
        let domain = if matches!(linkage, CXLinkage_Internal | CXLinkage_UniqueExternal) {
            command.unit_id.as_str()
        } else if linkage == CXLinkage_NoLinkage {
            // Fields, typedefs and source-local declarations share a source
            // identity across TUs; same-basename files still remain distinct.
            source_domain = location(clang_getCursorLocation(canonical), false)
                .map(|l| l.path)
                .unwrap_or_else(|| command.unit_id.clone());
            &source_domain
        } else {
            "external"
        };
        Some((key(&(&command.root, domain, &usr)), usr))
    }
}
fn sdk_arguments(arguments: &[String]) -> Vec<String> {
    #[cfg(target_os = "macos")]
    if !arguments
        .iter()
        .any(|a| a.starts_with("-isysroot") || a.starts_with("--sysroot"))
    {
        let path = std::env::var("SDKROOT")
            .ok()
            .filter(|p| Path::new(p).is_dir())
            .or_else(|| {
                let out = std::process::Command::new("/usr/bin/xcrun")
                    .arg("--show-sdk-path")
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            });
        if let Some(path) = path {
            return vec!["-isysroot".into(), path];
        }
    }
    Vec::new()
}
pub fn identity_key() -> Result<String> {
    clang_sys::load().map_err(|e| format!("libclang unavailable: {e}; set LIBCLANG_PATH"))?;
    let library = clang_sys::get_library().ok_or("libclang not loaded")?;
    let version = string(unsafe { clang_getClangVersion() });
    let env: Vec<_> = [
        "CPATH",
        "C_INCLUDE_PATH",
        "CPLUS_INCLUDE_PATH",
        "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET",
    ]
    .into_iter()
    .map(|name| (name, std::env::var(name).unwrap_or_default()))
    .collect();
    let sdk = sdk_arguments(&[]);
    if std::env::var_os("SYMWEFT_TRACE_PROVIDER").is_some() {
        eprintln!(
            "Semantic provider inputs: {}",
            serde_json::to_string(&(version.clone(), library.path(), &sdk, &env))?
        );
    }
    Ok(format!(
        "{PROVIDER_KEY}:{}",
        key(&(version, library.path(), sdk, env))
    ))
}
struct Index(CXIndex);
impl Drop for Index {
    fn drop(&mut self) {
        unsafe { clang_disposeIndex(self.0) }
    }
}
struct Unit(CXTranslationUnit);
impl Drop for Unit {
    fn drop(&mut self) {
        unsafe { clang_disposeTranslationUnit(self.0) }
    }
}
struct Walker<'a> {
    command: &'a CompileCommand,
    tu: CXTranslationUnit,
    symbols: Vec<Symbol>,
    references: Vec<Reference>,
    calls: Vec<Call>,
    owner: Option<String>,
    depth: usize,
    visited: usize,
    template_instantiations: usize,
    error: Option<String>,
}
impl Walker<'_> {
    fn project(&self, loc: &Location) -> bool {
        let path = Path::new(&loc.path);
        path.starts_with(&self.command.root) || path.starts_with(&self.command.build_root)
    }
    fn visit(&mut self, cursor: CXCursor) -> CXChildVisitResult {
        unsafe {
            self.visited += 1;
            if self.visited > 2_000_000
                || self.symbols.len() + self.references.len() + self.calls.len() > 250_000
                || self.depth > 256
            {
                self.error = Some("semantic extraction budget exceeded".into());
                return CXChildVisit_Break;
            }
            let Some(loc) = location(clang_getCursorLocation(cursor), false) else {
                return CXChildVisit_Continue;
            };
            if !self.project(&loc) {
                return CXChildVisit_Continue;
            }
            let kind = clang_getCursorKind(cursor);
            let name = string(clang_getCursorSpelling(cursor));
            let range = extent(cursor, false).unwrap_or_else(|| loc.clone());
            let spelling =
                extent(cursor, true).filter(|s| s.path != range.path || s.offset != range.offset);
            let mut owner = self.owner.clone();
            if clang_isDeclaration(kind) != 0 || kind == CXCursor_MacroDefinition {
                if let Some((id, usr)) = identity(cursor, self.command) {
                    let definition =
                        clang_isCursorDefinition(cursor) != 0 || kind == CXCursor_MacroDefinition;
                    let is_callable = callable(kind);
                    let mut named = loc.clone();
                    // A macro-generated name need not exist verbatim at its
                    // expansion site. Use the physical token, not name length.
                    named.end_offset = range
                        .end_offset
                        .min(named.offset.saturating_add(name.len().max(1) as u32));
                    let token = clang_getToken(self.tu, clang_getCursorLocation(cursor));
                    if !token.is_null() {
                        let end = location(
                            clang_getRangeEnd(clang_getTokenExtent(self.tu, *token)),
                            false,
                        );
                        if let Some(end) = end {
                            if end.path == named.path && end.offset > named.offset {
                                named.end_offset = end.offset;
                            }
                        }
                        clang_disposeTokens(self.tu, token, 1);
                    }
                    self.symbols.push(Symbol {
                        id: id.clone(),
                        usr,
                        name: name.clone(),
                        kind: string(clang_getCursorKindSpelling(kind)),
                        signature: string(clang_getTypeSpelling(clang_getCursorType(cursor))),
                        definition,
                        callable: is_callable,
                        location: named,
                        extent: range.clone(),
                    });
                    if is_callable {
                        owner = if definition { Some(id) } else { None };
                    }
                }
            }
            if kind == CXCursor_ParmDecl {
                owner = None;
            } // default arguments are not function-body execution.
            if kind == CXCursor_LambdaExpr {
                let usr = format!("lambda:{}:{}", loc.path, loc.offset);
                let id = key(&(&self.command.unit_id, &usr));
                owner = Some(id.clone());
                self.symbols.push(Symbol {
                    id,
                    usr,
                    name: "<lambda>".into(),
                    kind: "LambdaExpr".into(),
                    signature: String::new(),
                    definition: true,
                    callable: true,
                    location: loc.clone(),
                    extent: range.clone(),
                });
            }
            if kind == CXCursor_CallExpr {
                let target_cursor = clang_getCursorReferenced(cursor);
                let target_kind = clang_getCursorKind(target_cursor);
                let (target, resolution) = if callable(target_kind) {
                    (
                        identity(target_cursor, self.command).map(|p| p.0),
                        if clang_Cursor_isDynamicCall(cursor) != 0 {
                            "virtual_candidate"
                        } else {
                            "direct"
                        },
                    )
                } else if matches!(
                    target_kind,
                    CXCursor_VarDecl | CXCursor_ParmDecl | CXCursor_FieldDecl
                ) {
                    (None, "indirect")
                } else {
                    (None, "unresolved")
                };
                self.calls.push(Call {
                    caller: owner.clone(),
                    target: target.clone(),
                    name: name.clone(),
                    resolution: resolution.into(),
                    location: range.clone(),
                    spelling: spelling.clone(),
                });
                let template_cursor = clang_getSpecializedCursorTemplate(target_cursor);
                if clang_Cursor_isNull(template_cursor) == 0
                    && self.template_instantiations < crate::cpp_constructs::MAX_TEMPLATE_INSTANTIATIONS_PER_TU
                {
                    if let Some((primary_id, _)) = identity(template_cursor, self.command) {
                        if Some(&primary_id) != target.as_ref() {
                            self.template_instantiations += 1;
                            self.references.push(Reference {
                                target: primary_id,
                                name: name.clone(),
                                kind: "TemplateInstantiationRef".into(),
                                location: range.clone(),
                                spelling: spelling.clone(),
                            });
                        }
                    }
                }
            }
            if matches!(
                kind,
                CXCursor_DeclRefExpr
                    | CXCursor_MemberRefExpr
                    | CXCursor_TypeRef
                    | CXCursor_TemplateRef
                    | CXCursor_NamespaceRef
                    | CXCursor_MemberRef
                    | CXCursor_MacroExpansion
            ) {
                let referenced = clang_getCursorReferenced(cursor);
                if let Some((target, _)) = identity(referenced, self.command) {
                    self.references.push(Reference {
                        target,
                        name,
                        kind: string(clang_getCursorKindSpelling(kind)),
                        location: range,
                        spelling,
                    });
                }
            }
            let previous = std::mem::replace(&mut self.owner, owner);
            self.depth += 1;
            clang_visitChildren(cursor, visitor, self as *mut Self as CXClientData);
            self.depth -= 1;
            self.owner = previous;
            if self.error.is_some() {
                CXChildVisit_Break
            } else {
                CXChildVisit_Continue
            }
        }
    }
}
extern "C" fn visitor(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    // callback cannot escape this synchronous traversal; no references enter snapshots.
    unsafe { (&mut *(data as *mut Walker<'_>)).visit(cursor) }
}
extern "C" fn inclusion(
    file: CXFile,
    _stack: *mut CXSourceLocation,
    _count: u32,
    data: CXClientData,
) {
    if !file.is_null() {
        let path = string(unsafe { clang_getFileName(file) });
        if !path.is_empty() {
            unsafe {
                (&mut *(data as *mut BTreeMap<String, CXFile>)).insert(path, file);
            }
        }
    }
}
pub fn analyze(request: &WorkerRequest) -> Result<WorkerReply> {
    let profiling = crate::profile::enabled();
    let provider_clock = crate::profile::Clock::start(profiling);
    let mut measured = crate::profile::WorkerProfile::default();
    FILE_NAMES.with(|names| names.borrow_mut().clear());
    let provider = identity_key()?;
    let command = &request.command;
    if fs::metadata(&command.file)?.len() > 8 * 1024 * 1024 {
        return Err("semantic main file exceeds 8 MiB budget".into());
    }
    if request.overlays.len() > MAX_OVERLAY_DOCUMENTS
        || request
            .overlays
            .iter()
            .map(|o| o.content.len())
            .sum::<usize>()
            > MAX_OVERLAY_SOURCE_BYTES
    {
        return Err("overlay budget exceeded".into());
    }
    let mut args = command.arguments.clone();
    args.extend(sdk_arguments(&args));
    let args: Vec<_> = args
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<std::result::Result<_, _>>()?;
    let pointers: Vec<_> = args.iter().map(|a| a.as_ptr()).collect();
    let file = CString::new(command.file.to_string_lossy().as_bytes())?;
    let names: Vec<_> = request
        .overlays
        .iter()
        .map(|o| CString::new(o.path.as_str()))
        .collect::<std::result::Result<_, _>>()?;
    let contents: Vec<_> = request
        .overlays
        .iter()
        .map(|o| o.content.as_bytes())
        .collect();
    let mut unsaved: Vec<_> = names
        .iter()
        .zip(contents.iter())
        .map(|(name, content)| CXUnsavedFile {
            Filename: name.as_ptr(),
            Contents: content.as_ptr().cast(),
            Length: content.len() as _,
        })
        .collect();
    let parse = Instant::now();
    measured.prepare_micros = provider_clock.micros();
    let index = Index(unsafe { clang_createIndex(1, 0) });
    if index.0.is_null() {
        return Err("cannot create clang index".into());
    }
    let mut raw = ptr::null_mut();
    let code = unsafe {
        clang_parseTranslationUnit2(
            index.0,
            file.as_ptr(),
            pointers.as_ptr(),
            pointers.len() as _,
            unsaved.as_mut_ptr(),
            unsaved.len() as _,
            CXTranslationUnit_DetailedPreprocessingRecord,
            &mut raw,
        )
    };
    if raw.is_null() {
        return Err(format!("libclang parse failed, code {code}").into());
    }
    let tu = Unit(raw);
    let parse_micros = parse.elapsed().as_micros() as u64;
    measured.parse_micros = parse_micros;
    let diagnostics_clock = crate::profile::Clock::start(profiling);
    let mut diagnostics = Vec::new();
    for i in 0..unsafe { clang_getNumDiagnostics(tu.0) } {
        let diagnostic = unsafe { clang_getDiagnostic(tu.0, i) };
        diagnostics.push(Diagnostic {
            severity: unsafe { clang_getDiagnosticSeverity(diagnostic) },
            message: string(unsafe {
                clang_formatDiagnostic(diagnostic, clang_defaultDiagnosticDisplayOptions())
            }),
            location: location(unsafe { clang_getDiagnosticLocation(diagnostic) }, false),
        });
        unsafe { clang_disposeDiagnostic(diagnostic) };
    }
    measured.diagnostics_micros = diagnostics_clock.micros();
    if code != CXError_Success || diagnostics.iter().any(|d| d.severity >= CXDiagnostic_Error) {
        measured.provider_wall_micros = provider_clock.micros();
        return Ok(WorkerReply {
            snapshot: None,
            error: Some("compiler errors; no current semantic snapshot published".into()),
            diagnostics,
            profile: profiling.then_some(crate::profile::RunProfile {
                worker: Some(measured),
                ..Default::default()
            }),
        });
    }
    let started = Instant::now();
    let mut walk = Walker {
        command,
        tu: tu.0,
        symbols: Vec::new(),
        references: Vec::new(),
        calls: Vec::new(),
        owner: None,
        depth: 0,
        visited: 0,
        template_instantiations: 0,
        error: None,
    };
    unsafe {
        clang_visitChildren(
            clang_getTranslationUnitCursor(tu.0),
            visitor,
            &mut walk as *mut _ as CXClientData,
        )
    };
    if let Some(error) = walk.error {
        return Err(error.into());
    }
    measured.fact_extract_micros = started.elapsed().as_micros() as u64;
    let validation_clock = crate::profile::Clock::start(profiling);
    let mut files = BTreeMap::new();
    unsafe { clang_getInclusions(tu.0, inclusion, &mut files as *mut _ as CXClientData) };
    let main = unsafe { clang_getFile(tu.0, file.as_ptr()) };
    files.insert(command.file.to_string_lossy().into_owned(), main);
    let overlays: HashMap<_, _> = request
        .overlays
        .iter()
        .map(|o| (o.path.as_str(), o.content.as_bytes()))
        .collect();
    let mut inputs = command.extra_inputs.clone();
    let mut total_bytes = 0usize;
    for (path, file) in files {
        let path = if Path::new(&path).is_absolute() {
            path
        } else {
            command.directory.join(path).to_string_lossy().into_owned()
        };
        let mut length = 0;
        let bytes = unsafe { clang_getFileContents(tu.0, file, &mut length) };
        if bytes.is_null() {
            return Err(format!("cannot fingerprint parsed input {path}").into());
        }
        total_bytes = total_bytes
            .checked_add(length)
            .ok_or("input size overflow")?;
        if total_bytes > 256 * 1024 * 1024 {
            return Err("TU input budget exceeds 256 MiB".into());
        }
        let parsed = unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), length) };
        let current = if let Some(content) = overlays.get(path.as_str()) {
            content.to_vec()
        } else {
            fs::read(&path)?
        };
        if parsed != current {
            return Err(format!("input changed during analysis: {path}").into());
        }
        inputs.push(Input {
            path,
            hash: symweft_index::hash(parsed),
        });
    }
    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    inputs.dedup_by(|a, b| a.path == b.path);
    measured.input_validation_micros = validation_clock.micros();
    let usage = unsafe { clang_getCXTUResourceUsage(tu.0) };
    let entries = if usage.numEntries == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(usage.entries, usage.numEntries as _) }
    };
    #[allow(clippy::unnecessary_cast)] // C unsigned long is 32 bits on some targets.
    let clang_accounted_bytes = entries
        .iter()
        .filter(|e| {
            e.kind >= CXTUResourceUsage_AST && e.kind <= CXTUResourceUsage_Preprocessor_HeaderSearch
        })
        .map(|e| e.amount as u64)
        .sum();
    unsafe { clang_disposeCXTUResourceUsage(usage) };
    let worker_peak_rss_bytes = peak_rss();
    let snapshot = Snapshot {
        unit_id: command.unit_id.clone(),
        configuration: command.configuration.clone(),
        main_file: command.file.to_string_lossy().into_owned(),
        command_key: command.command_key.clone(),
        provider,
        inputs,
        symbols: walk.symbols,
        references: walk.references,
        calls: walk.calls,
        diagnostics: diagnostics.clone(),
        parse_micros,
        extract_micros: started.elapsed().as_micros() as u64,
        clang_accounted_bytes,
        worker_peak_rss_bytes,
    };
    measured.provider_wall_micros = provider_clock.micros();
    Ok(WorkerReply {
        snapshot: Some(snapshot),
        error: None,
        diagnostics,
        profile: profiling.then_some(crate::profile::RunProfile {
            worker: Some(measured),
            ..Default::default()
        }),
    })
}

fn peak_rss() -> u64 {
    #[cfg(unix)]
    {
        let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        if cfg!(target_os = "macos") {
            usage.ru_maxrss as u64
        } else {
            usage.ru_maxrss as u64 * 1024
        }
    }
    #[cfg(not(unix))]
    {
        0
    }
}
