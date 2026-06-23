# lsm

A persistent, embedded key-value storage engine built on an LSM-tree (Log-Structured Merge-tree) architecture, written in Rust.

## Status

Early development. Writes land in an in-memory memtable, full memtables rotate
to immutable ones, and immutable memtables flush to on-disk L0 SSTables tracked
in a JSON catalog. Reads check the writable memtable, then immutable memtables,
then L0 SSTables. WAL, compaction beyond L0, and background flushing are not yet
implemented.

## Architecture

```
put/delete ─▶ writable memtable (lock-free skiplist)
                   │ full
                   ▼
             immutable memtables (queue)
                   │ flush_oldest_immutable()
                   ▼
             L0 SSTables on disk  ◀── CATALOG.json
```

- **Memtable** — `crossbeam-skiplist`, 4 MiB capacity per table.
- **SSTable** — block-based, read on demand; CRC32 block checksums.
- **Catalog** — `CATALOG.json` records flushed SSTables so they survive reopen.

## Usage

```rust
use lsm::{Db, LookupResult};

let db = Db::open("./db")?;

db.put("user:1", "Alice")?;
db.put("user:2", "Bob")?;

match db.get("user:1")? {
    LookupResult::Value(v) => println!("user:1 => {v}"),
    LookupResult::NotFound => println!("missing"),
}

db.delete("user:2")?;
db.close()?;
```

Run the example:

```sh
cargo run --example db
```

## API

`Db::open` · `get` · `put` · `delete` · `close` · `len` · `is_empty` ·
`used_bytes` · `remaining_bytes` · `immutable_memtable_count` ·
`level0_sstable_count` · `flush_oldest_immutable`.

## Development

```sh
cargo test          # tests
cargo bench         # criterion benchmarks (benches/db_bench.rs)
```

Design notes live in [`docs/`](docs/).

## License

MIT OR Apache-2.0.
