use super::*;
use crate::model::{DiskBaseline, QueryVersion};

const COMPLETENESS: &str = "current overlay TU plus other validated persistent contributions; the overlay unit's disk contribution is masked; unindexed units/changes may exist and results are not stable pages";

fn take_rows(result: &mut Value, key: &str) -> Vec<Value> {
    result
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default()
}

fn take_disk_baseline(result: &mut Value) -> Result<DiskBaseline> {
    let baseline = result
        .as_object_mut()
        .and_then(|object| object.remove("disk_baseline"))
        .ok_or("project disk query baseline unavailable")?;
    Ok(serde_json::from_value(baseline)?)
}

fn project_query_version(status: &Value, disk: DiskBaseline) -> Result<QueryVersion> {
    let mut version: QueryVersion =
        serde_json::from_value(status["project_query_version"].clone())?;
    version.disk = disk;
    Ok(version)
}

fn overlay_unit(status: &Value) -> Result<String> {
    status["ticket"]["unit_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "project preview unit unavailable".into())
}

fn merge_rows(mut overlay: Vec<Value>, disk: Vec<Value>, limit: usize) -> Vec<Value> {
    overlay.extend(disk);
    overlay.truncate(limit);
    overlay
}

fn unique_entity(references: &[Value], declarations: &[Value]) -> Option<String> {
    let entities: HashSet<&str> = references
        .iter()
        .filter_map(|row| row.get("target").and_then(Value::as_str))
        .chain(
            declarations
                .iter()
                .filter_map(|row| row.get("id").and_then(Value::as_str)),
        )
        .collect();
    if entities.len() == 1 {
        entities.into_iter().next().map(str::to_owned)
    } else {
        None
    }
}

impl Controller {
    pub fn project_at(
        &mut self,
        job_id: u64,
        path: &str,
        revision: u64,
        offset: u64,
    ) -> Result<Value> {
        let status = self.check_preview(job_id, path, revision)?;
        let unit_id = overlay_unit(&status)?;
        let full = self.absolute_path(path)?;
        let mut overlay = self.preview_at(job_id, path, revision, offset)?;
        let mut disk = self.query_store.at_except(
            &self.database.configuration,
            &full,
            offset,
            &self.provider,
            &unit_id,
        )?;
        let query_version = project_query_version(&status, take_disk_baseline(&mut disk)?)?;
        let references = merge_rows(
            take_rows(&mut overlay, "references"),
            take_rows(&mut disk, "references"),
            32,
        );
        let declarations = merge_rows(
            take_rows(&mut overlay, "declarations"),
            take_rows(&mut disk, "declarations"),
            32,
        );
        let enclosing_functions = merge_rows(
            take_rows(&mut overlay, "enclosing_functions"),
            take_rows(&mut disk, "enclosing_functions"),
            32,
        );
        let entity = unique_entity(&references, &declarations);
        Ok(json!({
            "evidence": "clang_semantic_project",
            "completeness": COMPLETENESS,
            "entity_id": entity,
            "references": references,
            "declarations": declarations,
            "enclosing_functions": enclosing_functions,
            "overlay_unit_id": unit_id,
            "query_version": query_version,
        }))
    }

    pub fn project_relations(
        &mut self,
        job_id: u64,
        path: &str,
        revision: u64,
        entity: &str,
        direction: &str,
        limit: usize,
    ) -> Result<Value> {
        let started = Instant::now();
        let status = self.check_preview(job_id, path, revision)?;
        let unit_id = overlay_unit(&status)?;
        let mut overlay =
            self.preview_relations(job_id, path, revision, entity, direction, limit)?;
        let mut disk = self.query_store.relations_except(
            &self.database.configuration,
            entity,
            direction,
            &self.provider,
            &unit_id,
            limit,
        )?;
        let query_version = project_query_version(&status, take_disk_baseline(&mut disk)?)?;
        let limit = limit.clamp(1, 1000);
        let rows = merge_rows(
            take_rows(&mut overlay, "rows"),
            take_rows(&mut disk, "rows"),
            limit,
        );
        Ok(json!({
            "evidence": "clang_semantic_project",
            "completeness": COMPLETENESS,
            "entity_id": entity,
            "direction": direction,
            "rows": rows,
            "limit": limit,
            "query_micros": started.elapsed().as_micros(),
            "overlay_unit_id": unit_id,
            "query_version": query_version,
        }))
    }
}
