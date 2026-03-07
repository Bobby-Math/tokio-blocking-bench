---
title: ""
---

# Executor Starvation in Async Rust: The Hidden Cost of Blocking Code

## Section 1: The Demonstration

### The execution model

Three levels of execution: hardware threads, worker threads, and tasks. Each level multiplexes onto the one below it.

Hardware threads are the logical CPUs your processor exposes: one per vCPU on AWS, or two per physical core with SMT. Tokio creates one worker thread (a real OS thread via std::thread::spawn) per logical CPU. These worker threads run Tokio's poll loop for the lifetime of the runtime.

Your code spawns tasks via tokio::spawn. These are the state machines the compiler generates from your async fn, with each .await point compiled into a state transition that yields control back to the worker.

A task has no thread on it's own. It borrows a worker thread for microseconds during each poll() call, then yields. Between yield points, the worker is free to poll other tasks, check the I/O driver for readiness events via mio, or steal work from other workers' queues.
On a 4-vCPU machine, 4 worker threads cycle through thousands of tasks. The ratio of tasks to workers gives async Rust its efficiency, and its most insidious failure mode.

### The scenario

Consider a production service built on this model.

The service handles incoming requests asynchronously across four worker threads.

Somewhere in the codebase, a synchronous function (a config fetch, a DNS lookup, a file read) sits in a code path that roughly 30% of requests traverse.

The function takes 50 milliseconds, completes successfully every time, and logs no errors.

At low traffic, these blocking calls rarely overlap and the worker pool absorbs them easily.

At higher traffic, multiple blocking calls land simultaneously, saturating the pool.

The blocking code does not change. Only the traffic level changes.

To reproduce these results, build and run the demo:

```bash
cargo build --release
./target/release/demo_per_request --run-all
```

```
=== Per-Request Blocking: Traffic Ramp ===

  Workers:              4
  Blocking probability: 30% of requests hit the sync code path
  Blocking duration:    50ms per blocking call
  Async work:           10ms per request
  Timeout:              100ms
  Requests per task:    20

Concurrent         Total Ops  Block Calls    Succeeded    Timed Out  Failure %
---------------------------------------------------------------------------
10                       200           60          200            0       0.0%
15                       300           90          300            0       0.0%
20                       400          120          378           22       5.5%
25                       500          150          419           81      16.2%
30                       600          180          376          224      37.3%
40                       800          240          206          594      74.2%
50                      1000          300           62          938      93.8%
```

At 15 concurrent requests, zero failures.

At 20 concurrent requests, 5.5% of operations fail.

At 30 concurrent requests, 37% fail. At 50, 94% fail.

The cliff lands between 15 and 20 concurrent requests. That is not a stress test. That is the difference between manual testing and any automated load test.

### What the reader should notice

The blocking code did not change.

The blocking code did not fail.

The blocking code is not mentioned in any error output.

A developer looking at the failures would see timeouts inside async handler code and conclude that the handlers are too slow, or that the timeout is set too aggressively.

The actual cause is blocking calls overlapping as traffic increases, saturating the worker pool.

The blocking code succeeded in every run and appears in no failure trace.

### Other triggers

The same cliff can be reached through two other paths: adding more blocking code to a fixed workload, or increasing async task count against fixed blocking.

The mechanism is identical in all cases; only the trigger differs.

The benchmark repository includes [demonstrations of all three triggers](ADDITIONAL_DEMOS.md).

---

## Section 2: What a Tokio Worker Thread Actually Does

Each worker thread runs a poll loop: pull a task from a queue, call poll() on it, and check the I/O driver for readiness events. The poll()
method returns Poll::Ready when the task is complete, or Poll::Pending when the task cannot make progress yet. When a task returns Pending, it registers a Waker with the I/O source it is waiting on. When that source becomes ready, it calls waker.wake(), placing the task back on a run queue. This is cooperative scheduling: tasks yield voluntarily, and the runtime resumes them only when they declare they can make progress.

The Rust compiler transforms every async fn into a state machine, with each .await point becoming a state transition. A worker is occupied only during the brief moments between .await points—typically microseconds. The I/O operation might take 5 milliseconds, but the worker is held for microseconds. Each worker maintains a local task queue, and when a worker's queue is empty, it steals tasks from other workers' queues. 

This is the mechanism that allows one free worker to absorb the load of blocked workers. The worker thread has no timeout, no preemption mechanism, and no way to distinguish a 2-microsecond poll from a 200-millisecond poll. From the worker's perspective, both are function calls that have not yet returned.

---

## Section 3: What Blocking Does to the Poll Loop

Now consider what happens when a task calls `std::thread::sleep`, `reqwest::blocking::get`, `std::fs::read`, or any other function that blocks the OS thread.

### The mechanism

The blocking call does not return `Poll::Pending`. It does not register a `Waker`. It simply occupies the OS thread until the operation completes. 

The worker's loop is frozen. It cannot pull the next task, cannot check the I/O driver, cannot participate in work-stealing. Every task in that worker's queue is stalled until the blocking call returns.

### Why the compiler cannot catch this

The immediate objection is that blocking code should not exist inside an async context. Every Rust async tutorial says this. Tokio's documentation says this.

And yet it happens, because Rust's compiler, which catches data races, use-after-free, dangling references, and unhandled errors at compile time, has no mechanism to detect blocking inside async.

There is no trait bound that distinguishes a blocking function from a non-blocking function. There is no `#[must_not_block]` attribute in the language. Clippy has no default lint for calling `std::thread::sleep` or `std::fs::read` inside an `async fn`.

The following code compiles without any warning:

```rust
async fn fetch_config() -> Result<Vec<u8>, std::io::Error> {
    // This blocks the Tokio worker thread for the duration of the disk read.
    // rustc emits no warning. Clippy emits no warning.
    let bytes = std::fs::read("/etc/app/config.toml")?;
    Ok(bytes)
}
```

The function signature says `async fn`, so the compiler generates a state machine for it. But the body contains a blocking file read that will freeze the worker thread for the duration of the disk I/O.

A file read might return in 50 microseconds if the page is cached, or 5 milliseconds if it hits disk, or 500 milliseconds if the NFS server is slow. The compiler sees a function call that returns `Result<Vec<u8>, io::Error>` and has no way to know which of those scenarios will occur.

The compiler cannot distinguish this from legitimate CPU work that happens to take a long time. Static analysis cannot determine, in the general case, whether a function call will block. Blocking is a runtime property that depends on the kernel, the device, the network, and the current system load.

### How blocking code enters async codebases

Given that the compiler cannot help, blocking code enters production through several well-worn paths:

- Pre-async code: Utility functions that load config, parse static data, or read feature flags remain synchronous after a migration to async. They work correctly and are never rewritten.
- The standard library: std::fs, std::net, and std::thread are synchronous. A developer who reaches for these out of habit writes code that compiles without warning. Tokio provides async equivalents, but the compiler does not suggest them.
- Non-obvious blocking: println! to a slow stdout, serde_json::from_slice on large payloads, env::var under contention. These do not look like blocking I/O in code review.
- FFI calls: Any call into a C library is blocking by default. Crypto libraries, compression, DNS resolution via libc—they all block the calling thread.
- Transitive dependencies: A crate three layers deep calls std::fs::metadata or reqwest::blocking::get. You never see it in your source code. It compiles, passes CI, and blocks in production.

Across all paths, the blocking code is correct. It produces the right output. The defect is not in what the code does, but in where it runs.

### The saturation threshold 

Why spare capacity hides the damage and increased load exposes the damage? The point where blocking transitions from invisible to catastrophic.

At low concurrency, blocking calls rarely overlap. If one worker is blocked, the other three continue their poll loop, stealing tasks from the blocked worker's queue. The stolen tasks experience some additional latency, but the system stays within timeout thresholds. The service appears healthy.

The self-healing breaks when blocking calls overlap enough to saturate the worker pool. When the number of blocked workers equals the total worker count, there are zero free workers running the poll loop. No tasks are polled. No I/O events are checked. No work-stealing happens.

This is a cliff, not gradual degradation. Section 1 showed this empirically: zero failures at 15 concurrent requests, 94% at 50. The threshold is determined by the ratio of blocked workers to total workers. Below 1.0, the system self-heals. At 1.0, every async task is starved.

---

## Section 4: The Benchmark

The demonstration in Section 1 showed the cliff through operational failures: timeouts and cascading errors. To understand the cliff with more precision, we need to measure the scheduling overhead directly.

### Results

The benchmark ran on an EC2 c6i.xlarge instance (4 vCPUs, 8 GB RAM) with Tokio configured for 4 worker threads. Each scenario ran 3 times; the table below shows representative results from Run 1 (all runs showed the same pattern).

```
Scenario                  Workers  Async  Block   p50(μs)   p99(μs)    max(μs)
--------------------------------------------------------------------------------
baseline/no-block              4    500      0      1230      1387       1410
high-async/1-blocker           4    500      1      1253      1490       1505
high-async/2-blockers          4    500      2      1341      1959       1960
high-async/3-blockers          4    500      3      1310      1564       1567
high-async/4-blockers          4    500      4      1300    140415     150271
```

### Three observations

The cliff is real and sharp. With 3 blockers on 4 workers, p99 is 1.5ms. With 4 blockers, p99 is 140ms—a 90x increase from one task. The system does not degrade linearly; it collapses when blocked workers equal total workers.

p50 is blind to the problem. At 3 blockers, p50 is 1,310μs. At 4 blockers, p50 is 1,300μs. The median is virtually unchanged. The damage is entirely in the tail.

One worker is the margin. The difference between 3 and 4 blockers is the difference between one free worker and zero. With one free worker, the system self-heals. With zero, p99 explodes by two orders of magnitude.

### Scaling to production hardware

On 4 workers, the cliff appears at roughly 20 concurrent requests with 30% blocking probability. That is integration testing territory. On a 16-vCPU production machine with 16 workers, the same mechanism applies, but the cliff moves to higher concurrency: perhaps 200-300 concurrent requests before enough blocking calls overlap to saturate the pool. More workers do not prevent the cliff. They delay it to a concurrency level that passes staging and appears only in production. The larger the machine, the wider the gap between testing load and failure load.

---

## Section 5: From Scheduling Delay to Panic

Sections 3 and 4 established that blocking code inflates scheduling overhead from microseconds to hundreds of milliseconds. That overhead, by itself, does not cause a panic. A task that takes 150 milliseconds instead of 10 milliseconds is slow, but slow is not broken. The panic comes from a secondary mechanism that converts latency into an error. There are four common mechanisms in production async services, and all four are triggered by the same scheduling delay.

### The timeout path

The most direct path from scheduling delay to panic runs through timeouts. Async libraries and frameworks routinely wrap operations in `tokio::time::timeout`:

```rust
let result = tokio::time::timeout(
    Duration::from_millis(100),
    db.query("SELECT * FROM users WHERE id = $1", &[&id])
).await;
```

Under normal conditions, the database query completes in 10 milliseconds. The 100-millisecond timeout provides a 90-millisecond margin for scheduling jitter, network variance, and database load. Under worker starvation, the task sits in the ready queue for 140 milliseconds before a worker polls it. The query itself still takes 10 milliseconds, but the total elapsed time is 150 milliseconds. The timeout fires, `timeout` returns `Err(Elapsed)`, and if the calling code handles this with `.unwrap()`, `.expect()`, or propagates it to a handler that panics on error, the service panics. This is exactly what the demonstration in Section 1 showed: 290 operations exceeded a 100-millisecond timeout because their scheduling delay alone consumed the entire margin.

### The channel backpressure path

Bounded channels introduce a second failure path. A common async architecture uses `tokio::sync::mpsc` channels to decouple producers from consumers:

```rust
let (tx, mut rx) = tokio::sync::mpsc::channel(100); // bounded, capacity 100

// Producer task
tokio::spawn(async move {
    for item in items {
        tx.send(process(item)).await.unwrap();
    }
});

// Consumer task
tokio::spawn(async move {
    while let Some(item) = rx.recv().await {
        handle(item).await;
    }
});
```

The consumer task calls `rx.recv().await`, which returns `Poll::Pending` when the channel is empty and waits for a Waker from the sender. Under worker starvation, the consumer task is woken (a message is available) but sits in the ready queue waiting for a worker to poll it. Meanwhile, the producer keeps sending messages. The bounded channel fills to its capacity of 100. The producer's `tx.send().await` now returns `Poll::Pending` because the channel is full, and the producer is also parked.

If the producer uses `tx.try_send()` instead, it gets `Err(TrySendError::Full)` and must decide whether to drop the message, buffer it, or propagate the error. If the producer uses `tx.send_timeout()`, the timeout fires and returns an error. In any of these cases, a component that was working correctly is now failing because a worker thread somewhere else in the runtime is blocked. The error message says "channel full" or "send timeout," not "worker thread blocked by std::fs::read."

### The connection pool exhaustion path

Database connection pools introduce a third failure path. Pools like sqlx, deadpool, and bb8 maintain a fixed number of connections:

```rust
let pool = PgPool::connect_with(
    PgConnectOptions::new().host("db.internal")
).await?;
// Pool has a default max of, say, 10 connections.
```

An async task acquires a connection, sends a query, and awaits the response. Under normal conditions, the task holds the connection for the duration of the query (10-50 milliseconds), then releases it. Under worker starvation, the task holds the connection while sitting in the ready queue. The query response has arrived (the kernel has buffered it on the socket), but no worker is available to poll the task and process the response. The connection is occupied but idle: it is not doing work, and it is not released back to the pool.

Other tasks that need a database connection call `pool.acquire().await`. If all connections are held by stalled tasks, `acquire` returns `Poll::Pending` and the requesting task waits. If the pool has an acquisition timeout (and it should), the timeout fires. The service now reports "connection pool exhausted" or "acquire timeout" errors. The database is healthy, reachable, and underloaded. The pool is exhausted because connections are held by tasks that cannot be polled, not because the database is slow. The error points at the database layer, not at the blocking code that starved the workers.

### The mutex contention path

Shared state protected by `tokio::sync::Mutex` introduces a fourth failure path. Under normal scheduling, a task acquires the mutex, performs a short critical section, and releases it within microseconds. Under worker starvation, a task acquires the mutex, then returns `Poll::Pending` at an `.await` inside the critical section. The task holds the mutex while parked in the ready queue. Every other task that attempts to acquire the same mutex is now blocked behind a task that is not being polled.

If the mutex guard is held across an `.await` point (which `tokio::sync::Mutex` is specifically designed to allow), scheduling delay directly translates into lock hold time. The result is cascading contention: tasks wait for a lock held by a task that is waiting for a worker. The error surfaces as increased latency across every task that touches the shared state, or as a deadlock if the contention is severe enough.

### Why blame lands on the wrong code

In all four failure paths, the error surfaces far from the blocking code. The panic trace shows a timeout inside an async handler, a channel send failure, a connection pool exhaustion error, or a lock contention timeout. The blocking code is running on a different worker thread, in a different task, with no direct call-stack relationship to the failing code. The blocking code completes successfully, returns correct data, and logs no errors.

The timeline the team observes is: workload increased (or a new library was integrated, or a traffic spike occurred), then failures started. The timeline that actually matters is: blocking code existed in the codebase, then the worker pool reached saturation, then scheduling delay exceeded failure thresholds in downstream components. The first timeline is visible in deployment logs, traffic graphs, and incident reports. The second timeline is invisible without knowledge of how the Tokio worker pool operates.

### The diagnostic gap

This failure mode is invisible to stack traces. The blocking code is not in the panicking task's call stack. The panic originates in a timeout, a channel operation, or a pool acquisition, none of which reference the function that blocked the worker.

It is invisible to CPU profilers. The blocking code is waiting on network I/O or disk I/O. It consumes negligible CPU time. A flame graph shows the blocking function as a thin sliver of wall-clock time with almost no on-CPU samples. The profiler does not flag it because it is not burning CPU.

It is invisible to application logs. The blocking code completes successfully and logs "config downloaded" or "file read complete." The async code that fails logs "timeout exceeded" or "connection pool exhausted." There is no log entry that connects the two.

It is invisible to p50-based monitoring. Section 4 showed that p50 increases by less than 9% even at full blockage. A dashboard displaying median latency shows a healthy service while 1% of requests experience 140 milliseconds of scheduling delay.

It is invisible to code review. The blocking code is a correct, well-tested function that produces the right output. It would pass any review focused on correctness, error handling, or code style. The defect is contextual: the function is safe in synchronous code and hazardous in async code, and nothing in the function itself reveals which context it runs in.

It is visible to `tokio-console`, which shows per-task poll latency and worker thread utilization. It is visible to targeted benchmarks that measure scheduling overhead under controlled conditions. It is visible to engineers who understand the cooperative scheduling contract and know to look for violations of it. The next section covers how to use these tools and how to prevent the problem from occurring.

---

## Section 6: Detection and Prevention

### Identifying blocking code

The first step is knowing what constitutes blocking in an async context. The definition is mechanical: any function call that does not return `Poll::Pending` and holds the OS thread for a duration that impacts scheduling. In practice, this falls into four categories.

**Synchronous I/O:** `std::fs::read`, `std::fs::write`, `std::fs::metadata`, `std::net::TcpStream::connect`, `reqwest::blocking::get`. These enter the kernel, put the thread in a sleep state, and do not return until the I/O completes.

**Thread-level sleep:** `std::thread::sleep`. This explicitly asks the kernel to deschedule the thread for a fixed duration.

**CPU-bound computation that exceeds the cooperative threshold:** `serde_json::from_slice` on a multi-megabyte payload, image encoding, cryptographic operations, compression. These do not enter the kernel, but they hold the worker for the duration of the computation without yielding.

**FFI calls:** Any function call across the FFI boundary into a C library that performs I/O, computation, or synchronization internally. The Rust compiler has no visibility into what a C function does, and the function will not return `Poll::Pending` because it does not know it is running inside an async context.

The common factor across all four categories is that the worker's poll loop cannot advance until the call returns.

### Replacing blocking calls with async equivalents

The simplest fix is to replace the blocking call with its async counterpart when one exists.

```rust
// BEFORE: blocks the worker for the entire disk read.
let config = std::fs::read("/etc/app/config.toml")?;

// AFTER: yields the worker during the disk read.
let config = tokio::fs::read("/etc/app/config.toml").await?;
```

For HTTP clients:

```rust
// BEFORE: blocks the worker for the full HTTP round-trip.
let bytes = reqwest::blocking::get(url)?.bytes()?;

// AFTER: yields at each .await during the round-trip.
let bytes = reqwest::get(url).await?.bytes().await?;
```

For sleep:

```rust
// BEFORE: puts the OS thread to sleep.
std::thread::sleep(Duration::from_millis(100));

// AFTER: parks the task, frees the worker, wakes via the timer wheel.
tokio::time::sleep(Duration::from_millis(100)).await;
```

For DNS resolution:

```rust
// BEFORE: std::net::ToSocketAddrs blocks during DNS lookup.
let addr = "db.internal:5432".to_socket_addrs()?.next().unwrap();

// AFTER: Tokio's async DNS resolution.
let addr = tokio::net::lookup_host("db.internal:5432").await?.next().unwrap();
```

In every case, the network latency or disk latency is identical. The CPU work is identical. What changes is that the worker thread is no longer held hostage during the wait. The state machine yields at the `.await` point, the worker polls other tasks, and the task resumes when the I/O completes.

### Using spawn_blocking when async equivalents do not exist

When the blocking code cannot be replaced with an async equivalent, Tokio provides `tokio::task::spawn_blocking`. It moves the closure to a separate, dedicated thread pool that is distinct from the worker threads. The dedicated pool exists solely for blocking operations: its threads do not run poll loops, do not service async tasks, and do not participate in work-stealing. When the closure completes, the returned `JoinHandle` wakes the calling task via the standard Waker mechanism, and the worker resumes the task on its next poll iteration.

```rust
// FFI call that blocks: isolate it from the worker pool.
let result = tokio::task::spawn_blocking(move || {
    ffi_crypto_lib::verify(payload)
}).await?;
```

```rust
// CPU-heavy computation: isolate it from the worker pool.
let parsed = tokio::task::spawn_blocking(move || {
    serde_json::from_slice::<LargeStruct>(&bytes)
}).await??;
```

```rust
// Legacy synchronous library with no async API.
let config = tokio::task::spawn_blocking(move || {
    legacy_config_lib::load("/etc/app/config.toml")
}).await??;
```

The cost of `spawn_blocking` is one cross-thread dispatch (the closure is sent to the blocking pool via a channel) and one Waker notification (when the closure completes). This overhead is measured in single-digit microseconds. The cost of blocking a worker thread is measured in the hundreds of milliseconds of scheduling delay inflicted on every other task in the runtime. The asymmetry between these costs is the engineering argument for always erring on the side of `spawn_blocking` when in doubt.

### Detecting blocking at runtime

Prevention requires knowing where blocking code exists, but some blocking calls are buried in dependencies or are intermittent (a file read that only blocks when the page is not cached). For these cases, runtime detection is necessary.

`tokio-console` is the primary tool. It is a diagnostic tool that connects to a running Tokio application via a subscriber and displays real-time information about tasks and worker threads. The key metric is task poll duration: the time between a worker calling `poll` on a task and the `poll` returning. In a healthy application, poll durations are microseconds. A task that contains blocking code will show poll durations in the milliseconds or longer. `tokio-console` also shows worker thread utilization: a worker that is blocked will appear as "busy" (from the runtime's perspective, it is inside a `poll` call) with zero task throughput.

The `tokio::runtime::metrics` API provides programmatic access to the same data. `RuntimeMetrics::worker_poll_count` reports how many tasks each worker has polled. A blocked worker will show a poll count that falls behind other workers. `RuntimeMetrics::worker_busy_duration` reports how long each worker has spent inside `poll` calls. A worker that is blocked will show high busy duration with disproportionately low poll count: it is spending all its time inside a single `poll` call rather than cycling through many tasks. Emitting these metrics to a monitoring system (Prometheus, Datadog, CloudWatch) and alerting on divergence between workers is a direct signal of blocking in the runtime.

### The engineering rule

The guidance condenses to a single rule: if a function touches the network, reads from or writes to disk, calls into a C library through FFI, or runs CPU-intensive computation for more than a few hundred microseconds, it does not belong inside an async task without either an async equivalent or `spawn_blocking`.

Treat this rule with the same discipline applied to `unsafe` code. `unsafe` tells the compiler: "I am taking responsibility for a safety invariant you cannot verify." A blocking call inside an async task is the scheduling equivalent: "I am taking responsibility for the cooperative contract the runtime cannot enforce." The difference is that `unsafe` requires an explicit keyword. Blocking requires nothing. The compiler will not catch it. The tests will not catch it (unless they run at production-level concurrency). The staging environment will not catch it (unless it matches production load patterns). Only understanding the runtime beneath your code will catch it.

---

## Conclusion

The Tokio multi-threaded runtime is a cooperative system. Its performance depends on every task honoring a contract: yield at `.await` points, return `Poll::Pending` when waiting, and never hold the worker thread hostage. Blocking code violates this contract silently. The runtime has no mechanism to detect the violation, because a `poll` call that takes 200 milliseconds is indistinguishable from one that takes 2 microseconds: both are function calls that have not yet returned. Blocking code enters the codebase easily, compiles without warning, and works surprisingly well until it does not. When failures begin, the compiler points nowhere useful, and developers are left searching for a culprit that appears in no error trace.

Spare worker capacity absorbs the damage. Work-stealing redistributes tasks from blocked workers to free ones. As long as one worker remains free, the system self-heals and latency stays bounded. When the last free worker is lost, the self-healing mechanism collapses and scheduling delay jumps by two orders of magnitude.

The benchmark in this article measured the cliff with precision: p99 scheduling overhead increased from 1.5 milliseconds to 140 milliseconds when the last free worker was blocked, while p50 increased by less than 9%. The demonstration showed the cliff in operational terms: the same blocking code produced zero failures at 15 concurrent requests and a 94% failure rate at 50 concurrent requests. In both cases, the blocking code completed successfully, returned correct data, logged no errors, and appeared in no failure trace.

The failure paths are varied (timeouts, channel backpressure, connection pool exhaustion, mutex contention) but the root cause is singular: a worker thread that cannot run its poll loop. Every diagnostic artifact (stack traces, logs, metrics, profiler output) points at the async code that failed, not at the blocking code that caused the failure.

Detection requires runtime introspection: `tokio-console` for development, `tokio::runtime::metrics` for production monitoring. Prevention requires discipline: replace blocking calls with async equivalents where they exist, isolate the rest behind `spawn_blocking`, and treat the cooperative contract with the same seriousness as memory safety.

The compiler enforces memory safety. The compiler does not enforce scheduling safety. That responsibility falls on the engineer.

---

*Full benchmark code and additional demonstrations available at [github.com/bobby-math/tokio-blocking-bench](https://github.com/bobby-math/tokio-blocking-bench)*
