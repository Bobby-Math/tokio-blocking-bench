# tokio-blocking-bench

Benchmarks, graphs, and analysis for one narrow question:

How do async Rust systems fail when memory safety is intact, but scheduling safety is not?

Rust gives strong guarantees about memory safety and race freedom. It does not guarantee forward progress, fair scheduling, or good lock lifetime discipline inside a Tokio runtime. This repository studies those failures directly.

## Current Thesis

The repo now explores two scheduling-safety bug classes:

1. `Executor starvation`
   Blocking work runs on Tokio worker threads, steals polling capacity, and produces a sharp latency and failure cliff when blocked workers approach total workers.

2. `Shared-state convoy`
   Shared mutable state turns scheduler delay into reduced effective capacity. This appears in two subtypes:
   - peer-task mutex convoy
   - coordinator suspension convoy

These are not memory-safety bugs. They are progress and capacity bugs.

## What Is In This Repo

### Article and graphs

- Main article draft: [docs/index.md](docs/index.md)
- Additional demo notes: [docs/ADDITIONAL_DEMOS.md](docs/ADDITIONAL_DEMOS.md)
- Failure cliff graph: [docs/blocking_failure_cliff.png](docs/blocking_failure_cliff.png)
- Scheduling cliff graph: [docs/benchmark_scheduling_cliff.png](docs/benchmark_scheduling_cliff.png)
- Tokio console screenshots:
  - [docs/task-starvation-3-blockers.png](docs/task-starvation-3-blockers.png)
  - [docs/task-starvation-4-blockers.png](docs/task-starvation-4-blockers.png)

### Benchmarks

- [src/main.rs](src/main.rs)
  - primary histogram benchmark for granular scheduling-delay degradation
  - measures p50/p95/p99/max overhead on repeated `tokio::time::sleep(10ms)`
  - this is the main benchmark used for local and EC2 scheduling-cliff analysis

- [src/bin/demo_panic.rs](src/bin/demo_panic.rs)
  - operational failure demo
  - shows the cliff through timeouts and cascading failure

- [src/bin/demo_per_request.rs](src/bin/demo_per_request.rs)
  - measures scheduling delay directly as overhead on repeated `tokio::time::sleep(10ms)`

- [src/bin/demo_load_ramp.rs](src/bin/demo_load_ramp.rs)
  - shows that keeping one worker free is not the same as being safe under arbitrary load

- [src/bin/demo_mutex_convoy.rs](src/bin/demo_mutex_convoy.rs)
  - models peer-task shared-state contention
  - shows how holding a `tokio::sync::Mutex` across `.await` lowers effective capacity

- [src/bin/demo_suspension_convoy.rs](src/bin/demo_suspension_convoy.rs)
  - models a coordinator task that suspends while still holding shared state
  - compares `good` vs `bad` lock lifetime under otherwise identical conditions

### Design notes

- [mutex_contention.md](mutex_contention.md)
  - benchmark note for peer-task mutex convoying

- [suspension_convoy.md](suspension_convoy.md)
  - benchmark note for coordinator suspension convoying

## Conceptual Map

### 1. Executor starvation

This is the Part 1 problem.

Blocking code inside async tasks violates Tokio's cooperative model:

- workers are fixed
- tasks must yield
- blocking code does not yield
- spare workers can absorb initial damage
- once the pool saturates, async latency explodes

Primary artifact:
- scheduling delay

Primary demos:
- `demo_panic`
- `demo_per_request`
- `demo_load_ramp`

### 2. Shared-state convoy

This is the Part 2 direction.

The system can still collapse even when it is not simply "out of threads". Shared mutable state changes the structure of progress:

- tasks are no longer independent
- progress depends on resource release
- scheduler delay becomes lock hold time
- effective capacity drops before the runtime looks obviously dead

Two variants are now modeled:

- `demo_mutex_convoy`
  - many peer tasks queue behind a shared mutex

- `demo_suspension_convoy`
  - one coordinator task suspends while still holding shared state

## Why `demo_suspension_convoy` Matters

This benchmark isolates a different failure shape than raw worker starvation.

It keeps the coordinator, shared state, event channel, and consumer constant. The only intentional difference is whether the coordinator performs `send().await` before or after releasing the shared-state lock.

That makes the proof signal clean:

- `good`: coordinator lock-hold p95 stays around `1us`
- `bad`: coordinator lock-hold p95 grows to roughly `6400us`

The main signal is lock hold time, not just late-event percentage.

## Recommended Reading Order

1. [docs/index.md](docs/index.md)
2. [src/main.rs](src/main.rs)
3. [src/bin/demo_panic.rs](src/bin/demo_panic.rs)
4. [src/bin/demo_per_request.rs](src/bin/demo_per_request.rs)
5. [src/bin/demo_mutex_convoy.rs](src/bin/demo_mutex_convoy.rs)
6. [src/bin/demo_suspension_convoy.rs](src/bin/demo_suspension_convoy.rs)
7. [mutex_contention.md](mutex_contention.md)
8. [suspension_convoy.md](suspension_convoy.md)

## Build

```bash
cargo build --release
```

## Benchmark Usage

### Part 1: executor starvation

Run the primary granular scheduling benchmark:

```bash
./target/release/tokio-blocking-bench --run-all --runs 3
```

Run the operational failure demo:

```bash
./target/release/demo_panic --run-all
```

Run the direct scheduling-delay benchmark:

```bash
./target/release/demo_per_request --run-all
```

Run the load-ramp comparison:

```bash
./target/release/demo_load_ramp --run-all
```

### Part 2: shared-state convoy

Run the mutex convoy benchmark:

```bash
./target/release/demo_mutex_convoy --run-all
```

Run the suspension convoy benchmark:

```bash
./target/release/demo_suspension_convoy --run-all
```

For full CLI details on any benchmark, run the binary with `--help`.

## Interpreting Results

### Executor starvation

Primary signals:

- failure cliff
- timeout rate
- scheduling overhead

Secondary signals:

- p50 stability
- total wall-clock duration

### Shared-state convoy

Primary signals:

- lock hold time
- lock acquisition delay

Secondary signals:

- event lateness
- end-to-end event latency

The important distinction is that event-level latency depends on thresholds and workload shape. Lock lifetime is the direct mechanism.

## Repo Status

This repository is no longer just a single blocking benchmark.

It now contains:

- the article draft
- graph assets
- the original starvation demos
- a peer-task mutex convoy benchmark
- a coordinator suspension convoy benchmark
- written design notes for both shared-state convoy benchmarks

The current research direction is:

- Rust enforces memory safety
- Tokio provides an efficient cooperative runtime
- scheduling safety remains the engineer's responsibility

## License

This code is written for research and article purposes.
