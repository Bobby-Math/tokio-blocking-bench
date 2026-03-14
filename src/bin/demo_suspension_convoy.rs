// tokio-blocking-bench/src/bin/demo_suspension_convoy.rs
//
// HYPOTHESIS: A coordinator task that suspends while holding shared state can
// collapse effective capacity even without classic blocked threads.
//
// WHAT WE COMPARE:
//   - Good variant: lock shared state, derive owned events, drop lock, then emit
//   - Bad variant: lock shared state, derive events, emit them while still locked
//
// The bad variant converts downstream scheduling and backpressure into lock hold
// time. Other tasks that need the same state wait longer, and the system stays
// busy while forward progress slows sharply.
//
// Usage:
//   cargo build --release --bin demo_suspension_convoy
//   ./target/release/demo_suspension_convoy --run-all
//
//   # Or run a single variant:
//   ./target/release/demo_suspension_convoy --mode bad
//   ./target/release/demo_suspension_convoy --mode good

use clap::Parser;
use hdrhistogram::Histogram;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Barrier, Mutex};
use tokio::task::JoinSet;

#[derive(Debug, Clone, Copy)]
enum Mode {
    Good,
    Bad,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Bad => "bad",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum InputKind {
    Snapshot,
    Delta,
    Metadata,
}

#[derive(Debug, Clone)]
struct InputMessage {
    kind: InputKind,
    seq: usize,
    created_at: Instant,
}

#[derive(Debug, Clone)]
struct OutputEvent {
    created_at: Instant,
}

#[derive(Debug)]
struct SharedState {
    entities: HashMap<u64, u64>,
    version: u64,
}

#[derive(Default)]
struct Metrics {
    input_messages: AtomicUsize,
    output_events: AtomicUsize,
    late_events: AtomicUsize,
    enrichment_runs: AtomicUsize,
    coordinator_lock_wait_us: AtomicUsize,
    coordinator_lock_holds_us: AtomicUsize,
    enrichment_lock_wait_us: AtomicUsize,
    coordinator_lock_count: AtomicUsize,
    enrichment_lock_count: AtomicUsize,
    max_coordinator_hold_us: AtomicUsize,
    max_coordinator_wait_us: AtomicUsize,
    max_event_latency_us: AtomicUsize,
    coordinator_wait_hist: StdMutex<Option<Histogram<u64>>>,
    coordinator_hold_hist: StdMutex<Option<Histogram<u64>>>,
    event_latency_hist: StdMutex<Option<Histogram<u64>>>,
    enrichment_wait_hist: StdMutex<Option<Histogram<u64>>>,
}

impl Metrics {
    fn new() -> Self {
        Self {
            coordinator_wait_hist: StdMutex::new(Some(
                Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
            )),
            coordinator_hold_hist: StdMutex::new(Some(
                Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
            )),
            event_latency_hist: StdMutex::new(Some(
                Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
            )),
            enrichment_wait_hist: StdMutex::new(Some(
                Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
            )),
            ..Self::default()
        }
    }
}

#[derive(Parser, Debug, Clone)]
#[command(name = "demo_suspension_convoy")]
struct Args {
    /// Tokio worker threads.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Number of messages sent by each input stream.
    #[arg(long, default_value_t = 60)]
    messages_per_stream: usize,

    /// Interval for snapshot messages in milliseconds.
    #[arg(long, default_value_t = 8)]
    snapshot_every_ms: u64,

    /// Interval for delta messages in milliseconds.
    #[arg(long, default_value_t = 4)]
    delta_every_ms: u64,

    /// Interval for metadata messages in milliseconds.
    #[arg(long, default_value_t = 10)]
    metadata_every_ms: u64,

    /// Downstream event channel capacity. Small values create backpressure.
    #[arg(long, default_value_t = 1)]
    event_channel_capacity: usize,

    /// Simulated downstream consumer work per event.
    #[arg(long, default_value_t = 1)]
    consumer_ms: u64,

    /// Event latency budget in milliseconds.
    #[arg(long, default_value_t = 200)]
    latency_budget_ms: u64,

    /// Spawn one enrichment task every N coordinator messages. Set to 0 to disable.
    #[arg(long, default_value_t = 5)]
    enrichment_every: usize,

    /// Delay before enrichment re-enters shared state.
    #[arg(long, default_value_t = 5)]
    enrichment_ms: u64,

    /// Number of unrelated blocking tasks to simulate external worker pressure.
    #[arg(long, default_value_t = 0)]
    blocking_tasks: usize,

    /// Duration of each blocking sleep in milliseconds.
    #[arg(long, default_value_t = 50)]
    blocking_ms: u64,

    /// Add +/- jitter to blocking sleeps to avoid phase locking.
    #[arg(long, default_value_t = 10)]
    blocking_jitter_ms: u64,

    /// Benchmark mode: good, bad, or run-all.
    #[arg(long, default_value = "run-all")]
    mode: String,

    /// Run the comparison matrix.
    #[arg(long, default_value_t = false)]
    run_all: bool,

    /// Number of times to repeat each scenario and report medians.
    #[arg(long, default_value_t = 3)]
    runs: usize,
}

#[derive(Debug, Clone)]
struct ScenarioResult {
    mode: String,
    blocking_tasks: usize,
    total_inputs: usize,
    output_events: usize,
    late_events: usize,
    late_pct: f64,
    avg_coordinator_wait_us: f64,
    p95_coordinator_wait_us: u64,
    avg_coordinator_hold_us: f64,
    p95_coordinator_hold_us: u64,
    avg_enrichment_wait_us: f64,
    p95_enrichment_wait_us: u64,
    p95_event_latency_us: u64,
    max_event_latency_us: u64,
    total_duration: Duration,
}

fn parse_mode(mode: &str) -> Result<Mode, String> {
    match mode {
        "good" => Ok(Mode::Good),
        "bad" => Ok(Mode::Bad),
        "run-all" => Ok(Mode::Bad),
        other => Err(format!(
            "invalid mode '{other}', expected 'good', 'bad', or 'run-all'"
        )),
    }
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
        blocking_tasks: first.blocking_tasks,
        total_inputs: first.total_inputs,
        output_events: median_usize(results.iter().map(|r| r.output_events).collect()),
        late_events: median_usize(results.iter().map(|r| r.late_events).collect()),
        late_pct: median_f64(results.iter().map(|r| r.late_pct).collect()),
        avg_coordinator_wait_us: median_f64(results.iter().map(|r| r.avg_coordinator_wait_us).collect()),
        p95_coordinator_wait_us: median_u64(results.iter().map(|r| r.p95_coordinator_wait_us).collect()),
        avg_coordinator_hold_us: median_f64(results.iter().map(|r| r.avg_coordinator_hold_us).collect()),
        p95_coordinator_hold_us: median_u64(results.iter().map(|r| r.p95_coordinator_hold_us).collect()),
        avg_enrichment_wait_us: median_f64(results.iter().map(|r| r.avg_enrichment_wait_us).collect()),
        p95_enrichment_wait_us: median_u64(results.iter().map(|r| r.p95_enrichment_wait_us).collect()),
        p95_event_latency_us: median_u64(results.iter().map(|r| r.p95_event_latency_us).collect()),
        max_event_latency_us: median_u64(results.iter().map(|r| r.max_event_latency_us).collect()),
        total_duration: Duration::from_millis(median_u64(
            results
                .iter()
                .map(|r| r.total_duration.as_millis() as u64)
                .collect(),
        )),
    }
}

fn blocker_sleep_duration(base_ms: u64, jitter_ms: u64, blocker_id: usize, round: usize) -> Duration {
    if jitter_ms == 0 {
        return Duration::from_millis(base_ms);
    }

    let span = (jitter_ms * 2) + 1;
    let seed = (blocker_id as u64)
        .wrapping_mul(6364136223846793005)
        .wrapping_add((round as u64).wrapping_mul(1442695040888963407));
    let offset = (seed % span) as i64 - jitter_ms as i64;
    let adjusted = (base_ms as i64 + offset).max(0) as u64;
    Duration::from_millis(adjusted)
}

fn record_hist(hist: &StdMutex<Option<Histogram<u64>>>, value: u64) {
    if let Ok(mut guard) = hist.lock() {
        if let Some(hist) = guard.as_mut() {
            let _ = hist.record(value.max(1));
        }
    }
}

fn update_max(target: &AtomicUsize, value: usize) {
    let mut current = target.load(Ordering::Relaxed);
    while value > current {
        match target.compare_exchange(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

fn build_events(message: &InputMessage, state: &mut SharedState) -> Vec<OutputEvent> {
    let entity_id = (message.seq % 32) as u64;
    let entry = state.entities.entry(entity_id).or_insert(0);
    *entry += 1;
    state.version += 1;

    let count = match message.kind {
        InputKind::Snapshot => 3,
        InputKind::Delta => 1,
        InputKind::Metadata => 1,
    };

    (0..count)
        .map(|_| OutputEvent {
            created_at: message.created_at,
        })
        .collect()
}

fn should_spawn_enrichment(args: &Args, processed_index: usize) -> bool {
    args.enrichment_every > 0 && processed_index % args.enrichment_every == 0
}

async fn spawn_enrichment_task(
    shared: Arc<Mutex<SharedState>>,
    metrics: Arc<Metrics>,
    delay: Duration,
) {
    tokio::time::sleep(delay).await;

    let wait_start = Instant::now();
    let mut state = shared.lock().await;
    let wait_us = wait_start.elapsed().as_micros() as usize;
    metrics.enrichment_lock_wait_us.fetch_add(wait_us, Ordering::Relaxed);
    metrics.enrichment_lock_count.fetch_add(1, Ordering::Relaxed);
    record_hist(&metrics.enrichment_wait_hist, wait_us as u64);

    state.version += 1;
    metrics.enrichment_runs.fetch_add(1, Ordering::Relaxed);
}

async fn process_message_bad(
    message: InputMessage,
    shared: Arc<Mutex<SharedState>>,
    event_tx: &mpsc::Sender<OutputEvent>,
    metrics: Arc<Metrics>,
    enrichment_set: &mut JoinSet<()>,
    args: &Args,
    processed_index: usize,
) {
    let wait_start = Instant::now();
    let mut state = shared.lock().await;
    let wait_us = wait_start.elapsed().as_micros() as usize;
    metrics.coordinator_lock_wait_us.fetch_add(wait_us, Ordering::Relaxed);
    metrics.coordinator_lock_count.fetch_add(1, Ordering::Relaxed);
    record_hist(&metrics.coordinator_wait_hist, wait_us as u64);
    update_max(&metrics.max_coordinator_wait_us, wait_us);

    let hold_start = Instant::now();
    let events = build_events(&message, &mut state);

    if should_spawn_enrichment(args, processed_index) {
        let shared = shared.clone();
        let metrics = metrics.clone();
        let delay = Duration::from_millis(args.enrichment_ms);
        enrichment_set.spawn(async move {
            spawn_enrichment_task(shared, metrics, delay).await;
        });
    }

    for event in events {
        if event_tx.send(event).await.is_err() {
            break;
        }
    }

    drop(state);

    let hold_us = hold_start.elapsed().as_micros() as usize;
    metrics.coordinator_lock_holds_us.fetch_add(hold_us, Ordering::Relaxed);
    record_hist(&metrics.coordinator_hold_hist, hold_us as u64);
    update_max(&metrics.max_coordinator_hold_us, hold_us);
}

async fn process_message_good(
    message: InputMessage,
    shared: Arc<Mutex<SharedState>>,
    event_tx: &mpsc::Sender<OutputEvent>,
    metrics: Arc<Metrics>,
    enrichment_set: &mut JoinSet<()>,
    args: &Args,
    processed_index: usize,
) {
    let wait_start = Instant::now();
    let events = {
        let mut state = shared.lock().await;
        let wait_us = wait_start.elapsed().as_micros() as usize;
        metrics.coordinator_lock_wait_us.fetch_add(wait_us, Ordering::Relaxed);
        metrics.coordinator_lock_count.fetch_add(1, Ordering::Relaxed);
        record_hist(&metrics.coordinator_wait_hist, wait_us as u64);
        update_max(&metrics.max_coordinator_wait_us, wait_us);

        let hold_start = Instant::now();
        let events = build_events(&message, &mut state);
        let hold_us = hold_start.elapsed().as_micros() as usize;
        metrics.coordinator_lock_holds_us.fetch_add(hold_us, Ordering::Relaxed);
        record_hist(&metrics.coordinator_hold_hist, hold_us as u64);
        update_max(&metrics.max_coordinator_hold_us, hold_us);
        events
    };

    if should_spawn_enrichment(args, processed_index) {
        let shared = shared.clone();
        let metrics = metrics.clone();
        let delay = Duration::from_millis(args.enrichment_ms);
        enrichment_set.spawn(async move {
            spawn_enrichment_task(shared, metrics, delay).await;
        });
    }

    for event in events {
        if event_tx.send(event).await.is_err() {
            break;
        }
    }
}

async fn producer_task(
    barrier: Arc<Barrier>,
    tx: mpsc::UnboundedSender<InputMessage>,
    kind: InputKind,
    messages: usize,
    interval: Duration,
) {
    barrier.wait().await;
    for seq in 0..messages {
        let _ = tx.send(InputMessage {
            kind,
            seq,
            created_at: Instant::now(),
        });
        tokio::time::sleep(interval).await;
    }
}

async fn blocking_task(
    barrier: Arc<Barrier>,
    rounds: usize,
    base_ms: u64,
    jitter_ms: u64,
    blocker_id: usize,
) {
    barrier.wait().await;
    for round in 0..rounds {
        std::thread::sleep(blocker_sleep_duration(base_ms, jitter_ms, blocker_id, round));
        tokio::task::yield_now().await;
    }
}

async fn consumer_task(
    barrier: Arc<Barrier>,
    mut rx: mpsc::Receiver<OutputEvent>,
    metrics: Arc<Metrics>,
    consumer_delay: Duration,
    latency_budget: Duration,
) {
    barrier.wait().await;
    while let Some(event) = rx.recv().await {
        let latency = event.created_at.elapsed();
        let latency_us = latency.as_micros() as usize;
        metrics.output_events.fetch_add(1, Ordering::Relaxed);
        record_hist(&metrics.event_latency_hist, latency_us as u64);
        update_max(&metrics.max_event_latency_us, latency_us);
        if latency > latency_budget {
            metrics.late_events.fetch_add(1, Ordering::Relaxed);
        }
        if !consumer_delay.is_zero() {
            tokio::time::sleep(consumer_delay).await;
        }
    }
}

async fn coordinator_task(
    barrier: Arc<Barrier>,
    mut snapshot_rx: mpsc::UnboundedReceiver<InputMessage>,
    mut delta_rx: mpsc::UnboundedReceiver<InputMessage>,
    mut metadata_rx: mpsc::UnboundedReceiver<InputMessage>,
    event_tx: mpsc::Sender<OutputEvent>,
    shared: Arc<Mutex<SharedState>>,
    metrics: Arc<Metrics>,
    mode: Mode,
    args: Args,
) {
    barrier.wait().await;

    let mut snapshot_open = true;
    let mut delta_open = true;
    let mut metadata_open = true;
    let mut processed_index = 0usize;
    let mut enrichment_set = JoinSet::new();

    loop {
        tokio::select! {
            message = snapshot_rx.recv(), if snapshot_open => {
                match message {
                    Some(message) => {
                        metrics.input_messages.fetch_add(1, Ordering::Relaxed);
                        processed_index += 1;
                        match mode {
                            Mode::Bad => process_message_bad(
                                message,
                                shared.clone(),
                                &event_tx,
                                metrics.clone(),
                                &mut enrichment_set,
                                &args,
                                processed_index,
                            ).await,
                            Mode::Good => process_message_good(
                                message,
                                shared.clone(),
                                &event_tx,
                                metrics.clone(),
                                &mut enrichment_set,
                                &args,
                                processed_index,
                            ).await,
                        }
                    }
                    None => snapshot_open = false,
                }
            }
            message = delta_rx.recv(), if delta_open => {
                match message {
                    Some(message) => {
                        metrics.input_messages.fetch_add(1, Ordering::Relaxed);
                        processed_index += 1;
                        match mode {
                            Mode::Bad => process_message_bad(
                                message,
                                shared.clone(),
                                &event_tx,
                                metrics.clone(),
                                &mut enrichment_set,
                                &args,
                                processed_index,
                            ).await,
                            Mode::Good => process_message_good(
                                message,
                                shared.clone(),
                                &event_tx,
                                metrics.clone(),
                                &mut enrichment_set,
                                &args,
                                processed_index,
                            ).await,
                        }
                    }
                    None => delta_open = false,
                }
            }
            message = metadata_rx.recv(), if metadata_open => {
                match message {
                    Some(message) => {
                        metrics.input_messages.fetch_add(1, Ordering::Relaxed);
                        processed_index += 1;
                        match mode {
                            Mode::Bad => process_message_bad(
                                message,
                                shared.clone(),
                                &event_tx,
                                metrics.clone(),
                                &mut enrichment_set,
                                &args,
                                processed_index,
                            ).await,
                            Mode::Good => process_message_good(
                                message,
                                shared.clone(),
                                &event_tx,
                                metrics.clone(),
                                &mut enrichment_set,
                                &args,
                                processed_index,
                            ).await,
                        }
                    }
                    None => metadata_open = false,
                }
            }
            else => {
                break;
            }
        }

        if !snapshot_open && !delta_open && !metadata_open {
            break;
        }
    }

    while enrichment_set.join_next().await.is_some() {}
}

async fn run_scenario(mode: Mode, args: Args, blocking_tasks: usize) -> ScenarioResult {
    let producer_count = 3usize;
    let total_tasks = producer_count + blocking_tasks + 2; // coordinator + consumer + producers + blockers
    let barrier = Arc::new(Barrier::new(total_tasks + 1));

    let metrics = Arc::new(Metrics::new());
    let shared = Arc::new(Mutex::new(SharedState {
        entities: HashMap::new(),
        version: 0,
    }));

    let (snapshot_tx, snapshot_rx) = mpsc::unbounded_channel();
    let (delta_tx, delta_rx) = mpsc::unbounded_channel();
    let (metadata_tx, metadata_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel(args.event_channel_capacity);

    let mut handles = Vec::with_capacity(total_tasks);
    let start = Instant::now();

    handles.push(tokio::spawn(producer_task(
        barrier.clone(),
        snapshot_tx,
        InputKind::Snapshot,
        args.messages_per_stream,
        Duration::from_millis(args.snapshot_every_ms),
    )));
    handles.push(tokio::spawn(producer_task(
        barrier.clone(),
        delta_tx,
        InputKind::Delta,
        args.messages_per_stream,
        Duration::from_millis(args.delta_every_ms),
    )));
    handles.push(tokio::spawn(producer_task(
        barrier.clone(),
        metadata_tx,
        InputKind::Metadata,
        args.messages_per_stream,
        Duration::from_millis(args.metadata_every_ms),
    )));

    handles.push(tokio::spawn(consumer_task(
        barrier.clone(),
        event_rx,
        metrics.clone(),
        Duration::from_millis(args.consumer_ms),
        Duration::from_millis(args.latency_budget_ms),
    )));

    handles.push(tokio::spawn(coordinator_task(
        barrier.clone(),
        snapshot_rx,
        delta_rx,
        metadata_rx,
        event_tx,
        shared,
        metrics.clone(),
        mode,
        args.clone(),
    )));

    let blocker_rounds = args.messages_per_stream.max(1);
    for blocker_id in 0..blocking_tasks {
        handles.push(tokio::spawn(blocking_task(
            barrier.clone(),
            blocker_rounds,
            args.blocking_ms,
            args.blocking_jitter_ms,
            blocker_id,
        )));
    }

    barrier.wait().await;

    for handle in handles {
        handle.await.unwrap();
    }

    let total_duration = start.elapsed();
    let total_inputs = metrics.input_messages.load(Ordering::Relaxed);
    let output_events = metrics.output_events.load(Ordering::Relaxed);
    let late_events = metrics.late_events.load(Ordering::Relaxed);

    let coordinator_lock_count = metrics.coordinator_lock_count.load(Ordering::Relaxed) as f64;
    let enrichment_lock_count = metrics.enrichment_lock_count.load(Ordering::Relaxed) as f64;

    let p95_coordinator_wait = metrics
        .coordinator_wait_hist
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|hist| if hist.len() > 0 { hist.value_at_percentile(95.0) } else { 0 }))
        .unwrap_or(0);
    let p95_coordinator_hold = metrics
        .coordinator_hold_hist
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|hist| if hist.len() > 0 { hist.value_at_percentile(95.0) } else { 0 }))
        .unwrap_or(0);
    let p95_event_latency = metrics
        .event_latency_hist
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|hist| if hist.len() > 0 { hist.value_at_percentile(95.0) } else { 0 }))
        .unwrap_or(0);
    let p95_enrichment_wait = metrics
        .enrichment_wait_hist
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|hist| if hist.len() > 0 { hist.value_at_percentile(95.0) } else { 0 }))
        .unwrap_or(0);

    ScenarioResult {
        mode: mode.as_str().to_string(),
        blocking_tasks,
        total_inputs,
        output_events,
        late_events,
        late_pct: if output_events > 0 {
            (late_events as f64 / output_events as f64) * 100.0
        } else {
            0.0
        },
        avg_coordinator_wait_us: if coordinator_lock_count > 0.0 {
            metrics.coordinator_lock_wait_us.load(Ordering::Relaxed) as f64 / coordinator_lock_count
        } else {
            0.0
        },
        p95_coordinator_wait_us: p95_coordinator_wait,
        avg_coordinator_hold_us: if coordinator_lock_count > 0.0 {
            metrics.coordinator_lock_holds_us.load(Ordering::Relaxed) as f64 / coordinator_lock_count
        } else {
            0.0
        },
        p95_coordinator_hold_us: p95_coordinator_hold,
        avg_enrichment_wait_us: if enrichment_lock_count > 0.0 {
            metrics.enrichment_lock_wait_us.load(Ordering::Relaxed) as f64 / enrichment_lock_count
        } else {
            0.0
        },
        p95_enrichment_wait_us: p95_enrichment_wait,
        p95_event_latency_us: p95_event_latency,
        max_event_latency_us: metrics.max_event_latency_us.load(Ordering::Relaxed) as u64,
        total_duration,
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
        results.push(rt.block_on(run_scenario(mode, args.clone(), args.blocking_tasks)));
    }

    let summary = summarize_results(&results);

    println!();
    println!(
        "  Mode: {} | Inputs/stream: {} | Blockers: {} | Runs: {}",
        summary.mode, args.messages_per_stream, summary.blocking_tasks, run_count
    );
    println!(
        "  Median output events: {} | Median late events: {} ({:.1}%)",
        summary.output_events, summary.late_events, summary.late_pct
    );
    println!(
        "  Median avg coordinator lock wait: {:.0}μs | p95: {}μs",
        summary.avg_coordinator_wait_us, summary.p95_coordinator_wait_us
    );
    println!(
        "  Median avg coordinator lock hold: {:.0}μs | p95: {}μs",
        summary.avg_coordinator_hold_us, summary.p95_coordinator_hold_us
    );
    if summary.avg_enrichment_wait_us > 0.0 {
        println!(
            "  Median avg enrichment lock wait: {:.0}μs | p95: {}μs",
            summary.avg_enrichment_wait_us, summary.p95_enrichment_wait_us
        );
    }
    println!(
        "  Median p95 event latency: {}μs | max: {}μs",
        summary.p95_event_latency_us, summary.max_event_latency_us
    );
    println!("  Median total duration: {:?}", summary.total_duration);
    println!();
}

fn run_comparison_matrix(args: Args) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.workers)
        .enable_all()
        .build()
        .unwrap();

    let blocker_counts = if args.blocking_tasks == 0 {
        vec![0, 1]
    } else {
        vec![0, args.blocking_tasks]
    };

    println!();
    println!("Suspension convoy comparison matrix");
    println!(
        "workers={} inputs/stream={} consumer={}ms chan_cap={} enrichment_every={} runs={}",
        args.workers,
        args.messages_per_stream,
        args.consumer_ms,
        args.event_channel_capacity,
        args.enrichment_every,
        args.runs.max(1)
    );
    println!();
    println!(
        "{:<8} {:>8} {:>10} {:>12} {:>12} {:>12} {:>12}",
        "mode", "blockers", "late%", "avg_wait", "p95_hold", "p95_evt", "duration"
    );
    println!("{}", "-".repeat(86));

    for &blocking_tasks in &blocker_counts {
        for &mode in &[Mode::Good, Mode::Bad] {
            let mut results = Vec::with_capacity(args.runs.max(1));
            for _ in 0..args.runs.max(1) {
                results.push(rt.block_on(run_scenario(mode, args.clone(), blocking_tasks)));
            }
            let summary = summarize_results(&results);
            println!(
                "{:<8} {:>8} {:>9.1}% {:>10.0}μs {:>10}μs {:>10}μs {:>10?}",
                summary.mode,
                summary.blocking_tasks,
                summary.late_pct,
                summary.avg_coordinator_wait_us,
                summary.p95_coordinator_hold_us,
                summary.p95_event_latency_us,
                summary.total_duration
            );
        }
    }

    println!();
    println!("Interpretation:");
    println!("  - good: drops shared-state lock before emitting events");
    println!("  - bad: emits events while still holding the shared-state lock");
    println!("  - higher lock hold and event latency in 'bad' indicate a suspension convoy");
    println!();
}
