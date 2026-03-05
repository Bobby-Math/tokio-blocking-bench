// tokio-blocking-bench/src/main.rs
//
// Reproduction benchmark: Blocking in async Rust is invisible at low load,
// catastrophic at high load.
//
// Thesis: When synchronous (blocking) code runs inside an async task, it
// monopolizes a Tokio worker thread. At low concurrency, spare workers absorb
// the damage. At high concurrency, every blocked worker is a stalled polling
// loop, and async task latency explodes.
//
// What we measure:
//   - "Async I/O" tasks call tokio::time::sleep(10ms) and record how long
//     they ACTUALLY waited. In a healthy runtime, actual ~ 10ms. Under
//     worker starvation, actual >> 10ms because the timer-fired Waker cannot
//     be serviced until a worker becomes free to poll it.
//
//   - "Blocking" tasks simulate accidental blocking (sync DB call, DNS
//     lookup, CPU-heavy parsing) by calling std::thread::sleep inside an
//     async context. This is the anti-pattern under test.

use clap::Parser;
use hdrhistogram::Histogram;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Barrier;

/// Benchmark: async task latency under varying blocking load.
#[derive(Parser, Debug, Clone)]
#[command(name = "tokio-blocking-bench")]
struct Args {
    /// Number of Tokio worker threads (0 = use num_cpus default).
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Number of async I/O tasks (the "good" tasks we measure).
    #[arg(long, default_value_t = 200)]
    async_tasks: usize,

    /// Number of blocking tasks (the "bad" tasks that monopolize workers).
    #[arg(long, default_value_t = 0)]
    blocking_tasks: usize,

    /// Duration of each async sleep in milliseconds (simulated I/O latency).
    #[arg(long, default_value_t = 10)]
    async_sleep_ms: u64,

    /// Duration of each blocking sleep in milliseconds (simulated sync call).
    #[arg(long, default_value_t = 50)]
    blocking_sleep_ms: u64,

    /// Number of iterations each async task performs (repeated sleeps).
    #[arg(long, default_value_t = 10)]
    iterations: usize,

    /// Number of iterations each blocking task performs.
    #[arg(long, default_value_t = 5)]
    blocking_iterations: usize,

    /// Run all predefined scenarios instead of a single custom run.
    #[arg(long, default_value_t = false)]
    run_all: bool,

    /// Number of times to repeat the full scenario matrix (for --run-all).
    #[arg(long, default_value_t = 1)]
    runs: usize,
}

/// Result of a single benchmark scenario.
#[derive(Debug)]
struct ScenarioResult {
    label: String,
    workers: usize,
    async_tasks: usize,
    blocking_tasks: usize,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    mean_us: f64,
    total_duration: Duration,
}

/// Runs one benchmark scenario and returns latency statistics.
async fn run_scenario(
    workers: usize,
    async_tasks: usize,
    blocking_tasks: usize,
    async_sleep: Duration,
    blocking_sleep: Duration,
    iterations: usize,
    blocking_iterations: usize,
) -> ScenarioResult {
    // Histogram: track microsecond-level latencies, max 60 seconds, 3 sig figs.
    let hist = Arc::new(tokio::sync::Mutex::new(
        Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
    ));

    let total_tasks = async_tasks + blocking_tasks;

    // Barrier ensures all tasks start at the same instant.
    // +1 for the coordinator (the current task).
    let barrier = Arc::new(Barrier::new(total_tasks + 1));

    let start = Instant::now();

    // --- Spawn async I/O tasks (the ones we measure) ---
    let mut handles = Vec::with_capacity(total_tasks);

    for _ in 0..async_tasks {
        let barrier = barrier.clone();
        let hist = hist.clone();
        let sleep_dur = async_sleep;

        handles.push(tokio::spawn(async move {
            barrier.wait().await;

            for _ in 0..iterations {
                let t0 = Instant::now();
                tokio::time::sleep(sleep_dur).await;
                let elapsed_us = t0.elapsed().as_micros() as u64;

                // Record the OVERHEAD: actual - expected.
                // In a healthy runtime this is near zero.
                // Under starvation this is large.
                let expected_us = sleep_dur.as_micros() as u64;
                let overhead_us = elapsed_us.saturating_sub(expected_us);

                hist.lock().await.record(overhead_us).ok();
            }
        }));
    }

    // --- Spawn blocking tasks (the bad pattern) ---
    for _ in 0..blocking_tasks {
        let barrier = barrier.clone();
        let block_dur = blocking_sleep;

        handles.push(tokio::spawn(async move {
            barrier.wait().await;

            for _ in 0..blocking_iterations {
                // THIS IS THE ANTI-PATTERN.
                // std::thread::sleep blocks the entire worker thread.
                // The worker's poll loop cannot service any other task's Waker
                // until this returns.
                std::thread::sleep(block_dur);

                // Yield to give the scheduler a chance between blocking calls.
                // In real code, the blocking call might be in a library you
                // don't control, so this yield may not exist.
                tokio::task::yield_now().await;
            }
        }));
    }

    // Release all tasks simultaneously.
    barrier.wait().await;

    // Wait for all tasks to complete.
    for handle in handles {
        handle.await.unwrap();
    }

    let total_duration = start.elapsed();

    let h = hist.lock().await;

    ScenarioResult {
        label: String::new(), // filled by caller
        workers,
        async_tasks,
        blocking_tasks,
        p50_us: h.value_at_percentile(50.0),
        p95_us: h.value_at_percentile(95.0),
        p99_us: h.value_at_percentile(99.0),
        max_us: h.max(),
        mean_us: h.mean(),
        total_duration,
    }
}

fn print_header() {
    println!(
        "\n{:<30} {:>7} {:>7} {:>7} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Scenario", "Workers", "Async", "Block", "p50(us)", "p95(us)", "p99(us)", "max(us)", "mean(us)", "total(ms)"
    );
    println!("{}", "-".repeat(131));
}

fn print_result(r: &ScenarioResult) {
    println!(
        "{:<30} {:>7} {:>7} {:>7} {:>10} {:>10} {:>10} {:>10} {:>10.0} {:>10}",
        r.label,
        r.workers,
        r.async_tasks,
        r.blocking_tasks,
        r.p50_us,
        r.p95_us,
        r.p99_us,
        r.max_us,
        r.mean_us,
        r.total_duration.as_millis()
    );
}

fn main() {
    let args = Args::parse();

    if args.run_all {
        run_all_scenarios(args);
    } else {
        run_single(args);
    }
}

fn run_single(args: Args) {
    let workers = if args.workers == 0 {
        num_cpus()
    } else {
        args.workers
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();

    let result = rt.block_on(run_scenario(
        workers,
        args.async_tasks,
        args.blocking_tasks,
        Duration::from_millis(args.async_sleep_ms),
        Duration::from_millis(args.blocking_sleep_ms),
        args.iterations,
        args.blocking_iterations,
    ));

    let result = ScenarioResult {
        label: format!("custom(a={},b={})", args.async_tasks, args.blocking_tasks),
        ..result
    };

    print_header();
    print_result(&result);
    println!();
}

fn run_all_scenarios(args: Args) {
    let workers = if args.workers == 0 {
        num_cpus()
    } else {
        args.workers
    };

    let async_sleep = Duration::from_millis(args.async_sleep_ms);
    let blocking_sleep = Duration::from_millis(args.blocking_sleep_ms);

    // Scenario matrix: clean 0-through-N blocker progression.
    //
    // The key variable is: blocking_tasks / workers.
    // Below 1.0 = spare capacity absorbs the damage.
    // At 1.0    = total starvation, latency cliff.
    //
    // We test at two async load levels to show that the cliff is
    // load-dependent: same blocking ratio, different visibility.
    //
    // Deliberately omitted: over-blocked scenarios (blockers > workers).
    // The interaction between queued blocking tasks introduces confounding
    // dynamics that obscure the core finding.
    let scenarios: Vec<(&str, usize, usize)> = vec![
        // --- Baseline: no blocking, establishes floor ---
        ("baseline/no-block",         500,   0),

        // --- Low async load: blocking is invisible ---
        ("low-async/0-blockers",      50,   0),
        ("low-async/1-blocker",       50,   1),
        ("low-async/2-blockers",      50,   2),
        ("low-async/3-blockers",      50,   3),
        ("low-async/4-blockers",      50,   4),

        // --- High async load: blocking becomes catastrophic ---
        ("high-async/0-blockers",    500,   0),
        ("high-async/1-blocker",     500,   1),
        ("high-async/2-blockers",    500,   2),
        ("high-async/3-blockers",    500,   3),
        ("high-async/4-blockers",    500,   4),
    ];

    let runs = args.runs;

    for run in 1..=runs {
        if runs > 1 {
            println!("\n=== Run {}/{} ===", run, runs);
        }

        print_header();

        for (label, async_tasks, blocking_tasks) in &scenarios {
            // Each scenario gets a fresh runtime to avoid cross-contamination.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .unwrap();

            let result = rt.block_on(run_scenario(
                workers,
                *async_tasks,
                *blocking_tasks,
                async_sleep,
                blocking_sleep,
                args.iterations,
                args.blocking_iterations,
            ));

            let result = ScenarioResult {
                label: label.to_string(),
                ..result
            };

            print_result(&result);

            // Drop the runtime explicitly before the next scenario.
            drop(rt);
        }
    }

    println!();
    println!("Config: workers={}, async_sleep={}ms, blocking_sleep={}ms, iterations={}, blocking_iterations={}, runs={}",
        workers, args.async_sleep_ms, args.blocking_sleep_ms, args.iterations, args.blocking_iterations, runs);
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
