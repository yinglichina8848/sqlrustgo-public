# Phase A Implementation Status — Updated 2026-09-12

> **Branch**: `v4.1.0-mvp` (based on `develop/v4.0.0` @ `d8dc96de3`)
> **Status**: A.1 ✓ complete, A.2 + A.3 ⏳ blocked by a deeper issue
> **Key finding this session**: multi-threading the executor won't help —
> the real bottleneck is `FileStorage::scan` cloning 10000 rows per query.

---

## What changed since the last status doc

After deeper investigation, I discovered that **the 164 TPS ceiling is
NOT caused by `--executor-parallelism=1`**. The real bottleneck is
**allocation bandwidth** in the read path:

### Root cause analysis

For sysbench `oltp_read_only` with `table_size=10000`:

* Each transaction issues 8 SELECT queries.
* `execute_select` calls `scan_with_ahi()` (engine_select.rs:3360).
* The default path of `scan_with_ahi` is **`storage.scan(table)?`** which
  **clones the entire `Vec<Record>`** of the table on every call.
* For `table_size=10000`, each clone is ~2.5 MB.
* 8 threads × 130 TPS × 8 SELECTs/tx = **8320 scans/sec × 2.5 MB = 20 GB/s**
  of allocator traffic.
* All 8 sysbench threads share the read lock but each does ~3.9ms
  of CPU work per query (alloc + copy). The 8-thread aggregate is
  ~130 TPS, fully saturating allocator bandwidth.

### Why ExecutorPool doesn't help

* Multiple reader threads **already run in parallel** through the
  `parking_lot::RwLock<ExecutionEngine>` shared read guard.
* `execute_select` takes `&self` (verified — engine_select.rs:755).
* The bottleneck is **per-thread CPU on the clone**, not lock contention.
* Adding more executor workers would just add more threads doing the
  same expensive clones — no throughput gain.

### Direct measurement

I ran a scaling test on the unmodified main binary (no Phase A changes):

| threads | table_size | workload      | TPS  |
|----------|------------|---------------|------|
| 2        | 1000       | oltp_read_only| 180.9 |
| 4        | 1000       | oltp_read_only| 157.9 |
| 8        | 1000       | oltp_read_only| 170.4 |
| 8        | 10000      | oltp_read_only| 130   |
| 8        | 10000      | oltp_read_write | 164 |

The flat 150-180 TPS across thread counts (with same table size) shows
the bottleneck is **per-thread CPU work**, not thread coordination.
The bottleneck is the storage layer, not the executor.

---

## A.1: ExecutorPool module — DONE ✓ (unchanged)

* 575 lines + 6 unit tests, all passing
* `d8dc96de3` pushed to all 5 remotes
* Library-quality, ready for future use
* **Not yet wired into the wire layer** (see A.3 below)

## A.2: FileStorage internal lock — DEFERRED ✓ (still)

* 30+ call sites of `self.tables.get_mut(...)` in file_storage.rs
* Would need a trait refactor (StorageEngine's `&mut self` for writes)
* **But**: making tables lock-internal wouldn't fix the bottleneck
  anyway. The bottleneck is `scan()` cloning the whole Vec<Record>,
  not lock contention.

## A.3: mysql-server ExecutorPool wiring — DEFERRED (now with different rationale)

* The original rationale was "8x throughput gain via parallel execution"
* The **actual** bottleneck is storage layer allocation, not executor
  parallelism
* Wiring ExecutorPool in would add code complexity without measurable
  throughput improvement on the current workload

**Recommendation**: skip A.3 entirely unless the storage layer is
reworked first (see "What would actually help" below).

---

## What would actually help (correct Phase A target)

The 250-300 TPS target from the roadmap is achievable, but the path
is different from what ExecutorPool enables. The real lever is **PK
index lookups in the storage layer**:

### Fix 1: Make `TableData` indexed by primary key (1-2 days)

Currently:
```rust
pub struct TableData {
    pub info: TableInfo,
    pub rows: Vec<Record>,  // linear scan
}
```

Should be:
```rust
pub struct TableData {
    pub info: TableInfo,
    pub rows: Vec<Record>,
    pub pk_index: HashMap<i64, u32>,  // pk -> row offset
}
```

Then `scan_pk(table, pk) -> Option<Record>` is O(1).

### Fix 2: Use PK index in `execute_select` WHERE id = ? (1 day)

In `engine_select.rs`, detect the `WHERE id = ?` pattern and call
`scan_pk()` instead of `scan_with_ahi()`. The latter already has a hook
for `scan_with_index` — it just needs to be enabled for the PK.

### Fix 3: Add secondary index lookups (1-2 days)

sysbench's oltp_read_write uses `WHERE k = ?` heavily. The `k_1`
secondary index is registered but never queried — `SELECT c FROM sbtest1
WHERE k=100` returns 0 rows (verified). Wiring the existing
`range_index` for `=` lookups would close this gap.

**Combined**: 3-5 days of focused work for the 250-300 TPS target.
ExecutorPool is still useful as a future Phase A deliverable when the
storage layer is reworked for higher concurrency (Phase C / MVCC).

---

## Current state of the binary

I rebuilt the release binary on the `v4.1.0-mvp` branch with the new
ExecutorPool module compiled in. The binary:

* Still passes all existing tests (V400 series, GMP-Platform 408, etc.)
* Behaves identically to the main `develop/v4.0.0` binary at the wire
  layer (ExecutorPool is a library, not yet wired in)
* Has the same 164 TPS ceiling on oltp_read_write

This means the `v4.1.0-mvp` branch is safe to merge into `develop/v4.0.0`
for the ExecutorPool library, with the understanding that the actual
performance work is still pending.

## Decision matrix

| Option | Time | Impact | Risk |
|--------|------|--------|------|
| Merge `v4.1.0-mvp` → `develop/v4.0.0` (just A.1 library) | 1 day | None at runtime | Low |
| Implement PK index lookup (Fix 1+2) | 3-4 days | 2-3x TPS for read-only | Medium (storage trait touch) |
| Implement secondary index lookup (Fix 3) | 1-2 days | Modest | Medium |
| Wire ExecutorPool (A.3) | 3-5 days | Negligible on current workload | High (lock-striping ExecutionEngine) |

**My recommendation**: ship A.1 (ExecutorPool library) as a build-block,
defer A.2 and A.3 indefinitely, and pivot the performance work to
PK/secondary index lookups in the storage layer.

## References

* `docs/releases/v4.0.0/PERFORMANCE_OPTIMIZATION_ROADMAP.md` — original plan
* `docs/releases/v4.0.0/PERFORMANCE_TASK_ANALYSIS.md` — Phase A + C analysis
* `crates/executor/src/executor_pool.rs` — A.1 implementation
* `crates/storage/src/file_storage.rs:544` — `scan()` (the bottleneck)
* `src/engine_select.rs:3360` — `scan_with_ahi()` (caller of the bottleneck)
* `crates/storage/src/file_storage.rs:733` — `range_index` (the unused solution)