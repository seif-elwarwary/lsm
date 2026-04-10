use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;
use thiserror::Error;

use crate::memtable::{LookupResult, MemTable, MemTableError};

#[derive(Debug)]
pub struct Db {
    state: RwLock<DbState>,
    is_open: AtomicBool,
}

#[derive(Debug)]
struct DbState {
    writable_memtable: Arc<MemTable>,
    immutable_memtables: VecDeque<Arc<MemTable>>,
}

impl Default for Db {
    fn default() -> Self {
        Self::open()
    }
}

impl Db {
    pub fn open() -> Self {
        Self {
            state: RwLock::new(DbState {
                writable_memtable: Arc::new(MemTable::new()),
                immutable_memtables: VecDeque::new(),
            }),
            is_open: AtomicBool::new(true),
        }
    }

    pub fn close(&self) -> Result<(), DbError> {
        self.ensure_open()?;
        self.is_open.store(false, Ordering::Release);
        Ok(())
    }

    pub fn put(&self, key: impl Into<String>, value: impl Into<String>) -> Result<(), DbError> {
        self.ensure_open()?;
        let key = key.into();
        let value = value.into();
        self.write_with_rotation(|memtable| memtable.put(key.clone(), value.clone()))
    }

    pub fn get(&self, key: &str) -> Result<LookupResult, DbError> {
        self.ensure_open()?;
        let (writable_memtable, immutable_memtables) = self.memtable_snapshot();

        if let Some(entry) = writable_memtable.get_entry(key) {
            return Ok(entry.to_lookup_result());
        }

        for memtable in immutable_memtables {
            if let Some(entry) = memtable.get_entry(key) {
                return Ok(entry.to_lookup_result());
            }
        }

        Ok(LookupResult::NotFound)
    }

    pub fn delete(&self, key: impl Into<String>) -> Result<(), DbError> {
        self.ensure_open()?;
        let key = key.into();
        self.write_with_rotation(|memtable| memtable.delete(key.clone()))
    }

    pub fn len(&self) -> Result<usize, DbError> {
        self.ensure_open()?;
        let (writable_memtable, immutable_memtables) = self.memtable_snapshot();
        Ok(writable_memtable.len()
            + immutable_memtables
                .iter()
                .map(|memtable| memtable.len())
                .sum::<usize>())
    }

    pub fn is_empty(&self) -> Result<bool, DbError> {
        self.ensure_open()?;
        Ok(self.len()? == 0)
    }

    pub fn used_bytes(&self) -> Result<usize, DbError> {
        self.ensure_open()?;
        let (writable_memtable, immutable_memtables) = self.memtable_snapshot();
        Ok(writable_memtable.used_bytes()
            + immutable_memtables
                .iter()
                .map(|memtable| memtable.used_bytes())
                .sum::<usize>())
    }

    pub fn remaining_bytes(&self) -> Result<usize, DbError> {
        self.ensure_open()?;
        Ok(self.current_writable_memtable().remaining_bytes())
    }

    pub fn immutable_memtable_count(&self) -> Result<usize, DbError> {
        self.ensure_open()?;
        Ok(self.state.read().immutable_memtables.len())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pop_oldest_immutable_memtable(&self) -> Result<Option<Arc<MemTable>>, DbError> {
        self.ensure_open()?;
        Ok(self.state.write().immutable_memtables.pop_back())
    }

    fn ensure_open(&self) -> Result<(), DbError> {
        if self.is_open.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(DbError::Closed)
        }
    }

    fn write_with_rotation<F>(&self, mut op: F) -> Result<(), DbError>
    where
        F: FnMut(&MemTable) -> Result<(), MemTableError>,
    {
        loop {
            let writable_memtable = self.current_writable_memtable();

            match op(&writable_memtable) {
                Ok(()) => return Ok(()),
                Err(MemTableError::MemTableFull { .. }) => {
                    self.rotate_writable_memtable_if_current(&writable_memtable);
                }
                Err(error) => return Err(DbError::from(error)),
            }
        }
    }

    fn current_writable_memtable(&self) -> Arc<MemTable> {
        Arc::clone(&self.state.read().writable_memtable)
    }

    fn memtable_snapshot(&self) -> (Arc<MemTable>, Vec<Arc<MemTable>>) {
        let state = self.state.read();
        (
            Arc::clone(&state.writable_memtable),
            state.immutable_memtables.iter().cloned().collect(),
        )
    }

    fn rotate_writable_memtable_if_current(&self, current_writable_memtable: &Arc<MemTable>) {
        let mut state = self.state.write();
        if !Arc::ptr_eq(&state.writable_memtable, current_writable_memtable) {
            return;
        }

        let frozen_memtable = Arc::clone(&state.writable_memtable);
        state.writable_memtable = Arc::new(MemTable::new());
        state.immutable_memtables.push_front(frozen_memtable);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn rotate_writable_memtable(&self) -> Option<Arc<MemTable>> {
        let mut state = self.state.write();
        if state.writable_memtable.is_empty() {
            return None;
        }

        let frozen_memtable = Arc::clone(&state.writable_memtable);
        state.writable_memtable = Arc::new(MemTable::new());
        state
            .immutable_memtables
            .push_front(Arc::clone(&frozen_memtable));
        Some(frozen_memtable)
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

    fn fixed_string(byte_len: usize, fill: char) -> String {
        std::iter::repeat_n(fill, byte_len).collect()
    }

    #[test]
    fn open_put_get_delete_close_round_trip_works() {
        let db = Db::open();

        assert!(db.is_empty().unwrap());
        db.put("alpha", "value").unwrap();
        assert_eq!(db.len().unwrap(), 1);
        assert!(db.used_bytes().unwrap() > 0);
        assert!(db.remaining_bytes().unwrap() < crate::memtable::MEMTABLE_CAPACITY_BYTES);
        assert_eq!(
            db.get("alpha").unwrap(),
            LookupResult::Value("value".into())
        );

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

        assert_eq!(
            db.put("", "value").unwrap_err(),
            DbError::MemTable(MemTableError::EmptyKey)
        );
    }

    #[test]
    fn deleting_a_non_existent_key_creates_a_tombstone_and_returns_not_found() {
        let db = Db::open();

        db.delete("missing").unwrap();

        assert_eq!(db.get("missing").unwrap(), LookupResult::NotFound);
        assert_eq!(db.len().unwrap(), 1);
    }

    #[test]
    fn full_writable_memtable_rotates_without_losing_reads() {
        let db = Db::open();
        let value = fixed_string(
            crate::memtable::ENTRY_BUDGET_BYTES - crate::memtable::METADATA_OVERHEAD_BYTES - 8,
            'v',
        );

        for index in
            0..(crate::memtable::MEMTABLE_CAPACITY_BYTES / crate::memtable::ENTRY_BUDGET_BYTES)
        {
            db.put(format!("k{index:07}"), value.clone()).unwrap();
        }

        assert_eq!(db.immutable_memtable_count().unwrap(), 0);

        db.put("z0000000", value.clone()).unwrap();

        assert_eq!(db.immutable_memtable_count().unwrap(), 1);
        assert_eq!(
            db.get("k0000000").unwrap(),
            LookupResult::Value(value.clone())
        );
        assert_eq!(db.get("z0000000").unwrap(), LookupResult::Value(value));
        assert_eq!(
            db.remaining_bytes().unwrap(),
            crate::memtable::MEMTABLE_CAPACITY_BYTES - crate::memtable::ENTRY_BUDGET_BYTES
        );
    }

    #[test]
    fn newer_tombstone_hides_older_value_across_memtables() {
        let db = Db::open();

        db.put("alpha", "value").unwrap();
        assert!(db.rotate_writable_memtable().is_some());

        db.delete("alpha").unwrap();

        assert_eq!(db.get("alpha").unwrap(), LookupResult::NotFound);
        assert_eq!(db.len().unwrap(), 2);
        assert_eq!(db.immutable_memtable_count().unwrap(), 1);
    }

    #[test]
    fn immutable_memtables_are_popped_oldest_first_for_future_flushes() {
        let db = Db::open();

        db.put("alpha", "one").unwrap();
        db.rotate_writable_memtable().unwrap();
        db.put("beta", "two").unwrap();
        db.rotate_writable_memtable().unwrap();

        let oldest = db.pop_oldest_immutable_memtable().unwrap().unwrap();
        let next = db.pop_oldest_immutable_memtable().unwrap().unwrap();

        assert_eq!(
            oldest.entries_snapshot(),
            vec![("alpha".into(), oldest.get_entry("alpha").unwrap(),)]
        );
        assert_eq!(
            next.entries_snapshot(),
            vec![("beta".into(), next.get_entry("beta").unwrap(),)]
        );
        assert!(db.pop_oldest_immutable_memtable().unwrap().is_none());
    }
}
