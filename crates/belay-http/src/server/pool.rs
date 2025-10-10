//! An elastic thread pool with a hard cap on concurrent jobs.
//!
//! Each job here is a whole connection, which may live for minutes (keep-
//! alive, long polls, event streams). A fixed pool smaller than the number
//! of connections would let a few idle clients starve everyone else, so
//! instead the pool grows up to `max_active` threads, reuses idle threads,
//! and lets threads that have been idle for `thread_idle` exit. Admission
//! is decided up front: when `max_active` jobs are queued or running,
//! [`WorkerPool::try_execute`] hands the job back so the caller can refuse
//! it cheaply.
//!
//! A new thread is spawned exactly when queued jobs outnumber threads that
//! are not busy, so a queued job never waits behind a long-running one.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::sync::lock;

/// Why a job was handed back.
#[derive(Debug)]
pub(crate) enum RejectReason {
    /// `max_active` jobs are already queued or running.
    Saturated,
    /// The pool is shutting down.
    Closed,
    /// No thread was available and a new one could not be spawned.
    Spawn(io::Error),
}

#[derive(Debug)]
pub(crate) struct Rejected<J> {
    pub(crate) job: J,
    pub(crate) reason: RejectReason,
}

struct State<J> {
    queue: VecDeque<J>,
    /// Jobs queued or running.
    active: usize,
    threads: usize,
    busy: usize,
    closing: bool,
    handles: Vec<JoinHandle<()>>,
    spawned: usize,
}

struct Inner<J> {
    state: Mutex<State<J>>,
    work: Condvar,
    finished: Condvar,
    run: Box<dyn Fn(J) + Send + Sync>,
    max_active: usize,
    thread_idle: Duration,
    name: String,
}

pub(crate) struct WorkerPool<J> {
    inner: Arc<Inner<J>>,
}

impl<J: Send + 'static> WorkerPool<J> {
    pub(crate) fn new(
        name: &str,
        max_active: usize,
        thread_idle: Duration,
        run: impl Fn(J) + Send + Sync + 'static,
    ) -> Self {
        WorkerPool {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    queue: VecDeque::new(),
                    active: 0,
                    threads: 0,
                    busy: 0,
                    closing: false,
                    handles: Vec::new(),
                    spawned: 0,
                }),
                work: Condvar::new(),
                finished: Condvar::new(),
                run: Box::new(run),
                max_active,
                thread_idle,
                name: name.to_owned(),
            }),
        }
    }

    /// Queues `job`, spawning a thread if every existing one is busy.
    pub(crate) fn try_execute(&self, job: J) -> Result<(), Rejected<J>> {
        let mut state = lock(&self.inner.state);
        if state.closing {
            return Err(Rejected {
                job,
                reason: RejectReason::Closed,
            });
        }
        if state.active >= self.inner.max_active {
            return Err(Rejected {
                job,
                reason: RejectReason::Saturated,
            });
        }
        state.active += 1;
        state.queue.push_back(job);
        if state.queue.len() <= state.threads - state.busy {
            self.inner.work.notify_one();
            return Ok(());
        }
        state.handles.retain(|h| !h.is_finished());
        state.spawned += 1;
        let inner = Arc::clone(&self.inner);
        let spawned = thread::Builder::new()
            .name(format!("{}-{}", self.inner.name, state.spawned))
            .spawn(move || worker_loop(&inner));
        match spawned {
            Ok(handle) => {
                state.threads += 1;
                state.handles.push(handle);
                Ok(())
            }
            Err(e) => {
                state.active -= 1;
                let job = state
                    .queue
                    .pop_back()
                    .expect("the job pushed above is still queued: the lock was held throughout");
                Err(Rejected {
                    job,
                    reason: RejectReason::Spawn(e),
                })
            }
        }
    }

    /// Jobs queued or running.
    pub(crate) fn active(&self) -> usize {
        lock(&self.inner.state).active
    }

    /// Stops accepting jobs; idle threads exit once the queue is empty.
    pub(crate) fn close(&self) {
        lock(&self.inner.state).closing = true;
        self.inner.work.notify_all();
    }

    /// Waits until no job is queued or running; returns whether that
    /// happened within `timeout`.
    pub(crate) fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = lock(&self.inner.state);
        while state.active > 0 {
            let remaining = match deadline {
                Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if remaining.is_zero() {
                return false;
            }
            state = match self.inner.finished.wait_timeout(state, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        true
    }

    /// Joins every thread. Only call after [`close`](Self::close) and a
    /// successful [`wait_idle`](Self::wait_idle), or this blocks until the
    /// remaining jobs finish.
    pub(crate) fn join(&self) {
        let handles = std::mem::take(&mut lock(&self.inner.state).handles);
        for handle in handles {
            // Jobs run under catch_unwind, so a worker cannot panic.
            let _ = handle.join();
        }
    }
}

fn worker_loop<J>(inner: &Inner<J>) {
    let mut state = lock(&inner.state);
    loop {
        if let Some(job) = state.queue.pop_front() {
            state.busy += 1;
            drop(state);
            // The connection handler contains handler panics itself; this
            // is a backstop so a bug there cannot shrink the pool.
            let _ = catch_unwind(AssertUnwindSafe(|| (inner.run)(job)));
            state = lock(&inner.state);
            state.busy -= 1;
            state.active -= 1;
            inner.finished.notify_all();
            continue;
        }
        if state.closing {
            break;
        }
        let (guard, wait) = match inner.work.wait_timeout(state, inner.thread_idle) {
            Ok(pair) => pair,
            Err(poisoned) => poisoned.into_inner(),
        };
        state = guard;
        if wait.timed_out() && state.queue.is_empty() && !state.closing {
            break;
        }
    }
    state.threads -= 1;
}

impl<J> fmt::Debug for WorkerPool<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.inner.state);
        f.debug_struct("WorkerPool")
            .field("active", &state.active)
            .field("threads", &state.threads)
            .field("busy", &state.busy)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    type Gate = mpsc::Sender<()>;

    fn pool_with_gate(max: usize) -> (WorkerPool<u32>, Gate, mpsc::Receiver<u32>) {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate_rx = Mutex::new(gate_rx);
        let (done_tx, done_rx) = mpsc::channel();
        let done_tx = Mutex::new(done_tx);
        let pool = WorkerPool::new("test-pool", max, Duration::from_secs(5), move |job: u32| {
            // Every job blocks until the test opens the gate once for it.
            let _ = lock(&gate_rx).recv();
            let _ = lock(&done_tx).send(job);
        });
        (pool, gate_tx, done_rx)
    }

    #[test]
    fn long_jobs_do_not_block_new_ones() {
        let (pool, gate, done) = pool_with_gate(4);
        for job in 0..3 {
            pool.try_execute(job).expect("capacity available");
        }
        assert_eq!(pool.active(), 3);
        for _ in 0..3 {
            gate.send(()).expect("workers listening");
        }
        let mut finished: Vec<u32> = (0..3)
            .map(|_| {
                done.recv_timeout(Duration::from_secs(5))
                    .expect("job finished")
            })
            .collect();
        finished.sort_unstable();
        assert_eq!(finished, [0, 1, 2]);
        assert!(pool.wait_idle(Duration::from_secs(5)));
    }

    #[test]
    fn rejects_beyond_capacity_and_hands_the_job_back() {
        let (pool, gate, done) = pool_with_gate(2);
        pool.try_execute(1).expect("first fits");
        pool.try_execute(2).expect("second fits");
        let rejected = pool.try_execute(3).expect_err("pool is full");
        assert_eq!(rejected.job, 3);
        assert!(matches!(rejected.reason, RejectReason::Saturated));
        gate.send(()).expect("workers listening");
        done.recv_timeout(Duration::from_secs(5))
            .expect("one job finished");
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.active() > 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        pool.try_execute(4).expect("capacity after a job finished");
        gate.send(()).expect("workers listening");
        gate.send(()).expect("workers listening");
        assert!(pool.wait_idle(Duration::from_secs(5)));
    }

    #[test]
    fn idle_threads_are_reused() {
        let (pool, gate, done) = pool_with_gate(8);
        for job in 0..5 {
            gate.send(()).expect("buffered");
            pool.try_execute(job).expect("capacity");
            done.recv_timeout(Duration::from_secs(5))
                .expect("job finished");
            assert!(pool.wait_idle(Duration::from_secs(5)));
        }
        let threads = lock(&pool.inner.state).threads;
        assert_eq!(threads, 1, "sequential jobs should share one thread");
    }

    #[test]
    fn closed_pool_rejects_and_joins() {
        let (pool, gate, _done) = pool_with_gate(2);
        pool.try_execute(1).expect("capacity");
        gate.send(()).expect("buffered");
        pool.close();
        assert!(matches!(
            pool.try_execute(2).expect_err("closed").reason,
            RejectReason::Closed
        ));
        assert!(pool.wait_idle(Duration::from_secs(5)));
        pool.join();
        assert_eq!(lock(&pool.inner.state).threads, 0);
    }

    #[test]
    fn panicking_jobs_do_not_leak_slots() {
        let pool = WorkerPool::new("panicky", 1, Duration::from_secs(5), |fail: bool| {
            if fail {
                panic!("job failed");
            }
        });
        pool.try_execute(true).expect("capacity");
        assert!(pool.wait_idle(Duration::from_secs(5)));
        pool.try_execute(false).expect("slot was released");
        assert!(pool.wait_idle(Duration::from_secs(5)));
    }

    #[test]
    fn wait_idle_times_out_while_jobs_run() {
        let (pool, gate, _done) = pool_with_gate(1);
        pool.try_execute(1).expect("capacity");
        assert!(!pool.wait_idle(Duration::from_millis(20)));
        gate.send(()).expect("worker listening");
        assert!(pool.wait_idle(Duration::from_secs(5)));
    }
}
