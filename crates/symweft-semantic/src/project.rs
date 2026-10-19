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
        let completeness = status
            .get("completeness")
            .and_then(Value::as_str)
            .unwrap_or(COMPLETENESS);
        let includer_coverage = status.get("includer_coverage").cloned();
        Ok(json!({
            "evidence": "clang_semantic_project",
            "completeness": completeness,
            "includer_coverage": includer_coverage,
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
        let mut overlay_rows = take_rows(&mut overlay, "rows");
        if let Some(ticket) = &self.overlay.ticket {
            for (sec_unit_id, completed) in &self.overlay.results {
                if sec_unit_id != &unit_id {
                    let matches_current = completed.reply.snapshot.as_ref().map_or(false, |s| {
                        ticket.documents.iter().all(|doc| {
                            s.inputs.iter().any(|inp| inp.path == doc.path && inp.hash == doc.hash)
                        })
                    });
                    if matches_current {
                        if let Some(snapshot) = &completed.reply.snapshot {
                            let sec_rows: Vec<_> = match direction {
                                "definitions" | "declarations" => snapshot
                                    .symbols
                                    .iter()
                                    .filter(|v| v.id == entity && (direction != "definitions" || v.definition))
                                    .take(limit)
                                    .map(|v| self.overlay_row_for(v, sec_unit_id, ticket.revision, ticket.job_id, &snapshot.main_file))
                                    .collect(),
                                "references" => snapshot
                                    .references
                                    .iter()
                                    .filter(|v| v.target == entity)
                                    .take(limit)
                                    .map(|v| self.overlay_row_for(v, sec_unit_id, ticket.revision, ticket.job_id, &snapshot.main_file))
                                    .collect(),
                                "incoming" => snapshot
                                    .calls
                                    .iter()
                                    .filter(|v| v.target == entity)
                                    .take(limit)
                                    .map(|v| self.overlay_row_for(v, sec_unit_id, ticket.revision, ticket.job_id, &snapshot.main_file))
                                    .collect(),
                                "outgoing" => snapshot
                                    .calls
                                    .iter()
                                    .filter(|v| v.caller == entity)
                                    .take(limit)
                                    .map(|v| self.overlay_row_for(v, sec_unit_id, ticket.revision, ticket.job_id, &snapshot.main_file))
                                    .collect(),
                                _ => Vec::new(),
                            };
                            overlay_rows.extend(sec_rows);
                        }
                    }
                }
            }
        }
        let rows = merge_rows(
            overlay_rows,
            take_rows(&mut disk, "rows"),
            limit,
        );
        let completeness = status
            .get("completeness")
            .and_then(Value::as_str)
            .unwrap_or(COMPLETENESS);
        let includer_coverage = status.get("includer_coverage").cloned();
        Ok(json!({
            "evidence": "clang_semantic_project",
            "completeness": completeness,
            "includer_coverage": includer_coverage,
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
