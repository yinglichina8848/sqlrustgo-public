# Phase A Implementation Status

> **Last Updated**: 2026-09-12
> **Branch**: `v4.1.0-mvp` (based on `develop/v4.0.0` @ `d8dc96de3`)
> **Status**: A.1 ✓ complete, A.2 + A.3 ⏳ pending

---

## A.1: ExecutorPool module — DONE ✓

### What was built (`crates/executor/src/executor_pool.rs`)

A standalone, well-tested work-stealing task pool ready to back any future
move away from the `RwLock<ExecutionEngine>::execute()` single-thread
bottlen.:

* **6 unit tests pass**:
  - `default_config_respects_cores` — `num_workers` clamp(1, 1024)
  - `submit_one_runs_to_completion` — basic happy path
  - `many_tasks_distribute_across_workers` — 1000 tasks, ≥2 workers see work
  - `failed_task_propagates_error` — closure errors surface via `TaskHandle`
  - `shutdown_releases_threads` — clean teardown
  - `work_stealing_balances_load` — 200 × 5ms tasks, 2 workers, ≤70% sequential
* **Architecture**: `Injector` (MPSC submission) → `Worker::steal_batch_and_pop`
  (atomic chunk move into per-thread LIFO) → `Stealer` (work-stealing on idle)
* **Why global Injector instead of per-worker local push**: `Worker` is not
  `Sync` (internal `Cell<Buffer<T>>`), so we can't hold the same `Worker` in
  the pool AND in the worker thread. The Injector+`steal_batch_and_pop`
  pattern preserves the cache-friendly LIFO property for hot loops while
  removing the Sync constraint. Documented in the module header.

### Commits

* `d8dc96de3` feat(executor): Phase A.1 — ExecutorPool with work-stealing scheduler
* Pushed to all 5 remotes: gitea252, gitea250, github, gitcode, origin

### What this does NOT yet do

The pool is a **library**; no code path in the binary uses it yet. The
wire layer (`crates/mysql-server/src/lib.rs`) still does
`engine.read()/engine.write()` + `ExecutionEngine::execute(&mut self, sql)`,
which is the 8x throughput bottleneck that Phase A targets.

---

## A.2: FileStorage internal lock — DEFERRED

### Why deferred

The `StorageEngine` trait (`crates/storage/src/engine.rs`) requires `&mut self`
for `insert/update/delete`. That means the trait itself forces serialization —
multiple storage trait objects can NOT execute concurrently even if the
underlying `FileStorage` has an internal lock.

The real Phase A.2 win would be:

1. **Change the trait** to take `&self` for all write ops (use `&mut self`
   only for the storage wrapper, not for individual ops).
2. **Wrap `tables` in `RwLock<HashMap>`** so readers can run concurrently.
3. **Replace 30+ call sites** of `self.tables.get_mut(...)` with proper
   lock guards.

### Estimate

3-5 days of refactor + heavy test verification (the `FileStorage`
implementation alone is ~5000 lines across 100+ methods).

### Why I didn't do it in this session

The refactor touches the public storage trait, which is consumed by:

* `crates/storage/` (5,000+ lines)
* `src/execution_engine.rs` (60+ `execute_*` methods)
* `src/storage.rs`, `src/engine_dml.rs`, etc.
* **ALL 22 V400 tests** + **GMP-Platform 408 eval** depend on storage

Risk of regressing the SOAK-leak fix (commits `239c00533`, `dad601829`) is
non-trivial. Better to do A.2 in a dedicated, reviewable PR with extensive
test coverage.

---

## A.3: mysql-server ExecutorPool wiring — DEFERRED

### Why deferred

The COM_QUERY path in `crates/mysql-server/src/lib.rs:11889-12375` looks like:

```rust
match is_read_only {
    Some(ReadOnlyStmt) => engine.read().execute_select(stmt),  // shared lock OK
    None              => engine.write().execute(stmt_sql),       // EXCLUSIVE lock
};
```

The `engine.write()` is **globally exclusive** — even though only the
writer thread needs `&mut self`, the lock blocks all readers. With
sysbench `oltp_read_write` (70% reads / 30% writes), the 30% writes
serialize ALL traffic through this single `&mut ExecutionEngine`.

To wire `ExecutorPool` in:

1. Change `engine.read()/engine.write()` to per-call guard acquisition.
2. Submit `execute_select(s)` and `execute(sql_sql)` as `ClosureTask`s
   into the pool.
3. Make `ExecutionEngine` shareable via `Arc<>` (currently `Arc<RwLock<E>>`
   — needs more granular locking or an `Arc<Inner>` pattern).
4. Verify SHOW STATUS, SHOW PROCESSLIST, and other engine-introspection
   paths still work (they read `engine.read()` for stats).

### Estimate

5-8 days. The hard part is the **lock-striping of `ExecutionEngine`** —
right now every method needs `&mut self` because the executor holds:

* `stats: Arc<RwLock<HashMap<String, TableStats>>>`
* `current_tx_id: ThreadLocal<Cell<u64>>`
* `tx_undo_log: Vec<UndoOp>`

These all need to be either internal-locked or moved out of the hot path.

### Why I didn't do it in this session

Each of the 60+ `execute_*` methods in `execution_engine.rs` would need
to be analyzed for thread-safety. Touching them without a clear correctness
argument risks the `current_tx_id` thread-local leaking across operations,
which is a footgun for transaction semantics.

---

## What this Phase A.1 commit actually buys us

1. **A reusable, well-tested scheduler**. Future PRs (Phase A.2, A.3,
   Phase B query optimizer) can plug `ExecutorPool` in incrementally.
2. **Validation of the design**: the 6 unit tests prove the scheduler
   semantics (LIFO locality, MPSC submission, work-stealing) are
   correct on this platform. The scheduler primitives (crossbeam-deque)
   were already transitively present via rayon.
3. **Foundation for Phase B**: the CBO query planner (`crates/optimizer/`)
   can use the same pool for parallel plan evaluation.

## What still needs to happen to hit the 250-300 TPS Phase A target

1. **Phase A.2** (storage internal lock): 3-5 days
2. **Phase A.3** (mysql-server integration): 5-8 days
3. **End-to-end SOAK**: 1 day to verify the 1.5-1.8x TPS target

Combined ~10-14 days of focused work.

## Recommendation

* **Land A.1** (already done — `d8dc96de3`) on `develop/v4.0.0` once we
  have the bandwidth to do the lock-stripping refactor. Even unused,
  it provides a tested building block.
* **Defer A.2 + A.3** to v4.1.0 proper (post-v4.0.0 GA), where they can
  land together with proper storage-trait refactoring.
* **Don't merge v4.1.0-mvp → develop/v4.0.0** without first completing
  A.3 and re-running the SOAK gate (target ≥250 TPS with ≤50 MB RSS).

## References

* `docs/releases/v4.0.0/PERFORMANCE_OPTIMIZATION_ROADMAP.md` — 3-phase plan
* `docs/releases/v4.0.0/PERFORMANCE_TASK_ANALYSIS.md` — Phase A + C code analysis
* `crates/executor/src/executor_pool.rs` — the actual ExecutorPool
* `crates/executor/src/lib.rs` — module wiring
* `crates/executor/Cargo.toml` — `crossbeam-deque` + `crossbeam-utils` deps
* `Cargo.toml` — workspace-level dep declarations

---

**Reviewer**: when picking this up, start with `crates/executor/src/executor_pool.rs`
(575 lines, heavily commented, 6 passing tests). The design rationale is
in the module header — read that first.