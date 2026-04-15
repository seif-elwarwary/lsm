use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const CATALOG_FILE_NAME: &str = "CATALOG.json";
const CATALOG_TMP_FILE_NAME: &str = "CATALOG.json.tmp";
const CATALOG_VERSION: u32 = 1;

/// A single SSTable registered with the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: u64,
    /// Path to the SSTable file relative to the DB directory, e.g. `"sst/00000001.sst"`.
    pub path: String,
    pub entry_count: u32,
    pub smallest_key: String,
    pub largest_key: String,
    pub data_crc32: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct CatalogFile {
    version: u32,
    /// Level-0 SSTables, newest (highest id) first. Reads walk this in order.
    level0: Vec<CatalogEntry>,
}

/// In-memory catalog backed by a JSON file in the DB directory.
///
/// The on-disk catalog is the single source of truth for which SSTables
/// exist and in what order reads must consult them. On every mutation we
/// write to a tmp file and atomically rename it over the real catalog so
/// we can't crash mid-write and leave a half-serialized file behind.
#[derive(Debug)]
pub struct Catalog {
    db_path: PathBuf,
    level0: Vec<CatalogEntry>,
    next_id: u64,
}

impl Catalog {
    pub fn load_or_create(db_path: impl AsRef<Path>) -> Result<Self, CatalogError> {
        let db_path = db_path.as_ref().to_owned();
        let catalog_path = db_path.join(CATALOG_FILE_NAME);

        let level0 = if catalog_path.exists() {
            let raw = fs::read_to_string(&catalog_path)?;
            let parsed: CatalogFile = serde_json::from_str(&raw)?;
            if parsed.version != CATALOG_VERSION {
                return Err(CatalogError::UnsupportedVersion(parsed.version));
            }
            parsed.level0
        } else {
            Vec::new()
        };

        let next_id = level0.iter().map(|entry| entry.id).max().unwrap_or(0) + 1;

        Ok(Self {
            db_path,
            level0,
            next_id,
        })
    }

    /// Reserve the next SSTable id. Callers are expected to write the SSTable
    /// file at `sst/{id:08}.sst` then register it via `add_level0`.
    pub fn allocate_sst_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Register a freshly-flushed L0 SSTable. Newest entries are pushed to the
    /// front so that reads encounter the most recent SSTable first.
    pub fn add_level0(&mut self, entry: CatalogEntry) -> Result<(), CatalogError> {
        self.level0.insert(0, entry);
        self.persist()
    }

    /// L0 SSTables in read order: newest first.
    pub fn level0_entries(&self) -> &[CatalogEntry] {
        &self.level0
    }

    fn persist(&self) -> Result<(), CatalogError> {
        let file = CatalogFile {
            version: CATALOG_VERSION,
            level0: self.level0.clone(),
        };
        let json = serde_json::to_string_pretty(&file)?;

        let tmp_path = self.db_path.join(CATALOG_TMP_FILE_NAME);
        let final_path = self.db_path.join(CATALOG_FILE_NAME);
        fs::write(&tmp_path, json)?;
        fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported catalog version {0}")]
    UnsupportedVersion(u32),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_entry(id: u64, smallest: &str, largest: &str) -> CatalogEntry {
        CatalogEntry {
            id,
            path: format!("sst/{id:08}.sst"),
            entry_count: 10,
            smallest_key: smallest.into(),
            largest_key: largest.into(),
            data_crc32: 0,
        }
    }

    #[test]
    fn empty_catalog_starts_with_id_one() {
        let dir = TempDir::new().unwrap();
        let mut catalog = Catalog::load_or_create(dir.path()).unwrap();
        assert_eq!(catalog.allocate_sst_id(), 1);
        assert_eq!(catalog.allocate_sst_id(), 2);
    }

    #[test]
    fn add_level0_puts_newest_first_and_persists() {
        let dir = TempDir::new().unwrap();
        let mut catalog = Catalog::load_or_create(dir.path()).unwrap();

        catalog.add_level0(sample_entry(1, "a", "m")).unwrap();
        catalog.add_level0(sample_entry(2, "n", "z")).unwrap();

        let entries = catalog.level0_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 2);
        assert_eq!(entries[1].id, 1);

        // Reload from disk
        let reloaded = Catalog::load_or_create(dir.path()).unwrap();
        let entries = reloaded.level0_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 2);
        assert_eq!(entries[1].id, 1);
    }

    #[test]
    fn next_id_continues_after_reload() {
        let dir = TempDir::new().unwrap();
        {
            let mut catalog = Catalog::load_or_create(dir.path()).unwrap();
            catalog.add_level0(sample_entry(1, "a", "m")).unwrap();
            catalog.add_level0(sample_entry(2, "n", "z")).unwrap();
        }

        let mut reloaded = Catalog::load_or_create(dir.path()).unwrap();
        assert_eq!(reloaded.allocate_sst_id(), 3);
    }
}
