//! Experimental immutable append pack. It supplies snapshot get, not graph indexes.
use crate::{ANALYZER_KEY, MAX_PAYLOAD, Result, Snapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    source_hash: [u8; 32],
    checksum: [u8; 32],
    source_bytes: u64,
    offset: u64,
    length: u64,
    dependencies: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Directory {
    version: u32,
    analyzer: String,
    entries: BTreeMap<String, Entry>,
}
pub struct Pack {
    file: File,
    path: PathBuf,
    entries: BTreeMap<String, Entry>,
}
impl Pack {
    pub fn open(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory)?;
        let path = directory.join("snapshots.dir");
        let data = directory.join("snapshots.pack");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data)?;
        if file.metadata()?.len() == 0 {
            file.write_all(b"SWPACK01")?;
        }
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != b"SWPACK01" {
            return Err("pack magic mismatch".into());
        }
        let entries = if path.exists() {
            let bytes = fs::read(&path)?;
            if bytes.len() > 32 * 1024 * 1024 {
                return Err("pack directory too large".into());
            }
            let (dir, used): (Directory, usize) = bincode::serde::decode_from_slice(
                &bytes,
                bincode::config::standard().with_limit::<{ 32 * 1024 * 1024 }>(),
            )?;
            if used != bytes.len() || dir.version != 1 || dir.analyzer != ANALYZER_KEY {
                return Err("pack version mismatch".into());
            }
            dir.entries
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            file,
            path,
            entries,
        })
    }
    pub fn put(&mut self, snapshot: &Snapshot) -> Result<()> {
        if snapshot.payload.len() > MAX_PAYLOAD {
            return Err("pack payload too large".into());
        }
        let offset = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&snapshot.payload)?;
        self.entries.insert(
            snapshot.path.clone(),
            Entry {
                source_hash: snapshot.source_hash,
                checksum: snapshot.checksum,
                source_bytes: snapshot.source_bytes,
                offset,
                length: snapshot.payload.len() as u64,
                dependencies: snapshot.dependencies.clone(),
            },
        );
        Ok(())
    }
    pub fn commit(&mut self) -> Result<()> {
        self.file.sync_all()?;
        let directory = Directory {
            version: 1,
            analyzer: ANALYZER_KEY.into(),
            entries: self.entries.clone(),
        };
        let bytes = bincode::serde::encode_to_vec(&directory, bincode::config::standard())?;
        let temp = self.path.with_extension("dir.tmp");
        {
            let mut file = File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(temp, &self.path)?;
        #[cfg(unix)]
        File::open(self.path.parent().ok_or("missing pack parent")?)?.sync_all()?;
        Ok(())
    }
    pub fn get(&mut self, path: &str) -> Result<Option<Snapshot>> {
        let Some(entry) = self.entries.get(path) else {
            return Ok(None);
        };
        let size = self.file.metadata()?.len();
        if entry.length > MAX_PAYLOAD as u64
            || entry.offset < 8
            || entry
                .offset
                .checked_add(entry.length)
                .is_none_or(|end| end > size)
        {
            return Err("pack record outside file".into());
        }
        self.file.seek(SeekFrom::Start(entry.offset))?;
        let mut payload = vec![0; entry.length as usize];
        self.file.read_exact(&mut payload)?;
        Ok(Some(Snapshot {
            path: path.into(),
            source_hash: entry.source_hash,
            checksum: entry.checksum,
            source_bytes: entry.source_bytes,
            payload,
            dependencies: entry.dependencies.clone(),
        }))
    }
    pub fn paths(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
    pub fn source_hash(&self, path: &str) -> Option<[u8; 32]> {
        self.entries.get(path).map(|e| e.source_hash)
    }
    pub fn directory_estimated_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(k, e)| {
                k.capacity()
                    + std::mem::size_of::<Entry>()
                    + 64
                    + e.dependencies.capacity() * std::mem::size_of::<String>()
                    + e.dependencies.iter().map(String::capacity).sum::<usize>()
            })
            .sum()
    }
}
