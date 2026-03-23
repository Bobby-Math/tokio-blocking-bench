# Lean TUI Plan

## Goal

Build a small terminal UI on top of this repo so developers can:

- change benchmark parameters quickly
- run a scenario without rewriting long CLI commands
- watch key metrics update live
- compare healthy and failing runs
- understand which Tokio failure mechanism they triggered

The TUI is not a replacement for the CLI. It is an educational layer over the same benchmark engine.

## Why TUI

For this repo, a TUI is the right next step because it improves:

- immediacy: faster parameter iteration than raw CLI flags
- visibility: metrics and interpretation are visible together
- teaching value: users can see cliffs and convoys emerge in real time
- implementation cost: much smaller than a GUI

A GUI is optional later. The TUI should come after a shared engine is extracted from the current binaries.

## Product Shape

One binary, two interfaces:

- CLI for reproducible runs, scripting, CI, and exported reports
- TUI for interactive exploration and live observation

Suggested command layout:

```bash
tokio-runtime-lab starvation ...
tokio-runtime-lab request-path ...
tokio-runtime-lab mutex-convoy ...
tokio-runtime-lab suspension-convoy ...
tokio-runtime-lab sweep ...
tokio-runtime-lab tui
```

The TUI should launch with `tokio-runtime-lab tui` and use the same scenario/config types as the CLI subcommands.

## Scope

The TUI should cover the parameter space already present in this repo:

- workers
- async task count / concurrency
- blocking task count
- blocking duration
- async work duration
- timeout budget
- rounds
- runs
- blocking probability
- requests per task
- contention fraction
- awaits under lock
- blocking jitter
- event channel capacity
- consumer delay
- latency budget
- enrichment frequency
- enrichment delay
- mode selection where applicable

Non-goal: simulate every Tokio runtime behavior. The TUI should only expose the mechanisms this repo already studies well.

## Core UX Principles

- Start simple: one screen, one selected scenario, one current run
- Keep parameters visible while results stream in
- Prefer direct manipulation over deep menus
- Make comparisons cheap
- Explain the failure mechanism, not just the numbers
- Avoid charts that imply precision the model does not actually have

## Recommended Stack

- `ratatui` for layout/widgets
- `crossterm` for terminal events/backend
- `tokio` for background execution and streaming updates
- existing `clap` remains for CLI mode
- shared internal engine crate/module for scenarios and metrics

## Required Refactor Before TUI

The current binaries should be reorganized into shared modules.

Proposed internal structure:

```text
src/
  main.rs
  cli.rs
  tui.rs
  app.rs
  app/
    state.rs
    actions.rs
    render.rs
  engine.rs
  engine/
    scenario.rs
    metrics.rs
    runner.rs
    starvation.rs
    request_path.rs
    mutex_convoy.rs
    suspension_convoy.rs
```

Key rule: the TUI must not reimplement benchmark logic. It should call the same engine the CLI uses.

## TUI Layout

Lean first version: one main screen with four panels.

### 1. Scenario Panel

Purpose:

- choose the active scenario
- show a short textual description of the failure mode

Scenarios:

- `starvation`
- `request-path`
- `mutex-convoy`
- `suspension-convoy`

### 2. Parameters Panel

Purpose:

- edit current scenario parameters
- show only fields relevant to the selected scenario

Behavior:

- arrow keys move between fields
- left/right or `h/l` adjusts numeric values
- `Enter` toggles enum/boolean values
- `r` runs the scenario
- `R` repeats with current `runs`

### 3. Live Metrics Panel

Purpose:

- show metrics while the run is in progress

Common metrics:

- elapsed time
- completed operations
- success count
- timeout count
- failure percentage
- p50/p95/p99 latency
- max latency

Scenario-specific metrics:

- average/p95/max lock wait
- average/p95 coordinator hold
- p95 event latency
- late-event percentage

### 4. Interpretation Panel

Purpose:

- convert numbers into a plain-language explanation

Examples:

- "Workers remain available; blocking is not yet saturating the pool."
- "One additional blocker pushed the worker pool to effective starvation."
- "Lock hold time, not raw worker count, is now the dominant bottleneck."
- "Backpressure is extending coordinator lock lifetime."

This panel is where the educational value lives.

## Lean Interaction Model

Keyboard only:

- `1-4`: select scenario
- `Tab` / `Shift-Tab`: move focus
- `Up` / `Down`: select parameter
- `Left` / `Right`: change value
- `Enter`: toggle mode/enum
- `r`: run current scenario
- `c`: clone current config into comparison slot
- `x`: swap between baseline and candidate
- `s`: save run result to JSON
- `q`: quit

No mouse support in the first version.

## Lean Comparison Model

The TUI should support one comparison slot only.

Flow:

1. run a baseline
2. copy it into comparison
3. change one parameter
4. rerun
5. show delta

Comparison output should highlight:

- parameter changed
- timeout delta
- p95 delta
- lock-wait delta
- total-duration delta

Do not add multi-run dashboards or historical browsing in v1.

## Execution Model

Runs should execute in a background Tokio task and stream progress back to the UI.

The engine should emit:

- run started
- progress tick
- partial metric update
- run completed
- run failed

The TUI app state should remain responsive during execution.

## Data Model

Suggested shared types:

```rust
enum ScenarioKind {
    Starvation,
    RequestPath,
    MutexConvoy,
    SuspensionConvoy,
}

struct CommonConfig {
    workers: usize,
    timeout_ms: u64,
    rounds: usize,
    runs: usize,
}

enum ScenarioConfig {
    Starvation { ... },
    RequestPath { ... },
    MutexConvoy { ... },
    SuspensionConvoy { ... },
}

struct RunSummary {
    scenario: ScenarioKind,
    config: ScenarioConfig,
    metrics: MetricSummary,
    interpretation: String,
}
```

The important design constraint is that both CLI and TUI consume the same `ScenarioConfig` and `RunSummary`.

## Output

The TUI should support exporting the latest run to:

- JSON in v1

Possible later additions:

- CSV
- markdown summary
- saved comparison report

## Phased Delivery

### Phase 1: Shared Engine

- extract scenario code from the current binaries
- unify metrics structs where practical
- define common scenario/config/result types
- keep old binaries working until the new CLI replaces them

### Phase 2: Unified CLI

- introduce subcommands for each scenario
- preserve current benchmark semantics
- add `--output table|json`
- add basic sweep support for one varying parameter

### Phase 3: Lean TUI

- add `tui` subcommand
- implement single-screen layout
- support parameter editing and single run execution
- show live metrics and plain-language interpretation
- support one baseline/comparison slot
- add preset-driven onboarding so the first run is guided rather than blank

### Phase 4: Polish

- better validation and field help
- small sparklines or compact trend indicators
- result export
- expand the preset library beyond the onboarding set if needed

## Presets

Presets are part of the core onboarding experience, not a secondary extra.
The initial TUI should open with a small set of curated presets so users can
trigger recognizable runtime behaviors without understanding every parameter first.

Useful presets for teaching:

- `healthy`
- `near-cliff`
- `starved`
- `request-spike`
- `mutex-convoy`
- `suspension-backpressure`

Presets should change multiple parameters at once and update the interpretation panel immediately.

Minimum onboarding behavior:

- highlight a recommended starter preset on launch
- allow loading a preset with one keypress
- show a short explanation of what the preset is intended to demonstrate
- let the user tweak parameters after the preset loads

## Risks

- If benchmark logic remains duplicated, CLI and TUI will drift
- If too many parameters are editable at once, the TUI will feel noisy
- If metrics are not normalized across scenarios, comparison will confuse users
- If the interpretation text is weak, the TUI becomes a glorified control panel

## V1 Scope Guard

V1 should stay intentionally narrow. The TUI is meant to make the current study interactive, not become a general-purpose runtime observability suite.

Explicitly out of scope for v1 beyond the current single-screen design:

- anything more than simple sparklines or compact trend indicators
- multi-run history browsers or session timelines
- plugin or extension systems
- advanced charting or plotting frameworks

## Non-Goals For V1

- desktop GUI
- remote/distributed execution
- embedded graphs with heavy plotting dependencies
- simultaneous multi-scenario live execution
- modeling behavior outside the mechanisms already established in this repo

## Success Criteria

The TUI is successful if a developer can:

1. launch it without reading docs
2. pick a scenario in a few seconds
3. increase one parameter and trigger a visible cliff or convoy effect
4. understand from the interpretation panel why the system degraded
5. export the final run and reproduce it later via CLI

## Bottom Line

This repo has enough parameter coverage to justify a real TUI. The correct implementation is not a brand-new app, but a thin interactive layer over a shared benchmark engine and unified CLI.
