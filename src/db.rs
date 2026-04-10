use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

use crate::memtable::{LookupResult, MemTable, MemTableError};

#[derive(Debug)]
pub struct Db {
    memtable: MemTable,
    is_open: AtomicBool,
}

impl Default for Db {
    fn default() -> Self {
        Self::open()
    }
}

impl Db {
    pub fn open() -> Self {
        Self {
            memtable: MemTable::new(),
            is_open: AtomicBool::new(true),
        }
    }

    pub fn close(&self) -> Result<(), DbError> {
        self.ensure_open()?;
        self.is_open.store(false, Ordering::Release);
        Ok(())
    }

    pub fn put(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), DbError> {
        self.ensure_open()?;
        self.memtable.put(key, value).map_err(DbError::from)
    }

    pub fn get(&self, key: &str) -> Result<LookupResult, DbError> {
        self.ensure_open()?;
        Ok(self.memtable.get(key))
    }

    pub fn delete(&self, key: impl Into<String>) -> Result<(), DbError> {
        self.ensure_open()?;
        self.memtable.delete(key).map_err(DbError::from)
    }

    pub fn len(&self) -> Result<usize, DbError> {
        self.ensure_open()?;
        Ok(self.memtable.len())
    }

    pub fn is_empty(&self) -> Result<bool, DbError> {
        self.ensure_open()?;
        Ok(self.memtable.is_empty())
    }

    pub fn used_bytes(&self) -> Result<usize, DbError> {
        self.ensure_open()?;
        Ok(self.memtable.used_bytes())
    }

    pub fn remaining_bytes(&self) -> Result<usize, DbError> {
        self.ensure_open()?;
        Ok(self.memtable.remaining_bytes())
    }

    fn ensure_open(&self) -> Result<(), DbError> {
        if self.is_open.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(DbError::Closed)
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DbError {
    #[error("database is closed")]
    Closed,
    #[error(transparent)]
    MemTable(#[from] MemTableError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_put_get_delete_close_round_trip_works() {
        let db = Db::open();

        assert!(db.is_empty().unwrap());
        db.put("alpha", "value").unwrap();
        assert_eq!(db.len().unwrap(), 1);
        assert!(db.used_bytes().unwrap() > 0);
        assert!(db.remaining_bytes().unwrap() < crate::memtable::MEMTABLE_CAPACITY_BYTES);
        assert_eq!(db.get("alpha").unwrap(), LookupResult::Value("value".into()));

        db.delete("alpha").unwrap();
        assert_eq!(db.get("alpha").unwrap(), LookupResult::NotFound);

        db.close().unwrap();
    }

    #[test]
    fn operations_fail_after_close() {
        let db = Db::open();
        db.close().unwrap();

        assert_eq!(db.get("alpha").unwrap_err(), DbError::Closed);
        assert_eq!(db.put("alpha", "value").unwrap_err(), DbError::Closed);
        assert_eq!(db.delete("alpha").unwrap_err(), DbError::Closed);
        assert_eq!(db.len().unwrap_err(), DbError::Closed);
        assert_eq!(db.close().unwrap_err(), DbError::Closed);
    }

    #[test]
    fn validation_errors_bubble_up_through_db() {
        let db = Db::open();

        assert_eq!(db.put("", "value").unwrap_err(), DbError::MemTable(MemTableError::EmptyKey));
    }

    #[test]
    fn deleting_a_non_existent_key_creates_a_tombstone_and_returns_not_found() {
        let db = Db::open();

        db.delete("missing").unwrap();

        assert_eq!(db.get("missing").unwrap(), LookupResult::NotFound);
        assert_eq!(db.len().unwrap(), 1);
    }
}
