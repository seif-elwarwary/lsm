# RFC 0002: MemTable Benchmark Methodology

- Status: Draft
- Author: Codex
- Created: 2026-04-08

## Summary

This RFC documents the benchmark harness used to evaluate the current memtable implementation.
The benchmark suite lives in [benches/memtable_bench.rs](/Users/seif/_pc/lsm/benches/memtable_bench.rs) and uses `criterion` to measure throughput and timing for a small set of representative workloads.

## Goals

- Measure memtable throughput under read-heavy and write-only traffic.
- Report per-workload QPS and p99 latency in addition to Criterion's normal timing output.
- Compare single-thread and 4-thread behavior.
- Use fixed payload sizes so benchmark runs are repeatable.

## Non-Goals

- Simulate a full database with WAL, flush, compaction, and IO.
- Produce production-grade latency histograms.
- Benchmark the entire LSM stack.
- Benchmark scan performance.

## Workloads

The suite currently includes two workloads:

### 90% Read / 10% Write

- Prefill the memtable with a stable keyspace of `2048` keys.
- Use `24`-byte keys and `128`-byte values.
- For each thread:
  - every tenth operation is a write
  - all other operations are point reads
- Writes overwrite existing keys so the workload stays within the memtable capacity limit.

This workload is intended to reflect a hot-keyspace scenario with frequent reads and occasional updates.

### 100% Write

- Start with an empty memtable for each measured run.
- Use unique keys per operation.
- Use `24`-byte keys and `128`-byte values.
- Stop well below the 4 MiB memtable limit for each benchmark run.

This workload isolates write-path cost without mixing in reads.

## Concurrency Matrix

Each workload currently runs with:

- `1` thread
- `4` threads

This is enough to expose whether the write-side mutex and skiplist behavior shift meaningfully under moderate contention.

## Metrics

The suite records two categories of metrics:

### Criterion Metrics

Criterion reports:

- sample-based elapsed-time measurements
- confidence ranges
- derived throughput

These are the canonical benchmark outputs for regression tracking.

### Custom Metrics

The benchmark also prints a per-scenario summary with:

- `qps`
- `p99`
- `total_ops`
- wall-clock `elapsed`

QPS is computed as:

- `total_operations / elapsed_seconds`

p99 is computed from a vector of per-operation timings captured with `Instant::now()` around each operation.

## Limitations

The current p99 calculation is useful for rough comparison, but it has important caveats:

- it includes measurement overhead from `Instant::now()`
- it is not a streaming histogram
- it aggregates latencies gathered inside worker threads rather than a coordinated global latency recorder
- it is sensitive to CPU scheduling noise

For the current stage of the project, this is acceptable because the purpose is relative comparison, not production SLO enforcement.

## Why These Parameters

### Key Size: 24 bytes

This is small enough to fit common ID-like keys while still being large enough to avoid unrealistic tiny-key artifacts.

### Value Size: 128 bytes

This keeps the benchmark focused on memtable overhead and synchronization rather than large-object copying.

### 2048-Key Read-Heavy Keyspace

This is large enough to avoid an overly trivial single-key benchmark, while still keeping the benchmark compact and repeatable.

### 1 and 4 Threads

These thread counts show both single-thread baseline behavior and modest parallel contention without making the suite too slow for routine development use.

## How to Run

Build and execute the benchmark with:

```bash
cargo bench --bench memtable_bench
```

The bench target is registered in [Cargo.toml](/Users/seif/_pc/lsm/Cargo.toml).

## Interpretation Guidance

When evaluating results:

- use Criterion's reported ranges to compare code revisions
- use the printed QPS and p99 values as fast, human-readable summaries
- treat cross-machine comparisons cautiously
- expect 4-thread write-heavy results to reflect contention from the current write-side mutex

## Example Output Shape

The benchmark prints lines in this form before each Criterion group:

```text
[memtable_90_read_10_write/1_threads] qps=1183461.36, p99=1834ns, total_ops=2000, elapsed=1.689958ms
```

This summary is supplementary and should be read alongside Criterion's statistical output.

## Future Work

- Add benchmarks for delete-heavy workloads.
- Add larger values and near-limit entries.
- Add byte-oriented benchmarks once the memtable API moves away from `String`.
- Add scan and iterator benchmarks after those APIs exist.
- Replace the ad hoc p99 implementation with a histogram-based recorder if deeper latency analysis becomes necessary.
