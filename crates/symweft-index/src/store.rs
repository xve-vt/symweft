use crate::{ANALYZER_KEY, Result, Snapshot, simple_name, symbol_key};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use symweft_analysis::Facts;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS snapshots(path TEXT PRIMARY KEY,source_hash BLOB NOT NULL,checksum BLOB NOT NULL,source_bytes INTEGER NOT NULL,payload BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS pack_location(path TEXT PRIMARY KEY REFERENCES snapshots(path) ON DELETE CASCADE,offset INTEGER NOT NULL,length INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS definitions(entity_key TEXT PRIMARY KEY,path TEXT NOT NULL,local_id INTEGER NOT NULL,name TEXT NOT NULL,kind TEXT NOT NULL,callable INTEGER NOT NULL,line INTEGER NOT NULL) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS def_path ON definitions(path);
CREATE INDEX IF NOT EXISTS def_name ON definitions(name);
CREATE TABLE IF NOT EXISTS lexical_calls(path TEXT NOT NULL,name TEXT NOT NULL,sites INTEGER NOT NULL,PRIMARY KEY(path,name)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS calls_name ON lexical_calls(name,path,sites);
CREATE TABLE IF NOT EXISTS dependencies(path TEXT NOT NULL,target TEXT NOT NULL,PRIMARY KEY(path,target)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS usage(entity_key TEXT PRIMARY KEY,path TEXT NOT NULL,active_ms INTEGER NOT NULL,opens INTEGER NOT NULL,attention REAL NOT NULL,last_seen REAL NOT NULL,attention_rank REAL NOT NULL) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS usage_path ON usage(path);
"#;

pub struct Database {
    pub(crate) connection: Connection,
}
impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if rusqlite::version_number() < 3_051_003 {
            return Err("SQLite lacks required WAL fixes".into());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_millis(250))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-4096; PRAGMA mmap_size=0; PRAGMA foreign_keys=ON;")?;
        connection.execute_batch(SCHEMA)?;
        let key: Option<String> = connection
            .query_row("SELECT value FROM meta WHERE key='analyzer'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if key.as_deref().is_some_and(|k| k != ANALYZER_KEY) {
            return Err("incompatible analyzer cache".into());
        }
        connection.execute(
            "INSERT OR IGNORE INTO meta VALUES('analyzer',?1)",
            [ANALYZER_KEY],
        )?;
        Ok(Self { connection })
    }
    pub fn sqlite_version(&self) -> &'static str {
        rusqlite::version()
    }
    pub fn put_batch(&mut self, values: &[Snapshot], secondary: bool) -> Result<()> {
        self.write_batch(values, secondary, None)
    }
    pub fn put_external_batch(
        &mut self,
        values: &[Snapshot],
        secondary: bool,
        locations: &[(u64, u64)],
    ) -> Result<()> {
        if values.len() != locations.len() {
            return Err("external location count".into());
        }
        self.write_batch(values, secondary, Some(locations))
    }
    fn write_batch(
        &mut self,
        values: &[Snapshot],
        secondary: bool,
        locations: Option<&[(u64, u64)]>,
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        {
            let mut put=transaction.prepare_cached("INSERT INTO snapshots VALUES(?1,?2,?3,?4,?5) ON CONFLICT(path) DO UPDATE SET source_hash=excluded.source_hash,checksum=excluded.checksum,source_bytes=excluded.source_bytes,payload=excluded.payload")?;
            for (index, snapshot) in values.iter().enumerate() {
                put.execute(params![
                    snapshot.path,
                    &snapshot.source_hash[..],
                    &snapshot.checksum[..],
                    snapshot.source_bytes as i64,
                    if locations.is_some() {
                        &[][..]
                    } else {
                        snapshot.payload.as_slice()
                    }
                ])?;
                if let Some(locations) = locations {
                    transaction.execute("INSERT INTO pack_location VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET offset=excluded.offset,length=excluded.length",params![snapshot.path,locations[index].0 as i64,locations[index].1 as i64])?;
                }
                if secondary {
                    let facts = snapshot.decode()?;
                    transaction
                        .execute("DELETE FROM definitions WHERE path=?1", [&snapshot.path])?;
                    transaction
                        .execute("DELETE FROM lexical_calls WHERE path=?1", [&snapshot.path])?;
                    let mut definition = transaction
                        .prepare_cached("INSERT INTO definitions VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
                    for symbol in &facts.symbols {
                        definition.execute(params![
                            symbol_key(&snapshot.path, &facts, symbol.local_id),
                            snapshot.path,
                            symbol.local_id,
                            symbol.name,
                            symbol.kind,
                            symbol.is_callable,
                            symbol.range.start_line as i64
                        ])?;
                    }
                    let mut counts = BTreeMap::<&str, usize>::new();
                    for call in &facts.calls {
                        if call.kind == "call" {
                            if let Some(name) = simple_name(&call.callee) {
                                *counts.entry(name).or_default() += 1;
                            }
                        }
                    }
                    let mut call =
                        transaction.prepare_cached("INSERT INTO lexical_calls VALUES(?1,?2,?3)")?;
                    for (name, count) in counts {
                        call.execute(params![snapshot.path, name, count as i64])?;
                    }
                    transaction
                        .execute("DELETE FROM dependencies WHERE path=?1", [&snapshot.path])?;
                    let mut dep = transaction
                        .prepare_cached("INSERT OR IGNORE INTO dependencies VALUES(?1,?2)")?;
                    for target in &snapshot.dependencies {
                        dep.execute(params![snapshot.path, target])?;
                    }
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }
    pub fn get(&self, path: &str) -> Result<Option<Snapshot>> {
        Ok(self
            .connection
            .query_row(
                "SELECT source_hash,checksum,source_bytes,payload FROM snapshots WHERE path=?1",
                [path],
                |r| {
                    let source: Vec<u8> = r.get(0)?;
                    let checksum: Vec<u8> = r.get(1)?;
                    let source_hash = source
                        .try_into()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    let checksum = checksum
                        .try_into()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok(Snapshot {
                        path: path.into(),
                        source_hash,
                        checksum,
                        source_bytes: r.get::<_, i64>(2)? as u64,
                        payload: r.get(3)?,
                        dependencies: Vec::new(),
                    })
                },
            )
            .optional()?)
    }
    pub fn paths(&self) -> Result<Vec<String>> {
        Ok(self
            .connection
            .prepare("SELECT path FROM snapshots ORDER BY path")?
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn checkpoint(&self) -> Result<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
    pub fn remove_paths(&mut self, paths: &[String]) -> Result<()> {
        let tx = self.connection.transaction()?;
        for path in paths {
            for table in [
                "snapshots",
                "definitions",
                "lexical_calls",
                "dependencies",
                "usage",
            ] {
                tx.execute(&format!("DELETE FROM {table} WHERE path=?1"), [path])?;
            }
            tx.execute("DELETE FROM dependencies WHERE target=?1", [path])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn set_durable_writes(&self) -> Result<()> {
        self.connection.execute_batch("PRAGMA synchronous=FULL")?;
        Ok(())
    }
    pub fn set_page_budget(&self, kib: usize) -> Result<()> {
        self.connection
            .pragma_update(None, "cache_size", -(kib.clamp(256, 16384) as i64))?;
        Ok(())
    }
    pub fn set_dependencies(&mut self, path: &str, targets: &[String]) -> Result<()> {
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM dependencies WHERE path=?1", [path])?;
        {
            let mut insert =
                tx.prepare_cached("INSERT OR IGNORE INTO dependencies VALUES(?1,?2)")?;
            for target in targets {
                insert.execute(params![path, target])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn neighbors(&self, path: &str, limit: usize) -> Result<Vec<String>> {
        Ok(self.connection.prepare_cached("SELECT d.target FROM dependencies d JOIN snapshots s ON s.path=d.target WHERE d.path=?1 ORDER BY d.target LIMIT ?2")?.query_map(params![path,limit as i64],|r|r.get(0))?.collect::<std::result::Result<_,_>>()?)
    }
    pub fn record_usage(
        &mut self,
        path: &str,
        facts: &Facts,
        local_id: Option<u32>,
        active_ms: u64,
        opened: bool,
    ) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        let ms = active_ms.min(5000);
        let mut keys = vec![format!("file:{path}")];
        if let Some(id) = local_id.filter(|&id| (id as usize) < facts.symbols.len()) {
            keys.push(symbol_key(path, facts, id));
        }
        let tx = self.connection.transaction()?;
        for key in keys {
            let previous: Option<(f64, f64)> = tx
                .query_row(
                    "SELECT attention,last_seen FROM usage WHERE entity_key=?1",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let score = previous.map_or(0.0, |(s, t)| {
                s * 2f64.powf(-(now - t).max(0.0) / (7.0 * 86400.0))
            }) + ms as f64 / 1000.0
                + if opened { 2.0 } else { 0.0 };
            let rank = if score > 0.0 {
                score.log2() + now / 604800.0
            } else {
                0.0
            };
            tx.execute("INSERT INTO usage VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(entity_key) DO UPDATE SET active_ms=active_ms+excluded.active_ms,opens=opens+excluded.opens,attention=excluded.attention,last_seen=excluded.last_seen,attention_rank=excluded.attention_rank",params![key,path,ms as i64,i64::from(opened),score,now,rank])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn clear_usage(&mut self) -> Result<()> {
        self.connection.execute("DELETE FROM usage", [])?;
        Ok(())
    }
    pub fn heat(&self, kind: &str, sort: &str, limit: usize) -> Result<Vec<HeatRow>> {
        let order = match sort {
            "attention" => "COALESCE(u.attention_rank,0) DESC",
            _ => "proxy_sites DESC",
        };
        let query = if kind == "file" {
            format!(
                r#"
WITH incoming AS (SELECT name,SUM(sites) AS n FROM lexical_calls GROUP BY name),
file_names AS (SELECT DISTINCT path,name FROM definitions WHERE callable=1),
definitions_count AS (SELECT name,COUNT(*) AS n FROM definitions WHERE callable=1 GROUP BY name),
scores AS (SELECT f.path,SUM(COALESCE(i.n,0)) AS n,MAX(COALESCE(c.n,0)>1) AS ambiguous FROM file_names f LEFT JOIN incoming i USING(name) LEFT JOIN definitions_count c USING(name) GROUP BY f.path)
SELECT 'file:'||s.path,s.path,s.path,'file',COALESCE(q.n,0) AS proxy_sites,COALESCE(q.ambiguous,0),
COALESCE(u.active_ms,0),COALESCE(u.opens,0),COALESCE(u.attention,0) AS attention,COALESCE(u.last_seen,0)
FROM snapshots s LEFT JOIN scores q ON q.path=s.path LEFT JOIN usage u ON u.entity_key='file:'||s.path
ORDER BY {order},s.path LIMIT ?1"#
            )
        } else {
            format!(
                r#"
WITH incoming AS (SELECT name,SUM(sites) AS n FROM lexical_calls GROUP BY name),
definitions_count AS (SELECT name,COUNT(*) AS n FROM definitions WHERE callable=1 GROUP BY name)
SELECT d.entity_key,d.path,d.name,d.kind,COALESCE(i.n,0) AS proxy_sites,COALESCE(c.n,0)>1,
COALESCE(u.active_ms,0),COALESCE(u.opens,0),COALESCE(u.attention,0) AS attention,COALESCE(u.last_seen,0)
FROM definitions d LEFT JOIN incoming i USING(name) LEFT JOIN definitions_count c USING(name) LEFT JOIN usage u USING(entity_key)
ORDER BY {order},d.entity_key LIMIT ?1"#
            )
        };
        // Order by the stored logarithmic rank; decay the displayed score at read time.
        let mut rows: Vec<HeatRow> = self
            .connection
            .prepare(&query)?
            .query_map([limit.clamp(1, 100_000) as i64], |r| {
                Ok(HeatRow {
                    entity_key: r.get(0)?,
                    path: r.get(1)?,
                    name: r.get(2)?,
                    kind: r.get(3)?,
                    syntactic_incoming_proxy: r.get::<_, i64>(4)?.max(0) as u64,
                    ambiguous: r.get(5)?,
                    active_ms: r.get::<_, i64>(6)?.max(0) as u64,
                    open_count: r.get::<_, i64>(7)?.max(0) as u64,
                    attention_score: r.get(8)?,
                    last_seen: r.get(9)?,
                    evidence_level: "syntax_proxy",
                    resolved_incoming: None,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        for row in &mut rows {
            row.attention_score *= 2f64.powf(-(now - row.last_seen).max(0.0) / 604800.0);
        }
        if sort == "attention" {
            rows.sort_by(|a, b| b.attention_score.total_cmp(&a.attention_score));
        }
        Ok(rows)
    }
    pub fn file_priority(&self, path: &str) -> Result<f64> {
        let attention: Option<(f64, f64)> = self
            .connection
            .query_row(
                "SELECT attention,last_seen FROM usage WHERE entity_key=?1",
                [format!("file:{path}")],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        let attention =
            attention.map_or(0.0, |(a, t)| a * 2f64.powf(-(now - t).max(0.0) / 604800.0));
        let sites:f64=self.connection.query_row("SELECT COALESCE(SUM((SELECT SUM(sites) FROM lexical_calls c WHERE c.name=d.name)),0) FROM (SELECT DISTINCT name FROM definitions WHERE path=?1 AND callable=1) d",[path],|r|r.get(0))?;
        Ok(attention.ln_1p().min(8.0) * 0.2 + sites.ln_1p().min(8.0) * 0.05)
    }
}

#[derive(Debug, Serialize)]
pub struct HeatRow {
    pub entity_key: String,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub syntactic_incoming_proxy: u64,
    pub ambiguous: bool,
    pub resolved_incoming: Option<u64>,
    pub evidence_level: &'static str,
    pub active_ms: u64,
    pub open_count: u64,
    pub attention_score: f64,
    pub last_seen: f64,
}
