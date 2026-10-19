//! Content-addressed compact facts; each TU retains independent contributions.
use crate::{
    Result,
    compdb::{CompileCommand, CompileDatabase},
    model::{Call, DiskBaseline, Reference, Snapshot, Symbol},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    time::Instant,
};
const MAX_FACT: usize = 2 * 1024 * 1024;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS semantic_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
INSERT OR IGNORE INTO semantic_meta VALUES('schema','a2-store-v2');
INSERT OR IGNORE INTO semantic_meta VALUES('store_id',lower(hex(randomblob(16))));
INSERT OR IGNORE INTO semantic_meta VALUES('disk_revision','0');
CREATE TABLE IF NOT EXISTS units(pk INTEGER PRIMARY KEY,id TEXT NOT NULL UNIQUE,configuration TEXT NOT NULL,main_file TEXT NOT NULL,command_key TEXT NOT NULL,provider TEXT NOT NULL,state TEXT NOT NULL,report TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS units_configuration ON units(configuration,state);
CREATE TABLE IF NOT EXISTS inputs(unit_pk INTEGER REFERENCES units(pk) ON DELETE CASCADE,path TEXT NOT NULL,hash BLOB NOT NULL,PRIMARY KEY(unit_pk,path)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS inputs_path ON inputs(path,unit_pk);
CREATE TABLE IF NOT EXISTS input_aliases(unit_pk INTEGER REFERENCES units(pk) ON DELETE CASCADE,real_path TEXT NOT NULL,input_path TEXT NOT NULL,PRIMARY KEY(unit_pk,real_path,input_path)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS input_aliases_path ON input_aliases(real_path,unit_pk);
CREATE TABLE IF NOT EXISTS entities(pk INTEGER PRIMARY KEY,key TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS paths(pk INTEGER PRIMARY KEY,key TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS facts(pk INTEGER PRIMARY KEY,digest BLOB NOT NULL UNIQUE,payload BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS symbols(unit_pk INTEGER REFERENCES units(pk) ON DELETE CASCADE,n INTEGER NOT NULL,entity INTEGER REFERENCES entities(pk),path INTEGER REFERENCES paths(pk),name_start INTEGER NOT NULL,name_end INTEGER NOT NULL,start INTEGER NOT NULL,end INTEGER NOT NULL,definition INTEGER NOT NULL,callable INTEGER NOT NULL,name TEXT NOT NULL,fact INTEGER REFERENCES facts(pk),PRIMARY KEY(unit_pk,n)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS symbols_entity ON symbols(entity,definition,unit_pk);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name,definition);
CREATE INDEX IF NOT EXISTS symbols_path ON symbols(path,name_start,name_end);
CREATE INDEX IF NOT EXISTS symbols_scope ON symbols(path,callable,start,end);
CREATE INDEX IF NOT EXISTS symbols_fact ON symbols(fact);
CREATE TABLE IF NOT EXISTS refs(unit_pk INTEGER REFERENCES units(pk) ON DELETE CASCADE,n INTEGER NOT NULL,target INTEGER REFERENCES entities(pk),path INTEGER REFERENCES paths(pk),start INTEGER NOT NULL,end INTEGER NOT NULL,fact INTEGER REFERENCES facts(pk),PRIMARY KEY(unit_pk,n)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS refs_target ON refs(target,unit_pk);
CREATE INDEX IF NOT EXISTS refs_path ON refs(path,start,end);
CREATE INDEX IF NOT EXISTS refs_fact ON refs(fact);
CREATE TABLE IF NOT EXISTS calls(unit_pk INTEGER REFERENCES units(pk) ON DELETE CASCADE,n INTEGER NOT NULL,caller INTEGER REFERENCES entities(pk),target INTEGER REFERENCES entities(pk),resolution TEXT NOT NULL,path INTEGER REFERENCES paths(pk),start INTEGER NOT NULL,end INTEGER NOT NULL,fact INTEGER REFERENCES facts(pk),PRIMARY KEY(unit_pk,n)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS calls_out ON calls(caller,unit_pk);
CREATE INDEX IF NOT EXISTS calls_in ON calls(target,unit_pk);
CREATE INDEX IF NOT EXISTS calls_path ON calls(path,start,end);
CREATE INDEX IF NOT EXISTS calls_fact ON calls(fact);
"#;
#[derive(Serialize)]
enum FactRef<'a> {
    Symbol(&'a Symbol),
    Reference(&'a Reference),
    Call(&'a Call),
}
#[derive(Deserialize)]
enum Fact {
    Symbol(Symbol),
    Reference(Reference),
    Call(Call),
}
fn unpack(
    digest: &[u8],
    packed: &[u8],
    decoder: &mut zstd::bulk::Decompressor<'_>,
) -> Result<Vec<u8>> {
    if packed.len() < 8 || &packed[..4] != b"SMF2" {
        return Err("semantic fact magic".into());
    }
    let length = u32::from_le_bytes(packed[4..8].try_into()?) as usize;
    if length > MAX_FACT {
        return Err("semantic fact size limit".into());
    }
    let bytes = decoder.decompress(&packed[8..], length)?;
    if bytes.len() != length || symweft_index::hash(&bytes).as_slice() != digest {
        return Err("semantic fact checksum/length".into());
    }
    Ok(bytes)
}
fn decode(
    digest: &[u8],
    packed: &[u8],
    decoder: &mut zstd::bulk::Decompressor<'_>,
) -> Result<Value> {
    let bytes = unpack(digest, packed, decoder)?;
    let (fact, used): (Fact, usize) = bincode::serde::decode_from_slice(
        &bytes,
        bincode::config::standard().with_limit::<MAX_FACT>(),
    )?;
    if used != bytes.len() {
        return Err("semantic fact trailing bytes".into());
    }
    Ok(match fact {
        Fact::Symbol(s) => serde_json::to_value(s)?,
        Fact::Reference(r) => serde_json::to_value(r)?,
        Fact::Call(c) => serde_json::to_value(c)?,
    })
}
fn intern(
    tx: &Transaction<'_>,
    table: &str,
    value: &str,
    memo: &mut HashMap<String, i64>,
) -> Result<i64> {
    if let Some(id) = memo.get(value) {
        return Ok(*id);
    }
    let id = tx
        .query_row(
            &format!("SELECT pk FROM {table} WHERE key=?1"),
            [value],
            |r| r.get(0),
        )
        .optional()?;
    let id = if let Some(id) = id {
        id
    } else {
        tx.execute(&format!("INSERT INTO {table}(key) VALUES(?1)"), [value])?;
        tx.last_insert_rowid()
    };
    memo.insert(value.into(), id);
    Ok(id)
}
fn fact(
    tx: &Transaction<'_>,
    value: FactRef<'_>,
    encoder: &mut zstd::bulk::Compressor<'_>,
    decoder: &mut zstd::bulk::Decompressor<'_>,
    profile: &mut crate::profile::StoreProfile,
) -> Result<i64> {
    let encode = crate::profile::Clock::start(profile.enabled);
    let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())?;
    profile.fact_encode_nanos += encode.nanos();
    if bytes.len() > MAX_FACT {
        return Err("semantic fact size limit".into());
    }
    let hash = crate::profile::Clock::start(profile.enabled);
    let digest = symweft_index::hash(&bytes);
    profile.fact_hash_nanos += hash.nanos();
    let existing: Option<(i64, Vec<u8>)> = tx
        .prepare_cached("SELECT pk,payload FROM facts WHERE digest=?1")?
        .query_row([&digest[..]], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if let Some((pk, packed)) = existing {
        let verify = crate::profile::Clock::start(profile.enabled);
        if unpack(&digest, &packed, decoder)? != bytes {
            return Err("semantic fact digest collision".into());
        }
        profile.reused_fact_verify_nanos += verify.nanos();
        profile.reused_facts += 1;
        return Ok(pk);
    }
    let compress = crate::profile::Clock::start(profile.enabled);
    let compressed = encoder.compress(&bytes)?;
    profile.fact_compress_nanos += compress.nanos();
    profile.new_facts += 1;
    let mut packed = b"SMF2".to_vec();
    packed.extend((bytes.len() as u32).to_le_bytes());
    packed.extend(compressed);
    tx.prepare_cached("INSERT INTO facts(digest,payload) VALUES(?1,?2)")?
        .execute(params![&digest[..], packed])?;
    Ok(tx.last_insert_rowid())
}
fn bump_baseline(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        "UPDATE semantic_meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='disk_revision'",
        [],
    )?;
    Ok(())
}
pub struct Store {
    connection: Connection,
    fingerprints: RefCell<crate::fingerprint::Fingerprints>,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_millis(250))?;
        connection.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=NORMAL;PRAGMA cache_size=-2048;PRAGMA mmap_size=0;PRAGMA foreign_keys=ON;")?;
        connection.execute_batch(SCHEMA)?;
        let version: String = connection.query_row(
            "SELECT value FROM semantic_meta WHERE key='schema'",
            [],
            |r| r.get(0),
        )?;
        if version != "a2-store-v2" {
            return Err("incompatible semantic schema".into());
        }
        Ok(Self {
            connection,
            fingerprints: RefCell::new(crate::fingerprint::Fingerprints::default()),
        })
    }
    pub fn disk_baseline(&self) -> Result<DiskBaseline> {
        let (schema, store_id, revision): (String, String, String) = self.connection.query_row(
            "SELECT (SELECT value FROM semantic_meta WHERE key='schema'),(SELECT value FROM semantic_meta WHERE key='store_id'),(SELECT value FROM semantic_meta WHERE key='disk_revision')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(DiskBaseline {
            schema,
            store_id,
            revision: revision.parse()?,
        })
    }
    pub fn cache_validation(&self, enabled: bool) {
        self.fingerprints.borrow_mut().enable(enabled);
    }
    pub fn clear_validation(&self) {
        self.fingerprints.borrow_mut().clear();
    }
    pub fn fingerprint_stats(&self) -> crate::fingerprint::Stats {
        self.fingerprints.borrow().stats()
    }
    pub fn input_digest(&self, path: &Path) -> Option<[u8; 32]> {
        self.fingerprints.borrow_mut().digest(path)
    }
    pub fn reconcile(&mut self, db: &CompileDatabase) -> Result<usize> {
        let keep: HashSet<_> = db.commands.iter().map(|c| c.unit_id.as_str()).collect();
        let existing = self
            .connection
            .prepare("SELECT id FROM units WHERE configuration=?1")?
            .query_map([&db.configuration], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let tx = self.connection.transaction()?;
        let mut removed = 0;
        for id in existing {
            if !keep.contains(id.as_str()) {
                removed += tx.execute("DELETE FROM units WHERE id=?1", [id])?;
            }
        }
        if removed > 0 {
            bump_baseline(&tx)?;
        }
        tx.commit()?;
        Ok(removed)
    }
    pub fn mark(
        &self,
        command: &CompileCommand,
        provider: &str,
        state: &str,
        report: &Value,
    ) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("INSERT INTO units(id,configuration,main_file,command_key,provider,state,report) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET state=excluded.state,report=excluded.report",params![command.unit_id,command.configuration,command.file.to_string_lossy(),command.command_key,provider,state,serde_json::to_string(report)?])?;
        bump_baseline(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn publish(&mut self, snapshot: &Snapshot) -> Result<()> {
        self.publish_profiled(snapshot).map(|_| ())
    }
    pub fn publish_profiled(
        &mut self,
        snapshot: &Snapshot,
    ) -> Result<crate::profile::StoreProfile> {
        let mut profile = crate::profile::StoreProfile {
            enabled: crate::profile::enabled(),
            ..Default::default()
        };
        let total = crate::profile::Clock::start(profile.enabled);
        let mut encoder = zstd::bulk::Compressor::new(1)?;
        let mut decoder = zstd::bulk::Decompressor::new()?;
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO units(id,configuration,main_file,command_key,provider,state,report) VALUES(?1,?2,?3,?4,?5,'current',?6) ON CONFLICT(id) DO UPDATE SET provider=excluded.provider,state='current',report=excluded.report",params![snapshot.unit_id,snapshot.configuration,snapshot.main_file,snapshot.command_key,snapshot.provider,serde_json::to_string(&json!({"symbols":snapshot.symbols.len(),"references":snapshot.references.len(),"calls":snapshot.calls.len(),"parse_micros":snapshot.parse_micros,"extract_micros":snapshot.extract_micros,"clang_accounted_bytes":snapshot.clang_accounted_bytes,"worker_peak_rss_bytes":snapshot.worker_peak_rss_bytes,"input_key":crate::compdb::key(&snapshot.inputs),"diagnostics":snapshot.diagnostics}))?])?;
        let unit: i64 = tx.query_row(
            "SELECT pk FROM units WHERE id=?1",
            [&snapshot.unit_id],
            |r| r.get(0),
        )?;
        for table in ["inputs", "input_aliases", "symbols", "refs", "calls"] {
            tx.execute(&format!("DELETE FROM {table} WHERE unit_pk=?1"), [unit])?;
        }
        {
            let mut put = tx.prepare_cached("INSERT INTO inputs VALUES(?1,?2,?3)")?;
            let mut alias = tx.prepare_cached("INSERT INTO input_aliases VALUES(?1,?2,?3)")?;
            for input in &snapshot.inputs {
                put.execute(params![unit, input.path, &input.hash[..]])?;
                alias.execute(params![
                    unit,
                    Path::new(&input.path).canonicalize()?.to_string_lossy(),
                    input.path
                ])?;
            }
        }
        let (mut entities, mut paths) = (HashMap::new(), HashMap::new());
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO symbols VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;
            for (n, s) in snapshot.symbols.iter().enumerate() {
                let entity = intern(&tx, "entities", &s.id, &mut entities)?;
                let path = intern(&tx, "paths", &s.location.path, &mut paths)?;
                let payload = fact(
                    &tx,
                    FactRef::Symbol(s),
                    &mut encoder,
                    &mut decoder,
                    &mut profile,
                )?;
                put.execute(params![
                    unit,
                    n as i64,
                    entity,
                    path,
                    s.location.offset,
                    s.location.end_offset,
                    s.extent.offset,
                    s.extent.end_offset,
                    s.definition,
                    s.callable,
                    s.name,
                    payload
                ])?;
            }
        }
        {
            let mut put = tx.prepare_cached("INSERT INTO refs VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
            for (n, r) in snapshot.references.iter().enumerate() {
                let target = intern(&tx, "entities", &r.target, &mut entities)?;
                let path = intern(&tx, "paths", &r.location.path, &mut paths)?;
                let payload = fact(
                    &tx,
                    FactRef::Reference(r),
                    &mut encoder,
                    &mut decoder,
                    &mut profile,
                )?;
                put.execute(params![
                    unit,
                    n as i64,
                    target,
                    path,
                    r.location.offset,
                    r.location.end_offset,
                    payload
                ])?;
            }
        }
        {
            let mut put =
                tx.prepare_cached("INSERT INTO calls VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
            for (n, c) in snapshot.calls.iter().enumerate() {
                let caller = c
                    .caller
                    .as_deref()
                    .map(|key| intern(&tx, "entities", key, &mut entities))
                    .transpose()?;
                let target = c
                    .target
                    .as_deref()
                    .map(|key| intern(&tx, "entities", key, &mut entities))
                    .transpose()?;
                let path = intern(&tx, "paths", &c.location.path, &mut paths)?;
                let payload = fact(
                    &tx,
                    FactRef::Call(c),
                    &mut encoder,
                    &mut decoder,
                    &mut profile,
                )?;
                put.execute(params![
                    unit,
                    n as i64,
                    caller,
                    target,
                    c.resolution,
                    path,
                    c.location.offset,
                    c.location.end_offset,
                    payload
                ])?;
            }
        }
        bump_baseline(&tx)?;
        let commit = crate::profile::Clock::start(profile.enabled);
        tx.commit()?;
        profile.commit_micros = commit.micros();
        profile.publish_wall_micros = total.micros();
        profile.sqlite_and_other_micros = profile.publish_wall_micros.saturating_sub(
            (profile.fact_encode_nanos
                + profile.fact_hash_nanos
                + profile.fact_compress_nanos
                + profile.reused_fact_verify_nanos)
                / 1000
                + profile.commit_micros,
        );
        Ok(profile)
    }
    fn valid(
        &self,
        id: &str,
        provider: &str,
        memo: &mut HashMap<String, Option<[u8; 32]>>,
    ) -> Result<bool> {
        let meta: Option<(i64, String, String)> = self
            .connection
            .query_row(
                "SELECT pk,state,provider FROM units WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((pk, state, p)) = meta else {
            return Ok(false);
        };
        if state != "current" || p != provider {
            return Ok(false);
        }
        let inputs = self
            .connection
            .prepare_cached("SELECT path,hash FROM inputs WHERE unit_pk=?1")?
            .query_map([pk], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if inputs.is_empty() {
            return Ok(false);
        }
        for (path, expected) in inputs {
            let actual = memo
                .entry(path.clone())
                .or_insert_with(|| self.input_digest(Path::new(&path)));
            if actual.as_ref().map(|h| h.as_slice()) != Some(expected.as_slice()) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn reusable(&self, command: &CompileCommand, provider: &str) -> Result<bool> {
        self.valid(&command.unit_id, provider, &mut HashMap::new())
    }
    pub fn affected(&self, configuration: &str, path: &str) -> Result<Vec<String>> {
        Ok(self.connection.prepare_cached("SELECT u.id FROM units u WHERE u.main_file=?1 AND u.configuration=?2 UNION SELECT u.id FROM input_aliases i JOIN units u ON u.pk=i.unit_pk WHERE (i.real_path=?1 OR i.input_path=?1) AND u.configuration=?2")?.query_map(params![path,configuration],|r|r.get(0))?.collect::<std::result::Result<_,_>>()?)
    }
    pub fn invalidate(&self, configuration: &str, path: &str) -> Result<usize> {
        let tx = self.connection.unchecked_transaction()?;
        let changed = tx.execute("UPDATE units SET state='stale' WHERE configuration=?1 AND state<>'stale' AND pk IN (SELECT unit_pk FROM input_aliases WHERE real_path=?2 OR input_path=?2)",params![configuration,path])?;
        if changed > 0 {
            bump_baseline(&tx)?;
        }
        tx.commit()?;
        Ok(changed)
    }
    pub fn recorded_units(&self, configuration: &str) -> Result<Vec<String>> {
        Ok(self
            .connection
            .prepare_cached("SELECT id FROM units WHERE configuration=?1")?
            .query_map([configuration], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn invalidate_units(&mut self, configuration: &str, ids: &HashSet<String>) -> Result<()> {
        let tx = self.connection.transaction()?;
        let mut changed = 0;
        for id in ids {
            changed += tx.execute(
                "UPDATE units SET state='stale' WHERE configuration=?1 AND id=?2 AND state<>'stale'",
                params![configuration, id],
            )?;
        }
        if changed > 0 {
            bump_baseline(&tx)?;
        }
        tx.commit()?;
        self.clear_validation();
        Ok(())
    }
    pub fn incompatible_provider_units(&self, configuration: &str, provider: &str) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM units WHERE configuration=?1 AND state='current' AND provider<>?2",
            params![configuration, provider],
            |row| row.get(0),
        )?)
    }
    pub fn status(&self, configuration: &str) -> Result<Value> {
        let mut units = Vec::new();
        for row in self.connection.prepare("SELECT id,main_file,state,report FROM units WHERE configuration=?1 ORDER BY main_file")?.query_map([configuration],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?{
            let(id,file,state,report)=row?;units.push(json!({"unit_id":id,"file":file,"state":state,"report":serde_json::from_str::<Value>(&report)?}));
        }
        let counts=self.connection.query_row("SELECT (SELECT COUNT(*) FROM symbols s JOIN units u ON u.pk=s.unit_pk WHERE u.configuration=?1 AND u.state='current'),(SELECT COUNT(*) FROM refs r JOIN units u ON u.pk=r.unit_pk WHERE u.configuration=?1 AND u.state='current'),(SELECT COUNT(*) FROM calls c JOIN units u ON u.pk=c.unit_pk WHERE u.configuration=?1 AND u.state='current')",[configuration],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))?;
        let facts = self.connection.query_row(
            "SELECT COUNT(*),COALESCE(SUM(length(payload)),0) FROM facts",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok(
            json!({"units":units,"symbols":counts.0,"references":counts.1,"calls":counts.2,"unique_facts_all_configurations":facts.0,"compressed_fact_bytes":facts.1,"status_freshness":"last_recorded; query rows validate contributing input hashes"}),
        )
    }
    fn rows(
        &self,
        sql: &str,
        parameters: &[&dyn rusqlite::ToSql],
        provider: &str,
    ) -> Result<Vec<Value>> {
        let transaction = if self.connection.is_autocommit() {
            Some(self.connection.unchecked_transaction()?)
        } else {
            None
        };
        let candidates = self
            .connection
            .prepare_cached(sql)?
            .query_map(parameters, |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let (mut valid, mut inputs, mut reports) = (HashMap::new(), HashMap::new(), HashMap::new());
        let mut output = Vec::new();
        let mut decoder = zstd::bulk::Decompressor::new()?;
        for (id, file, digest, packed) in candidates {
            let current = if let Some(valid) = valid.get(&id) {
                *valid
            } else {
                let current = self.valid(&id, provider, &mut inputs)?;
                valid.insert(id.clone(), current);
                current
            };
            if !current {
                continue;
            }
            let mut row = decode(&digest, &packed, &mut decoder)?;
            if let Some(caller) = row.get("caller").and_then(Value::as_str) {
                let name:Option<String>=self.connection.query_row("SELECT s.name FROM symbols s JOIN units u ON u.pk=s.unit_pk WHERE s.entity=(SELECT pk FROM entities WHERE key=?1) AND u.id=?2 ORDER BY s.definition DESC LIMIT 1",params![caller,id],|r|r.get(0)).optional()?;
                row["caller_name"] = name.into();
            }
            if !reports.contains_key(&id) {
                let report: String = self.connection.query_row(
                    "SELECT report FROM units WHERE id=?1",
                    [&id],
                    |r| r.get(0),
                )?;
                reports.insert(id.clone(), serde_json::from_str::<Value>(&report)?);
            }
            row["input_key"] = reports[&id]
                .get("input_key")
                .cloned()
                .unwrap_or(Value::Null);
            row["unit_id"] = id.into();
            row["main_file"] = file.into();
            row["evidence"] = "clang_semantic".into();
            row["freshness"] = "included_inputs_validated".into();
            output.push(row);
        }
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(output)
    }
    pub fn symbols(
        &self,
        configuration: &str,
        name: &str,
        provider: &str,
        limit: usize,
    ) -> Result<Value> {
        self.symbols_page(configuration, name, provider, limit, 0)
    }
    pub fn symbols_page(
        &self,
        configuration: &str,
        name: &str,
        provider: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        let limit_val = limit.clamp(1, 1000) as i64;
        let offset_val = offset as i64;
        let count_sql = "SELECT COUNT(*) FROM symbols s JOIN units u ON u.pk=s.unit_pk WHERE u.configuration=?1 AND u.state='current' AND s.name=?2";
        let sql = "SELECT u.id,u.main_file,f.digest,f.payload FROM symbols s JOIN units u ON u.pk=s.unit_pk JOIN facts f ON f.pk=s.fact WHERE u.configuration=?1 AND u.state='current' AND s.name=?2 ORDER BY s.definition DESC,s.path,s.name_start LIMIT ?3 OFFSET ?4";
        let transaction = self.connection.unchecked_transaction()?;
        let total: i64 = transaction.query_row(count_sql, params![configuration, name], |r| r.get(0))?;
        let rows = self.rows(sql, &[&configuration, &name, &limit_val, &offset_val], provider)?;
        let disk_baseline = self.disk_baseline()?;
        transaction.commit()?;
        let total = total as usize;
        let has_more = offset + rows.len() < total;
        let truncated = has_more;
        let truncation_reason = if truncated {
            Some(format!(
                "results truncated at page limit {limit}; {} row(s) remain; fetch next page with offset {}",
                total.saturating_sub(offset + rows.len()),
                offset + rows.len()
            ))
        } else {
            None
        };
        Ok(json!({
            "evidence": "clang_semantic",
            "rows": rows,
            "limit": limit,
            "offset": offset,
            "total": total,
            "has_more": has_more,
            "truncated": truncated,
            "truncation_reason": truncation_reason,
            "completeness": "indexed contributions only; unindexed units/changes may exist",
            "disk_baseline": disk_baseline
        }))
    }
    pub fn relations(
        &self,
        configuration: &str,
        entity: &str,
        direction: &str,
        provider: &str,
        limit: usize,
    ) -> Result<Value> {
        self.relations_page(configuration, entity, direction, provider, limit, 0)
    }
    pub fn relations_page(
        &self,
        configuration: &str,
        entity: &str,
        direction: &str,
        provider: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        self.relations_inner(configuration, entity, direction, provider, None, limit, offset)
    }
    pub fn relations_except(
        &self,
        configuration: &str,
        entity: &str,
        direction: &str,
        provider: &str,
        excluded_unit: &str,
        limit: usize,
    ) -> Result<Value> {
        self.relations_except_page(
            configuration,
            entity,
            direction,
            provider,
            excluded_unit,
            limit,
            0,
        )
    }
    pub fn relations_except_page(
        &self,
        configuration: &str,
        entity: &str,
        direction: &str,
        provider: &str,
        excluded_unit: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        self.relations_inner(
            configuration,
            entity,
            direction,
            provider,
            Some(excluded_unit),
            limit,
            offset,
        )
    }
    fn relations_inner(
        &self,
        configuration: &str,
        entity: &str,
        direction: &str,
        provider: &str,
        excluded_unit: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        let (table, predicate) = match direction {
            "definitions" => (
                "symbols",
                "x.entity=(SELECT pk FROM entities WHERE key=?2) AND x.definition=1",
            ),
            "declarations" => ("symbols", "x.entity=(SELECT pk FROM entities WHERE key=?2)"),
            "references" => ("refs", "x.target=(SELECT pk FROM entities WHERE key=?2)"),
            "incoming" => ("calls", "x.target=(SELECT pk FROM entities WHERE key=?2)"),
            "outgoing" => ("calls", "x.caller=(SELECT pk FROM entities WHERE key=?2)"),
            _ => {
                return Err(
                    "expected definitions/declarations/references/incoming/outgoing".into(),
                );
            }
        };
        let limit_val = limit.clamp(0, 1000) as i64;
        let offset_val = offset as i64;
        let started = Instant::now();
        let exclusion = excluded_unit.map(|_| " AND u.id<>?3").unwrap_or("");
        let count_sql = format!(
            "SELECT COUNT(*) FROM {table} x JOIN units u ON u.pk=x.unit_pk WHERE u.configuration=?1 AND u.state='current' AND {predicate}{exclusion}"
        );
        let limit_parameter = if excluded_unit.is_some() { 4 } else { 3 };
        let offset_parameter = if excluded_unit.is_some() { 5 } else { 4 };
        let sql = format!(
            "SELECT u.id,u.main_file,f.digest,f.payload FROM {table} x JOIN units u ON u.pk=x.unit_pk JOIN facts f ON f.pk=x.fact WHERE u.configuration=?1 AND u.state='current' AND {predicate}{exclusion} ORDER BY x.unit_pk,x.n LIMIT ?{limit_parameter} OFFSET ?{offset_parameter}"
        );
        let mut parameters: Vec<&dyn rusqlite::ToSql> = vec![&configuration, &entity];
        if let Some(excluded_unit) = &excluded_unit {
            parameters.push(excluded_unit);
        }
        parameters.push(&limit_val);
        parameters.push(&offset_val);
        let transaction = self.connection.unchecked_transaction()?;
        let total: i64 = match excluded_unit {
            Some(ex) => transaction.query_row(&count_sql, params![configuration, entity, ex], |r| r.get(0))?,
            None => transaction.query_row(&count_sql, params![configuration, entity], |r| r.get(0))?,
        };
        let rows = if limit_val > 0 {
            self.rows(&sql, &parameters, provider)?
        } else {
            Vec::new()
        };
        let disk_baseline = self.disk_baseline()?;
        transaction.commit()?;
        let total = total as usize;
        let effective_limit = if limit == 0 { 100 } else { limit.clamp(1, 1000) };
        let has_more = offset + rows.len() < total;
        let truncated = has_more;
        let truncation_reason = if truncated {
            Some(format!(
                "results truncated at page limit {effective_limit}; {} row(s) remain; fetch next page with offset {}",
                total.saturating_sub(offset + rows.len()),
                offset + rows.len()
            ))
        } else {
            None
        };
        let completeness = if truncated {
            let page = (offset / effective_limit) + 1;
            let total_pages = (total + effective_limit - 1) / effective_limit;
            format!(
                "showing rows {}..{} of {total}; page {page} of {total_pages}; indexed contributions only; virtual/indirect targets are not expanded",
                offset,
                offset + rows.len()
            )
        } else if total > 0 && offset > 0 {
            format!(
                "showing rows {}..{} of {total} (final page); indexed contributions only; virtual/indirect targets are not expanded",
                offset,
                offset + rows.len()
            )
        } else {
            "indexed contributions only; virtual/indirect targets are not expanded".to_string()
        };
        Ok(json!({
            "evidence": "clang_semantic",
            "entity_id": entity,
            "direction": direction,
            "rows": rows,
            "limit": effective_limit,
            "offset": offset,
            "total": total,
            "has_more": has_more,
            "truncated": truncated,
            "truncation_reason": truncation_reason,
            "query_micros": started.elapsed().as_micros(),
            "completeness": completeness,
            "disk_baseline": disk_baseline
        }))
    }
    pub fn at(
        &self,
        configuration: &str,
        path: &str,
        offset: u64,
        provider: &str,
    ) -> Result<Value> {
        self.at_inner(configuration, path, offset, provider, None)
    }
    pub fn at_except(
        &self,
        configuration: &str,
        path: &str,
        offset: u64,
        provider: &str,
        excluded_unit: &str,
    ) -> Result<Value> {
        self.at_inner(configuration, path, offset, provider, Some(excluded_unit))
    }
    fn at_inner(
        &self,
        configuration: &str,
        path: &str,
        offset: u64,
        provider: &str,
        excluded_unit: Option<&str>,
    ) -> Result<Value> {
        let transaction = self.connection.unchecked_transaction()?;
        let offset = i64::try_from(offset)?;
        let exclusion = excluded_unit.map(|_| " AND u.id<>?4").unwrap_or("");
        let refs_sql = format!(
            "SELECT u.id,u.main_file,f.digest,f.payload FROM refs r JOIN units u ON u.pk=r.unit_pk JOIN facts f ON f.pk=r.fact WHERE u.configuration=?1 AND u.state='current'{exclusion} AND r.path=(SELECT pk FROM paths WHERE key=?2) AND r.start<=?3 AND r.end>?3 ORDER BY r.end-r.start LIMIT 32"
        );
        let declarations_sql = format!(
            "SELECT u.id,u.main_file,f.digest,f.payload FROM symbols s JOIN units u ON u.pk=s.unit_pk JOIN facts f ON f.pk=s.fact WHERE u.configuration=?1 AND u.state='current'{exclusion} AND s.path=(SELECT pk FROM paths WHERE key=?2) AND s.name_start<=?3 AND s.name_end>?3 ORDER BY s.definition DESC LIMIT 32"
        );
        let enclosing_sql = format!(
            "SELECT u.id,u.main_file,f.digest,f.payload FROM symbols s JOIN units u ON u.pk=s.unit_pk JOIN facts f ON f.pk=s.fact WHERE u.configuration=?1 AND u.state='current'{exclusion} AND s.path=(SELECT pk FROM paths WHERE key=?2) AND s.callable=1 AND s.definition=1 AND s.start<=?3 AND s.end>?3 ORDER BY s.end-s.start LIMIT 32"
        );
        let mut parameters: Vec<&dyn rusqlite::ToSql> = vec![&configuration, &path, &offset];
        if let Some(excluded_unit) = &excluded_unit {
            parameters.push(excluded_unit);
        }
        let refs = self.rows(&refs_sql, &parameters, provider)?;
        let declarations = self.rows(&declarations_sql, &parameters, provider)?;
        let enclosing = self.rows(&enclosing_sql, &parameters, provider)?;
        let entities: HashSet<&str> = refs
            .iter()
            .filter_map(|row| row.get("target").and_then(Value::as_str))
            .chain(
                declarations
                    .iter()
                    .filter_map(|row| row.get("id").and_then(Value::as_str)),
            )
            .collect();
        let entity = if entities.len() == 1 {
            entities.into_iter().next().map(str::to_owned)
        } else {
            None
        };
        let disk_baseline = self.disk_baseline()?;
        transaction.commit()?;
        Ok(
            json!({"entity_id":entity,"references":refs,"declarations":declarations,"enclosing_functions":enclosing,"evidence":"clang_semantic","completeness":"indexed contributions; multiple variants remain separate","disk_baseline":disk_baseline}),
        )
    }
    pub fn checkpoint(&self) -> Result<()> {
        // Reclaim unreachable facts; SQLite reuses freed pages on later updates.
        self.connection.execute_batch("DELETE FROM facts WHERE NOT EXISTS(SELECT 1 FROM symbols WHERE symbols.fact=facts.pk) AND NOT EXISTS(SELECT 1 FROM refs WHERE refs.fact=facts.pk) AND NOT EXISTS(SELECT 1 FROM calls WHERE calls.fact=facts.pk);PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
}
