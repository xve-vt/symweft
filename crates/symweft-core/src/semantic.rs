use super::*;
use symweft_semantic::service::{Controller, Limits};
fn storage(root: &Path) -> PathBuf {
    cache_directory(root)
        .parent()
        .expect("cache has parent")
        .join("a2-clang-v2/index.sqlite3")
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
        "semantic.symbols" => semantic
            .store()
            .map_err(index_error)?
            .symbols(
                &semantic.database.configuration,
                parameter(params, "name")?,
                &semantic.provider,
                limit,
            )
            .map_err(index_error),
        "semantic.relations" => semantic
            .store()
            .map_err(index_error)?
            .relations(
                &semantic.database.configuration,
                parameter(params, "entity_id")?,
                parameter(params, "direction")?,
                &semantic.provider,
                limit,
            )
            .map_err(index_error),
        "semantic.at" => {
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
            semantic
                .store()
                .map_err(index_error)?
                .at(
                    &semantic.database.configuration,
                    &path,
                    number(params, "byte_offset")?,
                    &semantic.provider,
                )
                .map_err(index_error)
        }
        "semantic.preview" => {
            let path = parameter(params, "path")?;
            let revision = number(params, "revision")?;
            let source = session
                .engine
                .source(path, revision)
                .map_err(analysis_error)?;
            let mut result = semantic
                .preview(path, source, params.get("unit_id").and_then(Value::as_str))
                .map_err(index_error)?;
            result["revision"] = revision.into();
            Ok(result)
        }
        "semantic.preview.start"
        | "semantic.preview.progress"
        | "semantic.preview.at"
        | "semantic.preview.relations" => {
            let path = parameter(params, "path")?;
            let revision = number(params, "revision")?;
            let source = session
                .engine
                .source(path, revision)
                .map_err(analysis_error)?;
            if request.method == "semantic.preview.start" {
                let full = semantic.absolute_path(path).map_err(index_error)?;
                for dirty in &session.semantic_dirty {
                    if semantic.absolute_path(dirty).map_err(index_error)? != full {
                        return Err(CoreError::new(
                            "multiple_dirty_buffers",
                            "Preview currently supports one unsaved buffer; close or reload other edited buffers first.",
                        ));
                    }
                }
                return semantic
                    .start_preview(
                        path,
                        revision,
                        source,
                        params.get("unit_id").and_then(Value::as_str),
                        params
                            .get("memory_mib")
                            .and_then(Value::as_u64)
                            .unwrap_or(32),
                    )
                    .map_err(index_error);
            }
            let job_id = number(params, "job_id")?;
            match request.method.as_str() {
                "semantic.preview.progress" => semantic
                    .check_preview(job_id, path, revision)
                    .map_err(index_error),
                "semantic.preview.at" => {
                    let byte = offset(params, "byte_offset")?;
                    if byte > source.len() || !source.is_char_boundary(byte) {
                        return Err(CoreError::new(
                            "invalid_params",
                            "Byte offset must be a UTF-8 boundary within the buffer.",
                        ));
                    }
                    semantic
                        .preview_at(job_id, path, revision, byte as u64)
                        .map_err(index_error)
                }
                _ => semantic
                    .preview_relations(
                        job_id,
                        path,
                        revision,
                        parameter(params, "entity_id")?,
                        parameter(params, "direction")?,
                        limit,
                    )
                    .map_err(index_error),
            }
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
