use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_skiplist::SkipMap;
use parking_lot::Mutex;
use thiserror::Error;

pub const MEMTABLE_CAPACITY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_KEY_SIZE_BYTES: usize = 1024;
pub const ENTRY_BUDGET_BYTES: usize = 4096;
pub const METADATA_OVERHEAD_BYTES: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupResult {
    Value(String),
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemTableEntry {
    pub value: Option<String>,
    pub key_size_bytes: usize,
    pub value_size_bytes: usize,
    pub entry_size_bytes: usize,
    pub is_tombstone: bool,
}

impl MemTableEntry {
    fn new_value(key: &str, value: String) -> Result<Self, MemTableError> {
        Self::build(key, Some(value))
    }

    fn new_tombstone(key: &str) -> Result<Self, MemTableError> {
        Self::build(key, None)
    }

    fn build(key: &str, value: Option<String>) -> Result<Self, MemTableError> {
        validate_key(key)?;

        let key_size_bytes = key.len();
        let value_size_bytes = value.as_ref().map_or(0, |value| value.len());
        let max_value_size_bytes = max_value_size_bytes_for_key(key_size_bytes);

        if value_size_bytes > max_value_size_bytes {
            return Err(MemTableError::ValueTooLarge {
                actual: value_size_bytes,
                max: max_value_size_bytes,
            });
        }

        let entry_size_bytes = key_size_bytes + value_size_bytes + METADATA_OVERHEAD_BYTES;
        if entry_size_bytes > ENTRY_BUDGET_BYTES {
            return Err(MemTableError::EntryTooLarge {
                actual: entry_size_bytes,
                max: ENTRY_BUDGET_BYTES,
            });
        }
        let is_tombstone = value.is_none();

        Ok(Self {
            value,
            key_size_bytes,
            value_size_bytes,
            entry_size_bytes,
            is_tombstone,
        })
    }

    pub(crate) fn to_lookup_result(&self) -> LookupResult {
        match &self.value {
            Some(value) => LookupResult::Value(value.clone()),
            None => LookupResult::NotFound,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MemTableError {
    #[error("keys must not be empty")]
    EmptyKey,
    #[error("key size {actual} exceeds maximum {max} bytes")]
    KeyTooLarge { actual: usize, max: usize },
    #[error("value size {actual} exceeds maximum {max} bytes for this key")]
    ValueTooLarge { actual: usize, max: usize },
    #[error("entry size {actual} exceeds maximum {max} bytes")]
    EntryTooLarge { actual: usize, max: usize },
    #[error(
        "memtable is full: requested {requested} bytes, remaining {remaining} bytes, capacity {capacity} bytes"
    )]
    MemTableFull {
        requested: usize,
        remaining: usize,
        capacity: usize,
    },
}

#[derive(Debug)]
pub struct MemTable {
    entries: SkipMap<String, MemTableEntry>,
    used_bytes: AtomicUsize,
    write_lock: Mutex<()>,
}

impl Default for MemTable {
    fn default() -> Self {
        Self::new()
    }
}

impl MemTable {
    pub fn new() -> Self {
        Self {
            entries: SkipMap::new(),
            used_bytes: AtomicUsize::new(0),
            write_lock: Mutex::new(()),
        }
    }

    pub fn put(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), MemTableError> {
        let key = key.into();
        let value = value.into();
        let entry = MemTableEntry::new_value(&key, value)?;
        self.upsert_entry(key, entry)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, key: &str) -> LookupResult {
        self.get_entry(key)
            .map(|entry| entry.to_lookup_result())
            .unwrap_or(LookupResult::NotFound)
    }

    pub fn delete(&self, key: impl Into<String>) -> Result<(), MemTableError> {
        let key = key.into();
        let entry = MemTableEntry::new_tombstone(&key)?;
        self.upsert_entry(key, entry)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Acquire)
    }

    pub fn remaining_bytes(&self) -> usize {
        MEMTABLE_CAPACITY_BYTES.saturating_sub(self.used_bytes())
    }

    pub(crate) fn get_entry(&self, key: &str) -> Option<MemTableEntry> {
        self.entries.get(key).map(|entry| entry.value().clone())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn entries_snapshot(&self) -> Vec<(String, MemTableEntry)> {
        self.entries
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    fn upsert_entry(&self, key: String, entry: MemTableEntry) -> Result<(), MemTableError> {
        let _guard = self.write_lock.lock();
        let current_used_bytes = self.used_bytes.load(Ordering::Acquire);
        let replaced_bytes = self
            .entries
            .get(&key)
            .map(|existing| existing.value().entry_size_bytes)
            .unwrap_or(0);

        let remaining_bytes = MEMTABLE_CAPACITY_BYTES
            .saturating_sub(current_used_bytes.saturating_sub(replaced_bytes));

        if entry.entry_size_bytes > remaining_bytes {
            return Err(MemTableError::MemTableFull {
                requested: entry.entry_size_bytes,
                remaining: remaining_bytes,
                capacity: MEMTABLE_CAPACITY_BYTES,
            });
        }

        self.entries.insert(key, entry.clone());
        let updated_used_bytes = current_used_bytes - replaced_bytes + entry.entry_size_bytes;
        self.used_bytes.store(updated_used_bytes, Ordering::Release);
        Ok(())
    }
}

fn validate_key(key: &str) -> Result<(), MemTableError> {
    if key.is_empty() {
        return Err(MemTableError::EmptyKey);
    }

    let key_size_bytes = key.len();
    if key_size_bytes > MAX_KEY_SIZE_BYTES {
        return Err(MemTableError::KeyTooLarge {
            actual: key_size_bytes,
            max: MAX_KEY_SIZE_BYTES,
        });
    }

    Ok(())
}

fn max_value_size_bytes_for_key(key_size_bytes: usize) -> usize {
    ENTRY_BUDGET_BYTES.saturating_sub(METADATA_OVERHEAD_BYTES + key_size_bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    fn fixed_string(byte_len: usize, fill: char) -> String {
        std::iter::repeat_n(fill, byte_len).collect()
    }

    #[test]
    fn empty_memtable_returns_not_found() {
        let memtable = MemTable::new();

        assert_eq!(memtable.get("missing"), LookupResult::NotFound);
        assert_eq!(memtable.used_bytes(), 0);
        assert_eq!(memtable.remaining_bytes(), MEMTABLE_CAPACITY_BYTES);
        assert!(memtable.is_empty());
    }

    #[test]
    fn put_get_delete_round_trip_works() {
        let memtable = MemTable::new();

        memtable.put("alpha", "value").unwrap();
        assert_eq!(memtable.get("alpha"), LookupResult::Value("value".into()));

        memtable.delete("alpha").unwrap();
        assert_eq!(memtable.get("alpha"), LookupResult::NotFound);
        assert_eq!(memtable.len(), 1);
    }

    #[test]
    fn exact_boundary_sizes_pass_and_one_byte_over_fails() {
        let memtable = MemTable::new();

        let max_key = fixed_string(MAX_KEY_SIZE_BYTES, 'k');
        let max_value_size = ENTRY_BUDGET_BYTES - METADATA_OVERHEAD_BYTES - max_key.len();
        let max_value = fixed_string(max_value_size, 'v');

        memtable.put(max_key.clone(), max_value).unwrap();

        let oversized_key = fixed_string(MAX_KEY_SIZE_BYTES + 1, 'k');
        assert_eq!(
            memtable.put(oversized_key, "value").unwrap_err(),
            MemTableError::KeyTooLarge {
                actual: MAX_KEY_SIZE_BYTES + 1,
                max: MAX_KEY_SIZE_BYTES,
            }
        );

        let oversized_value = fixed_string(max_value_size + 1, 'v');
        assert_eq!(
            memtable.put(max_key, oversized_value).unwrap_err(),
            MemTableError::ValueTooLarge {
                actual: max_value_size + 1,
                max: max_value_size,
            }
        );
    }

    #[test]
    fn overwrite_replaces_accounting_instead_of_double_counting() {
        let memtable = MemTable::new();
        let key = "accounted";

        memtable.put(key, "one").unwrap();
        let first_size = memtable.used_bytes();

        memtable.put(key, "longer-value").unwrap();
        let second_size = memtable.used_bytes();

        assert_eq!(memtable.len(), 1);
        assert!(second_size > first_size);
        assert_eq!(
            second_size,
            key.len() + "longer-value".len() + METADATA_OVERHEAD_BYTES
        );
    }

    #[test]
    fn capacity_exhaustion_returns_memtable_full() {
        let memtable = MemTable::new();
        let value = fixed_string(ENTRY_BUDGET_BYTES - METADATA_OVERHEAD_BYTES - 8, 'v');

        for index in 0..(MEMTABLE_CAPACITY_BYTES / ENTRY_BUDGET_BYTES) {
            let key = format!("k{index:07}");
            memtable.put(key, value.clone()).unwrap();
        }

        let error = memtable.put("z0000000", value).unwrap_err();
        assert_eq!(
            error,
            MemTableError::MemTableFull {
                requested: ENTRY_BUDGET_BYTES,
                remaining: 0,
                capacity: MEMTABLE_CAPACITY_BYTES,
            }
        );
    }

    #[test]
    fn concurrent_writers_on_distinct_keys_complete_without_overflow() {
        let memtable = Arc::new(MemTable::new());
        let thread_count = 4;
        let keys_per_thread = 128;
        let barrier = Arc::new(Barrier::new(thread_count));

        let handles: Vec<_> = (0..thread_count)
            .map(|thread_index| {
                let memtable = Arc::clone(&memtable);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for key_index in 0..keys_per_thread {
                        let key = format!("thread-{thread_index}-key-{key_index:03}");
                        let value = fixed_string(128, 'v');
                        memtable.put(key, value).unwrap();
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(memtable.len(), thread_count * keys_per_thread);
        assert!(memtable.used_bytes() <= MEMTABLE_CAPACITY_BYTES);
        assert_eq!(
            memtable.get("thread-1-key-042"),
            LookupResult::Value(fixed_string(128, 'v'))
        );
    }

    #[test]
    fn concurrent_overwrite_read_races_return_valid_results() {
        let memtable = Arc::new(MemTable::new());
        memtable.put("shared", "seed").unwrap();

        let reader_table = Arc::clone(&memtable);
        let writer_table = Arc::clone(&memtable);

        let reader = thread::spawn(move || {
            for _ in 0..2_000 {
                match reader_table.get("shared") {
                    LookupResult::Value(value) => {
                        assert!(value == "seed" || value.starts_with("writer-"));
                    }
                    LookupResult::NotFound => {}
                }
            }
        });

        let writer = thread::spawn(move || {
            for index in 0..1_000 {
                writer_table
                    .put("shared", format!("writer-{index:04}"))
                    .unwrap();
            }
        });

        reader.join().unwrap();
        writer.join().unwrap();

        assert!(matches!(memtable.get("shared"), LookupResult::Value(_)));
        assert!(memtable.used_bytes() <= MEMTABLE_CAPACITY_BYTES);
    }

    #[test]
    fn concurrent_delete_read_races_only_expose_lookup_contract() {
        let memtable = Arc::new(MemTable::new());
        memtable.put("shared", "seed").unwrap();
        let barrier = Arc::new(Barrier::new(3));

        let reader_table = Arc::clone(&memtable);
        let reader_barrier = Arc::clone(&barrier);
        let reader = thread::spawn(move || {
            reader_barrier.wait();
            for _ in 0..2_000 {
                match reader_table.get("shared") {
                    LookupResult::Value(value) => assert!(!value.is_empty()),
                    LookupResult::NotFound => {}
                }
            }
        });

        let writer_table = Arc::clone(&memtable);
        let writer_barrier = Arc::clone(&barrier);
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            for index in 0..1_000 {
                writer_table
                    .put("shared", format!("value-{index:04}"))
                    .unwrap();
                writer_table.delete("shared").unwrap();
            }
        });

        barrier.wait();
        reader.join().unwrap();
        writer.join().unwrap();

        assert!(matches!(
            memtable.get("shared"),
            LookupResult::Value(_) | LookupResult::NotFound
        ));
        assert!(memtable.used_bytes() <= MEMTABLE_CAPACITY_BYTES);
    }

    #[test]
    fn entries_snapshot_is_sorted_and_preserves_tombstones() {
        let memtable = MemTable::new();

        memtable.put("beta", "two").unwrap();
        memtable.put("alpha", "one").unwrap();
        memtable.delete("beta").unwrap();

        let snapshot = memtable.entries_snapshot();

        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].0, "alpha");
        assert_eq!(
            snapshot[0].1.to_lookup_result(),
            LookupResult::Value("one".into())
        );
        assert_eq!(snapshot[1].0, "beta");
        assert!(snapshot[1].1.is_tombstone);
        assert_eq!(snapshot[1].1.to_lookup_result(), LookupResult::NotFound);
    }

    mod loom_model_tests {
        use std::collections::BTreeMap;

        use loom::sync::{Arc, Mutex};
        use loom::thread;

        use super::*;

        #[derive(Debug, Default)]
        struct ModeledMemTable {
            entries: BTreeMap<String, MemTableEntry>,
            used_bytes: usize,
        }

        impl ModeledMemTable {
            fn put(&mut self, key: &str, value: &str) {
                let entry = MemTableEntry::new_value(key, value.to_owned()).unwrap();
                let replaced_bytes = self
                    .entries
                    .get(key)
                    .map(|existing| existing.entry_size_bytes)
                    .unwrap_or(0);
                let next_used_bytes = self.used_bytes - replaced_bytes + entry.entry_size_bytes;
                assert!(next_used_bytes <= MEMTABLE_CAPACITY_BYTES);
                self.entries.insert(key.to_owned(), entry);
                self.used_bytes = next_used_bytes;
            }

            fn delete(&mut self, key: &str) {
                let entry = MemTableEntry::new_tombstone(key).unwrap();
                let replaced_bytes = self
                    .entries
                    .get(key)
                    .map(|existing| existing.entry_size_bytes)
                    .unwrap_or(0);
                let next_used_bytes = self.used_bytes - replaced_bytes + entry.entry_size_bytes;
                assert!(next_used_bytes <= MEMTABLE_CAPACITY_BYTES);
                self.entries.insert(key.to_owned(), entry);
                self.used_bytes = next_used_bytes;
            }

            fn get(&self, key: &str) -> LookupResult {
                self.entries
                    .get(key)
                    .map(MemTableEntry::to_lookup_result)
                    .unwrap_or(LookupResult::NotFound)
            }
        }

        #[test]
        fn loom_models_put_delete_interleavings_preserve_lookup_contract() {
            loom::model(|| {
                // This models our wrapper invariants under different interleavings.
                // It does not attempt to model crossbeam-skiplist internals.
                let memtable = Arc::new(Mutex::new(ModeledMemTable::default()));

                let writer_table = Arc::clone(&memtable);
                let writer = thread::spawn(move || {
                    writer_table.lock().unwrap().put("alpha", "value");
                });

                let deleter_table = Arc::clone(&memtable);
                let deleter = thread::spawn(move || {
                    deleter_table.lock().unwrap().delete("alpha");
                });

                writer.join().unwrap();
                deleter.join().unwrap();

                let memtable = memtable.lock().unwrap();
                assert!(matches!(
                    memtable.get("alpha"),
                    LookupResult::Value(_) | LookupResult::NotFound
                ));
                assert!(memtable.used_bytes <= MEMTABLE_CAPACITY_BYTES);
            });
        }

        #[test]
        fn loom_models_overwrite_accounting_invariants() {
            loom::model(|| {
                // This focuses on replace-accounting behavior in our memtable wrapper.
                let memtable = Arc::new(Mutex::new(ModeledMemTable::default()));

                let first_writer = Arc::clone(&memtable);
                let first = thread::spawn(move || {
                    first_writer.lock().unwrap().put("shared", "first");
                });

                let second_writer = Arc::clone(&memtable);
                let second = thread::spawn(move || {
                    second_writer.lock().unwrap().put("shared", "second-value");
                });

                first.join().unwrap();
                second.join().unwrap();

                let memtable = memtable.lock().unwrap();
                let live_entry = memtable.entries.get("shared").unwrap();
                assert_eq!(memtable.used_bytes, live_entry.entry_size_bytes);
                assert!(matches!(
                    memtable.get("shared"),
                    LookupResult::Value(_) | LookupResult::NotFound
                ));
            });
        }
    }
}
