// tokio-blocking-bench/src/bin/demo_panic.rs
//
// DEMONSTRATION: The same blocking code produces zero failures at low load
// and cascading timeouts at high load.
//
// This is not a benchmark. This is a reproduction of the production failure
// pattern described in the article. Run it twice with different --async-tasks
// values and observe the transition from "everything works" to "timeouts
// everywhere."
//
// Usage:
//   cargo build --release
//
//   # 3 blockers: 100% success, blocking is invisible.
//   ./target/release/demo_panic --async-tasks 500 --blocking-tasks 3 --rounds 10
//
//   # 4 blockers: ~34% timeouts, blocking causes cascade.
//   ./target/release/demo_panic --async-tasks 500 --blocking-tasks 4 --rounds 10
//
//   # The blocking code is IDENTICAL in both runs.
//   # The only difference is ONE additional blocking task.

use clap::Parser;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

#[derive(Parser, Debug)]
#[command(name = "demo_panic")]
struct Args {
    /// Tokio worker threads.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Number of async tasks (simulated request handlers).
    #[arg(long, default_value_t = 50)]
    async_tasks: usize,

    /// Number of blocking tasks (simulated sync file downloads).
    #[arg(long, default_value_t = 4)]
    blocking_tasks: usize,

    /// Timeout for async operations in milliseconds.
    /// In production, this would be a library's internal timeout,
    /// a database connection timeout, or an HTTP client timeout.
    #[arg(long, default_value_t = 100)]
    timeout_ms: u64,

    /// Duration of the simulated async work in milliseconds.
    /// This is the actual computation/IO the async task performs,
    /// excluding scheduling overhead.
    #[arg(long, default_value_t = 10)]
    work_ms: u64,

    /// Duration of the blocking call in milliseconds.
    /// Simulates: reqwest::blocking::get(), std::fs::read(),
    /// synchronous DNS lookup, FFI call into a C library.
    #[arg(long, default_value_t = 80)]
    blocking_ms: u64,

    /// Number of request rounds each async task processes.
    #[arg(long, default_value_t = 5)]
    rounds: usize,
}

/// Simulates a request handler in a production async service.
///
/// This is the code that PANICS (or would panic with .unwrap()).
/// The timeout exists because in production, you can't wait forever
/// for a database query or an RPC call.
async fn handle_request(
    task_id: usize,
    work_duration: Duration,
    timeout_duration: Duration,
    success_count: Arc<AtomicUsize>,
    timeout_count: Arc<AtomicUsize>,
) {
    // Simulate async work: multiple sequential async operations.
    // Each .await is a scheduling point where delay accumulates.
    // This models: query DB, then call API, then write cache.
    //
    // With 10 steps of 1ms each, total work is 10ms, but each step
    // requires a worker to poll. Under starvation, each poll is delayed,
    // and the delays accumulate: 10 steps × 50ms delay = 500ms total.
    let step_duration = work_duration / 10;

    let result = tokio::time::timeout(timeout_duration, async {
        for _ in 0..10 {
            tokio::time::sleep(step_duration).await;
        }
        // In a real service, this would be:
        //   let row = sqlx::query("SELECT ...").fetch_one(&pool).await?;
        //   let resp = reqwest::get(api_url).await?.json().await?;
        42 // placeholder return value
    })
    .await;

    match result {
        Ok(_value) => {
            success_count.fetch_add(1, Ordering::Relaxed);
        }
        Err(_elapsed) => {
            // In production, this is where the panic happens:
            //   result.unwrap()      -> panic: "deadline has elapsed"
            //   result.expect("...")  -> panic with message
            //   or: return Err(...)   -> propagates up to a handler that panics
            timeout_count.fetch_add(1, Ordering::Relaxed);

            // Print first few timeouts so the user can see them happening.
            let total = timeout_count.load(Ordering::Relaxed);
            if total <= 5 {
                eprintln!(
                    "  [task {}] TIMEOUT: async work took >{}ms to be scheduled \
                     (work itself only needs {}ms)",
                    task_id,
                    timeout_duration.as_millis(),
                    work_duration.as_millis()
                );
            } else if total == 6 {
                eprintln!("  ... (further timeout messages suppressed)");
            }
        }
    }
}

/// Simulates a blocking utility function that exists somewhere in the codebase.
///
/// This is the code that CAUSES the panics but is NEVER in the panic trace.
/// It completes successfully every time. It logs no errors. It returns
/// the correct result. From its own perspective, nothing is wrong.
async fn download_config(task_id: usize, blocking_duration: Duration, rounds: usize) {
    for round in 0..rounds {
        // THIS IS THE ANTI-PATTERN.
        //
        // In a real codebase, this might be:
        //   let bytes = reqwest::blocking::get(config_url)?.bytes()?;
        //   let data = std::fs::read("/etc/app/config.toml")?;
        //   let resolved = dns_lookup::lookup_host("db.internal")?;
        //   let result = ffi_crypto_lib::verify(payload)?;
        //
        // The compiler does not warn. Clippy does not warn.
        // The function signature says `async fn`.
        // The body blocks the OS thread.
        std::thread::sleep(blocking_duration);

        // The blocking function completes successfully every time.
        if round == 0 {
            eprintln!(
                "  [blocker {}] config download complete ({}ms, success)",
                task_id,
                blocking_duration.as_millis()
            );
        }

        // Yield between rounds. In real code, the blocking call might be
        // in a loop (periodic config refresh) or called once per request.
        tokio::task::yield_now().await;
    }
}

fn main() {
    let args = Args::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.workers)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let total_tasks = args.async_tasks + args.blocking_tasks;
        let barrier = Arc::new(Barrier::new(total_tasks + 1));
        let success_count = Arc::new(AtomicUsize::new(0));
        let timeout_count = Arc::new(AtomicUsize::new(0));

        let work_duration = Duration::from_millis(args.work_ms);
        let timeout_duration = Duration::from_millis(args.timeout_ms);
        let blocking_duration = Duration::from_millis(args.blocking_ms);

        let total_operations = args.async_tasks * args.rounds;

        println!();
        println!("=== Blocking in Async: Failure Reproduction ===");
        println!();
        println!("  Workers:        {}", args.workers);
        println!("  Async tasks:    {} ({} operations each = {} total)",
            args.async_tasks, args.rounds, total_operations);
        println!("  Blocking tasks: {} (each blocks worker for {}ms)",
            args.blocking_tasks, args.blocking_ms);
        println!("  Timeout:        {}ms", args.timeout_ms);
        println!("  Async work:     {}ms", args.work_ms);
        println!();
        println!("  Blocking ratio: {}/{} workers = {:.0}%",
            args.blocking_tasks, args.workers,
            (args.blocking_tasks as f64 / args.workers as f64) * 100.0);
        println!();

        let mut handles = Vec::with_capacity(total_tasks);

        // Spawn async request handlers.
        for i in 0..args.async_tasks {
            let barrier = barrier.clone();
            let sc = success_count.clone();
            let tc = timeout_count.clone();

            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                for _ in 0..args.rounds {
                    handle_request(i, work_duration, timeout_duration, sc.clone(), tc.clone())
                        .await;
                }
            }));
        }

        // Spawn blocking tasks (the hidden problem).
        for i in 0..args.blocking_tasks {
            let barrier = barrier.clone();

            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                download_config(i, blocking_duration, args.rounds).await;
            }));
        }

        // Release all tasks.
        barrier.wait().await;

        // Wait for completion.
        for handle in handles {
            handle.await.unwrap();
        }

        let successes = success_count.load(Ordering::Relaxed);
        let timeouts = timeout_count.load(Ordering::Relaxed);
        let timeout_pct = (timeouts as f64 / total_operations as f64) * 100.0;

        println!();
        println!("=== Results ===");
        println!();
        println!("  Successful operations:  {}/{}", successes, total_operations);
        println!("  Timed-out operations:   {}/{} ({:.1}%)", timeouts, total_operations, timeout_pct);
        println!();

        if timeouts == 0 {
            println!("  VERDICT: All operations succeeded.");
            println!("  The blocking code is invisible. Tests pass. Staging looks healthy.");
            println!("  Nothing appears wrong.");
            println!();
            println!("  Now run again with --async-tasks 500 and watch what happens.");
        } else {
            println!("  VERDICT: {} operations timed out.", timeouts);
            println!("  In production, each timeout would be a failed request.");
            println!("  If any of these hit an .unwrap(), the service panics.");
            println!();
            println!("  The blocking code (download_config) completed successfully");
            println!("  every time. It is not in any timeout's call stack. It logged");
            println!("  no errors. A developer looking at the timeout traces would");
            println!("  blame the async handler, not the config download.");
            println!();
            println!("  The fix: replace std::thread::sleep with tokio::time::sleep,");
            println!("  or wrap the blocking call in tokio::task::spawn_blocking.");
        }

        println!();
    });
}
