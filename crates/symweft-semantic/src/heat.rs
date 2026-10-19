//! A2 structural heat calculation with precise deduplication across TU variants
//! and explicit separation from A1 syntactic proxies.

use crate::Result;
use crate::{
    hot_pool::{PoolMode, SemanticHotPool},
    model::QueryVersion,
    prefetch::SemanticPrefetcher,
    store::Store,
};
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Precise structural heat metrics for a semantic entity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityStructuralHeat {
    pub entity_key: String,
    pub name: String,
    pub kind: String,
    pub main_file: String,
    /// Number of distinct call expressions targeting this entity after source-location deduplication.
    pub call_sites: u64,
    /// Number of distinct caller entities calling this entity (fan-in).
    pub distinct_callers: u64,
    /// Total raw call sites before TU variant deduplication.
    pub raw_call_sites: u64,
    /// Number of candidate/indirect dispatch call edges.
    pub candidate_call_sites: u64,
    /// Non-call references targeting this entity.
    pub reference_count: u64,
    /// Declaration and definition occurrences across active compilation units.
    pub declaration_count: u64,
    /// In-degree: number of distinct caller entities.
    pub fan_in: u64,
    /// Out-degree: number of distinct callee entities called by this entity.
    pub fan_out: u64,
    /// Deduplication policy statement.
    pub dedup_policy: String,
    /// Evidence level: clang_semantic (never mixed with syntactic text proxies).
    pub evidence_level: String,
    /// A1 syntactic proxy occurrences (explicitly separated, not conflated with resolved calls).
    pub syntactic_incoming_proxy: Option<u64>,
    /// Logarithmic structural importance score.
    pub structural_score: f64,
}

/// Structural heat metrics aggregated at the source file level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileStructuralHeat {
    pub path: String,
    pub total_symbols: u64,
    pub internal_calls: u64,
    pub incoming_calls: u64,
    pub outgoing_calls: u64,
    pub distinct_caller_files: u64,
    pub distinct_callee_files: u64,
    pub dedup_policy: String,
    pub evidence_level: String,
    pub structural_score: f64,
}

pub fn compute_entity_structural_heat(
    conn: &Connection,
    configuration: &str,
    entity_key: &str,
) -> Result<Option<EntityStructuralHeat>> {
    let entity_pk: Option<i64> = conn
        .query_row(
            "SELECT pk FROM entities WHERE key = ?1",
            params![entity_key],
            |r| r.get(0),
        )
        .optional()?;

    let Some(entity_pk) = entity_pk else {
        return Ok(None);
    };

    // 1. Primary symbol details
    let symbol_info: Option<(String, String, String)> = conn
        .query_row(
            r#"SELECT s.name, u.main_file, p.key
               FROM symbols s
               JOIN units u ON u.pk = s.unit_pk
               JOIN paths p ON p.pk = s.path
               WHERE s.entity = ?1 AND u.configuration = ?2 AND u.state != 'stale'
               ORDER BY s.definition DESC, u.pk ASC
               LIMIT 1"#,
            params![entity_pk, configuration],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let (name, main_file, _def_path) = match symbol_info {
        Some((n, m, p)) => (n, m, p),
        None => ("<unknown>".to_string(), "<unknown>".to_string(), "<unknown>".to_string()),
    };

    // 2. Incoming calls: query all incoming calls across active units
    let mut stmt = conn.prepare(
        r#"SELECT c.path, c.start, c.end, c.caller, c.resolution
           FROM calls c
           JOIN units u ON u.pk = c.unit_pk
           WHERE c.target = ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
    )?;

    let mut raw_call_sites = 0u64;
    let mut direct_sites = HashSet::new();
    let mut candidate_sites = HashSet::new();
    let mut callers = HashSet::new();

    let rows = stmt.query_map(params![entity_pk, configuration], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;

    for row in rows {
        let (path_pk, start, end, caller_pk, resolution) = row?;
        raw_call_sites += 1;
        callers.insert(caller_pk);
        let site_key = (path_pk, start, end, caller_pk);
        if resolution == "direct" || resolution == "known" {
            direct_sites.insert(site_key);
        } else {
            candidate_sites.insert(site_key);
        }
    }

    let call_sites = direct_sites.len() as u64;
    let candidate_call_sites = candidate_sites.len() as u64;
    let distinct_callers = callers.len() as u64;
    let fan_in = distinct_callers;

    // 3. Outgoing calls (fan_out)
    let fan_out: u64 = conn.query_row(
        r#"SELECT COUNT(DISTINCT c.target)
           FROM calls c
           JOIN units u ON u.pk = c.unit_pk
           WHERE c.caller = ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
        params![entity_pk, configuration],
        |r| Ok(r.get::<_, i64>(0)? as u64),
    )?;

    // 4. Non-call references
    let reference_count: u64 = conn.query_row(
        r#"SELECT COUNT(DISTINCT r.path || ':' || r.start || ':' || r.end)
           FROM refs r
           JOIN units u ON u.pk = r.unit_pk
           WHERE r.target = ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
        params![entity_pk, configuration],
        |r| Ok(r.get::<_, i64>(0)? as u64),
    )?;

    // 5. Declaration count
    let declaration_count: u64 = conn.query_row(
        r#"SELECT COUNT(*)
           FROM symbols s
           JOIN units u ON u.pk = s.unit_pk
           WHERE s.entity = ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
        params![entity_pk, configuration],
        |r| Ok(r.get::<_, i64>(0)? as u64),
    )?;

    // 6. Logarithmic structural score
    let structural_score = (distinct_callers as f64 + 1.0).ln() * 0.5
        + (call_sites as f64 + 1.0).ln() * 0.3
        + (reference_count as f64 + 1.0).ln() * 0.2;

    Ok(Some(EntityStructuralHeat {
        entity_key: entity_key.to_string(),
        name,
        kind: "function".to_string(),
        main_file,
        call_sites,
        distinct_callers,
        raw_call_sites,
        candidate_call_sites,
        reference_count,
        declaration_count,
        fan_in,
        fan_out,
        dedup_policy: "dedup_by_source_location".to_string(),
        evidence_level: "clang_semantic".to_string(),
        syntactic_incoming_proxy: None,
        structural_score,
    }))
}

pub fn compute_all_structural_heat(
    conn: &Connection,
    configuration: &str,
    limit: usize,
) -> Result<Vec<EntityStructuralHeat>> {
    let mut stmt = conn.prepare(
        r#"SELECT DISTINCT e.key
           FROM entities e
           JOIN symbols s ON s.entity = e.pk
           JOIN units u ON u.pk = s.unit_pk
           WHERE u.configuration = ?1 AND u.state != 'stale'"#,
    )?;

    let entity_keys: Vec<String> = stmt
        .query_map(params![configuration], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut results = Vec::new();
    for key in entity_keys {
        if let Some(heat) = compute_entity_structural_heat(conn, configuration, &key)? {
            results.push(heat);
        }
    }

    results.sort_by(|a, b| b.structural_score.total_cmp(&a.structural_score));
    if results.len() > limit {
        results.truncate(limit);
    }

    Ok(results)
}

pub fn compute_file_structural_heat(
    conn: &Connection,
    configuration: &str,
    limit: usize,
) -> Result<Vec<FileStructuralHeat>> {
    let mut stmt = conn.prepare(
        r#"SELECT p.pk, p.key
           FROM paths p
           JOIN symbols s ON s.path = p.pk
           JOIN units u ON u.pk = s.unit_pk
           WHERE u.configuration = ?1 AND u.state != 'stale'
           GROUP BY p.pk, p.key"#,
    )?;

    let path_rows: Vec<(i64, String)> = stmt
        .query_map(params![configuration], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    let mut results = Vec::new();
    for (path_pk, path) in path_rows {
        let total_symbols: u64 = conn.query_row(
            r#"SELECT COUNT(DISTINCT s.entity)
               FROM symbols s
               JOIN units u ON u.pk = s.unit_pk
               WHERE s.path = ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
            params![path_pk, configuration],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )?;

        // Incoming calls into functions defined in this file from other files
        let incoming_calls: u64 = conn.query_row(
            r#"SELECT COUNT(DISTINCT c.path || ':' || c.start || ':' || c.end)
               FROM calls c
               JOIN units u ON u.pk = c.unit_pk
               JOIN symbols s ON s.entity = c.target
               WHERE s.path = ?1 AND c.path != ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
            params![path_pk, configuration],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )?;

        // Outgoing calls made from inside this file
        let outgoing_calls: u64 = conn.query_row(
            r#"SELECT COUNT(DISTINCT c.path || ':' || c.start || ':' || c.end)
               FROM calls c
               JOIN units u ON u.pk = c.unit_pk
               WHERE c.path = ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
            params![path_pk, configuration],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )?;

        let internal_calls: u64 = conn.query_row(
            r#"SELECT COUNT(DISTINCT c.path || ':' || c.start || ':' || c.end)
               FROM calls c
               JOIN units u ON u.pk = c.unit_pk
               JOIN symbols s ON s.entity = c.target
               WHERE c.path = ?1 AND s.path = ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
            params![path_pk, configuration],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )?;

        let distinct_caller_files: u64 = conn.query_row(
            r#"SELECT COUNT(DISTINCT c.path)
               FROM calls c
               JOIN units u ON u.pk = c.unit_pk
               JOIN symbols s ON s.entity = c.target
               WHERE s.path = ?1 AND c.path != ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
            params![path_pk, configuration],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )?;

        let distinct_callee_files: u64 = conn.query_row(
            r#"SELECT COUNT(DISTINCT s.path)
               FROM calls c
               JOIN units u ON u.pk = c.unit_pk
               JOIN symbols s ON s.entity = c.target
               WHERE c.path = ?1 AND s.path != ?1 AND u.configuration = ?2 AND u.state != 'stale'"#,
            params![path_pk, configuration],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )?;

        let structural_score = (incoming_calls as f64 + 1.0).ln() * 0.5
            + (outgoing_calls as f64 + 1.0).ln() * 0.3
            + (total_symbols as f64 + 1.0).ln() * 0.2;

        results.push(FileStructuralHeat {
            path,
            total_symbols,
            internal_calls,
            incoming_calls,
            outgoing_calls,
            distinct_caller_files,
            distinct_callee_files,
            dedup_policy: "dedup_by_source_location".to_string(),
            evidence_level: "clang_semantic".to_string(),
            structural_score,
        });
    }

    results.sort_by(|a, b| b.structural_score.total_cmp(&a.structural_score));
    if results.len() > limit {
        results.truncate(limit);
    }

    Ok(results)
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymbolAttention {
    pub configuration: String,
    pub path: String,
    pub usr: String,
    pub name: String,
    pub active_ms: u64,
    pub opens: u64,
    pub attention_score: f64,
    pub last_seen: f64,
}

pub fn ensure_attention_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS semantic_attention (
            configuration TEXT NOT NULL,
            path TEXT NOT NULL,
            usr TEXT NOT NULL,
            name TEXT NOT NULL,
            active_ms INTEGER NOT NULL,
            opens INTEGER NOT NULL,
            attention_score REAL NOT NULL,
            last_seen REAL NOT NULL,
            PRIMARY KEY(configuration, path, usr)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS semantic_attention_rank ON semantic_attention(configuration, attention_score DESC);"#,
    )?;
    Ok(())
}

pub fn record_semantic_attention(
    conn: &Connection,
    configuration: &str,
    path: &str,
    usr: &str,
    name: &str,
    active_ms: u64,
    opened: bool,
    now: f64,
) -> Result<()> {
    ensure_attention_schema(conn)?;

    let existing: Option<(u64, u64, f64, f64)> = conn
        .query_row(
            "SELECT active_ms, opens, attention_score, last_seen FROM semantic_attention WHERE configuration=?1 AND path=?2 AND usr=?3",
            params![configuration, path, usr],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    let (prev_ms, prev_opens, prev_score, prev_last_seen) = existing.unwrap_or((0, 0, 0.0, now));
    let dt = (now - prev_last_seen).max(0.0);
    // 7-day half life: 604800.0 seconds
    let decayed_score = prev_score * 2.0_f64.powf(-dt / 604800.0);
    // Clamp individual heartbeat contribution to 60s
    let added_score = (active_ms as f64 / 1000.0).min(60.0);
    let new_score = decayed_score + added_score;
    let new_ms = prev_ms + active_ms;
    let new_opens = prev_opens + if opened { 1 } else { 0 };

    conn.execute(
        r#"INSERT INTO semantic_attention(configuration, path, usr, name, active_ms, opens, attention_score, last_seen)
           VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
           ON CONFLICT(configuration, path, usr) DO UPDATE SET
               name = excluded.name,
               active_ms = excluded.active_ms,
               opens = excluded.opens,
               attention_score = excluded.attention_score,
               last_seen = excluded.last_seen"#,
        params![configuration, path, usr, name, new_ms as i64, new_opens as i64, new_score, now],
    )?;

    Ok(())
}

pub fn get_semantic_attention(
    conn: &Connection,
    configuration: &str,
    path: &str,
    usr: &str,
) -> Result<Option<SymbolAttention>> {
    ensure_attention_schema(conn)?;
    conn.query_row(
        "SELECT configuration, path, usr, name, active_ms, opens, attention_score, last_seen FROM semantic_attention WHERE configuration=?1 AND path=?2 AND usr=?3",
        params![configuration, path, usr],
        |r| Ok(SymbolAttention {
            configuration: r.get(0)?,
            path: r.get(1)?,
            usr: r.get(2)?,
            name: r.get(3)?,
            active_ms: r.get::<_, i64>(4)? as u64,
            opens: r.get::<_, i64>(5)? as u64,
            attention_score: r.get(6)?,
            last_seen: r.get(7)?,
        }),
    ).optional().map_err(Into::into)
}

pub fn migrate_semantic_attention(
    conn: &Connection,
    configuration: &str,
    from_path: &str,
    from_usr: &str,
    to_path: &str,
    to_usr: &str,
    new_name: &str,
) -> Result<bool> {
    ensure_attention_schema(conn)?;
    let source = get_semantic_attention(conn, configuration, from_path, from_usr)?;
    let Some(src) = source else {
        return Ok(false);
    };

    let target = get_semantic_attention(conn, configuration, to_path, to_usr)?;
    match target {
        Some(tgt) => {
            let merged_ms = tgt.active_ms + src.active_ms;
            let merged_opens = tgt.opens + src.opens;
            let merged_score = tgt.attention_score.max(src.attention_score);
            let merged_last_seen = tgt.last_seen.max(src.last_seen);
            conn.execute(
                "UPDATE semantic_attention SET active_ms=?1, opens=?2, attention_score=?3, last_seen=?4, name=?5 WHERE configuration=?6 AND path=?7 AND usr=?8",
                params![merged_ms as i64, merged_opens as i64, merged_score, merged_last_seen, new_name, configuration, to_path, to_usr],
            )?;
            conn.execute(
                "DELETE FROM semantic_attention WHERE configuration=?1 AND path=?2 AND usr=?3",
                params![configuration, from_path, from_usr],
            )?;
        }
        None => {
            conn.execute(
                "UPDATE semantic_attention SET path=?1, usr=?2, name=?3 WHERE configuration=?4 AND path=?5 AND usr=?6",
                params![to_path, to_usr, new_name, configuration, from_path, from_usr],
            )?;
        }
    }
    Ok(true)
}

pub fn list_semantic_attention(
    conn: &Connection,
    configuration: &str,
    limit: usize,
) -> Result<Vec<SymbolAttention>> {
    ensure_attention_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT configuration, path, usr, name, active_ms, opens, attention_score, last_seen FROM semantic_attention WHERE configuration=?1 ORDER BY attention_score DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![configuration, limit as i64], |r| {
        Ok(SymbolAttention {
            configuration: r.get(0)?,
            path: r.get(1)?,
            usr: r.get(2)?,
            name: r.get(3)?,
            active_ms: r.get::<_, i64>(4)? as u64,
            opens: r.get::<_, i64>(5)? as u64,
            attention_score: r.get(6)?,
            last_seen: r.get(7)?,
        })
    })?;
    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}


pub fn clear_semantic_attention(
    conn: &Connection,
    configuration: Option<&str>,
) -> Result<usize> {
    ensure_attention_schema(conn)?;
    let deleted = match configuration {
        Some(config) => conn.execute(
            "DELETE FROM semantic_attention WHERE configuration = ?1",
            params![config],
        )?,
        None => conn.execute("DELETE FROM semantic_attention", [])?,
    };
    Ok(deleted)
}

pub fn decay_attention_score(prev_score: f64, last_seen: f64, now: f64) -> f64 {
    let dt = (now - last_seen).max(0.0);
    prev_score * 2.0_f64.powf(-dt / 604800.0)
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeatExportRow {
    pub path: String,
    pub entity_key: String,
    pub name: String,
    pub kind: String,
    pub structural_metric: String,
    pub structural_value: u64,
    pub structural_score: f64,
    pub evidence_level: String,
    pub ambiguous: bool,
    pub active_ms: u64,
    pub open_count: u64,
    pub attention_score: f64,
    pub last_seen: f64,
}

pub fn export_heat_report(
    conn: &Connection,
    configuration: &str,
    kind: &str,
    sort: &str,
    format: &str,
    limit: usize,
    now: f64,
) -> Result<serde_json::Value> {
    ensure_attention_schema(conn)?;

    let mut rows = Vec::new();
    if kind == "symbol" {
        let structural_list = compute_all_structural_heat(conn, configuration, limit * 2)?;
        let mut struct_map: std::collections::HashMap<String, EntityStructuralHeat> = structural_list
            .into_iter()
            .map(|h| (h.entity_key.clone(), h))
            .collect();

        let attention_list = list_semantic_attention(conn, configuration, limit * 2)?;
        let mut attn_map: std::collections::HashMap<String, SymbolAttention> = attention_list
            .into_iter()
            .map(|a| (a.usr.clone(), a))
            .collect();

        let mut all_keys: HashSet<String> = struct_map.keys().cloned().collect();
        all_keys.extend(attn_map.keys().cloned());

        for key in all_keys {
            let s = struct_map.remove(&key);
            let a = attn_map.remove(&key);

            let path = s.as_ref().map(|x| x.main_file.clone()).or_else(|| a.as_ref().map(|x| x.path.clone())).unwrap_or_default();
            let name = s.as_ref().map(|x| x.name.clone()).or_else(|| a.as_ref().map(|x| x.name.clone())).unwrap_or_default();
            let kind_str = s.as_ref().map(|x| x.kind.clone()).unwrap_or_else(|| "symbol".to_string());
            let structural_val = s.as_ref().map(|x| x.call_sites).unwrap_or(0);
            let structural_score = s.as_ref().map(|x| x.structural_score).unwrap_or(0.0);
            let active_ms = a.as_ref().map(|x| x.active_ms).unwrap_or(0);
            let open_count = a.as_ref().map(|x| x.opens).unwrap_or(0);
            let last_seen = a.as_ref().map(|x| x.last_seen).unwrap_or(0.0);
            let raw_score = a.as_ref().map(|x| x.attention_score).unwrap_or(0.0);
            let decayed_score = decay_attention_score(raw_score, last_seen, now);

            rows.push(HeatExportRow {
                path,
                entity_key: key,
                name,
                kind: kind_str,
                structural_metric: "call_sites".to_string(),
                structural_value: structural_val,
                structural_score,
                evidence_level: "clang_semantic".to_string(),
                ambiguous: false,
                active_ms,
                open_count,
                attention_score: decayed_score,
                last_seen,
            });
        }

        if sort == "attention" {
            rows.sort_by(|a, b| b.attention_score.total_cmp(&a.attention_score));
        } else {
            rows.sort_by(|a, b| b.structural_score.total_cmp(&a.structural_score));
        }
    } else {
        let file_heats = compute_file_structural_heat(conn, configuration, limit)?;
        for f in file_heats {
            rows.push(HeatExportRow {
                path: f.path.clone(),
                entity_key: f.path.clone(),
                name: f.path,
                kind: "file".to_string(),
                structural_metric: "incoming_calls".to_string(),
                structural_value: f.incoming_calls,
                structural_score: f.structural_score,
                evidence_level: "clang_semantic".to_string(),
                ambiguous: false,
                active_ms: 0,
                open_count: 0,
                attention_score: 0.0,
                last_seen: 0.0,
            });
        }
    }

    if rows.len() > limit {
        rows.truncate(limit);
    }

    let scope = format!("configuration: {configuration}, total: {} rows", rows.len());

    if format == "csv" {
        let mut csv = String::from("path,entity_key,name,kind,structural_metric,structural_value,evidence_level,ambiguous,active_ms,open_count,attention_score,last_seen\n");
        for r in &rows {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{:.4},{:.1}\n",
                r.path, r.entity_key, r.name, r.kind, r.structural_metric, r.structural_value,
                r.evidence_level, r.ambiguous, r.active_ms, r.open_count, r.attention_score, r.last_seen
            ));
        }
        Ok(serde_json::json!({
            "scope": scope,
            "updated_at": now,
            "kind": kind,
            "sort": sort,
            "format": "csv",
            "content": csv,
            "row_count": rows.len(),
        }))
    } else {
        Ok(serde_json::json!({
            "scope": scope,
            "updated_at": now,
            "kind": kind,
            "sort": sort,
            "format": "json",
            "rows": rows,
            "row_count": rows.len(),
        }))
    }
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayTraceStep {
    pub entity_id: String,
    pub direction: String,
    pub is_hot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayComparisonResult {
    pub strategy: String,
    pub total_requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub hit_ratio: f64,
    pub avg_latency_us: f64,
    pub p95_latency_us: f64,
    pub peak_bytes: usize,
    pub prefetch_useful_ratio: f64,
    pub prefetch_wasted_bytes: usize,
    pub real_user_attention_modified: bool,
}

pub fn generate_synthetic_replay_trace(
    hot_entities: &[String],
    cold_entities: &[String],
    steps: usize,
) -> Vec<ReplayTraceStep> {
    let mut trace = Vec::with_capacity(steps);
    for i in 0..steps {
        let is_hot = (i % 5) != 0;
        let entity_id = if is_hot && !hot_entities.is_empty() {
            hot_entities[i % hot_entities.len()].clone()
        } else if !cold_entities.is_empty() {
            cold_entities[i % cold_entities.len()].clone()
        } else {
            "unknown".to_string()
        };
        let direction = if i % 2 == 0 { "incoming" } else { "definitions" };
        trace.push(ReplayTraceStep {
            entity_id,
            direction: direction.to_string(),
            is_hot,
        });
    }
    trace
}

pub fn run_heat_replay_benchmark(
    store: &Store,
    version: &QueryVersion,
    trace: &[ReplayTraceStep],
    budget_bytes: usize,
    use_heat_and_prefetch: bool,
) -> Result<ReplayComparisonResult> {
    let initial_attention_count = store.top_attention(&version.configuration.id, 1000)?.len();

    let mut pool = SemanticHotPool::new(PoolMode::Manual, budget_bytes);
    let mut prefetcher = if use_heat_and_prefetch {
        Some(SemanticPrefetcher::new(8, budget_bytes / 2, 50))
    } else {
        None
    };

    let mut hits = 0u64;
    let mut misses = 0u64;
    let mut latencies_us = Vec::with_capacity(trace.len());
    let mut peak_bytes = 0usize;

    let cancel = AtomicBool::new(false);

    for step in trace {
        let t0 = Instant::now();
        let hit = pool.get(version, &step.entity_id, &step.direction);
        let elapsed_us = t0.elapsed().as_micros() as f64;
        latencies_us.push(elapsed_us);

        if let Some(rel) = hit {
            hits += 1;
            if let Some(p) = prefetcher.as_mut() {
                p.record_access(&step.entity_id, &step.direction, version, rel.estimated_bytes);
            }
        } else {
            misses += 1;
            if let Ok(res) = store.relations(&version.configuration.id, &step.entity_id, &step.direction, &version.configuration.provider, 16) {
                let total = res.get("total").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
                if use_heat_and_prefetch {
                    let struct_heat = store
                        .entity_structural_heat(&version.configuration.id, &step.entity_id)?
                        .map_or(1.0, |h| h.structural_score);
                    let attn_score = store
                        .get_attention(&version.configuration.id, "dummy.c", &step.entity_id)?
                        .map_or(0.0, |a| a.attention_score);

                    pool.insert_with_heat(
                        version,
                        &step.entity_id,
                        &step.direction,
                        res,
                        total,
                        struct_heat,
                        attn_score,
                    );

                    if let Some(p) = prefetcher.as_mut() {
                        let _ = p.prefetch_neighbors(store, &mut pool, version, &step.entity_id, &cancel);
                    }
                } else {
                    pool.insert(version, &step.entity_id, &step.direction, res, total);
                }
            }
        }
        peak_bytes = peak_bytes.max(pool.current_bytes());
    }

    let after_attention_count = store.top_attention(&version.configuration.id, 1000)?.len();
    let real_user_attention_modified = after_attention_count != initial_attention_count;

    latencies_us.sort_by(|a, b| a.total_cmp(b));
    let total_requests = hits + misses;
    let hit_ratio = if total_requests > 0 { hits as f64 / total_requests as f64 } else { 0.0 };
    let avg_latency_us = if !latencies_us.is_empty() {
        latencies_us.iter().sum::<f64>() / latencies_us.len() as f64
    } else {
        0.0
    };
    let p95_idx = ((latencies_us.len() as f64 * 0.95) as usize).min(latencies_us.len().saturating_sub(1));
    let p95_latency_us = latencies_us.get(p95_idx).copied().unwrap_or(0.0);

    let (useful_ratio, wasted_bytes) = if let Some(p) = prefetcher {
        let m = p.metrics();
        (m.useful_ratio, m.wasted_bytes)
    } else {
        (0.0, 0)
    };

    Ok(ReplayComparisonResult {
        strategy: if use_heat_and_prefetch {
            "heat_driven_utility_with_prefetch".to_string()
        } else {
            "baseline_fifo_lru".to_string()
        },
        total_requests,
        hits,
        misses,
        hit_ratio,
        avg_latency_us,
        p95_latency_us,
        peak_bytes,
        prefetch_useful_ratio: useful_ratio,
        prefetch_wasted_bytes: wasted_bytes,
        real_user_attention_modified,
    })
}
