# Additional Demonstrations

This file accompanies the article ["Executor Starvation in Async Rust: The Hidden Cost of Blocking Code"](https://github.com/bobby-math/tokio-blocking-bench).

The main article demonstrates the cliff using per-request blocking that scales with traffic. This file documents two additional triggers that reach the same cliff through different paths.

All three triggers produce the same outcome: the blocking code succeeds, appears in no failure trace, and causes cascading timeouts in unrelated async code.

---

## Trigger 1: Adding blocking code to a fixed workload

**Binary:** `demo_panic`

**Scenario:** Fixed async load (500 tasks), increase blocking tasks from 3 to 4.

```bash
# 3 blockers: zero failures
./target/release/demo_panic --async-tasks 500 --blocking-tasks 3 --rounds 10

# 4 blockers: cascading failures
./target/release/demo_panic --async-tasks 500 --blocking-tasks 4 --rounds 10
```

**What happens:**
- With 3 blocking tasks on 4 workers, one worker remains free to poll async tasks.
- With 4 blocking tasks on 4 workers, all workers are periodically blocked.
- The single additional blocker is the difference between 0% and 34% failure rate.

**Real-world cause:** A new dependency with synchronous I/O, an added blocking call, or a library that internally uses `std::fs` or `reqwest::blocking`.

---

## Trigger 2: Increasing async load against fixed blocking

**Binary:** `demo_load_ramp`

**Scenario:** Fixed blocking (3 tasks), increase async tasks from 50 to 20,000.

```bash
./target/release/demo_load_ramp --run-all
```

**Sample output:**
```
Async Tasks        Total Ops    Succeeded    Timed Out  Failure %
-----------------------------------------------------------------
50                       250          250            0       0.0%
100                      500          500            0       0.0%
500                     2500         2500            0       0.0%
1000                    5000         5000            0       0.0%
2000                   10000        10000            0       0.0%
5000                   25000        25000            0       0.0%
10000                  50000        49764          236       0.5%
20000                 100000         3712        96288      96.3%
```

**What happens:**
- The blocking code is identical across all runs: 3 tasks, each blocking for 80ms.
- At 5,000 async tasks, zero failures.
- At 20,000 async tasks, 96% of operations fail.
- The single free worker cannot cycle through the task queue fast enough.

**Real-world cause:** Traffic growth, integrating a new async library, feature launch that spawns more concurrent tasks.

---

## Trigger 3: Per-request blocking that scales with traffic (main article)

**Binary:** `demo_per_request`

**Scenario:** 30% of requests hit a blocking code path. Increase concurrent requests from 10 to 2,000.

```bash
./target/release/demo_per_request --run-all
```

**What happens:**
- At 10 concurrent requests, blocking calls rarely overlap. Zero failures.
- At 50 concurrent requests, 93% of operations fail.
- The cliff lands at realistic production traffic, not stress test levels.

**Real-world cause:** Normal traffic growth with per-request blocking embedded in the handler (config fetch, DNS lookup, file read).

---

## Summary

| Trigger | What changes | What stays fixed | Cliff point |
|---------|--------------|------------------|-------------|
| Trigger 1 | Blocking tasks (3→4) | Async load (500 tasks) | 4 blockers = 4 workers |
| Trigger 2 | Async load (50→20,000) | Blocking tasks (3) | ~10,000 tasks |
| Trigger 3 | Concurrent requests (10→2,000) | Blocking probability (30%) | 50 concurrent |

In all three cases:
- The blocking code succeeds every time.
- The blocking code appears in no failure trace.
- The failures manifest as timeouts in unrelated async code.
- A developer looking at the error traces would blame the async handlers, not the blocking code.
