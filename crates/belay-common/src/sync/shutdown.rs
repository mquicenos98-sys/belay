//! A one-shot, cloneable shutdown signal.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use super::{lock, wait, wait_timeout};

type Callback = Box<dyn FnOnce() + Send>;

struct Inner {
    /// Fast path for `is_triggered`, read without taking the mutex.
    triggered: AtomicBool,
    /// Pending callbacks; `None` once triggered. The mutex also pairs with
    /// the condvar for blocking waits.
    callbacks: Mutex<Option<Vec<Callback>>>,
    condvar: Condvar,
}

/// A signal that is triggered once and observed by any number of threads.
///
/// Clones share the signal. Long-running loops poll
/// [`is_triggered`](Self::is_triggered) or sleep with
/// [`wait_timeout`](Self::wait_timeout), which returns early when shutdown
/// begins, so there is never a need to sleep in fixed increments.
///
/// ```
/// use belay_common::sync::Shutdown;
/// use std::time::Duration;
///
/// let shutdown = Shutdown::new();
/// let worker = {
///     let shutdown = shutdown.clone();
///     std::thread::spawn(move || {
///         let mut rounds = 0;
///         while !shutdown.wait_timeout(Duration::from_millis(5)) {
///             rounds += 1;
///         }
///         rounds
///     })
/// };
/// shutdown.trigger();
/// let _rounds = worker.join().unwrap();
/// ```
#[derive(Clone)]
pub struct Shutdown {
    inner: Arc<Inner>,
}

impl Shutdown {
    /// A signal that has not been triggered.
    #[must_use]
    pub fn new() -> Shutdown {
        Shutdown {
            inner: Arc::new(Inner {
                triggered: AtomicBool::new(false),
                callbacks: Mutex::new(Some(Vec::new())),
                condvar: Condvar::new(),
            }),
        }
    }

    /// Triggers the signal: wakes all waiters and runs registered callbacks
    /// on the calling thread. Returns `true` for the call that actually
    /// triggered it and `false` if it was already triggered.
    pub fn trigger(&self) -> bool {
        let callbacks = {
            let mut slot = lock(&self.inner.callbacks);
            match slot.take() {
                Some(callbacks) => {
                    self.inner.triggered.store(true, Ordering::SeqCst);
                    callbacks
                }
                None => return false,
            }
        };
        self.inner.condvar.notify_all();
        // Run outside the lock so callbacks may use this signal themselves.
        for callback in callbacks {
            callback();
        }
        true
    }

    /// Whether [`trigger`](Self::trigger) has been called.
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        self.inner.triggered.load(Ordering::SeqCst)
    }

    /// Blocks until triggered.
    pub fn wait(&self) {
        let mut slot = lock(&self.inner.callbacks);
        while slot.is_some() {
            slot = wait(&self.inner.condvar, slot);
        }
    }

    /// Blocks until triggered or until `timeout` elapses. Returns `true` if
    /// the signal is triggered.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => self.wait_deadline(deadline),
            None => {
                self.wait();
                true
            }
        }
    }

    /// Blocks until triggered or until `deadline`. Returns `true` if the
    /// signal is triggered.
    pub fn wait_deadline(&self, deadline: Instant) -> bool {
        let mut slot = lock(&self.inner.callbacks);
        while slot.is_some() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            slot = wait_timeout(&self.inner.condvar, slot, deadline - now).0;
        }
        true
    }

    /// Registers `f` to run when the signal triggers, or runs it right away
    /// if it already has. Useful to unblock threads that wait on something
    /// other than this signal, e.g. a condition variable or a socket.
    pub fn on_trigger<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let mut slot = lock(&self.inner.callbacks);
        match slot.as_mut() {
            Some(callbacks) => callbacks.push(Box::new(f)),
            None => {
                drop(slot);
                f();
            }
        }
    }

    /// A new signal that triggers when this one does but can also be
    /// triggered on its own, e.g. to stop one subsystem early.
    ///
    /// The parent keeps only a weak reference, but it does keep one small
    /// callback per child until it triggers, so create children for
    /// long-lived components rather than per request.
    #[must_use]
    pub fn child(&self) -> Shutdown {
        let child = Shutdown::new();
        let weak: Weak<Inner> = Arc::downgrade(&child.inner);
        self.on_trigger(move || {
            if let Some(inner) = weak.upgrade() {
                Shutdown { inner }.trigger();
            }
        });
        child
    }
}
