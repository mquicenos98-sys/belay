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
