# Suspension Convoy

## Problem Statement

This document describes a class of async concurrency failures where a coordinator task holds shared state, then suspends before releasing it.

The important distinction is that this is not primarily a blocked-thread problem. The worker thread may still be free to run other tasks. The failure comes from blocked progress: while the coordinator is paused, the shared state remains unavailable, and every task that depends on it is forced to wait.

In practical terms, the system stays busy, but forward progress collapses.

This belongs to the same general family as mutex convoying, but it is a different subtype:

- `mutex convoy`: many peer tasks contend on the same mutex and hold it across `.await`
- `suspension convoy`: one coordinator task holds shared state and suspends before releasing it

Both failures reduce effective capacity. The system may still have worker threads available, but too much useful work is serialized behind one shared bottleneck.

## Core Mechanism

The failure sequence is:

1. A coordinator task receives input from several streams using `tokio::select!`.
2. It acquires a `tokio::sync::Mutex` protecting central shared state.
3. While still holding the lock, it emits an event or otherwise suspends.
4. Suspension delays the next poll of the coordinator.
5. Because the coordinator has not resumed, it has not released the lock.
6. Other tasks that need the same shared state pile up behind the lock.
7. Scheduler delay is converted into lock hold time.
8. Effective throughput drops sharply even though the runtime is still active.

The bug is subtle because the system does not look blocked in the classic sense. Threads may still be running. Tasks may still be waking. But the critical shared resource remains unavailable for too long, and that is enough to stall the system.

## Why This Is Different From Worker Starvation

Worker starvation happens when blocking code prevents Tokio workers from polling tasks.

Suspension convoying is different:

- the worker is not necessarily blocked
- the lock holder is paused, not dead
- the shared state remains locked while the task is suspended

That means this bug can exist even without blocking syscalls. However, if worker starvation is also present elsewhere in the process, it amplifies the problem by delaying the next poll of the coordinator and extending the lock hold even further.

## What The Benchmark Will Replicate

The benchmark will model the concurrency structure only. It will not reproduce any domain-specific behavior.

The public benchmark will use generic names and a synthetic workload:

- `snapshot_stream`
- `delta_stream`
- `metadata_stream`
- `shared_state`
- `EntityConnected`
- `EntityUpdated`
- `enrichment_task`

The benchmark will have one coordinator task that:

1. listens to multiple input streams with `tokio::select!`
2. locks a central shared map
3. computes output events
4. either:
   - bad variant: suspends while still holding the lock
   - good variant: drops the lock before any suspension

An optional background enrichment task will also be included so that the shared state is re-entered from outside the coordinator, which matches the real concurrency pressure more closely.

To make the suspension visible and repeatable, the benchmark uses a bounded downstream event channel and a slow consumer. That creates ordinary async backpressure. The bad variant performs `send().await` while still holding the shared-state lock. The good variant performs the same `send().await` calls only after the lock has been dropped.

## The Two Variants

### Bad Variant

The bad variant models the bug directly:

1. lock shared state
2. derive one or more output events
3. send those events into a bounded channel while the lock guard is still alive
4. release the lock only after the suspension points complete

This turns downstream poll delay into shared-state lock hold time.

### Good Variant

The good variant preserves the same logic but changes the lock lifetime:

1. lock shared state
2. derive owned output events
3. update shared state
4. drop the lock
5. send events afterward

This keeps the critical section local and prevents suspension from extending the lock hold.

## What The Benchmark Will Measure

The benchmark should measure:

- event throughput
- event latency
- time waiting to acquire the shared-state lock
- time holding the shared-state lock
- failure or timeout rate under increasing load

The key signal is this:

- in the bad variant, lock hold time grows with scheduling delay
- in the good variant, lock hold time stays bounded by local computation

If the bad variant degrades or fails significantly earlier than the good variant under the same load, that is evidence of a suspension convoy.

## What This Benchmark Proves

This benchmark does not prove that every production failure of this shape is caused by Tokio itself, or by blocking syscalls, or by one specific mutex.

It proves a narrower and more useful point:

When a coordinator task suspends while still holding shared state, scheduler delay is transformed into resource hold time, and that can collapse effective capacity even when the runtime still appears active.

That is the bug class this benchmark is meant to isolate.

## Relationship To `demo_mutex_convoy`

`demo_mutex_convoy` models many peer tasks contending on the same mutex.

`demo_suspension_convoy` models a single coordinator task that suspends while holding shared state.

They are related, but not identical:

- `demo_mutex_convoy`: peer-task contention
- `demo_suspension_convoy`: coordinator-mediated contention

Both belong to the broader class of shared-state convoy bugs.

Unlike `demo_mutex_convoy`, this benchmark holds the coordinator, shared state, channel, and consumer constant. The only intentional difference is whether suspension happens before or after the shared-state lock is released.

## Primary Signal

The primary proof signal in this benchmark is coordinator lock hold time.

That is the cleanest metric because it directly measures the mechanism under study: how long shared state remains unavailable when the coordinator suspends before releasing it.

Event lateness and end-to-end latency are secondary signals. They are still useful, but they depend on the chosen backpressure, consumer speed, and latency budget. Those metrics show operational impact, while lock hold time shows the underlying cause.

## Taxonomy

This benchmark fits into a broader taxonomy of async capacity failures:

- worker starvation: blocking code prevents tasks from being polled
- mutex convoy: many peer tasks contend on the same shared mutex
- suspension convoy: a coordinator task suspends while still holding shared state

These are distinct mechanisms, but they can compound each other inside the same runtime.
