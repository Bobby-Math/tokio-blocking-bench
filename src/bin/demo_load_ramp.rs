// tokio-blocking-bench/src/bin/demo_load_ramp.rs
//
// DEMONSTRATION: Fixed blocking code, increasing async load.
//
// This reproduces the real-world scenario: blocking code exists in the
// codebase and has been working fine. A developer integrates a new async
// library that increases the task count. The service starts failing.
//
// The blocking code is IDENTICAL across all runs.
// The only variable is the number of async tasks.
//
// Usage:
//   cargo build --release
//
//   # Run all load levels in sequence:
//   ./target/release/demo_load_ramp --run-all
//
//   # Or run individually:
//   ./target/release/demo_load_ramp --async-tasks 100
//   ./target/release/demo_load_ramp --async-tasks 1000
//   ./target/release/demo_load_ramp --async-tasks 5000
//   ./target/release/demo_load_ramp --async-tasks 20000

use clap::Parser;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

#[derive(Parser, Debug)]
#[command(name = "demo_load_ramp")]
struct Args {
    /// Tokio worker threads.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Number of async tasks. This is the variable we ramp.
    #[arg(long, default_value_t = 100)]
    async_tasks: usize,

    /// Number of blocking tasks (FIXED across runs).
    /// This represents pre-existing blocking code in the codebase.
    #[arg(long, default_value_t = 3)]
    blocking_tasks: usize,

    /// Timeout for async operations in milliseconds.
    #[arg(long, default_value_t = 100)]
    timeout_ms: u64,

    /// Duration of the simulated async work in milliseconds.
    #[arg(long, default_value_t = 10)]
    work_ms: u64,

    /// Duration of the blocking call in milliseconds.
    #[arg(long, default_value_t = 80)]
    blocking_ms: u64,

    /// Number of rounds per task.
    #[arg(long, default_value_t = 5)]
    rounds: usize,

    /// Run all predefined load levels to show the ramp.
    #[arg(long, default_value_t = false)]
    run_all: bool,
}

async fn run_scenario(
    workers: usize,
    async_tasks: usize,
    blocking_tasks: usize,
    work_duration: Duration,
    timeout_duration: Duration,
    blocking_duration: Duration,
    rounds: usize,
) -> (usize, usize) {
    let total_tasks = async_tasks + blocking_tasks;
    let barrier = Arc::new(Barrier::new(total_tasks + 1));
    let success_count = Arc::new(AtomicUsize::new(0));
    let timeout_count = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::with_capacity(total_tasks);

    // Spawn async request handlers (the measured tasks).
    for _i in 0..async_tasks {
        let barrier = barrier.clone();
        let sc = success_count.clone();
        let tc = timeout_count.clone();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;

            for _ in 0..rounds {
                // Multiple sequential awaits - each is a scheduling point
                // where delay accumulates under worker starvation.
                let step_duration = work_duration / 10;

                let result = tokio::time::timeout(timeout_duration, async {
                    for _ in 0..10 {
                        tokio::time::sleep(step_duration).await;
                    }
                    42
                })
                .await;

                match result {
                    Ok(_) => { sc.fetch_add(1, Ordering::Relaxed); }
                    Err(_) => { tc.fetch_add(1, Ordering::Relaxed); }
                }
            }
        }));
    }

    // Spawn blocking tasks (the FIXED, pre-existing blocking code).
    for _i in 0..blocking_tasks {
        let barrier = barrier.clone();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;

            for _ in 0..rounds {
                // Pre-existing blocking code. Does not change between runs.
                std::thread::sleep(blocking_duration);
                tokio::task::yield_now().await;
            }
        }));
    }

    barrier.wait().await;

    for handle in handles {
        handle.await.unwrap();
    }

    let successes = success_count.load(Ordering::Relaxed);
    let timeouts = timeout_count.load(Ordering::Relaxed);
    (successes, timeouts)
}

fn main() {
    let args = Args::parse();

    if args.run_all {
        run_all_loads(args);
    } else {
        run_single(args);
    }
}

fn run_single(args: Args) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.workers)
        .enable_all()
        .build()
        .unwrap();

    let total_ops = args.async_tasks * args.rounds;

    println!();
    println!("  Workers: {} | Async tasks: {} | Blockers: {} (fixed) | Timeout: {}ms",
        args.workers, args.async_tasks, args.blocking_tasks, args.timeout_ms);
    println!();

    let (successes, timeouts) = rt.block_on(run_scenario(
        args.workers,
        args.async_tasks,
        args.blocking_tasks,
        Duration::from_millis(args.work_ms),
        Duration::from_millis(args.timeout_ms),
        Duration::from_millis(args.blocking_ms),
        args.rounds,
    ));

    let timeout_pct = (timeouts as f64 / total_ops as f64) * 100.0;
    println!("  Results: {}/{} succeeded, {}/{} timed out ({:.1}%)",
        successes, total_ops, timeouts, total_ops, timeout_pct);
    println!();
}

fn run_all_loads(args: Args) {
    // Load levels to test. Blocking is constant across all runs.
    // This simulates: "the codebase had blocking code, then we added
    // more async work (integrated a new library, traffic grew, etc.)"
    let load_levels: Vec<usize> = vec![
        50,       // dev/testing: light load
        100,      // staging: moderate load
        500,      // low production
        1000,     // normal production
        2000,     // traffic spike
        5000,     // high production
        10000,    // peak load
        20000,    // stress test
    ];

    let work_duration = Duration::from_millis(args.work_ms);
    let timeout_duration = Duration::from_millis(args.timeout_ms);
    let blocking_duration = Duration::from_millis(args.blocking_ms);

    println!();
    println!("=== Load Ramp: Fixed Blocking, Increasing Async Load ===");
    println!();
    println!("  Workers:        {}", args.workers);
    println!("  Blocking tasks: {} (FIXED across all runs, each blocks for {}ms)",
        args.blocking_tasks, args.blocking_ms);
    println!("  Free workers:   {}", args.workers.saturating_sub(args.blocking_tasks));
    println!("  Timeout:        {}ms", args.timeout_ms);
    println!("  Async work:     {}ms", args.work_ms);
    println!("  Rounds/task:    {}", args.rounds);
    println!();
    println!("  The blocking code does not change. Only the async task count increases.");
    println!("  This simulates integrating a new async library or growing traffic.");
    println!();
    println!("{:<15} {:>12} {:>12} {:>12} {:>10}",
        "Async Tasks", "Total Ops", "Succeeded", "Timed Out", "Failure %");
    println!("{}", "-".repeat(65));

    for &task_count in &load_levels {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(args.workers)
            .enable_all()
            .build()
            .unwrap();

        let total_ops = task_count * args.rounds;

        let (successes, timeouts) = rt.block_on(run_scenario(
            args.workers,
            task_count,
            args.blocking_tasks,
            work_duration,
            timeout_duration,
            blocking_duration,
            args.rounds,
        ));

        let timeout_pct = if total_ops > 0 {
            (timeouts as f64 / total_ops as f64) * 100.0
        } else {
            0.0
        };

        println!("{:<15} {:>12} {:>12} {:>12} {:>9.1}%",
            task_count, total_ops, successes, timeouts, timeout_pct);

        drop(rt);
    }

    println!();
    println!("If a failure threshold exists, it means the {} free worker(s) could",
        args.workers.saturating_sub(args.blocking_tasks));
    println!("no longer cycle through the task queue within the {}ms timeout.",
        args.timeout_ms);
    println!();
    println!("The blocking code succeeded in every run. It is never in the failure trace.");
    println!();
}
