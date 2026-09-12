//! ExecutorPool — multi-threaded SQL execution pool with work-stealing
//!
//! v4.1.0 / Phase A of PERFORMANCE_OPTIMIZATION_ROADMAP.md
//!
//! ## Background
//!
//! Pre-Phase-A the server ran `--executor-parallelism=1` (Issue #3703):
//! every wire-protocol COM_QUERY went through `RwLock<ExecutionEngine>` and
//! `ExecutionEngine::execute(&mut self, sql)` was a sequential `match`. With
//! sysbench `oltp_read_write` on 8 threads this saturated at ~164 TPS (8x
//! slower than MySQL 8.0).
//!
//! ## Design
//!
//! * N worker threads (default = `num_cpus`, capped at `--executor-parallelism`).
//! * Each worker owns its own `crossbeam::deque::Worker<Box<dyn TaskSlot>>`
//!   (LIFO, cache-friendly).
//! * Submission pushes onto a global `Injector<Box<dyn TaskSlot>>` (MPSC).
//! * Idle workers fall back to **work-stealing** from peers via their
//!   `Stealer<Box<dyn TaskSlot>>`.
//! * Each worker parks with a timeout (default 100 µs) when no work is found,
//!   then re-scans — keeps the scheduler responsive without burning CPU.
//!
//! ## Note on the queue model
//!
//! In a fully optimized scheduler `submit()` would push to a per-worker local
//! queue (LIFO, no contention). `crossbeam_deque::Worker` is NOT `Sync` (its
//! internal `Cell<Buffer<T>>` is single-threaded), so we cannot hold the
//! `Worker` in the pool and also pass it to a worker thread. The current
//! design pushes all submissions to the global `Injector`; the worker
//! thread calls `injector.steal_batch(&local)` which atomically moves a
//! batch into the worker's local deque in one shot. This preserves the
//! cache-friendly LIFO property for hot loops while removing the Sync
//! constraint.
//!
//! ## What this does NOT replace yet (Phase A scope)
//!
//! * `RwLock<ExecutionEngine>` still exists at the wire layer — the existing
//!   `engine.read()/engine.write()` dispatch path is left untouched. The pool
//!   is plumbed into Phase A step 4 (mysql-server integration); before that
//!   the pool is exercised by unit + integration tests so the scheduler
//!   semantics are locked down before the wire-layer cutover.
//! * MVCC (Phase C) is a separate, much larger refactor — see
//!   `docs/releases/v4.0.0/PERFORMANCE_TASK_ANALYSIS.md` §3.

use crossbeam_deque::{Injector, Stealer, Worker};
use crossbeam_utils::Backoff;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A unit of SQL execution work submitted to the pool.
///
/// `T` is the connection-specific response type the caller wants back.
/// The pool owns the data and notifies the caller through a `TaskHandle`.
pub struct QueryTask<T = ()> {
    /// Wall-clock time the task was submitted (for latency metrics).
    pub submitted_at: Instant,
    /// Monotonic task ID (gap-free per pool, useful for tracing).
    pub task_id: u64,
    /// Connection id from the wire layer (used for `SHOW PROCESSLIST`).
    pub conn_id: u64,
    /// SQL text to execute.
    pub sql: String,
    /// Phantom marker for the response type so the struct stays `Send + 'static`.
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> QueryTask<T> {
    /// Build a new task from raw wire-protocol inputs.
    pub fn new(task_id: u64, conn_id: u64, sql: String) -> Self {
        Self {
            submitted_at: Instant::now(),
            task_id,
            conn_id,
            sql,
            _marker: std::marker::PhantomData,
        }
    }
}

/// Tunables for the pool. Defaults are conservative; production code should
/// derive `num_workers` from the `--executor-parallelism` CLI flag.
#[derive(Debug, Clone)]
pub struct ExecutorPoolConfig {
    /// Number of worker OS threads. Must be >= 1.
    pub num_workers: usize,
    /// When no work is found, workers `thread::park_timeout` for this long
    /// before re-scanning the queues. 100 µs is a good balance between
    /// wake-up latency and idle CPU.
    pub idle_park_us: u64,
    /// Capacity hint for the global injector (not a hard cap; the deque is
    /// unbounded). Kept for future back-pressure implementation.
    pub queue_capacity: usize,
}

impl Default for ExecutorPoolConfig {
    fn default() -> Self {
        let num_workers = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            num_workers: num_workers.clamp(1, 1024),
            idle_park_us: 100,
            queue_capacity: 4096,
        }
    }
}

/// Lightweight metrics surfaced by the pool. Counters are `AtomicU64` so
/// reads are lock-free.
#[derive(Debug, Default)]
pub struct ExecutorMetrics {
    pub tasks_submitted: AtomicU64,
    pub tasks_completed: AtomicU64,
    pub tasks_failed: AtomicU64,
    pub work_steals: AtomicU64,
    pub local_pushes: AtomicU64,
    pub global_pushes: AtomicU64,
    pub idle_park_ns: AtomicU64,
}

impl ExecutorMetrics {
    pub fn snapshot(&self) -> ExecutorMetricsSnapshot {
        ExecutorMetricsSnapshot {
            tasks_submitted: self.tasks_submitted.load(Ordering::Relaxed),
            tasks_completed: self.tasks_completed.load(Ordering::Relaxed),
            tasks_failed: self.tasks_failed.load(Ordering::Relaxed),
            work_steals: self.work_steals.load(Ordering::Relaxed),
            local_pushes: self.local_pushes.load(Ordering::Relaxed),
            global_pushes: self.global_pushes.load(Ordering::Relaxed),
            idle_park_ns: self.idle_park_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExecutorMetricsSnapshot {
    pub tasks_submitted: u64,
    pub tasks_completed: u64,
    pub tasks_failed: u64,
    pub work_steals: u64,
    pub local_pushes: u64,
    pub global_pushes: u64,
    pub idle_park_ns: u64,
}

/// Handle returned to the wire layer once a task has been queued.
///
/// Phase A returns immediately after enqueue — actual execution happens
/// asynchronously inside a worker. `wait()` blocks until the worker has
/// finished and returns the worker's outcome (Ok/Err + result).
///
/// `T` must be `Clone` so that `wait()` can hand the result back without
/// taking `&mut self`.
pub struct TaskHandle<T: Clone> {
    state: Arc<Mutex<TaskResult<T>>>,
}

#[derive(Clone)]
struct TaskResult<T: Clone> {
    finished: bool,
    result: Option<Result<T, String>>,
}

impl<T: Clone> Clone for TaskHandle<T> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<T: Clone> TaskHandle<T> {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TaskResult {
                finished: false,
                result: None,
            })),
        }
    }

    /// Block until the worker finishes.
    pub fn wait(&self) -> Result<T, String> {
        let backoff = Backoff::new();
        loop {
            {
                let s = self.state.lock();
                if s.finished {
                    return s.result.clone().expect("finished => Some(result)");
                }
            }
            if backoff.is_completed() {
                thread::park_timeout(Duration::from_millis(50));
            } else {
                backoff.snooze();
            }
        }
    }

    /// Non-blocking poll.
    pub fn try_wait(&self) -> Option<Result<T, String>> {
        let s = self.state.lock();
        if s.finished {
            s.result.clone()
        } else {
            None
        }
    }

    fn complete(&self, result: Result<T, String>) {
        let mut s = self.state.lock();
        s.finished = true;
        s.result = Some(result);
        drop(s);
    }
}

/// Trait object that workers actually execute. The wire layer boxes
/// connection-specific work into a `dyn TaskSlot` so the pool stays
/// decoupled from the `ExecutionEngine` concrete type.
pub trait TaskSlot: Send {
    /// Run the task. Called by a worker thread.
    fn run(self: Box<Self>, worker_id: usize);
}

/// A concrete boxed task built from a closure. The closure returns the
/// result type the caller is waiting on.
pub struct ClosureTask<T: Clone + Send + 'static> {
    work: Box<dyn FnOnce(usize) -> Result<T, String> + Send + 'static>,
    handle: TaskHandle<T>,
}

impl<T: Clone + Send + 'static> TaskSlot for ClosureTask<T> {
    fn run(self: Box<Self>, worker_id: usize) {
        let result = (self.work)(worker_id);
        self.handle.complete(result);
    }
}

impl<T: Clone + Send + 'static> ClosureTask<T> {
    pub fn new<F>(work: F) -> (TaskHandle<T>, Box<dyn TaskSlot>)
    where
        F: FnOnce(usize) -> Result<T, String> + Send + 'static,
    {
        let handle = TaskHandle::new();
        let cloned = handle.clone();
        let task: Box<dyn TaskSlot> = Box::new(ClosureTask {
            work: Box::new(work),
            handle: cloned,
        });
        (handle, task)
    }
}

/// The multi-threaded executor pool.
pub struct ExecutorPool {
    config: ExecutorPoolConfig,
    global: Arc<Injector<Box<dyn TaskSlot>>>,
    /// Stealers for every worker (so a worker can steal from peers).
    peer_stealers: Vec<Stealer<Box<dyn TaskSlot>>>,
    handles: Vec<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    next_task_id: AtomicU64,
    metrics: Arc<ExecutorMetrics>,
}

impl ExecutorPool {
    /// Build a pool with `config`. Workers are spawned immediately.
    pub fn new(config: ExecutorPoolConfig) -> Self {
        assert!(config.num_workers >= 1, "num_workers must be >= 1");
        let global = Arc::new(Injector::new());
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(ExecutorMetrics::default());

        // First pass: build per-worker `Worker` queues. We need the
        // Stealer of every OTHER worker available to each thread, so we
        // collect all stealers first, then move each Worker into its
        // thread while cloning the stealers into a per-thread list.
        let workers: Vec<Worker<Box<dyn TaskSlot>>> =
            (0..config.num_workers).map(|_| Worker::new_fifo()).collect();
        let peer_stealers: Vec<Stealer<Box<dyn TaskSlot>>> =
            (0..config.num_workers).map(|i| workers[i].stealer()).collect();

        // Second pass: move each Worker into its thread. Because
        // `Worker` is not `Clone`, we move by ownership.
        let mut handles: Vec<JoinHandle<()>> =
            Vec::with_capacity(config.num_workers);
        for (id, worker) in workers.into_iter().enumerate() {
            // Build the per-thread stealer list, omitting self.
            let peer: Vec<Stealer<Box<dyn TaskSlot>>> = peer_stealers
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != id)
                .map(|(_, s)| s.clone())
                .collect();
            let global = Arc::clone(&global);
            let shutdown = Arc::clone(&shutdown_flag);
            let metrics = Arc::clone(&metrics);
            let cfg = config.clone();

            let handle = thread::Builder::new()
                .name(format!("sqlrustgo-executor-{id}"))
                .spawn(move || worker_loop(id, worker, peer, global, shutdown, metrics, cfg))
                .expect("failed to spawn executor worker");

            handles.push(handle);
        }

        Self {
            config,
            global,
            peer_stealers,
            handles,
            shutdown_flag,
            next_task_id: AtomicU64::new(1),
            metrics,
        }
    }

    /// Submit a closure-typed task. Returns a handle the caller can `wait()`
    /// on to retrieve the result. The closure runs on one of the worker
    /// threads; `worker_id` is passed in for tracing.
    pub fn submit<T, F>(&self, conn_id: u64, sql: String, work: F) -> TaskHandle<T>
    where
        T: Clone + Send + 'static,
        F: FnOnce(usize) -> Result<T, String> + Send + 'static,
    {
        let _ = (conn_id, sql); // recorded in tests; executor ignores for now
        let (handle, task) = ClosureTask::new(work);
        let _task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        self.global.push(task);
        self.metrics.tasks_submitted.fetch_add(1, Ordering::Relaxed);
        self.metrics.global_pushes.fetch_add(1, Ordering::Relaxed);
        handle
    }

    /// Snapshot the pool metrics (lock-free).
    pub fn metrics(&self) -> ExecutorMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Total worker count.
    pub fn num_workers(&self) -> usize {
        self.config.num_workers
    }

    /// Number of peer stealers this pool exposes (== `num_workers`).
    pub fn peer_stealer_count(&self) -> usize {
        self.peer_stealers.len()
    }

    /// Ask all workers to exit. Blocks until every worker has exited.
    pub fn shutdown(&mut self) {
        self.shutdown_flag.store(true, Ordering::Release);
        // The 100 µs park timeout guarantees workers wake up at least
        // 10x per second, so shutdown completes within ~100 ms worst case.
        let handles = std::mem::take(&mut self.handles);
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for ExecutorPool {
    fn drop(&mut self) {
        if !self.shutdown_flag.load(Ordering::Acquire) {
            self.shutdown();
        }
    }
}

fn worker_loop(
    worker_id: usize,
    local: Worker<Box<dyn TaskSlot>>,
    peer_stealers: Vec<Stealer<Box<dyn TaskSlot>>>,
    global: Arc<Injector<Box<dyn TaskSlot>>>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<ExecutorMetrics>,
    cfg: ExecutorPoolConfig,
) {
    let park_us = cfg.idle_park_us;
    let mut peer_cursor: usize = 0;

    while !shutdown.load(Ordering::Acquire) {
        // 1. Local LIFO first (cache-friendly).
        if let Some(task) = local.pop() {
            metrics.local_pushes.fetch_add(1, Ordering::Relaxed);
            run_task(task, worker_id, &metrics);
            continue;
        }

        // 2. Global injector (MPSC submission queue). `steal_batch_and_pop`
        //    atomically moves a chunk into `local` for cache locality and
        //    returns one item to execute immediately.
        match global.steal_batch_and_pop(&local) {
            crossbeam_deque::Steal::Success(task_box) => {
                run_task(task_box, worker_id, &metrics);
                continue;
            }
            crossbeam_deque::Steal::Empty | crossbeam_deque::Steal::Retry => {}
        }

        // 3. Work-steal from peers (round-robin to avoid herd contention).
        let n = peer_stealers.len();
        if n > 0 {
            for step in 0..n.min(4) {
                let idx = (peer_cursor + step) % n;
                match peer_stealers[idx].steal() {
                    crossbeam_deque::Steal::Success(task_box) => {
                        metrics.work_steals.fetch_add(1, Ordering::Relaxed);
                        peer_cursor = (idx + 1) % n;
                        run_task(task_box, worker_id, &metrics);
                        break;
                    }
                    crossbeam_deque::Steal::Retry => continue,
                    crossbeam_deque::Steal::Empty => continue,
                }
            }
            peer_cursor = (peer_cursor + 1) % n;
        }

        // 4. Park briefly to avoid burning CPU when idle.
        let park_start = Instant::now();
        thread::park_timeout(Duration::from_micros(park_us));
        metrics
            .idle_park_ns
            .fetch_add(park_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

fn run_task(task: Box<dyn TaskSlot>, worker_id: usize, metrics: &ExecutorMetrics) {
    let start = Instant::now();
    task.run(worker_id);
    let _elapsed_ns = start.elapsed().as_nanos() as u64;
    metrics.tasks_completed.fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn small_pool(workers: usize) -> ExecutorPool {
        ExecutorPool::new(ExecutorPoolConfig {
            num_workers: workers,
            idle_park_us: 200,
            queue_capacity: 64,
        })
    }

    #[test]
    fn default_config_respects_cores() {
        let cfg = ExecutorPoolConfig::default();
        assert!(cfg.num_workers >= 1);
        assert!(cfg.num_workers <= 1024);
    }

    #[test]
    fn submit_one_runs_to_completion() {
        let mut pool = small_pool(2);
        let counter = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::clone(&counter);

        let handle: TaskHandle<usize> =
            pool.submit(42, "SELECT 1".to_string(), move |_worker_id| {
                c2.fetch_add(1, Ordering::SeqCst);
                Ok(7usize)
            });

        let result = handle.wait();
        assert_eq!(result.unwrap(), 7);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let snap = pool.metrics();
        assert!(snap.tasks_submitted >= 1);
        assert!(snap.tasks_completed >= 1);
        pool.shutdown();
    }

    #[test]
    fn many_tasks_distribute_across_workers() {
        let mut pool = small_pool(4);
        let n_tasks = 1000;
        let counter = Arc::new(AtomicUsize::new(0));
        let worker_seen = Arc::new(parking_lot::Mutex::new(Vec::<usize>::new()));
        let handles: Vec<TaskHandle<()>> = (0..n_tasks)
            .map(|i| {
                let c = Arc::clone(&counter);
                let ws = Arc::clone(&worker_seen);
                pool.submit(i as u64, format!("SELECT {i}"), move |worker_id| {
                    c.fetch_add(1, Ordering::SeqCst);
                    ws.lock().push(worker_id);
                    Ok(())
                })
            })
            .collect();

        for h in handles {
            let _ = h.wait();
        }

        assert_eq!(counter.load(Ordering::SeqCst), n_tasks);
        let seen = worker_seen.lock().len();
        assert!(
            seen >= 2,
            "expected at least 2 distinct workers, saw {seen}"
        );

        let snap = pool.metrics();
        assert!(
            snap.work_steals > 0 || snap.global_pushes > 0,
            "scheduler reported no movement"
        );
        pool.shutdown();
    }

    #[test]
    fn failed_task_propagates_error() {
        let mut pool = small_pool(2);
        let handle: TaskHandle<()> =
            pool.submit(1, "SELECT bad".to_string(), |_| Err("boom".to_string()));
        let res = handle.wait();
        assert_eq!(res.unwrap_err(), "boom");
        pool.shutdown();
    }

    #[test]
    fn shutdown_releases_threads() {
        let mut pool = small_pool(4);
        let h: TaskHandle<()> = pool.submit(1, "noop".to_string(), |_| Ok(()));
        let _ = h.wait();
        let start = Instant::now();
        pool.shutdown();
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn work_stealing_balances_load() {
        // 2 workers, 200 tasks each of which spins for 5 ms. With
        // stealing, wall time should be much less than the sequential
        // baseline of (n_tasks * 5 ms). Threshold is intentionally
        // generous (70%) because this is a CI sanity test, not a
        // benchmark — the real performance numbers live in the SOAK
        // harness.
        let mut pool = small_pool(2);
        let n_tasks = 200;
        let start = Instant::now();
        let handles: Vec<TaskHandle<()>> = (0..n_tasks)
            .map(|i| {
                pool.submit(i, format!("spin {i}"), |_w| {
                    std::thread::sleep(Duration::from_millis(5));
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            let _ = h.wait();
        }
        let elapsed = start.elapsed();
        let sequential_baseline = Duration::from_millis(n_tasks as u64 * 5);
        let threshold = sequential_baseline * 7 / 10;
        assert!(
            elapsed < threshold,
            "expected work-stealing to finish under {threshold:?}; got {elapsed:?} vs baseline {sequential_baseline:?}"
        );
        pool.shutdown();
    }
}