//! Experimental full secondary indexes + external append-only payload segments.
use crate::{MAX_PAYLOAD, Result, Snapshot, store::Database};
use rusqlite::OptionalExtension;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};
pub struct Hybrid {
    pub db: Database,
    file: File,
}
impl Hybrid {
    pub fn open(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        let db = Database::open(&directory.join("index.sqlite3"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("facts.segment"))?;
        if file.metadata()?.len() == 0 {
            file.write_all(b"SWHYBR01")?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != b"SWHYBR01" {
            return Err("hybrid format".into());
        }
        Ok(Self { db, file })
    }
    pub fn put_batch(&mut self, values: &[Snapshot], secondary: bool) -> Result<()> {
        let mut locations = Vec::new();
        for value in values {
            if value.payload.len() > MAX_PAYLOAD {
                return Err("hybrid payload size".into());
            }
            let offset = self.file.seek(SeekFrom::End(0))?;
            self.file.write_all(&value.payload)?;
            locations.push((offset, value.payload.len() as u64));
        }
        // Commit ordering: durable data first, then atomically publish metadata/index rows.
        // Failed DB commit leaves unreachable tail bytes, never a dangling published offset.
        self.file.sync_all()?;
        self.db.put_external_batch(values, secondary, &locations)
    }
    pub fn get(&mut self, path: &str) -> Result<Option<Snapshot>> {
        let meta=self.db.connection.query_row("SELECT s.source_hash,s.checksum,s.source_bytes,l.offset,l.length FROM snapshots s JOIN pack_location l USING(path) WHERE s.path=?1",[path],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?))).optional()?;
        let Some((source, checksum, source_bytes, offset, length)) = meta else {
            return Ok(None);
        };
        if offset < 8
            || length < 0
            || length as usize > MAX_PAYLOAD
            || (offset as u64)
                .checked_add(length as u64)
                .is_none_or(|end| end > self.file.metadata().map(|m| m.len()).unwrap_or(0))
        {
            return Err("hybrid offset outside segment".into());
        }
        let mut payload = vec![0; length as usize];
        self.file.seek(SeekFrom::Start(offset as u64))?;
        self.file.read_exact(&mut payload)?;
        Ok(Some(Snapshot {
            path: path.into(),
            source_hash: source.try_into().map_err(|_| "source hash length")?,
            checksum: checksum.try_into().map_err(|_| "checksum length")?,
            source_bytes: source_bytes as u64,
            payload,
            dependencies: Vec::new(),
        }))
    }
}
