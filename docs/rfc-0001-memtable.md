# RFC 0001: MemTable Design

- Status: Draft
- Author: Codex
- Created: 2026-04-08

## Summary

This RFC documents the first memtable implementation for the `lsm` crate.
The memtable is an in-memory, ordered write buffer backed by `crossbeam_skiplist::SkipMap`.
It provides a small public API for `put`, `get`, and `delete`, while enforcing explicit storage limits that align with the current LSM constraints.

## Goals

- Provide a reusable memtable module exposed through the library crate.
- Keep the public API simple and string-oriented for the current phase.
- Enforce hard entry-size and table-size bounds.
- Represent delete operations through tombstones instead of physical removal.
- Return an explicit lookup result enum rather than `Option`.
- Keep enough metadata on each entry to support future WAL, flush, and SSTable work.

## Non-Goals

- Persistence or WAL integration.
- Sequence-number ordering across writes.
- Snapshot isolation or MVCC semantics.
- Range scans or iterators in the public API.
- Distinguishing "never existed" from "deleted" in lookup results.

## Constraints

The implementation currently enforces the following constants:

- Memtable capacity: `4 * 1024 * 1024` bytes
- Max key size: `1024` bytes
- Per-entry budget: `4096` bytes
- Metadata overhead: `60` bytes
- Max value size: `4096 - key_size - 60`

These values are defined in [src/memtable.rs](/Users/seif/_pc/lsm/src/memtable.rs).

## Public API

The memtable exposes:

- `MemTable::new()`
- `MemTable::put(key, value) -> Result<(), MemTableError>`
- `MemTable::get(key) -> LookupResult`
- `MemTable::delete(key) -> Result<(), MemTableError>`
- `MemTable::len() -> usize`
- `MemTable::is_empty() -> bool`
- `MemTable::used_bytes() -> usize`
- `MemTable::remaining_bytes() -> usize`

Lookup behavior is intentionally explicit:

- `LookupResult::Value(String)` means a live value is present.
- `LookupResult::NotFound` means the key is either absent or tombstoned.

This design lets the LSM behave like either a map or a set without exposing `Option` at the call site.

## Internal Model

Each key maps to a `MemTableEntry` with:

- `value: Option<String>`
- `key_size_bytes`
- `value_size_bytes`
- `entry_size_bytes`
- `is_tombstone`

A live write stores `Some(value)`.
A delete stores `None`, which acts as a tombstone.

The memtable uses `SkipMap<String, MemTableEntry>` because it gives:

- ordered keys
- concurrent reads
- a path toward future ordered flushes into SSTables

## Accounting Model

The memtable tracks aggregate bytes separately from the skiplist through an atomic `used_bytes` counter.
Writes are serialized behind a small mutex so overwrite accounting stays correct even with concurrent callers.

The write path follows this sequence:

1. Validate key and value sizes.
2. Build the new `MemTableEntry`.
3. Look up the replaced entry size, if any.
4. Check whether the replacement fits in the 4 MiB table budget.
5. Insert the new entry into the skiplist.
6. Update the aggregate byte counter.

Deletes are accounted for as tombstone entries.
They do not remove the key from the skiplist and they still consume bytes equal to:

- `key_size`
- `metadata_overhead`

## Errors

The memtable currently exposes these error variants:

- `EmptyKey`
- `KeyTooLarge { actual, max }`
- `ValueTooLarge { actual, max }`
- `EntryTooLarge { actual, max }`
- `MemTableFull { requested, remaining, capacity }`

These errors are intended to be deterministic, caller-facing validation failures.

## Concurrency

The current implementation aims for a simple and correct first version:

- reads use `SkipMap::get` directly
- writes use `SkipMap::insert`
- write-side accounting is protected by a mutex

This is not a fully lock-free memtable implementation.
The skiplist supports concurrent access, but size accounting and replacement logic are intentionally serialized for correctness and simplicity in v1.

## Testing

The current test suite covers:

- empty lookup behavior
- put/get/delete flow
- exact size boundaries and rejection paths
- overwrite accounting
- memtable capacity exhaustion
- concurrent write/read/delete stress behavior
- loom-modeled wrapper invariants

The loom tests model the memtable wrapper logic only.
They do not model the internal correctness of `crossbeam-skiplist`, because that crate is not loom-instrumented.

## Tradeoffs

### Why `String` instead of bytes?

This keeps the initial API easy to inspect and test.
It is not the final storage-engine-facing shape, but it is enough to validate the memtable behavior.

### Why tombstones instead of removal?

An LSM must preserve delete intent until compaction or flush can make that delete durable and visible downstream.
Physical removal inside the memtable would lose that semantic information.

### Why a mutex on writes?

The skiplist itself supports concurrent operations, but overwrite accounting must remain correct under races.
The mutex keeps the implementation easy to reason about while still allowing concurrent readers.

## Future Work

- Move from `String` to byte-oriented keys and values.
- Add sequence numbers or timestamps to support version ordering.
- Add iteration and range scan primitives.
- Integrate with a WAL and immutable memtable handoff.
- Add flush triggers and lifecycle states.
- Revisit the write-side lock if contention becomes measurable in benchmarks.
