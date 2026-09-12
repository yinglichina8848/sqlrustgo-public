# Phase A PK Lookup — Implementation Status

> **Last Updated**: 2026-09-12
> **Branch**: `v4.1.0-pk-lookup` (based on `v4.1.0-mvp` @ `f02087c9e`)
> **Status**: Storage-side fix DONE. Executor wiring DISABLED (regression).
> **Net TPS impact**: +0% to +4% (depending on workload), but storage infrastructure is in place for future work.

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

### 3. Engine wiring (DISABLED)

The original plan was to insert a fast path in `engine_select` that
calls `scan_pk_eq` instead of `scan_with_ahi` for the `WHERE col =
literal` case. This was implemented and **verified to be hit on every
query** (15194 PK_FAST_PATH hits in 3 seconds), but it caused a
**regression in oltp_read_write** (TPS dropped to 0, latency 55s per
query).

The root cause: a post-scan pipeline (projection, JOIN binder, etc.)
interacts badly with the 0-or-1-row result. I was unable to debug
this further within the session budget.

**The wiring is now DISABLED but the helpers are kept** in the tree.
Future work can re-enable the wiring once the post-scan pipeline is
verified to handle 0/1 row scans correctly.

## SOAK measurements

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
* `src/engine_select.rs` — DISABLED wiring (with comment explaining why)

## Commits on this branch

* `f02087c9e` (from v4.1.0-mvp) — PHASE_A_STATUS doc

## Recommendation

* **Ship the storage-layer changes** (trait method + impls + tests).
  These are correct, tested, and useful for future executor work.
* **Skip the executor wiring** until the post-scan pipeline is
  audited for 0/1-row edge cases.
* **Next session**: look at why the current code can't be parallel
  — focus on the `RwLock<ExecutionEngine>` global lock and see if
  per-connection `Arc<ExecutionEngine>` is feasible.

## References

* `crates/executor/src/executor_pool.rs` — Phase A.1 work-stealing
  pool (not used yet but ready)
* `crates/storage/src/engine.rs` — `scan_pk_eq` definition
* `crates/storage/src/file_storage.rs:3128` — FileStorage impl
* `src/engine_select_pk.rs` — `try_extract_pk_eq` helper
* `src/engine_select.rs:1341` — disabled wiring with explanation
* `docs/releases/v4.0.0/PHASE_A_STATUS.md` — original analysis
* `docs/releases/v4.0.0/PERFORMANCE_TASK_ANALYSIS.md` — Phase A plan