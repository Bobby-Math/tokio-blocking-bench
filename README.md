# tokio-blocking-bench

**Reproduction benchmark: Blocking in async Rust is invisible at low load, catastrophic at high load.**

## Thesis

When synchronous (blocking) code executes inside an async task on a Tokio multi-threaded runtime, it monopolizes one worker thread. The worker's polling loop cannot service any other task's Waker until the blocking call returns. At low concurrency, spare worker threads absorb the damage and latency remains normal. At high concurrency, blocked workers starve the runtime of polling capacity and async task latency degrades non-linearly, with a sharp cliff when blocked workers approach the total worker count.

This is not a bug in Tokio. It is a deterministic consequence of a fixed-size cooperative thread pool encountering non-cooperative code.

## What this benchmark measures

**Metric: scheduling overhead.** Each async task calls `tokio::time::sleep(10ms)` and measures how long it actually sleeps. The overhead is `actual_duration - expected_duration`. In a healthy runtime, overhead is near zero (sub-millisecond). Under worker starvation, the timer fires on time (the kernel delivers the epoll event), but no worker thread is free to poll the woken task. The task sits in the ready queue until a worker becomes available. That queuing delay is the overhead we report.

**Why this metric is correct.** We isolate the runtime's ability to service a Waker promptly. We do not measure total task completion time (which conflates queuing delay with work duration) or wall-clock throughput (which hides per-request degradation behind averages).

## Scenario matrix

The benchmark runs 11 scenarios across two dimensions:

**Dimension 1: Async load.** 50 tasks ("low") vs 500 tasks ("high"). This represents the difference between a dev/staging environment and production traffic.

**Dimension 2: Blocking tasks.** 0 through 4, where 4 equals the worker thread count. Each blocking task calls `std::thread::sleep(50ms)` inside an async context, simulating a synchronous cloud download, DNS lookup, or file read.

```
baseline/no-block          500 async,  0 blocking   (reference floor)

low-async/0-blockers        50 async,  0 blocking
low-async/1-blocker         50 async,  1 blocking
low-async/2-blockers        50 async,  2 blocking
low-async/3-blockers        50 async,  3 blocking
low-async/4-blockers        50 async,  4 blocking

high-async/0-blockers      500 async,  0 blocking
high-async/1-blocker       500 async,  1 blocking
high-async/2-blockers      500 async,  2 blocking
high-async/3-blockers      500 async,  3 blocking
high-async/4-blockers      500 async,  4 blocking
```

**Deliberately omitted:** Scenarios with blockers > workers. The interaction between queued blocking tasks introduces confounding scheduling dynamics (blocking tasks competing for workers among themselves) that obscure the core finding. The 0-to-N progression tells the story cleanly.

## Assumptions

1. **Fixed worker count.** Default: 4 workers. This simulates a typical production container (4 vCPUs). The critical variable is `blocking_tasks / worker_threads`. All claims are relative to this ratio.

2. **`std::thread::sleep` as the blocking proxy.** Real-world blocking sources include: synchronous HTTP clients (`reqwest::blocking`), synchronous DNS resolution, file I/O through `std::fs`, CPU-heavy parsing without yield points, and FFI calls into blocking C libraries. `std::thread::sleep` is chosen because it blocks for a deterministic, known duration with no side effects, isolating the scheduling impact.

3. **`tokio::time::sleep` as the async I/O proxy.** This exercises the same Waker machinery as TCP reads, HTTP requests, or database queries: the timer wheel fires, calls `waker.wake()`, and the task is re-enqueued for polling. The scheduling overhead measured here applies equally to all Waker-driven async operations.

4. **No cross-task data dependencies.** Tasks are independent. This isolates the scheduling effect from contention on shared state. In production, mutex/channel contention would compound the problem.

5. **Barrier-synchronized start.** All tasks begin simultaneously to measure steady-state concurrent behavior, not sequential ramp-up.

6. **Fresh runtime per scenario.** Each scenario constructs and drops its own Tokio runtime to prevent carryover effects.

## Data collection protocol

For publishable results, run the full matrix 3 times and report median values.

```bash
# Build
cargo build --release

# 3 runs, output captured to file
./target/release/tokio-blocking-bench --run-all --runs 3 > results_local.txt 2>&1

# Or on EC2
./target/release/tokio-blocking-bench --run-all --runs 3 > results_ec2.txt 2>&1
```

When reporting data in the article:
- State the exact command, instance type, and OS.
- State the number of runs.
- Report median values across runs for each scenario.
- If reporting a single run, state that explicitly.
- Never mix values from different runs in the same table.
- Compare against baseline from the SAME run, not across runs.

## Expected results (based on preliminary data)

| Blocking ratio | Low async (50 tasks) | High async (500 tasks) |
|---|---|---|
| 0/4 workers | p99 < 2ms overhead | p99 < 2ms overhead |
| 1/4 workers | p99 < 2ms | p99 < 2ms |
| 2/4 workers | p99 < 2ms | p99 2-3ms (mild degradation) |
| 3/4 workers | p99 < 3ms | p99 < 3ms |
| **4/4 workers** | **p99 increases** | **p99 100-200ms (cliff)** |

The cliff at 4/4 is the central finding. The contrast between low-async/4-blockers and high-async/4-blockers demonstrates that the same blocking code produces different outcomes depending on async load, which is the "invisible until production" thesis.

## Defensible claims (from preliminary data)

These three claims held up across both local (Pop!_OS desktop) and EC2 (c6i.xlarge) runs:

1. **The cliff is real.** Going from 3/4 blocked to 4/4 blocked causes a 100x+ p99 increase under high async load.

2. **p50 is blind.** Median scheduling overhead remains stable even at full blockage. Monitoring based on p50 or mean will not detect this failure until it cascades into timeouts or panics.

3. **One free worker is sufficient.** A single unblocked worker can keep 500 async tasks at sub-2ms p99. Zero free workers means 190ms+ p99. The system is binary: it works or it doesn't.

## Claims to verify with clean data

- Whether low-async/4-blockers also shows the cliff (expected: yes, but less dramatic due to fewer tasks competing for the one worker during yield gaps).
- Whether p50 truly does not move or moves slightly (~16% increase was observed on desktop but not EC2). Conservative framing: "p50 degradation is minimal relative to the p99 explosion."
- The exact multiplier (100x vs 136x depends on whether you compare to the 3-blocker scenario or the 0-blocker baseline; state which reference is used).

## Usage

### Run full scenario matrix with 3 repetitions

```bash
cargo build --release
./target/release/tokio-blocking-bench --run-all --runs 3
```

### Run a single custom scenario

```bash
./target/release/tokio-blocking-bench \
    --workers 4 \
    --async-tasks 500 \
    --blocking-tasks 3
```

### Vary worker count (multi-instance comparison)

```bash
# 2-core (t3.small / c6i.medium)
./target/release/tokio-blocking-bench --run-all --runs 3 --workers 2

# 4-core (c6i.xlarge)
./target/release/tokio-blocking-bench --run-all --runs 3 --workers 4

# 8-core (c6i.2xlarge)
./target/release/tokio-blocking-bench --run-all --runs 3 --workers 8
```

## CLI reference

| Flag | Default | Description |
|------|---------|-------------|
| `--workers` | 4 | Tokio worker thread count (0 = num_cpus) |
| `--async-tasks` | 200 | Number of async I/O simulation tasks |
| `--blocking-tasks` | 0 | Number of blocking tasks |
| `--async-sleep-ms` | 10 | Duration of each async sleep (ms) |
| `--blocking-sleep-ms` | 50 | Duration of each blocking call (ms) |
| `--iterations` | 10 | Sleep iterations per async task |
| `--blocking-iterations` | 5 | Blocking iterations per blocking task |
| `--run-all` | false | Run predefined scenario matrix |
| `--runs` | 1 | Number of full repetitions (for `--run-all`) |

## EC2 setup

| Instance | vCPUs | Cost/hr | Notes |
|----------|-------|---------|-------|
| c6i.large | 2 | ~$0.085 | Cliff appears earliest (fewer workers) |
| c6i.xlarge | 4 | ~$0.17 | Matches default config, primary data source |
| c6i.2xlarge | 8 | ~$0.34 | Shows how more workers delay the cliff |

```bash
# Amazon Linux 2023
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
# scp or git clone the project
cargo build --release
./target/release/tokio-blocking-bench --run-all --runs 3 > results.txt
cat results.txt
# terminate instance
```

Total cost for 3 instance sizes, 3 runs each: < $2.

## License

This code is written for research and article purposes.
