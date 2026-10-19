use super::*;
use symweft_semantic::{
    model::{
        DiskBaseline, MAX_OVERLAY_DOCUMENTS, MAX_OVERLAY_SOURCE_BYTES, PreviewDocument,
        QUERY_VERSION_CONTRACT, QueryCoverage, QueryVersion,
    },
    service::{Controller, Limits},
};
fn storage(root: &Path) -> PathBuf {
    cache_directory(root)
        .parent()
        .expect("cache has parent")
        .join("a2-clang-v2/index.sqlite3")
}
fn query_version_parameter(params: &Value) -> Result<Option<QueryVersion>, CoreError> {
    params
        .get("query_version")
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|error| {
                CoreError::new("invalid_params", format!("Invalid query_version: {error}"))
            })
        })
        .transpose()
}
fn validate_query_version(expected: &QueryVersion, actual: &QueryVersion) -> Result<(), CoreError> {
    if expected.contract != QUERY_VERSION_CONTRACT {
        return Err(CoreError::new(
            "unsupported_query_version",
            format!("Expected query contract {QUERY_VERSION_CONTRACT}; refresh the query version."),
        ));
    }
    if expected != actual {
        return Err(CoreError::new(
            "stale_query_version",
            "Query version no longer matches the workspace, configuration, disk baseline, documents, or coverage; refresh it before querying.",
        ));
    }
    Ok(())
}
fn finish_disk_query(
    semantic: &Controller,
    mut result: Value,
    expected: Option<&QueryVersion>,
) -> Result<Value, CoreError> {
    let baseline = result
        .as_object_mut()
        .and_then(|object| object.remove("disk_baseline"))
        .ok_or_else(|| CoreError::new("index_error", "Semantic query baseline is missing."))?;
    let baseline: DiskBaseline = serde_json::from_value(baseline).map_err(index_error)?;
    let actual = semantic.query_version(baseline, Vec::new(), QueryCoverage::PersistentIndex);
    if let Some(expected) = expected {
        validate_query_version(expected, &actual)?;
    }
    result["query_version"] = serde_json::to_value(actual).map_err(index_error)?;
    Ok(result)
}
fn validate_result_query_version(
    result: &Value,
    expected: Option<&QueryVersion>,
) -> Result<(), CoreError> {
    if let Some(expected) = expected {
        let actual: QueryVersion =
            serde_json::from_value(result["query_version"].clone()).map_err(index_error)?;
        validate_query_version(expected, &actual)?;
    }
    Ok(())
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewDocumentParam {
    path: String,
    revision: u64,
}
fn preview_documents(
    engine: &Engine,
    semantic_dirty: &std::collections::HashSet<String>,
    params: &Value,
    primary_path: &str,
    primary_revision: u64,
    primary_content: &str,
) -> Result<Vec<PreviewDocument>, CoreError> {
    let Some(mut documents) = params
        .get("documents")
        .map(|value| {
            serde_json::from_value::<Vec<PreviewDocumentParam>>(value.clone()).map_err(|error| {
                CoreError::new(
                    "invalid_params",
                    format!("Invalid preview documents: {error}"),
                )
            })
        })
        .transpose()?
    else {
        for dirty in semantic_dirty {
            if dirty != primary_path {
                return Err(CoreError::new(
                    "multiple_dirty_buffers",
                    "Preview without documents supports one unsaved buffer; include every dirty buffer in documents.",
                ));
            }
        }
        return Ok(vec![PreviewDocument {
            path: primary_path.into(),
            revision: primary_revision,
            content: primary_content.into(),
        }]);
    };
    if documents.is_empty() {
        return Err(CoreError::new(
            "invalid_params",
            "Preview documents must include the primary path/revision.",
        ));
    }
    if documents.len() > MAX_OVERLAY_DOCUMENTS {
        return Err(CoreError::new(
            "overlay_budget",
            format!("Preview accepts at most {MAX_OVERLAY_DOCUMENTS} documents."),
        ));
    }
    let mut paths = std::collections::HashSet::new();
    for document in &documents {
        if !paths.insert(document.path.clone()) {
            return Err(CoreError::new(
                "invalid_params",
                format!("Duplicate preview document: {}", document.path),
            ));
        }
    }
    let missing = semantic_dirty.difference(&paths).count();
    if missing > 0 {
        return Err(CoreError::new(
            "missing_overlay_documents",
            format!(
                "Preview documents are missing {missing} dirty buffers; refusing to read their disk contents."
            ),
        ));
    }
    let primary_index = documents
        .iter()
        .position(|document| document.path == primary_path && document.revision == primary_revision)
        .ok_or_else(|| {
            CoreError::new(
                "invalid_params",
                "Preview documents must contain the primary path and revision.",
            )
        })?;
    let primary = documents.remove(primary_index);
    documents.insert(0, primary);
    let mut total_bytes = 0usize;
    let mut output = Vec::with_capacity(documents.len());
    for document in documents {
        let content = if document.path == primary_path && document.revision == primary_revision {
            primary_content.to_owned()
        } else {
            engine
                .source(&document.path, document.revision)
                .map_err(analysis_error)?
                .to_owned()
        };
        total_bytes = total_bytes
            .checked_add(content.len())
            .ok_or_else(|| CoreError::new("overlay_budget", "Preview source size overflow."))?;
        if content.len() > MAX_OVERLAY_SOURCE_BYTES || total_bytes > MAX_OVERLAY_SOURCE_BYTES {
            return Err(CoreError::new(
                "overlay_budget",
                "Preview source exceeds the 8 MiB total budget.",
            ));
        }
        output.push(PreviewDocument {
            path: document.path,
            revision: document.revision,
            content,
        });
    }
    Ok(output)
}
pub fn dispatch(request: &Request, session: &mut Session) -> Result<Value, CoreError> {
    let params = &request.params;
    if request.method == "semantic.configure" {
        let root = session
            .root
            .as_ref()
            .ok_or_else(|| CoreError::new("workspace_required", "Open a workspace first."))?;
        let path = parameter(params, "compile_commands")?;
        let path = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            root.join(path)
        };
        let limits: Limits = serde_json::from_value(params.clone()).map_err(index_error)?;
        session.semantic.take();
        session.semantic = Some(
            Controller::open(
                root,
                &path,
                storage(root),
                std::env::current_exe().map_err(index_error)?,
                limits,
            )
            .map_err(index_error)?,
        );
        return session
            .semantic
            .as_ref()
            .expect("just configured")
            .status()
            .map_err(index_error);
    }
    if request.method == "semantic.status" && session.semantic.is_none() {
        return Ok(json!({"configured":false}));
    }
    let semantic = session.semantic.as_mut().ok_or_else(|| {
        CoreError::new(
            "semantic_not_configured",
            "Select compile_commands.json first.",
        )
    })?;
    if request.method == "semantic.cancel" {
        semantic.cancel();
        return Ok(json!({"cancelled":true}));
    }
    if request.method == "semantic.preview.cancel" {
        return semantic
            .cancel_preview(number(params, "job_id")?)
            .map_err(index_error);
    }
    if request.method == "semantic.watch" {
        let settings = serde_json::from_value(params.clone()).map_err(index_error)?;
        return semantic.configure_watch(settings).map_err(index_error);
    }
    if request.method != "semantic.cancel" {
        semantic
            .maintain(
                &session.semantic_dirty,
                !matches!(
                    request.method.as_str(),
                    "semantic.index" | "semantic.refresh" | "semantic.preview.start"
                ),
            )
            .map_err(index_error)?;
    }
    for path in &session.semantic_dirty {
        semantic.invalidate_path(path).map_err(index_error)?;
    }
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
    match request.method.as_str() {
        "semantic.refresh" => semantic.refresh_recorded().map_err(index_error),
        "semantic.index" => {
            let paths: Vec<String> =
                serde_json::from_value(params.get("paths").cloned().unwrap_or_else(|| json!([])))
                    .map_err(index_error)?;
            semantic
                .index(
                    &paths,
                    params
                        .get("limit")
                        .and_then(Value::as_u64)
                        .unwrap_or(100_000) as usize,
                )
                .map_err(index_error)
        }
        "semantic.status" => semantic.status().map_err(index_error),
        "semantic.progress" => semantic.progress().map_err(index_error),
        "semantic.invalidate" => {
            let path = semantic
                .absolute_path(parameter(params, "path")?)
                .map_err(index_error)?;
            let count = semantic
                .store()
                .map_err(index_error)?
                .invalidate(&semantic.database.configuration, &path)
                .map_err(index_error)?;
            Ok(json!({"invalidated_units":count}))
        }
        "semantic.symbols" => {
            let expected = query_version_parameter(params)?;
            let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            if offset > 0 && expected.is_none() {
                return Err(CoreError::new(
                    "missing_query_version",
                    "Paging with offset > 0 requires a stable query_version from previous page.",
                ));
            }
            let result = semantic
                .store()
                .map_err(index_error)?
                .symbols_page(
                    &semantic.database.configuration,
                    parameter(params, "name")?,
                    &semantic.provider,
                    limit,
                    offset,
                )
                .map_err(index_error)?;
            finish_disk_query(semantic, result, expected.as_ref())
        }
        "semantic.relations" => {
            let expected = query_version_parameter(params)?;
            let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            if offset > 0 && expected.is_none() {
                return Err(CoreError::new(
                    "missing_query_version",
                    "Paging with offset > 0 requires a stable query_version from previous page.",
                ));
            }
            let result = semantic
                .store()
                .map_err(index_error)?
                .relations_page(
                    &semantic.database.configuration,
                    parameter(params, "entity_id")?,
                    parameter(params, "direction")?,
                    &semantic.provider,
                    limit,
                    offset,
                )
                .map_err(index_error)?;
            finish_disk_query(semantic, result, expected.as_ref())
        }
        "semantic.at" => {
            let expected = query_version_parameter(params)?;
            let requested = parameter(params, "path")?;
            let path = semantic.absolute_path(requested).map_err(index_error)?;
            if let Some((opened, hash)) = &session.opened_hash {
                if semantic.absolute_path(opened).map_err(index_error)? == path
                    && semantic
                        .store()
                        .map_err(index_error)?
                        .input_digest(Path::new(&path))
                        != Some(*hash)
                {
                    return Err(CoreError::new(
                        "stale_document",
                        "Reader content differs from disk; reopen or use a semantic overlay preview.",
                    ));
                }
            }
            let result = semantic
                .store()
                .map_err(index_error)?
                .at(
                    &semantic.database.configuration,
                    &path,
                    number(params, "byte_offset")?,
                    &semantic.provider,
                )
                .map_err(index_error)?;
            finish_disk_query(semantic, result, expected.as_ref())
        }
        "semantic.preview" => {
            let path = parameter(params, "path")?;
            let revision = number(params, "revision")?;
            let source = session
                .engine
                .source(path, revision)
                .map_err(analysis_error)?;
            if params.get("documents").is_some() {
                return Err(CoreError::new(
                    "invalid_params",
                    "Synchronous semantic.preview accepts one document; use semantic.preview.start for multiple buffers.",
                ));
            }
            let documents = preview_documents(
                &session.engine,
                &session.semantic_dirty,
                params,
                path,
                revision,
                source,
            )?;
            let source = &documents[0].content;
            let mut result = semantic
                .preview(path, source, params.get("unit_id").and_then(Value::as_str))
                .map_err(index_error)?;
            result["revision"] = revision.into();
            Ok(result)
        }
        "semantic.preview.start"
        | "semantic.preview.progress"
        | "semantic.preview.at"
        | "semantic.preview.relations"
        | "semantic.project.at"
        | "semantic.project.relations" => {
            let path = parameter(params, "path")?;
            let revision = number(params, "revision")?;
            let source = session
                .engine
                .source(path, revision)
                .map_err(analysis_error)?;
            if request.method == "semantic.preview.start" {
                if let (Ok(full), None) = (semantic.absolute_path(path), params.get("documents")) {
                    if let Err(err) = semantic.preview_command(&full, params.get("unit_id").and_then(Value::as_str)) {
                        return Err(index_error(err));
                    }
                }
                let documents = preview_documents(
                    &session.engine,
                    &session.semantic_dirty,
                    params,
                    path,
                    revision,
                    source,
                )?;
                return semantic
                    .start_preview_documents(
                        documents,
                        params.get("unit_id").and_then(Value::as_str),
                        params
                            .get("memory_mib")
                            .and_then(Value::as_u64)
                            .unwrap_or(32),
                    )
                    .map_err(index_error);
            }
            let job_id = number(params, "job_id")?;
            let expected = query_version_parameter(params)?;
            let offset_val = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            if offset_val > 0 && expected.is_none() {
                return Err(CoreError::new(
                    "missing_query_version",
                    "Paging with offset > 0 requires a stable query_version from previous page.",
                ));
            }
            if request.method.starts_with("semantic.project.") && expected.is_none() {
                return Err(CoreError::new(
                    "invalid_params",
                    "Project queries require a current project query_version.",
                ));
            }
            let result = match request.method.as_str() {
                "semantic.preview.progress" => semantic
                    .check_preview(job_id, path, revision)
                    .map_err(index_error)?,
                "semantic.preview.at" | "semantic.project.at" => {
                    let byte = offset(params, "byte_offset")?;
                    if byte > source.len() || !source.is_char_boundary(byte) {
                        return Err(CoreError::new(
                            "invalid_params",
                            "Byte offset must be a UTF-8 boundary within the buffer.",
                        ));
                    }
                    if request.method == "semantic.project.at" {
                        semantic
                            .project_at(job_id, path, revision, byte as u64)
                            .map_err(index_error)?
                    } else {
                        semantic
                            .preview_at(job_id, path, revision, byte as u64)
                            .map_err(index_error)?
                    }
                }
                "semantic.preview.relations" => {
                    let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if offset > 0 && expected.is_none() {
                        return Err(CoreError::new(
                            "missing_query_version",
                            "Paging with offset > 0 requires a stable query_version from previous page.",
                        ));
                    }
                    semantic
                        .preview_relations_page(
                            job_id,
                            path,
                            revision,
                            parameter(params, "entity_id")?,
                            parameter(params, "direction")?,
                            limit,
                            offset,
                        )
                        .map_err(index_error)?
                }
                "semantic.project.relations" => {
                    let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if offset > 0 && expected.is_none() {
                        return Err(CoreError::new(
                            "missing_query_version",
                            "Paging with offset > 0 requires a stable query_version from previous page.",
                        ));
                    }
                    semantic
                        .project_relations_page(
                            job_id,
                            path,
                            revision,
                            parameter(params, "entity_id")?,
                            parameter(params, "direction")?,
                            limit,
                            offset,
                        )
                        .map_err(index_error)?
                }
                _ => {
                    return Err(CoreError::new(
                        "method_not_found",
                        "Unknown semantic method.",
                    ));
                }
            };
            if request.method != "semantic.preview.progress" {
                validate_result_query_version(&result, expected.as_ref())?;
            }
            Ok(result)
        }
        _ => Err(CoreError::new(
            "method_not_found",
            "Unknown semantic method.",
        )),
    }
}
pub fn index_cli(args: &[String]) -> symweft_semantic::Result<()> {
    let root = Path::new(&args[0]).canonicalize()?;
    let path = Path::new(&args[1]);
    let mut paths = Vec::new();
    let mut limit = 100_000;
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--limit" {
            i += 1;
            limit = args.get(i).ok_or("missing limit")?.parse()?;
        } else {
            paths.push(args[i].clone());
        }
        i += 1;
    }
    let mut controller = Controller::open(
        &root,
        path,
        storage(&root),
        std::env::current_exe()?,
        Limits::default(),
    )?;
    controller.index(&paths, limit)?;
    let status = controller.wait()?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    if status["job"]["state"] != "complete" {
        return Err("semantic indexing did not complete without errors; inspect report".into());
    }
    Ok(())
}
