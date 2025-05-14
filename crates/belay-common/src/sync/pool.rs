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
        let current = thread::current().id();
        for handle in self.workers.drain(..) {
            // The last handle to a pool may be dropped by one of its own
            // tasks; joining that worker from itself would deadlock, and it
            // exits by itself once the queue is drained.
            if handle.thread().id() != current {
                // Tasks run under catch_unwind, so a worker never panics.
                let _ = handle.join();
            }
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

fn worker_loop(shared: &Shared) {
    loop {
        let task = {
            let mut state = lock(&shared.state);
            loop {
                if let Some(task) = state.queue.pop_front() {
                    state.active += 1;
                    break task;
                }
                if state.closing {
                    return;
                }
                state = wait(&shared.work, state);
            }
        };

        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(task)) {
            shared.panicked.fetch_add(1, Ordering::SeqCst);
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            crate::warn!(
                "thread pool task panicked";
                pool = shared.name.as_str(),
                panic = message,
            );
        }
        shared.completed.fetch_add(1, Ordering::SeqCst);

        let mut state = lock(&shared.state);
        state.active -= 1;
        if state.active == 0 && state.queue.is_empty() {
            shared.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Duration;

    fn pool(name: &str, size: usize) -> ThreadPool {
        ThreadPool::new(name, size).expect("spawning test pool")
    }

    #[test]
    fn runs_every_task() {
        let p = pool("runs", 3);
        let count = Arc::new(AtomicUsize::new(0));
        for _ in 0..50 {
            let count = Arc::clone(&count);
            p.execute(move || {
                count.fetch_add(1, Ordering::SeqCst);
            });
        }
        p.wait_idle();
        assert_eq!(count.load(Ordering::SeqCst), 50);
        assert_eq!(p.completed(), 50);
        assert_eq!((p.queued(), p.active()), (0, 0));
    }

    #[test]
    fn rejects_zero_size() {
        let err = ThreadPool::new("zero", 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn worker_threads_are_named() {
        let p = pool("named", 2);
        let (tx, rx) = mpsc::channel();
        p.execute(move || {
            let name = thread::current().name().map(str::to_owned);
            let _ = tx.send(name);
        });
        let name = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("task did not run");
        let name = name.expect("worker thread has a name");
        assert!(name == "named-0" || name == "named-1", "{name}");
        assert_eq!(p.size(), 2);
    }

    #[test]
    fn panicking_task_does_not_kill_the_pool() {
        let p = pool("panicky", 1);
        p.execute(|| panic!("task failed on purpose"));
        let (tx, rx) = mpsc::channel();
        p.execute(move || {
            let _ = tx.send(42);
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(42));
        p.wait_idle();
        assert_eq!(p.panicked(), 1);
        assert_eq!(p.completed(), 2);
    }

    #[test]
    fn join_drains_queued_tasks() {
        let p = pool("drain", 1);
        let count = Arc::new(AtomicUsize::new(0));
        for _ in 0..100 {
            let count = Arc::clone(&count);
            p.execute(move || {
                count.fetch_add(1, Ordering::SeqCst);
            });
        }
        p.join();
        assert_eq!(count.load(Ordering::SeqCst), 100);
    }

    #[test]
    fn drop_is_graceful() {
        let count = Arc::new(AtomicUsize::new(0));
        {
            let p = pool("drop", 2);
            for _ in 0..20 {
                let count = Arc::clone(&count);
                p.execute(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                });
            }
        }
        assert_eq!(count.load(Ordering::SeqCst), 20);
    }

    #[test]
    fn all_workers_run_concurrently() {
        const SIZE: usize = 4;
        let p = pool("parallel", SIZE);
        let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        for _ in 0..SIZE {
            let gate = Arc::clone(&gate);
            let tx = tx.clone();
            p.execute(move || {
                let (count, cv) = &*gate;
                let mut n = lock(count);
                *n += 1;
                cv.notify_all();
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while *n < SIZE && std::time::Instant::now() < deadline {
                    n = super::super::wait_timeout(cv, n, Duration::from_millis(100)).0;
                }
                let _ = tx.send(*n == SIZE);
            });
        }
        for _ in 0..SIZE {
            assert_eq!(rx.recv_timeout(Duration::from_secs(20)), Ok(true));
        }
    }

    #[test]
    fn dropping_the_last_handle_inside_a_task_does_not_deadlock() {
        let p = Arc::new(pool("selfdrop", 2));
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        let inner = Arc::clone(&p);
        p.execute(move || {
            let _ = go_rx.recv();
            drop(inner);
            let _ = done_tx.send(());
        });
        drop(p);
        go_tx.send(()).expect("task is waiting");
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(10)), Ok(()));
    }

    #[test]
    fn debug_lists_counts() {
        let p = pool("dbg", 1);
        p.wait_idle();
        assert_eq!(
            format!("{p:?}"),
            "ThreadPool { name: \"dbg\", size: 1, queued: 0, active: 0 }"
        );
    }
}
