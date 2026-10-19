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
        let failed_units = status.get("failed_units").cloned().unwrap_or_else(|| json!([]));
        Ok(json!({
            "evidence": "clang_semantic_project",
            "completeness": completeness,
            "includer_coverage": includer_coverage,
            "failed_units": failed_units,
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
        self.project_relations_page(job_id, path, revision, entity, direction, limit, 0)
    }

    pub fn project_relations_page(
        &mut self,
        job_id: u64,
        path: &str,
        revision: u64,
        entity: &str,
        direction: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        let started = Instant::now();
        let status = self.check_preview(job_id, path, revision)?;
        let unit_id = overlay_unit(&status)?;
        let limit = limit.clamp(1, 1000);

        let mut all_overlay_rows = Vec::new();
        let primary_snapshot = self.overlay_snapshot()?;
        let primary_unit = unit_id.clone();
        let primary_file = primary_snapshot.main_file.clone();
        let ticket = self.overlay.ticket.as_ref();
        let rev = ticket.map_or(0, |t| t.revision);
        let jid = ticket.map_or(0, |t| t.job_id);
        let p_rows: Vec<_> = match direction {
            "definitions" | "declarations" => primary_snapshot
                .symbols
                .iter()
                .filter(|v| v.id == entity && (direction != "definitions" || v.definition))
                .map(|v| self.overlay_row_for(v, &primary_unit, rev, jid, &primary_file))
                .collect(),
            "references" => primary_snapshot
                .references
                .iter()
                .filter(|v| v.target == entity)
                .map(|v| self.overlay_row_for(v, &primary_unit, rev, jid, &primary_file))
                .collect(),
            "incoming" | "outgoing" => primary_snapshot
                .calls
                .iter()
                .filter(|v| {
                    if direction == "incoming" {
                        v.target.as_deref() == Some(entity)
                    } else {
                        v.caller.as_deref() == Some(entity)
                    }
                })
                .map(|v| self.overlay_row_for(v, &primary_unit, rev, jid, &primary_file))
                .collect(),
            _ => Vec::new(),
        };
        all_overlay_rows.extend(p_rows);

        if let Some(ticket) = &self.overlay.ticket {
            for (sec_unit_id, completed) in &self.overlay.results {
                if sec_unit_id != &unit_id {
                    let matches_current = completed.reply.snapshot.as_ref().map_or(false, |s| {
                        s.inputs.iter().all(|inp| {
                            if let Some(doc) = ticket.documents.iter().find(|d| d.path == inp.path) {
                                inp.hash == doc.hash
                            } else {
                                true
                            }
                        })
                    });
                    if matches_current {
                        if let Some(snapshot) = &completed.reply.snapshot {
                            let sec_rows: Vec<_> = match direction {
                                "definitions" | "declarations" => snapshot
                                    .symbols
                                    .iter()
                                    .filter(|v| v.id == entity && (direction != "definitions" || v.definition))
                                    .map(|v| self.overlay_row_for(v, sec_unit_id, ticket.revision, ticket.job_id, &snapshot.main_file))
                                    .collect(),
                                "references" => snapshot
                                    .references
                                    .iter()
                                    .filter(|v| v.target == entity)
                                    .map(|v| self.overlay_row_for(v, sec_unit_id, ticket.revision, ticket.job_id, &snapshot.main_file))
                                    .collect(),
                                "incoming" | "outgoing" => snapshot
                                    .calls
                                    .iter()
                                    .filter(|v| {
                                        if direction == "incoming" {
                                            v.target.as_deref() == Some(entity)
                                        } else {
                                            v.caller.as_deref() == Some(entity)
                                        }
                                    })
                                    .map(|v| self.overlay_row_for(v, sec_unit_id, ticket.revision, ticket.job_id, &snapshot.main_file))
                                    .collect(),
                                _ => Vec::new(),
                            };
                            all_overlay_rows.extend(sec_rows);
                        }
                    }
                }
            }
        }

        let total_overlay = all_overlay_rows.len();

        let (overlay_page_rows, disk_limit, disk_offset) = if offset < total_overlay {
            let end = std::cmp::min(offset + limit, total_overlay);
            let taken = all_overlay_rows[offset..end].to_vec();
            let needed = limit - taken.len();
            (taken, needed, 0)
        } else {
            (Vec::new(), limit, offset - total_overlay)
        };

        let mut disk = self.query_store.relations_except_page(
            &self.database.configuration,
            entity,
            direction,
            &self.provider,
            &unit_id,
            disk_limit,
            disk_offset,
        )?;

        let disk_total = disk.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
        let total = total_overlay + disk_total;

        let mut page_rows = overlay_page_rows;
        page_rows.extend(take_rows(&mut disk, "rows"));

        let has_more = offset + page_rows.len() < total;
        let truncated = has_more;
        let truncation_reason = if truncated {
            Some(format!(
                "results truncated at page limit {limit}; {} row(s) remain; fetch next page with offset {}",
                total.saturating_sub(offset + page_rows.len()),
                offset + page_rows.len()
            ))
        } else {
            None
        };

        let base_completeness = status
            .get("completeness")
            .and_then(Value::as_str)
            .unwrap_or(COMPLETENESS);

        let completeness = if truncated {
            let page = (offset / limit) + 1;
            let total_pages = (total + limit - 1) / limit;
            format!(
                "showing rows {}..{} of {total}; page {page} of {total_pages}; {base_completeness}",
                offset,
                offset + page_rows.len()
            )
        } else if total > 0 && offset > 0 {
            format!(
                "showing rows {}..{} of {total} (final page); {base_completeness}",
                offset,
                offset + page_rows.len()
            )
        } else {
            base_completeness.to_string()
        };

        let query_version = project_query_version(&status, take_disk_baseline(&mut disk)?)?;
        let includer_coverage = status.get("includer_coverage").cloned();
        let failed_units = status.get("failed_units").cloned().unwrap_or_else(|| json!([]));

        Ok(json!({
            "evidence": "clang_semantic_project",
            "completeness": completeness,
            "includer_coverage": includer_coverage,
            "failed_units": failed_units,
            "entity_id": entity,
            "direction": direction,
            "rows": page_rows,
            "limit": limit,
            "offset": offset,
            "total": total,
            "has_more": has_more,
            "truncated": truncated,
            "truncation_reason": truncation_reason,
            "query_micros": started.elapsed().as_micros(),
            "overlay_unit_id": unit_id,
            "query_version": query_version,
        }))
    }
}
