// tokio-blocking-bench/src/bin/demo_mutex_convoy.rs
//
// HYPOTHESIS: Mutex contention across .await points acts as a force multiplier
// for executor starvation. When tasks share state via tokio::sync::Mutex and
// hold the lock across .await, scheduling delay inflates lock hold time, which
// serializes more tasks behind the lock, which increases scheduling delay
// further. This feedback loop causes collapse at task counts and blocker counts
// that independent-task benchmarks predict should be survivable.
//
// WHAT WE COMPARE:
//   - Independent tasks (no shared state): baseline capacity
//   - Mutex-coupled tasks (shared state held across .await): reduced capacity
//   - Both under the same blocking conditions
//
// If the hypothesis is correct, the mutex-coupled variant will collapse at
// significantly lower task counts or blocker counts than the independent variant.
//
// Usage:
//   cargo build --release
//   ./target/release/demo_mutex_convoy --run-all
//
//   # Or compare specific configurations:
//   ./target/release/demo_mutex_convoy --mode independent --async-tasks 500 --blocking-tasks 1
//   ./target/release/demo_mutex_convoy --mode contention --async-tasks 500 --blocking-tasks 1

use clap::Parser;
use hdrhistogram::Histogram;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};
use tokio::sync::{Barrier, Mutex};

#[derive(Debug, Clone, Copy)]
enum Mode {
    Independent,
    Contention,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Independent => "independent",
            Self::Contention => "contention",
        }
    }
}

#[derive(Parser, Debug, Clone)]
#[command(name = "demo_mutex_convoy")]
struct Args {
    /// Tokio worker threads.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Number of async tasks.
    #[arg(long, default_value_t = 200)]
    async_tasks: usize,

    /// Number of blocking tasks (simulating pre-existing blocking code).
    #[arg(long, default_value_t = 1)]
    blocking_tasks: usize,

    /// Duration of blocking call in milliseconds.
    #[arg(long, default_value_t = 50)]
    blocking_ms: u64,

    /// Add +/- jitter to each blocking sleep to break phase locking.
    #[arg(long, default_value_t = 10)]
    blocking_jitter_ms: u64,

    /// Timeout for async operations in milliseconds.
    #[arg(long, default_value_t = 200)]
    timeout_ms: u64,

    /// Duration of async work per step in milliseconds.
    #[arg(long, default_value_t = 1)]
    work_step_ms: u64,

    /// Number of rounds each async task performs.
    #[arg(long, default_value_t = 10)]
    rounds: usize,

    /// Fraction of tasks that contend on the shared mutex (0.0 - 1.0).
    /// Only applies in "contention" mode.
    #[arg(long, default_value_t = 0.3)]
    contention_fraction: f64,

    /// Number of .await points while holding the mutex.
    /// Higher values = longer lock hold under starvation.
    #[arg(long, default_value_t = 3)]
    awaits_under_lock: usize,

    /// Task mode: "independent", "contention", or "run-all" for comparison.
    #[arg(long, default_value = "run-all")]
    mode: String,

    /// Run the full comparison matrix.
    #[arg(long, default_value_t = false)]
    run_all: bool,

    /// Number of times to repeat each scenario and report medians.
    #[arg(long, default_value_t = 3)]
    runs: usize,
}

/// Shared state that tasks contend on.
/// In a real application, this might be a cache, a connection registry,
/// a configuration store, or any shared mutable state.
struct SharedState {
    data: Vec<u64>,
    update_count: u64,
}

/// Independent task: no shared state, no mutex.
/// This is the baseline. Each task does its own work without coupling.
async fn independent_task(
    work_step: Duration,
    timeout: Duration,
    rounds: usize,
    success_count: Arc<AtomicUsize>,
    timeout_count: Arc<AtomicUsize>,
) {
    for _ in 0..rounds {
        let result = tokio::time::timeout(timeout, async {
            // Simulate multi-step async work (DB query, API call, cache write).
            // Each step is an .await point where scheduling delay can accumulate.
            for _ in 0..5 {
                tokio::time::sleep(work_step).await;
            }
            42u64
        })
        .await;

        match result {
            Ok(_) => { success_count.fetch_add(1, Ordering::Relaxed); }
            Err(_) => { timeout_count.fetch_add(1, Ordering::Relaxed); }
        }
    }
}

/// Mutex-coupled task: acquires a shared mutex and holds it across .await points.
/// This models the real-world pattern of shared caches, registries, or state
/// that is read-modify-written with async operations in the critical section.
///
/// When scheduling delay increases (due to blocking), the lock hold time
/// increases proportionally, because the task is parked with the lock held
/// while waiting for a worker to poll it past each .await.
async fn contention_task(
    task_id: usize,
    contender_tasks: usize,
    work_step: Duration,
    timeout: Duration,
    rounds: usize,
    awaits_under_lock: usize,
    shared: Arc<Mutex<SharedState>>,
    success_count: Arc<AtomicUsize>,
    timeout_count: Arc<AtomicUsize>,
    lock_wait_us: Arc<AtomicUsize>,
    lock_acquisitions: Arc<AtomicUsize>,
    lock_wait_hist: Arc<StdMutex<Histogram<u64>>>,
    max_lock_wait_us: Arc<AtomicUsize>,
) {
    // Determine if this task is one that contends on the mutex.
    // Use deterministic assignment based on task_id for reproducibility.
    let is_contender = task_id < contender_tasks;

    for _ in 0..rounds {
        let result = tokio::time::timeout(timeout, async {
            if is_contender {
                // Acquire the mutex. Under starvation, other holders are parked
                // with the lock held, so this .await may take much longer than
                // the actual critical section.
                let lock_start = Instant::now();
                let mut state = shared.lock().await;
                let wait_us = lock_start.elapsed().as_micros() as usize;
                lock_wait_us.fetch_add(wait_us, Ordering::Relaxed);
                lock_acquisitions.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut hist) = lock_wait_hist.lock() {
                    let _ = hist.record(wait_us as u64);
                }
                let mut current_max = max_lock_wait_us.load(Ordering::Relaxed);
                while wait_us > current_max {
                    match max_lock_wait_us.compare_exchange(
                        current_max,
                        wait_us,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(observed) => current_max = observed,
                    }
                }

                // Critical section with .await points.
                // This is the key pattern: holding the mutex across .await.
                //
                // We use yield_now rather than timed sleeps here because the
                // healthy case should be cheap: the lock is only held across
                // cooperative rescheduling points, not across built-in timer
                // delays. Under starvation, the time between yield and re-poll
                // stretches, and the lock is held for that entire gap.
                for _ in 0..awaits_under_lock {
                    state.update_count += 1;
                    state.data.push(task_id as u64);
                    if state.data.len() > 1000 {
                        state.data.truncate(500);
                    }
                    tokio::task::yield_now().await;
                }

                drop(state); // explicitly release the lock

                // Do remaining work without the lock.
                for _ in 0..(5 - awaits_under_lock.min(5)) {
                    tokio::time::sleep(work_step).await;
                }
            } else {
                // Non-contending task: same work pattern as independent.
                for _ in 0..5 {
                    tokio::time::sleep(work_step).await;
                }
            }

            42u64
        })
        .await;

        match result {
            Ok(_) => { success_count.fetch_add(1, Ordering::Relaxed); }
            Err(_) => { timeout_count.fetch_add(1, Ordering::Relaxed); }
        }
    }
}

struct ScenarioResult {
    mode: String,
    async_tasks: usize,
    blocking_tasks: usize,
    total_ops: usize,
    successes: usize,
    timeouts: usize,
    failure_pct: f64,
    avg_lock_wait_us: f64,
    p95_lock_wait_us: u64,
    max_lock_wait_us: u64,
    total_duration: Duration,
}

fn median_usize(mut values: Vec<usize>) -> usize {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn median_u64(mut values: Vec<u64>) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn median_f64(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn summarize_results(results: &[ScenarioResult]) -> ScenarioResult {
    let first = results.first().expect("results must not be empty");
    ScenarioResult {
        mode: first.mode.clone(),
        async_tasks: first.async_tasks,
        blocking_tasks: first.blocking_tasks,
        total_ops: first.total_ops,
        successes: median_usize(results.iter().map(|r| r.successes).collect()),
        timeouts: median_usize(results.iter().map(|r| r.timeouts).collect()),
        failure_pct: median_f64(results.iter().map(|r| r.failure_pct).collect()),
        avg_lock_wait_us: median_f64(results.iter().map(|r| r.avg_lock_wait_us).collect()),
        p95_lock_wait_us: median_u64(results.iter().map(|r| r.p95_lock_wait_us).collect()),
        max_lock_wait_us: median_u64(results.iter().map(|r| r.max_lock_wait_us).collect()),
        total_duration: Duration::from_millis(median_u64(
            results
                .iter()
                .map(|r| r.total_duration.as_millis() as u64)
                .collect(),
        )),
    }
}

fn blocker_sleep_duration(
    base_ms: u64,
    jitter_ms: u64,
    blocker_id: usize,
    round: usize,
) -> Duration {
    if jitter_ms == 0 {
        return Duration::from_millis(base_ms);
    }

    // Small deterministic jitter avoids phase-locking without adding RNG deps.
    let span = (jitter_ms * 2) + 1;
    let seed = (blocker_id as u64)
        .wrapping_mul(6364136223846793005)
        .wrapping_add((round as u64).wrapping_mul(1442695040888963407));
    let offset = (seed % span) as i64 - jitter_ms as i64;
    let adjusted = (base_ms as i64 + offset).max(0) as u64;
    Duration::from_millis(adjusted)
}

async fn run_scenario(
    mode: Mode,
    async_tasks: usize,
    blocking_tasks: usize,
    blocking_ms: u64,
    blocking_jitter_ms: u64,
    work_step: Duration,
    timeout: Duration,
    rounds: usize,
    contention_fraction: f64,
    awaits_under_lock: usize,
) -> ScenarioResult {
    let total_tasks = async_tasks + blocking_tasks;
    let barrier = Arc::new(Barrier::new(total_tasks + 1));
    let success_count = Arc::new(AtomicUsize::new(0));
    let timeout_count = Arc::new(AtomicUsize::new(0));
    let lock_wait_us = Arc::new(AtomicUsize::new(0));
    let lock_acquisitions = Arc::new(AtomicUsize::new(0));
    let max_lock_wait_us = Arc::new(AtomicUsize::new(0));
    let lock_wait_hist = Arc::new(StdMutex::new(
        Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
    ));
    let contender_tasks = ((async_tasks as f64) * contention_fraction)
        .ceil()
        .clamp(0.0, async_tasks as f64) as usize;

    let shared = Arc::new(Mutex::new(SharedState {
        data: Vec::with_capacity(1024),
        update_count: 0,
    }));

    let mut handles = Vec::with_capacity(total_tasks);

    let start = Instant::now();

    // Spawn async tasks.
    for i in 0..async_tasks {
        let barrier = barrier.clone();
        let sc = success_count.clone();
        let tc = timeout_count.clone();
        let lw = lock_wait_us.clone();
        let la = lock_acquisitions.clone();
        let hist = lock_wait_hist.clone();
        let max_wait = max_lock_wait_us.clone();
        let shared = shared.clone();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;

            match mode {
                Mode::Independent => {
                    independent_task(work_step, timeout, rounds, sc, tc).await;
                }
                Mode::Contention => {
                    contention_task(
                        i,
                        contender_tasks,
                        work_step,
                        timeout,
                        rounds,
                        awaits_under_lock,
                        shared,
                        sc,
                        tc,
                        lw,
                        la,
                        hist,
                        max_wait,
                    ).await;
                }
            }
        }));
    }

    // Spawn blocking tasks (the pre-existing blocking code).
    for blocker_id in 0..blocking_tasks {
        let barrier = barrier.clone();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;

            for round in 0..rounds {
                std::thread::sleep(blocker_sleep_duration(
                    blocking_ms,
                    blocking_jitter_ms,
                    blocker_id,
                    round,
                ));
                tokio::task::yield_now().await;
            }
        }));
    }

    barrier.wait().await;

    for handle in handles {
        handle.await.unwrap();
    }

    let total_duration = start.elapsed();
    let total_ops = async_tasks * rounds;
    let successes = success_count.load(Ordering::Relaxed);
    let timeouts = timeout_count.load(Ordering::Relaxed);
    let total_lock_wait = lock_wait_us.load(Ordering::Relaxed) as f64;
    let total_acquisitions = lock_acquisitions.load(Ordering::Relaxed) as f64;
    let max_lock_wait = max_lock_wait_us.load(Ordering::Relaxed) as u64;
    let p95_lock_wait = lock_wait_hist
        .lock()
        .map(|hist| if hist.len() > 0 { hist.value_at_percentile(95.0) } else { 0 })
        .unwrap_or(0);
    let avg_lock_wait = if total_acquisitions > 0.0 {
        total_lock_wait / total_acquisitions
    } else {
        0.0
    };

    ScenarioResult {
        mode: mode.as_str().to_string(),
        async_tasks,
        blocking_tasks,
        total_ops,
        successes,
        timeouts,
        failure_pct: if total_ops > 0 {
            (timeouts as f64 / total_ops as f64) * 100.0
        } else {
            0.0
        },
        avg_lock_wait_us: avg_lock_wait,
        p95_lock_wait_us: p95_lock_wait,
        max_lock_wait_us: max_lock_wait,
        total_duration,
    }
}

fn parse_mode(mode: &str) -> Result<Mode, String> {
    match mode {
        "independent" => Ok(Mode::Independent),
        "contention" => Ok(Mode::Contention),
        "run-all" => Ok(Mode::Contention),
        other => Err(format!(
            "invalid mode '{other}', expected 'independent', 'contention', or 'run-all'"
        )),
    }
}

fn main() {
    let args = Args::parse();

    if args.run_all {
        run_comparison_matrix(args);
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

    let mode = parse_mode(&args.mode).unwrap_or_else(|msg| panic!("{msg}"));

    let run_count = args.runs.max(1);
    let mut results = Vec::with_capacity(run_count);
    for _ in 0..run_count {
        results.push(rt.block_on(run_scenario(
            mode,
            args.async_tasks,
            args.blocking_tasks,
            args.blocking_ms,
            args.blocking_jitter_ms,
            Duration::from_millis(args.work_step_ms),
            Duration::from_millis(args.timeout_ms),
            args.rounds,
            args.contention_fraction,
            args.awaits_under_lock,
        )));
    }

    let summary = summarize_results(&results);

    println!();
    println!("  Mode: {} | Tasks: {} | Blockers: {} | Timeout: {}ms | Runs: {}",
        summary.mode, summary.async_tasks, summary.blocking_tasks, args.timeout_ms, run_count);
    println!("  Median succeeded: {}/{} | Median timed out: {}/{} ({:.1}%)",
        summary.successes, summary.total_ops, summary.timeouts, summary.total_ops, summary.failure_pct);
    if summary.avg_lock_wait_us > 0.0 {
        println!("  Median avg lock acquisition wait: {:.0}μs", summary.avg_lock_wait_us);
        println!("  Median p95 lock acquisition wait: {}μs", summary.p95_lock_wait_us);
        println!("  Median max lock acquisition wait: {}μs", summary.max_lock_wait_us);
    }
    println!("  Median total duration: {:?}", summary.total_duration);
    println!();
}

fn run_comparison_matrix(args: Args) {
    let work_step = Duration::from_millis(args.work_step_ms);
    let timeout = Duration::from_millis(args.timeout_ms);

    // Test matrix: compare independent vs contention at each blocker count.
    // The hypothesis predicts that the contention variant fails at lower
    // blocker counts than the independent variant.
    let blocker_counts: Vec<usize> = vec![0, 1, 2, 3];
    let task_counts: Vec<usize> = vec![100, 200, 500];

    println!();
    println!("=== Mutex Convoy Hypothesis: Independent vs Contention ===");
    println!();
    println!("  Workers:              {}", args.workers);
    println!("  Blocking duration:    {}ms per blocking call", args.blocking_ms);
    println!("  Blocking jitter:      +/- {}ms", args.blocking_jitter_ms);
    println!("  Timeout:              {}ms", args.timeout_ms);
    println!("  Work step:            {}ms per .await", args.work_step_ms);
    println!("  Rounds per task:      {}", args.rounds);
    println!("  Runs per scenario:    {}", args.runs.max(1));
    println!("  Contention fraction:  {:.0}% of tasks hold the shared mutex",
        args.contention_fraction * 100.0);
    println!("  .awaits under lock:   {}", args.awaits_under_lock);
    println!();
    println!("  INDEPENDENT: tasks share no state. Each task works alone.");
    println!("  CONTENTION:  {:.0}% of tasks acquire a shared tokio::sync::Mutex",
        args.contention_fraction * 100.0);
    println!("               and hold it across {} .await points.", args.awaits_under_lock);
    println!();
    println!("{:<14} {:>6} {:>8} {:>10} {:>10} {:>10} {:>12} {:>12}",
        "Mode", "Tasks", "Block", "Total Ops", "Timed Out", "Fail %", "Avg Lock(μs)", "P95 Lock(μs)");
    println!("{}", "-".repeat(92));

    for &task_count in &task_counts {
        for &blockers in &blocker_counts {
            for mode in [Mode::Independent, Mode::Contention] {
                let mut results = Vec::with_capacity(args.runs.max(1));
                for _ in 0..args.runs.max(1) {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(args.workers)
                        .enable_all()
                        .build()
                        .unwrap();

                    let result = rt.block_on(run_scenario(
                        mode,
                        task_count,
                        blockers,
                        args.blocking_ms,
                        args.blocking_jitter_ms,
                        work_step,
                        timeout,
                        args.rounds,
                        args.contention_fraction,
                        args.awaits_under_lock,
                    ));
                    results.push(result);
                    drop(rt);
                }

                let result = summarize_results(&results);

                let lock_str = if result.avg_lock_wait_us > 0.0 {
                    format!("{:.0}", result.avg_lock_wait_us)
                } else {
                    "-".to_string()
                };
                let p95_str = if result.p95_lock_wait_us > 0 {
                    result.p95_lock_wait_us.to_string()
                } else {
                    "-".to_string()
                };

                println!("{:<14} {:>6} {:>8} {:>10} {:>10} {:>9.1}% {:>12} {:>12}",
                    result.mode,
                    result.async_tasks,
                    result.blocking_tasks,
                    result.total_ops,
                    result.timeouts,
                    result.failure_pct,
                    lock_str,
                    p95_str,
                );
            }
        }
        println!("{}", "-".repeat(92));
    }

    println!();
    println!("INTERPRETATION:");
    println!("  If the hypothesis holds, the 'contention' rows will show higher failure");
    println!("  rates than the corresponding 'independent' rows at the same task/blocker");
    println!("  count. The 'Avg Lock(μs)' column shows how long tasks wait to acquire");
    println!("  the mutex. Under starvation, this value should increase dramatically,");
    println!("  indicating that scheduling delay is inflating lock hold times.");
    println!();
    println!("  The critical comparison: if 'independent' at N blockers shows 0% failure");
    println!("  but 'contention' at N blockers shows >0% failure, mutex contention is");
    println!("  acting as a force multiplier for starvation.");
    println!();
}
