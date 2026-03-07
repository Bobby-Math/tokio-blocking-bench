// tokio-blocking-bench/src/bin/demo_per_request.rs
//
// DEMONSTRATION: Blocking code in the request path, scaling with traffic.
//
// This is the most realistic reproduction. The blocking call happens inside
// the request handler (e.g., a sync config fetch, a DNS lookup, a file read).
// At low request rates, blocking calls rarely overlap. At high request rates,
// multiple blocking calls land on different workers simultaneously, starving
// the pool.
//
// Usage:
//   cargo build --release
//   ./target/release/demo_per_request --run-all
//
// What to watch: as concurrent requests increase, the probability of
// overlapping blocking calls increases until workers are saturated.

use clap::Parser;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

#[derive(Parser, Debug)]
#[command(name = "demo_per_request")]
struct Args {
    /// Tokio worker threads.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Number of concurrent request-handling tasks.
    #[arg(long, default_value_t = 50)]
    concurrent_requests: usize,

    /// Timeout for the async portion of request handling.
    #[arg(long, default_value_t = 100)]
    timeout_ms: u64,

    /// Duration of the async work in each request (ms).
    #[arg(long, default_value_t = 10)]
    async_work_ms: u64,

    /// Duration of the blocking call in each request (ms).
    /// This is the sync config fetch / DNS lookup / file read
    /// that exists inside the request handler.
    #[arg(long, default_value_t = 50)]
    blocking_call_ms: u64,

    /// Probability (0.0 - 1.0) that a given request hits the blocking path.
    /// In real code, maybe 1 in 5 requests needs a config refresh,
    /// or every request does a sync DNS lookup.
    #[arg(long, default_value_t = 0.3)]
    blocking_probability: f64,

    /// Number of sequential requests each task handles.
    #[arg(long, default_value_t = 20)]
    requests_per_task: usize,

    /// Run all predefined traffic levels.
    #[arg(long, default_value_t = false)]
    run_all: bool,
}

/// Simulates a request handler where SOME requests hit a blocking code path.
///
/// This is the realistic pattern:
///   1. Receive request
///   2. Maybe fetch config / resolve DNS / read cache (BLOCKING)
///   3. Do async work (database query, API call)
///   4. Return response
///
/// The blocking call is correct, fast, and succeeds every time.
/// It just happens to block the OS thread when it runs.
async fn handle_request(
    request_id: usize,
    async_work: Duration,
    blocking_call: Duration,
    timeout: Duration,
    blocking_probability: f64,
    success_count: Arc<AtomicUsize>,
    timeout_count: Arc<AtomicUsize>,
    blocking_hit_count: Arc<AtomicUsize>,
) {
    // Step 1: Determine if this request hits the blocking path.
    // Use a simple deterministic pattern based on request_id to be reproducible.
    // In real code, this might be: "if config cache is expired, fetch sync"
    // or "every request does a sync DNS lookup before connecting."
    let hits_blocking_path =
        (request_id as f64 * 0.618033988) % 1.0 < blocking_probability;

    if hits_blocking_path {
        // THE BLOCKING CALL.
        // In real code:
        //   let config = std::fs::read("/etc/app/config.toml")?;
        //   let addr = dns_lookup::lookup_host("db.internal")?;
        //   let bytes = reqwest::blocking::get(config_url)?.bytes()?;
        std::thread::sleep(blocking_call);
        blocking_hit_count.fetch_add(1, Ordering::Relaxed);
    }

    // Step 2: Do the async work (the part with a timeout).
    // Multiple sequential awaits - each is a scheduling point where delay accumulates.
    let step_duration = async_work / 10;
    let result = tokio::time::timeout(timeout, async {
        for _ in 0..10 {
            tokio::time::sleep(step_duration).await;
        }
        42
    })
    .await;

    match result {
        Ok(_) => {
            success_count.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            timeout_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn run_scenario(
    concurrent_requests: usize,
    requests_per_task: usize,
    async_work: Duration,
    blocking_call: Duration,
    timeout: Duration,
    blocking_probability: f64,
) -> (usize, usize, usize) {
    let total_tasks = concurrent_requests;
    let barrier = Arc::new(Barrier::new(total_tasks + 1));
    let success_count = Arc::new(AtomicUsize::new(0));
    let timeout_count = Arc::new(AtomicUsize::new(0));
    let blocking_hit_count = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::with_capacity(total_tasks);

    for task_id in 0..concurrent_requests {
        let barrier = barrier.clone();
        let sc = success_count.clone();
        let tc = timeout_count.clone();
        let bhc = blocking_hit_count.clone();

        handles.push(tokio::spawn(async move {
            barrier.wait().await;

            for round in 0..requests_per_task {
                let request_id = task_id * requests_per_task + round;
                handle_request(
                    request_id,
                    async_work,
                    blocking_call,
                    timeout,
                    blocking_probability,
                    sc.clone(),
                    tc.clone(),
                    bhc.clone(),
                )
                .await;
            }
        }));
    }

    barrier.wait().await;

    for handle in handles {
        handle.await.unwrap();
    }

    (
        success_count.load(Ordering::Relaxed),
        timeout_count.load(Ordering::Relaxed),
        blocking_hit_count.load(Ordering::Relaxed),
    )
}

fn main() {
    let args = Args::parse();

    if args.run_all {
        run_all_levels(args);
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

    let total_ops = args.concurrent_requests * args.requests_per_task;

    println!();
    println!("  Workers: {} | Concurrent requests: {} | Requests/task: {}",
        args.workers, args.concurrent_requests, args.requests_per_task);
    println!("  Blocking probability: {:.0}% | Blocking duration: {}ms",
        args.blocking_probability * 100.0, args.blocking_call_ms);
    println!("  Timeout: {}ms | Async work: {}ms", args.timeout_ms, args.async_work_ms);
    println!();

    let (successes, timeouts, blocking_hits) = rt.block_on(run_scenario(
        args.concurrent_requests,
        args.requests_per_task,
        Duration::from_millis(args.async_work_ms),
        Duration::from_millis(args.blocking_call_ms),
        Duration::from_millis(args.timeout_ms),
        args.blocking_probability,
    ));

    let timeout_pct = (timeouts as f64 / total_ops as f64) * 100.0;
    println!("  Total operations:  {}", total_ops);
    println!("  Blocking calls:    {} ({:.0}% of operations)",
        blocking_hits, (blocking_hits as f64 / total_ops as f64) * 100.0);
    println!("  Succeeded:         {}", successes);
    println!("  Timed out:         {} ({:.1}%)", timeouts, timeout_pct);
    println!();
}

fn run_all_levels(args: Args) {
    let traffic_levels: Vec<usize> = vec![
        10,       // minimal: dev testing
        15,       // light: slightly above dev
        20,       // moderate: basic load test
        25,       // growing: early staging
        30,       // busy: staging
        40,       // heavy: moderate production
        50,       // spike: normal production
    ];

    let async_work = Duration::from_millis(args.async_work_ms);
    let blocking_call = Duration::from_millis(args.blocking_call_ms);
    let timeout = Duration::from_millis(args.timeout_ms);

    println!();
    println!("=== Per-Request Blocking: Traffic Ramp ===");
    println!();
    println!("  Workers:              {}", args.workers);
    println!("  Blocking probability: {:.0}% of requests hit the sync code path",
        args.blocking_probability * 100.0);
    println!("  Blocking duration:    {}ms per blocking call", args.blocking_call_ms);
    println!("  Async work:           {}ms per request", args.async_work_ms);
    println!("  Timeout:              {}ms", args.timeout_ms);
    println!("  Requests per task:    {}", args.requests_per_task);
    println!();
    println!("  Every request handler is async. {:.0}% of requests call a sync function",
        args.blocking_probability * 100.0);
    println!("  (config fetch, DNS lookup, file read) that blocks the worker for {}ms.",
        args.blocking_call_ms);
    println!("  As concurrent requests increase, blocking calls overlap more frequently,");
    println!("  saturating the worker pool.");
    println!();
    println!("{:<15} {:>12} {:>12} {:>12} {:>12} {:>10}",
        "Concurrent", "Total Ops", "Block Calls", "Succeeded", "Timed Out", "Failure %");
    println!("{}", "-".repeat(75));

    for &concurrency in &traffic_levels {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(args.workers)
            .enable_all()
            .build()
            .unwrap();

        let total_ops = concurrency * args.requests_per_task;

        let (successes, timeouts, blocking_hits) = rt.block_on(run_scenario(
            concurrency,
            args.requests_per_task,
            async_work,
            blocking_call,
            timeout,
            args.blocking_probability,
        ));

        let timeout_pct = if total_ops > 0 {
            (timeouts as f64 / total_ops as f64) * 100.0
        } else {
            0.0
        };

        println!("{:<15} {:>12} {:>12} {:>12} {:>12} {:>9.1}%",
            concurrency, total_ops, blocking_hits, successes, timeouts, timeout_pct);

        drop(rt);
    }

    println!();
    println!("The blocking code path is identical at every traffic level.");
    println!("It succeeds every time. It is never in the failure trace.");
    println!("The only thing that changed is how many requests arrived concurrently.");
    println!();
}
