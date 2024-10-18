//! A fixed-size pool of named worker threads.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use super::{lock, wait};

type Task = Box<dyn FnOnce() + Send + 'static>;

struct State {
    queue: VecDeque<Task>,
    active: usize,
    closing: bool,
}

struct Shared {
    name: String,
    state: Mutex<State>,
    /// Signalled when a task is queued or the pool starts closing.
    work: Condvar,
    /// Signalled when the pool becomes idle.
    idle: Condvar,
    completed: AtomicU64,
    panicked: AtomicU64,
}

/// A fixed number of threads executing queued closures in FIFO order.
///
/// A panicking task is caught, counted and logged; the worker thread carries
/// on with the next task. Dropping the pool (or calling [`join`](Self::join))
/// lets the workers finish every queued task before they exit.
///
/// ```
/// use belay_common::sync::ThreadPool;
/// use std::sync::atomic::{AtomicUsize, Ordering};
/// use std::sync::Arc;
///
/// let pool = ThreadPool::new("demo", 4).unwrap();
/// let hits = Arc::new(AtomicUsize::new(0));
/// for _ in 0..100 {
///     let hits = Arc::clone(&hits);
///     pool.execute(move || {
///         hits.fetch_add(1, Ordering::Relaxed);
///     });
/// }
/// pool.join();
/// assert_eq!(hits.load(Ordering::Relaxed), 100);
/// ```
pub struct ThreadPool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl ThreadPool {
    /// Starts `size` threads named `{name}-0`, `{name}-1`, and so on.
    ///
    /// Fails if `size` is zero or a thread cannot be spawned; threads that
    /// were already started are shut down again in that case.
    pub fn new(name: &str, size: usize) -> io::Result<ThreadPool> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thread pool size must be at least 1",
            ));
        }
        let shared = Arc::new(Shared {
            name: name.to_owned(),
            state: Mutex::new(State {
                queue: VecDeque::new(),
                active: 0,
                closing: false,
            }),
            work: Condvar::new(),
            idle: Condvar::new(),
            completed: AtomicU64::new(0),
            panicked: AtomicU64::new(0),
        });
        let mut pool = ThreadPool {
            shared,
            workers: Vec::with_capacity(size),
        };
        for i in 0..size {
            let shared = Arc::clone(&pool.shared);
            let handle = thread::Builder::new()
                .name(format!("{name}-{i}"))
                .spawn(move || worker_loop(&shared))?;
            // On error, `pool` is dropped here, which stops the threads
            // spawned so far.
            pool.workers.push(handle);
        }
        Ok(pool)
    }

    /// Queues `task` for execution on one of the workers.
    pub fn execute<F>(&self, task: F)
    where
        F: FnOnce() + Send + 'static,
    {
        lock(&self.shared.state).queue.push_back(Box::new(task));
        self.shared.work.notify_one();
    }

    /// Number of worker threads.
    #[must_use]
    pub fn size(&self) -> usize {
        self.workers.len()
    }

    /// Tasks waiting for a free worker.
    #[must_use]
    pub fn queued(&self) -> usize {
        lock(&self.shared.state).queue.len()
    }

    /// Tasks currently running.
    #[must_use]
    pub fn active(&self) -> usize {
        lock(&self.shared.state).active
    }

    /// Tasks finished so far, including ones that panicked.
    #[must_use]
    pub fn completed(&self) -> u64 {
        self.shared.completed.load(Ordering::SeqCst)
    }

    /// Tasks that panicked so far.
    #[must_use]
    pub fn panicked(&self) -> u64 {
        self.shared.panicked.load(Ordering::SeqCst)
    }

    /// Blocks until the queue is empty and no task is running.
    pub fn wait_idle(&self) {
        let mut state = lock(&self.shared.state);
        while state.active > 0 || !state.queue.is_empty() {
            state = wait(&self.shared.idle, state);
        }
    }

    /// Runs every queued task, then stops and joins the workers.
    pub fn join(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        lock(&self.shared.state).closing = true;
        self.shared.work.notify_all();
        for handle in self.workers.drain(..) {
            // Tasks run under catch_unwind, so a worker never panics.
            let _ = handle.join();
        }
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl fmt::Debug for ThreadPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadPool")
            .field("name", &self.shared.name)
            .field("size", &self.size())
            .field("queued", &self.queued())
            .field("active", &self.active())
            .finish()
    }
}
