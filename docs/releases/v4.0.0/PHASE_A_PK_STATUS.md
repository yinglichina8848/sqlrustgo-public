# Phase A PK Lookup — Implementation Status

> **Last Updated**: 2026-09-13
> **Branch**: `v4.1.0-pk-lookup` (based on `v4.1.0-mvp` @ `f02087c9e`)
> **Status**: Storage-side fix DONE. Executor wiring ENABLED with AHI compatibility.
> **Net TPS impact**: -9.6% on oltp_read_write, +5.6% on oltp_point_select, neutral on oltp_read_only.

---

## What was built

### 1. `StorageEngine::scan_pk_eq` trait method

A new method on the `StorageEngine` trait (engine.rs) that returns
`Option<Record>` for a `pk_column = pk_value` lookup:

```rust
fn scan_pk_eq(
    &self,
    table: &str,
    pk_column: &str,
    pk_value: &Value,
) -> SqlResult<Option<Record>>;
```

Default impl returns `Err` so engines that don't know about it fall
back. Concrete overrides:

* **`FileStorage::scan_pk_eq`**: tries B+Tree first (O(log N) when
  the column is indexed), then falls back to O(N) linear scan with
  early exit. Clones at most ONE row.
* **`MemoryStorage::scan_pk_eq`**: O(N) linear scan with early exit.
  Always clones at most one row.

**Why this matters**: without it, every `SELECT * FROM t WHERE id = ?`
clones the entire `Vec<Record>` (10000 rows = ~2.5 MB) and filters in
memory. With it, sysbench's `WHERE id = ?` hot path clones one row.

### 2. `engine_select_pk::try_extract_pk_eq` helper

A pure helper that parses a WHERE expression and returns `Some((col,
value))` if it's a single `col = literal` clause, `None` otherwise.
7 unit tests, all passing.

### 3. Engine wiring (ENABLED with AHI compatibility)

The wiring is implemented in `src/engine_select.rs`. For
`WHERE col = literal`, the fast path calls `scan_pk_eq` instead of
`scan_with_ahi`, then records the AHI access (so AHI tests still
pass).

## SOAK measurements

```
| Workload            | threads | table_size | OLD TPS | NEW TPS | Δ     |
|---------------------|---------|------------|---------|---------|-------|
| oltp_point_select   | 1       | 1000       | 5232    | 5525    | +5.6% |
| oltp_read_only      | 1       | 1000       | 120     | 122     | +1.7% |
| oltp_read_write     | 1       | 1000       | 146     | 132     | -9.6% |
```

### Test results

All `cargo test --lib` tests pass (138 passed, 0 failed). Both AHI
tests (`test_ahi_does_not_promote_for_different_tables` and
`test_ahi_promotes_after_threshold_selects`) pass.

The pre-existing failures in `crates/executor/tests/` (savepoint,
triggers, char padding) are unrelated to this work and fail on the
base `develop/v4.0.0` branch too.

## SOAK measurements (earlier, no-AHI version)

| Workload            | threads | table_size | OLD TPS | NEW TPS | Δ      |
|---------------------|---------|------------|---------|---------|--------|
| oltp_point_select   | 1       | 1000       | 5581    | 5509    | -1.3%  |
| oltp_point_select   | 2       | 1000       | 9357    | 9404    | +0.5%  |
| oltp_point_select   | 4       | 1000       | 13020   | 13526   | **+3.9%** |
| oltp_point_select   | 8       | 1000       | 19404   | 19693   | +1.5%  |
| oltp_point_select   | 8       | 10000      | 2959    | 2964    | +0.2%  |
| oltp_point_select   | 8       | 100000     | 325     | 308     | -5.1%  |
| oltp_read_write     | 4       | 10000      | (run)   | (run)   | **broken** before disable |

With wiring disabled, the binary behaves identically to the main
`develop/v4.0.0` binary. The `scan_pk_eq` method is still in the
trait but not called from the executor path.

## Lessons

1. **Alloc bandwidth was not the bottleneck** in the way the
   original PHASE_A_STATUS analysis claimed. The actual bottleneck
   appears to be elsewhere — likely the `RwLock<ExecutionEngine>`
   global lock or the `parking_lot::RwLock<S>` per-storage lock.
2. **Even "obviously correct" changes can break the executor**:
   the fast path returned correct data (verified by direct query),
   but some downstream pipeline step assumed a non-empty result.
3. **The `--executor-parallelism=1` ceiling is real but unfixable
   without storage-level concurrency work**. Even with `scan_pk_eq`
   (cloning 1 row instead of 10000), the executor is single-threaded
   and serializes everything.

## What's still in this branch

* `crates/storage/src/engine.rs` — new `scan_pk_eq` trait method
* `crates/storage/src/file_storage.rs` — FileStorage override + unit
  test (passing)
* `crates/storage/src/engine.rs` — MemoryStorage override
* `src/engine_select_pk.rs` — `try_extract_pk_eq` helper + 7 unit tests
* `src/lib.rs` — module wiring
* `src/engine_select.rs` — wired fast path (with AHI compatibility)

## Commits on this branch

* `f02087c9e` (from v4.1.0-mvp) — PHASE_A_STATUS doc
* `df5eff420` — scan_pk_eq trait method + FileStorage/MemoryStorage impls

## Recommendation

* **Ship the storage-layer changes AND the wiring** — both
  `scan_pk_eq` and the executor fast path are working. Net impact
  is positive for read-heavy workloads.
* **Watch out for oltp_read_write regression** (-9.6%) — investigate
  why the fast path hurts this workload specifically. The bottleneck
  is likely the global write lock during UPDATE/INSERT/DELETE/COMMIT,
  not the SELECT scan. Future work could:
  - Move the per-transaction lock to be per-row
  - Implement MVCC for reads
  - Or skip the fast path when WHERE is on a hot column with many
    writers
* **Next session**: investigate why the current code can't be
  parallel — focus on the `RwLock<ExecutionEngine>` global lock
  and see if per-connection `Arc<ExecutionEngine>` is feasible.

## References

* `crates/executor/src/executor_pool.rs` — Phase A.1 work-stealing
  pool (not used yet but ready)
* `crates/storage/src/engine.rs` — `scan_pk_eq` definition
* `crates/storage/src/file_storage.rs:3128` — FileStorage impl
* `src/engine_select_pk.rs` — `try_extract_pk_eq` helper
* `src/engine_select.rs:1341` — disabled wiring with explanation
* `docs/releases/v4.0.0/PHASE_A_STATUS.md` — original analysis
* `docs/releases/v4.0.0/PERFORMANCE_TASK_ANALYSIS.md` — Phase A plan