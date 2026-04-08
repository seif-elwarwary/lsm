use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use lsm::{LookupResult, MemTable};

const KEY_BYTES: usize = 24;
const VALUE_BYTES: usize = 128;
const READ_HEAVY_THREADS: [usize; 2] = [1, 4];
const WRITE_ONLY_THREADS: [usize; 2] = [1, 4];
const READ_HEAVY_OPS_PER_THREAD: usize = 2_000;
const WRITE_ONLY_OPS_PER_THREAD: usize = 1_000;
const READ_HEAVY_KEYSPACE: usize = 2_048;

#[derive(Debug)]
struct WorkloadSummary {
    elapsed: Duration,
    total_ops: usize,
    qps: f64,
    p99_ns: u64,
}

fn fixed_key(index: usize) -> String {
    let key = format!("key-{index:020}");
    debug_assert_eq!(key.len(), KEY_BYTES);
    key
}

fn fixed_value(seed: usize) -> String {
    let prefix = format!("value-{seed:06}-");
    let suffix_len = VALUE_BYTES.saturating_sub(prefix.len());
    format!("{prefix}{}", "x".repeat(suffix_len))
}

fn p99_ns(latencies: &mut [u64]) -> u64 {
    latencies.sort_unstable();
    let percentile_index = latencies
        .len()
        .saturating_mul(99)
        .div_ceil(100)
        .saturating_sub(1);
    latencies[percentile_index]
}

fn run_read_heavy_workload(thread_count: usize, ops_per_thread: usize) -> WorkloadSummary {
    let memtable = Arc::new(MemTable::new());
    for index in 0..READ_HEAVY_KEYSPACE {
        memtable.put(fixed_key(index), fixed_value(index)).unwrap();
    }

    let started_at = Instant::now();
    let handles: Vec<_> = (0..thread_count)
        .map(|thread_index| {
            let memtable = Arc::clone(&memtable);
            thread::spawn(move || {
                let mut latencies = Vec::with_capacity(ops_per_thread);

                for op_index in 0..ops_per_thread {
                    let key_index =
                        (thread_index * ops_per_thread + op_index) % READ_HEAVY_KEYSPACE;
                    let key = fixed_key(key_index);
                    let started = Instant::now();

                    if op_index % 10 == 0 {
                        memtable
                            .put(key, fixed_value(thread_index * 10_000 + op_index))
                            .unwrap();
                    } else {
                        match memtable.get(&key) {
                            LookupResult::Value(_) | LookupResult::NotFound => {}
                        }
                    }

                    latencies.push(started.elapsed().as_nanos() as u64);
                }

                latencies
            })
        })
        .collect();

    let mut latencies = Vec::with_capacity(thread_count * ops_per_thread);
    for handle in handles {
        latencies.extend(handle.join().unwrap());
    }

    let elapsed = started_at.elapsed();
    let total_ops = thread_count * ops_per_thread;
    WorkloadSummary {
        elapsed,
        total_ops,
        qps: total_ops as f64 / elapsed.as_secs_f64(),
        p99_ns: p99_ns(&mut latencies),
    }
}

fn run_write_only_workload(thread_count: usize, ops_per_thread: usize) -> WorkloadSummary {
    let memtable = Arc::new(MemTable::new());
    let started_at = Instant::now();

    let handles: Vec<_> = (0..thread_count)
        .map(|thread_index| {
            let memtable = Arc::clone(&memtable);
            thread::spawn(move || {
                let mut latencies = Vec::with_capacity(ops_per_thread);

                for op_index in 0..ops_per_thread {
                    let global_index = thread_index * ops_per_thread + op_index;
                    let key = fixed_key(global_index);
                    let started = Instant::now();
                    memtable.put(key, fixed_value(global_index)).unwrap();
                    latencies.push(started.elapsed().as_nanos() as u64);
                }

                latencies
            })
        })
        .collect();

    let mut latencies = Vec::with_capacity(thread_count * ops_per_thread);
    for handle in handles {
        latencies.extend(handle.join().unwrap());
    }

    let elapsed = started_at.elapsed();
    let total_ops = thread_count * ops_per_thread;
    WorkloadSummary {
        elapsed,
        total_ops,
        qps: total_ops as f64 / elapsed.as_secs_f64(),
        p99_ns: p99_ns(&mut latencies),
    }
}

fn bench_read_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("memtable_90_read_10_write");
    group.sample_size(10);

    for &thread_count in &READ_HEAVY_THREADS {
        let summary = run_read_heavy_workload(thread_count, READ_HEAVY_OPS_PER_THREAD);
        println!(
            "[memtable_90_read_10_write/{thread_count}_threads] qps={:.2}, p99={}ns, total_ops={}, elapsed={:?}",
            summary.qps, summary.p99_ns, summary.total_ops, summary.elapsed
        );

        group.throughput(Throughput::Elements(
            (thread_count * READ_HEAVY_OPS_PER_THREAD) as u64,
        ));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{thread_count}_threads")),
            &thread_count,
            |b, &thread_count| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        total += run_read_heavy_workload(thread_count, READ_HEAVY_OPS_PER_THREAD)
                            .elapsed;
                    }
                    total
                });
            },
        );
    }

    group.finish();
}

fn bench_write_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("memtable_100_write");
    group.sample_size(10);

    for &thread_count in &WRITE_ONLY_THREADS {
        let summary = run_write_only_workload(thread_count, WRITE_ONLY_OPS_PER_THREAD);
        println!(
            "[memtable_100_write/{thread_count}_threads] qps={:.2}, p99={}ns, total_ops={}, elapsed={:?}",
            summary.qps, summary.p99_ns, summary.total_ops, summary.elapsed
        );

        group.throughput(Throughput::Elements(
            (thread_count * WRITE_ONLY_OPS_PER_THREAD) as u64,
        ));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{thread_count}_threads")),
            &thread_count,
            |b, &thread_count| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        total += run_write_only_workload(thread_count, WRITE_ONLY_OPS_PER_THREAD)
                            .elapsed;
                    }
                    total
                });
            },
        );
    }

    group.finish();
}

criterion_group!(memtable_benches, bench_read_heavy, bench_write_only);
criterion_main!(memtable_benches);
