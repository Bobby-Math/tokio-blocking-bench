# Mutex Contention Benchmark

This repository now includes `demo_mutex_convoy`, a benchmark for a specific hypothesis:

- Blocking code reduces executor capacity.
- If tasks also share a `tokio::sync::Mutex` and hold it across `.await`, that reduced capacity can inflate lock hold time.
- Inflated lock hold time lowers effective throughput and can cause timeouts at loads that an independent-task benchmark would survive.

## What It Compares

The benchmark runs the same task counts and blocker counts in two modes:

- `independent`: tasks share no state
- `contention`: a fraction of tasks acquire a shared `tokio::sync::Mutex` and hold it across several `.await` points

The only intended difference is shared-state coupling.

## Why It Uses `yield_now()`

The contention variant holds the mutex across `tokio::task::yield_now().await`, not `tokio::time::sleep(...)`.

That matters because:

- `sleep(...)` adds a built-in delay even in the healthy case
- `yield_now()` keeps the healthy case cheap
- under starvation, the time between yield and re-poll stretches, so the lock is held longer for scheduling reasons rather than timer reasons

This makes the benchmark a better model of the thesis: starvation amplifies contention.

## What It Measures

For each scenario, the benchmark reports:

- failure rate
- average lock acquisition wait
- p95 lock acquisition wait
- max lock acquisition wait in single-run mode

These are measured side by side with the independent baseline.

## How To Read It

The benchmark supports the hypothesis if:

- the independent mode survives a scenario
- the contention mode fails or degrades under the same task and blocker counts
- lock acquisition wait grows sharply in the contention mode as blockers increase

That pattern suggests the mutex is acting as a force multiplier for reduced executor capacity rather than being the sole bottleneck by itself.

## Stabilization

The benchmark includes two additive features to reduce timing artifacts:

- repeated runs with median reporting via `--runs`
- small deterministic blocker jitter via `--blocking-jitter-ms`

These exist to reduce phase-locking from perfectly synchronized starts and fixed blocker timing.

## Example

```bash
cargo build --release --bin demo_mutex_convoy
./target/release/demo_mutex_convoy --run-all
```

Useful tuning knobs:

- `--contention-fraction`
- `--awaits-under-lock`
- `--timeout-ms`
- `--blocking-jitter-ms`
- `--runs`
