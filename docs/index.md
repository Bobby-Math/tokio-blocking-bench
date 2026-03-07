# Executor Starvation in Async Rust: The Hidden Cost of Blocking Code

---

## Section 1: The Demonstration

### The execution model

Tokio's multi-threaded runtime operates on three levels of execution that are often conflated but must be understood separately.

At the bottom are hardware threads: the logical CPUs exposed by the processor. With SMT (Hyperthreading), each physical core exposes 2 hardware threads. AWS reports each hardware thread as a vCPU, so a 4-vCPU instance has 2 physical cores and 4 logical CPUs available for scheduling.

By default, Tokio creates one OS thread per logical CPU at startup. These are Tokio's worker threads.

Each worker thread runs a loop that pulls async tasks from a queue, calls poll() on them, and checks the I/O driver for readiness events.

At the top are tasks: the compiler-generated state machines produced by `async fn` and spawned via `tokio::spawn`.

A task has no thread of its own; it borrows a worker thread for the duration of each `poll()` call, typically microseconds, then yields.

On a 4-vCPU machine, this means 4 worker threads and potentially thousands of tasks multiplexed across them.

The ratio of tasks to worker threads is the source of async Rust's efficiency, and the source of its most insidious failure mode.

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

To understand why the failures in Section 1 occurred, you need a precise picture of what a worker thread does with its time.

### The poll loop

Each worker thread runs a loop that repeats three operations: pull a task from the local run queue, call `poll()` on it, and check the I/O driver for readiness events.

This loop runs for the entire lifetime of the runtime. The worker never sleeps voluntarily unless its queue is empty and no I/O events are pending.

When a task is available, the worker calls the task's `poll` method. `poll` is the single method defined by Rust's `Future` trait:

```rust
pub trait Future {
    type Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}
```

The return type, `Poll`, has exactly two variants: `Poll::Ready(value)` when the task has produced its final result, and `Poll::Pending` when the task cannot make progress yet.

If `poll` returns `Ready`, the task is complete and the worker moves on to the next task in the queue. If `poll` returns `Pending`, the task is parked: it leaves the run queue and will not be polled again until something wakes it.

A single iteration of this loop (pull, poll, check I/O) typically takes microseconds. This is how four worker threads can service thousands of tasks: each task occupies a worker for a few microseconds per poll, then yields.

### The Waker contract

When a task returns `Poll::Pending`, it makes a contract with the runtime.

The contract is: "I have registered a `Waker` with the I/O source I am waiting on. When that source is ready, it will call `waker.wake()`, which will place me back on a worker's run queue."

The `Waker` is provided to the task through the `Context` argument in the `poll` signature. The I/O source might be a TCP socket becoming readable, a timer expiring, or a channel receiving a message.

When the source is ready, it calls `wake()` on the stored `Waker`. The runtime receives the wake signal and pushes the task back onto a worker's local queue. The worker picks up the task on a future iteration of its loop, calls `poll` again, and the task resumes from where it left off.

This is cooperative scheduling: the task voluntarily yields by returning `Pending`, and the runtime resumes it only when the task has declared it can make progress.

The entire model depends on two properties: tasks must return `Pending` quickly, and tasks must register a `Waker` before returning `Pending`. If either property is violated, the model breaks.

### How async fn produces cooperative tasks

The cooperative model works because the Rust compiler transforms every `async fn` into a type that implements `Future`. The generated type is a state machine. Each `.await` point in the function body becomes a state transition in the machine.

Consider a simplified async function:

```rust
async fn handle_request(db: &Pool, id: u64) -> Result<Response, Error> {
    let row = db.query("SELECT * FROM users WHERE id = $1", &[&id]).await?;
    let profile = fetch_profile(row.profile_url).await?;
    Ok(build_response(row, profile))
}
```

This function has two `.await` points: the database query and the profile fetch. The compiler generates a state machine with roughly three states: waiting for the database query, waiting for the profile fetch, and completed.

When `poll` is called for the first time, the state machine initiates the database query and returns `Poll::Pending`, because the query has not completed yet. At this point, the worker thread is free. The database query's I/O source (the TCP socket to the database) holds the task's `Waker`. The worker immediately moves on to poll the next task in its queue.

When the database response arrives, the kernel signals the socket as readable, Tokio's I/O driver detects the readiness event, and the driver calls `wake()` on the stored `Waker`. The task re-enters the run queue.

A worker (possibly a different one) picks it up, calls `poll` again, and the state machine advances to the next state: it processes the database row, initiates the profile fetch, and returns `Poll::Pending` again. The cycle repeats until the state machine reaches its final state and returns `Poll::Ready(response)`.

The critical property of this entire sequence is that the worker thread was occupied only during the brief moments between `.await` points. The database query took 5 milliseconds, but the worker was held for microseconds. The profile fetch took 20 milliseconds, but the worker was held for microseconds. The rest of that time, the worker was polling other tasks.

### Work-stealing

Each worker thread maintains its own local task queue. When a task's `Waker` fires, the runtime typically pushes the task onto the queue of the worker that is running the I/O driver at that moment. This means tasks can end up unevenly distributed: one worker might have 200 tasks queued while another has 10.

To balance this, Tokio implements work-stealing. When a worker's local queue is empty, it checks other workers' queues and steals a batch of tasks. The steal operation takes tasks from the tail of another worker's queue (LIFO order for the stealer, FIFO for the victim), which tends to preserve cache locality for the victim's most recently queued tasks.

Work-stealing runs automatically and requires no configuration. Its relevance to the failure in Section 1 is this: when one worker thread becomes unavailable, the other workers steal its tasks and continue processing them. This is the mechanism that makes blocking invisible at low load.

As long as at least one worker is running its poll loop, it will steal and process tasks from blocked workers' queues. The tasks experience some additional latency from the steal operation and from competing with more tasks on fewer workers, but the system continues to function.

### The cooperative contract, summarized

A healthy Tokio runtime depends on every task obeying a simple contract: do a small amount of work, yield by returning `Poll::Pending`, and arrange to be woken when you can make progress.

Worker threads enforce no part of this contract. A worker calls `poll` and waits for the return value. It has no timeout on the poll call. It has no mechanism to preempt a task that is taking too long. It has no way to distinguish a task that is doing legitimate computation from a task that has blocked the OS thread.

From the worker's perspective, a `poll` that takes 2 microseconds and a `poll` that takes 200 milliseconds look identical: both are function calls that have not yet returned. The worker simply waits. Every other task in that worker's queue waits with it.

This is the mechanical foundation for everything that follows in Section 3.

---

## Section 3: What Blocking Does to the Poll Loop

Now consider what happens when a task calls `std::thread::sleep`, `reqwest::blocking::get`, `std::fs::read`, or any other function that blocks the OS thread.

### The mechanism

The blocking call does not return `Poll::Pending`. It does not register a `Waker`. It does not transition the state machine to a parked variant. It simply occupies the OS thread until the operation completes.

From the worker thread's perspective, it called `poll` on a task, and the `poll` method has not returned. The worker's loop is frozen. It cannot pull the next task from its queue. It cannot check the I/O driver for readiness events. It cannot participate in work-stealing.

Every task in that worker's queue, and every pending Waker that would have been serviced by that worker's I/O driver check, is stalled until the blocking call returns.

Compare this to the async example from Section 2. The database query in `handle_request` held the worker for microseconds, then returned `Pending`, freeing the worker to poll other tasks during the 5-millisecond network round-trip. A blocking version of the same query would hold the worker for the entire 5 milliseconds. During those 5 milliseconds, every other task on that worker is frozen.

The difference is not in the result (both return the same database row) or the total wall-clock time (both take 5 milliseconds). The difference is in how long the worker thread is held hostage. Microseconds versus milliseconds. One allows the worker to service hundreds of other tasks during the wait. The other allows it to service none.

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

Given that the compiler cannot help, blocking code enters production async services through several well-worn paths.

#### Pre-async code that survives a migration

The most common path is code that predates the async migration. A codebase starts as synchronous Rust. The team migrates the network layer to Tokio for better concurrency. The HTTP server becomes async, the request handlers become async, the database queries become async. But the utility functions that load configuration files, parse static data at startup, initialize logging, or read feature flags from disk remain synchronous. They work correctly. They are not on the apparent critical path. They were written before Tokio entered the picture, and nobody rewrites a working config loader just because the HTTP server is now async. These functions sit in the codebase for months or years, called from async handlers, blocking a worker thread on every invocation, invisible until the concurrency reaches the threshold.

#### The standard library itself

Rust's standard library provides synchronous-only APIs for common operations. `std::fs::read`, `std::fs::write`, `std::fs::metadata`, `std::net::TcpStream::connect`, and `std::thread::sleep` are all synchronous. Tokio provides async equivalents for each of these: `tokio::fs::read`, `tokio::net::TcpStream`, `tokio::time::sleep`. But the standard library is what every Rust developer learns first. A developer who reaches for `std::fs::read` out of habit, or who does not know that `tokio::fs` exists, writes code that compiles without warning, passes every test, and blocks a worker thread on every call. The compiler does not suggest the async alternative. Nothing in the error output distinguishes `std::fs::read` from `tokio::fs::read` except the presence of an `.await`.

#### Implicit blocking that does not look like I/O

Some blocking calls do not look like I/O operations at all. `println!` writes to stdout, which is typically line-buffered. If stdout is piped to a slow consumer, a logging daemon with a full buffer, or a container runtime that applies backpressure, the write blocks. `serde_json::from_slice` on a multi-megabyte payload is CPU-bound: it holds the worker thread for the duration of the parse, which can be tens of milliseconds on a large document. `env::var` on some platforms acquires a lock that can block under contention. None of these look like "blocking I/O" in a code review. They are function calls that a reviewer would scan past without a second thought, because they appear to be pure computation or simple output.

#### FFI calls into C libraries

Any call across the FFI boundary into a C library is blocking by default. The C function does not know it is running inside a Tokio worker thread. It does not return `Poll::Pending`. It does not register a Waker. It runs to completion on the calling thread, however long that takes. Crypto libraries (OpenSSL, ring's C components), compression libraries (zlib, zstd), image processing libraries, and system-level DNS resolution (getaddrinfo via libc) all block the calling thread. Rust's ownership and borrowing guarantees stop at the FFI boundary. The cooperative scheduling contract stops there too.

#### Transitive dependencies

The most difficult path to defend against is blocking code inside dependencies you do not control. Your own code may be clean. Your direct dependencies may be clean. But a crate three layers deep in your dependency tree calls `std::fs::metadata` to check whether a cache file exists, or `reqwest::blocking::get` to fetch a license validation response, or `std::thread::sleep` in a retry loop. You never see the blocking call in your own source code. It does not appear in any file you wrote or reviewed. It compiles, it passes CI, and it blocks a worker thread in production. Auditing transitive dependencies for blocking calls requires inspecting the source of every crate in your dependency tree, which is rarely practical.

#### The common thread

Across all five paths, the blocking code is correct. It produces the right output. It handles errors properly. It would pass any code review focused on correctness, style, or error handling. The defect is not in what the code does, but in where it runs. The same function that is perfectly safe in a synchronous context becomes a scheduling hazard in an async context. And the compiler cannot tell the difference.

### Why spare capacity hides the damage

Return to the scenario from Section 1: four worker threads, 30% of requests hitting a blocking path that takes 50 milliseconds.

At 10 concurrent requests, roughly 3 requests hit the blocking path. Even if all 3 blocking calls overlap (unlikely at low concurrency), one worker remains free. That free worker continues its poll loop: pulling tasks, checking I/O, stealing work from the queues of blocked workers.

It is a direct consequence of work-stealing. The stolen tasks experience some additional latency from competing with more tasks on fewer workers, but the latency stays well within timeout thresholds. From the outside, the service is healthy. Metrics show normal latency. The blocking code completes successfully and returns correct results. Nothing appears wrong.

Section 2 described how workers steal tasks from other workers' queues when their own queues are empty. When a worker is blocked, its queue grows because tasks are being woken by the I/O driver but the worker cannot poll them. Free workers detect the growing queue and steal from it. As long as the free workers can drain the stolen tasks faster than they accumulate, latency stays bounded. The system is self-healing, up to a point.

### Why increased load exposes the damage

The self-healing breaks when the load exceeds what the remaining free workers can absorb. This can happen through any of the three triggers shown in Section 1.

More blocking calls overlap (because traffic increased), reducing the number of free workers. More async tasks queue up (because a new library was integrated), increasing the work each free worker must handle. Or both happen simultaneously.

The critical transition is when the number of simultaneously blocked workers approaches or equals the total worker count. At that point, there are zero free workers running the poll loop. No tasks are being polled. No I/O events are being checked. No work-stealing is happening, because there is no worker available to steal.

Tasks only execute in the brief gaps when a blocking call completes and its worker resumes the poll loop for a few microseconds before the next blocking call lands on it. Scheduling delay goes from microseconds to tens or hundreds of milliseconds.

This transition is a cliff, rather than gradual. Section 1 showed this empirically: zero failures at 15 concurrent requests, 5.5% at 20, 37% at 30, 94% at 50.

The system does not degrade linearly. It functions normally until a threshold, then collapses. The threshold is determined by the ratio of simultaneously blocked workers to total workers. Below 1.0, the system self-heals through work-stealing. At 1.0, the self-healing mechanism is gone and every async task in the runtime is starved.

---

## Section 4: The Benchmark

The demonstration in Section 1 showed the cliff through operational failures: timeouts and cascading errors. To understand the cliff with more precision, we need to measure the scheduling overhead directly.

### Methodology

The benchmark measures async task scheduling latency under controlled blocking conditions. Each scenario spawns a fixed number of async tasks and blocking tasks, then measures how long async operations take to complete.

The core measurement loop is simple:

```rust
for _ in 0..iterations {
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let overhead = t0.elapsed().saturating_sub(Duration::from_millis(10));
    histogram.record(overhead.as_micros() as u64);
}
```

Each async task performs 10 sequential sleeps of 10 milliseconds each. For each sleep, the benchmark records the overhead: actual elapsed time minus the expected 10 milliseconds. In a healthy runtime, overhead per sleep is roughly 1 millisecond. Under worker starvation, the overhead per sleep grows to tens or hundreds of milliseconds as the task waits in the ready queue between each `.await` point.

Blocking tasks simulate the synchronous code path:

```rust
for _ in 0..5 {
    std::thread::sleep(Duration::from_millis(50));
    tokio::task::yield_now().await;
}
```

Each blocking task holds a worker thread for 50 milliseconds, yields briefly, then repeats. This models the pattern from Section 1: a blocking call that completes successfully and returns control to the runtime.

**A note on the barrier.** All tasks start simultaneously via a `tokio::sync::Barrier`. This creates a worst-case scenario where blocking calls overlap maximally at the start of each run. Real traffic patterns have staggered arrivals, which could shift the threshold in either direction depending on arrival rate and blocking duration. The barrier is a simplifying assumption for reproducibility. We are studying worker behavior at increased load; in the real world, that load could come from more blocking code, more non-blocking code, or traffic spikes. The mechanism is the same.

Full benchmark code is available at [github.com/bobby-math/tokio-blocking-bench](https://github.com/bobby-math/tokio-blocking-bench).

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

**First: the cliff is real and sharp.** With 3 blocking tasks on 4 workers, p99 latency is 1.5 milliseconds. With 4 blocking tasks on 4 workers, p99 latency is 140 milliseconds. That is a 90x increase from adding one blocking task. The system does not degrade linearly. It functions normally until the blocking count equals the worker count, then collapses.

**Second: p50 is blind to the problem.** Look at the p50 column. At 3 blockers, p50 is 1,310 microseconds. At 4 blockers, p50 is 1,300 microseconds. The median latency is virtually unchanged. A monitoring system that alerts on p50 would see nothing wrong. The damage is entirely in the tail: p99 and max. This is why blocking-induced starvation evades median-based monitoring and only surfaces in tail latency metrics or timeout rates.

**Third: one worker is the margin.** The difference between 3 blockers and 4 blockers is the difference between "one free worker" and "zero free workers." With one free worker, the system self-heals through work-stealing and maintains sub-2ms p99. With zero free workers, work-stealing cannot function and p99 explodes by two orders of magnitude. The last free worker is not a luxury. It is the entire margin between a functioning service and a failing one.

### Cross-run consistency

All three runs showed the same pattern:

| Scenario | Run 1 p99 | Run 2 p99 | Run 3 p99 |
|----------|-----------|-----------|-----------|
| high-async/3-blockers | 1,564 μs | 1,418 μs | 1,372 μs |
| high-async/4-blockers | 140,415 μs | 140,415 μs | 140,415 μs |

The cliff is not a fluke of a single run. It is a stable, reproducible property of the system. The threshold is deterministic: when blocked workers equal total workers, the runtime starves.

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
