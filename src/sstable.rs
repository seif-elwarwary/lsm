use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use bytes::BufMut;
use crc32fast::Hasher;
use thiserror::Error;

use crate::memtable::{LookupResult, MemTableEntry};

pub const MAGIC: [u8; 8] = *b"LSMSS001";
pub const VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 32;

const FLAG_TOMBSTONE: u8 = 0x01;
const ENTRY_HEADER_SIZE: usize = 7;

/// Logical record materialized from an SSTable on disk.
///
/// Mirrors the shape of [`MemTableEntry`] for parity: an absent `value`
/// means the record is a tombstone and shadows any older value for the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstRecord {
    pub key: String,
    pub value: Option<String>,
}

impl SstRecord {
    pub fn to_lookup_result(&self) -> LookupResult {
        match &self.value {
            Some(v) => LookupResult::Value(v.clone()),
            None => LookupResult::NotFound,
        }
    }
}

/// Summary of a freshly-written SSTable. The catalog stores this.
#[derive(Debug, Clone)]
pub struct SstableMeta {
    pub entry_count: u32,
    pub smallest_key: String,
    pub largest_key: String,
    pub data_crc32: u32,
}

/// Streaming SSTable writer.
///
/// File layout (v1):
/// ```text
/// HEADER (32 bytes, little-endian):
///   magic:       [u8; 8]  = b"LSMSS001"
///   version:     u32
///   entry_count: u32
///   data_crc32:  u32      (CRC32 of the entries block, excluding header)
///   _reserved:   [u8; 12] (space for future index offset / bloom offset)
///
/// ENTRIES (variable, sorted by key):
///   key_len:   u16
///   value_len: u32
///   flags:     u8         (bit 0 = tombstone)
///   key:       [u8; key_len]
///   value:     [u8; value_len]
/// ```
pub struct SstableWriter {
    writer: BufWriter<File>,
    hasher: Hasher,
    entry_count: u32,
    smallest_key: Option<String>,
    largest_key: Option<String>,
}

impl SstableWriter {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, SstableError> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(path.as_ref())?;
        let mut writer = BufWriter::new(file);
        // Reserve header bytes; we rewrite them in `finish()` once we know the CRC + count.
        writer.write_all(&[0u8; HEADER_SIZE])?;
        Ok(Self {
            writer,
            hasher: Hasher::new(),
            entry_count: 0,
            smallest_key: None,
            largest_key: None,
        })
    }

    /// Append a single record. Caller must write keys in ascending order — the
    /// memtable's `entries_snapshot()` already yields sorted entries.
    pub fn write_entry(&mut self, key: &str, entry: &MemTableEntry) -> Result<(), SstableError> {
        let key_bytes = key.as_bytes();
        let value_bytes = entry.value.as_deref().unwrap_or("").as_bytes();

        if key_bytes.len() > u16::MAX as usize {
            return Err(SstableError::Corrupt("key length exceeds u16".into()));
        }

        let flags: u8 = if entry.is_tombstone { FLAG_TOMBSTONE } else { 0 };

        let mut buf: Vec<u8> =
            Vec::with_capacity(ENTRY_HEADER_SIZE + key_bytes.len() + value_bytes.len());
        buf.put_u16_le(key_bytes.len() as u16);
        buf.put_u32_le(value_bytes.len() as u32);
        buf.put_u8(flags);
        buf.put_slice(key_bytes);
        buf.put_slice(value_bytes);

        self.hasher.update(&buf);
        self.writer.write_all(&buf)?;

        self.entry_count += 1;
        if self.smallest_key.is_none() {
            self.smallest_key = Some(key.to_owned());
        }
        self.largest_key = Some(key.to_owned());

        Ok(())
    }

    /// Finalize the file: rewrite the header with the entry count and CRC, then fsync.
    pub fn finish(mut self) -> Result<SstableMeta, SstableError> {
        let data_crc32 = self.hasher.finalize();
        self.writer.flush()?;

        let mut file = self
            .writer
            .into_inner()
            .map_err(|e| SstableError::Io(e.into_error()))?;

        file.seek(SeekFrom::Start(0))?;

        let mut header = [0u8; HEADER_SIZE];
        header[0..8].copy_from_slice(&MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[12..16].copy_from_slice(&self.entry_count.to_le_bytes());
        header[16..20].copy_from_slice(&data_crc32.to_le_bytes());
        // header[20..32] reserved for a future index offset / bloom offset.
        file.write_all(&header)?;
        file.sync_all()?;

        Ok(SstableMeta {
            entry_count: self.entry_count,
            smallest_key: self.smallest_key.unwrap_or_default(),
            largest_key: self.largest_key.unwrap_or_default(),
            data_crc32,
        })
    }
}

/// Read-side for a single SSTable.
///
/// For v1 we slurp the whole file into memory on `open()` and validate the CRC.
/// That is fine for small SSTables and keeps the code readable; swap to mmap or
/// a block-cache read path once files get large or L0 gets wide.
#[derive(Debug)]
pub struct SstableReader {
    data: Vec<u8>,
    entry_count: u32,
}

impl SstableReader {
    // TODO: donot read the whole file into memory; instead, read the header, validate the key range, then read blocks on demand. We can add a sparse index in the header to speed this up.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SstableError> {
        let mut file = File::open(path.as_ref())?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;

        if data.len() < HEADER_SIZE {
            return Err(SstableError::Corrupt("file smaller than header".into()));
        }
        if data[0..8] != MAGIC {
            return Err(SstableError::Corrupt("magic bytes mismatch".into()));
        }

        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(SstableError::UnsupportedVersion(version));
        }

        let entry_count = u32::from_le_bytes(data[12..16].try_into().unwrap());
        let stored_crc32 = u32::from_le_bytes(data[16..20].try_into().unwrap());

        let mut hasher = Hasher::new();
        hasher.update(&data[HEADER_SIZE..]);
        if hasher.finalize() != stored_crc32 {
            return Err(SstableError::Corrupt("CRC32 checksum mismatch".into()));
        }

        Ok(Self { data, entry_count })
    }

    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    /// Linear scan for `target_key`. Short-circuits once we pass the target
    /// (entries are stored in ascending key order). Replace with a binary
    /// search over a sparse index once we add one.
    pub fn get(&self, target_key: &str) -> Result<Option<SstRecord>, SstableError> {
        let data = &self.data;
        let mut cursor = HEADER_SIZE;

        for _ in 0..self.entry_count {
            if cursor + ENTRY_HEADER_SIZE > data.len() {
                return Err(SstableError::Corrupt("truncated entry header".into()));
            }

            let key_len =
                u16::from_le_bytes(data[cursor..cursor + 2].try_into().unwrap()) as usize;
            let value_len =
                u32::from_le_bytes(data[cursor + 2..cursor + 6].try_into().unwrap()) as usize;
            let flags = data[cursor + 6];
            cursor += ENTRY_HEADER_SIZE;

            if cursor + key_len + value_len > data.len() {
                return Err(SstableError::Corrupt("truncated entry body".into()));
            }

            let key_bytes = &data[cursor..cursor + key_len];
            cursor += key_len;
            let value_bytes = &data[cursor..cursor + value_len];
            cursor += value_len;

            let key = std::str::from_utf8(key_bytes)
                .map_err(|_| SstableError::Corrupt("non-UTF-8 key".into()))?;

            match key.cmp(target_key) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Greater => return Ok(None),
                std::cmp::Ordering::Equal => {
                    let is_tombstone = flags & FLAG_TOMBSTONE != 0;
                    let value = if is_tombstone {
                        None
                    } else {
                        Some(
                            std::str::from_utf8(value_bytes)
                                .map_err(|_| SstableError::Corrupt("non-UTF-8 value".into()))?
                                .to_owned(),
                        )
                    };
                    return Ok(Some(SstRecord {
                        key: key.to_owned(),
                        value,
                    }));
                }
            }
        }

        Ok(None)
    }
}

#[derive(Debug, Error)]
pub enum SstableError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt SSTable: {0}")]
    Corrupt(String),
    #[error("unsupported SSTable version {0}")]
    UnsupportedVersion(u32),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_entry(key: &str, value: Option<&str>) -> (String, MemTableEntry) {
        let is_tombstone = value.is_none();
        let value_owned = value.map(|v| v.to_owned());
        let key_size_bytes = key.len();
        let value_size_bytes = value_owned.as_ref().map_or(0, |v| v.len());
        let entry = MemTableEntry {
            value: value_owned,
            key_size_bytes,
            value_size_bytes,
            entry_size_bytes: key_size_bytes + value_size_bytes + 60,
            is_tombstone,
        };
        (key.to_owned(), entry)
    }

    #[test]
    fn round_trip_values_and_tombstones() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("00000001.sst");

        let mut writer = SstableWriter::create(&path).unwrap();
        for (k, v) in [
            write_entry("alpha", Some("one")),
            write_entry("beta", None),
            write_entry("gamma", Some("three")),
        ] {
            writer.write_entry(&k, &v).unwrap();
        }
        let meta = writer.finish().unwrap();

        assert_eq!(meta.entry_count, 3);
        assert_eq!(meta.smallest_key, "alpha");
        assert_eq!(meta.largest_key, "gamma");

        let reader = SstableReader::open(&path).unwrap();
        assert_eq!(reader.entry_count(), 3);
        assert_eq!(
            reader.get("alpha").unwrap().unwrap().to_lookup_result(),
            LookupResult::Value("one".into())
        );
        assert_eq!(
            reader.get("beta").unwrap().unwrap().to_lookup_result(),
            LookupResult::NotFound
        );
        assert_eq!(
            reader.get("gamma").unwrap().unwrap().to_lookup_result(),
            LookupResult::Value("three".into())
        );
        assert!(reader.get("missing").unwrap().is_none());
    }

    #[test]
    fn short_circuits_past_target_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("00000001.sst");

        let mut writer = SstableWriter::create(&path).unwrap();
        for (k, v) in [
            write_entry("alpha", Some("a")),
            write_entry("delta", Some("d")),
            write_entry("zulu", Some("z")),
        ] {
            writer.write_entry(&k, &v).unwrap();
        }
        writer.finish().unwrap();

        let reader = SstableReader::open(&path).unwrap();
        assert!(reader.get("bravo").unwrap().is_none());
        assert!(reader.get("charlie").unwrap().is_none());
    }

    #[test]
    fn corrupt_crc_is_detected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("00000001.sst");

        let mut writer = SstableWriter::create(&path).unwrap();
        let (k, v) = write_entry("alpha", Some("one"));
        writer.write_entry(&k, &v).unwrap();
        writer.finish().unwrap();

        // Corrupt a byte in the entries block
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let err = SstableReader::open(&path).unwrap_err();
        assert!(matches!(err, SstableError::Corrupt(_)));
    }
}
