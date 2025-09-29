//! Shutdown signalling shared by the accept loop, connections and handlers,
//! plus the poison-tolerant lock helper used throughout the crate.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Locks `mutex`, recovering the guard if a previous holder panicked.
///
/// No lock in this crate is held while user code runs, so a poisoned mutex
/// never guards half-updated state; propagating the poison would only turn
/// one failed handler into a dead server.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

type Waker = Box<dyn Fn() + Send>;

/// A one-shot, cloneable shutdown flag.
///
/// The server stores one in every request's extensions, so handlers that
/// block for a long time (long-poll leases, event streams) can notice that
/// the server is draining and return early:
///
/// ```
/// use std::time::Duration;
/// use belay_http::ShutdownSignal;
///
/// let signal = ShutdownSignal::new();
/// let remote = signal.clone();
/// std::thread::spawn(move || remote.trigger());
/// assert!(signal.wait_timeout(Duration::from_secs(5)));
/// ```
#[derive(Clone, Default)]
pub struct ShutdownSignal {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    triggered: AtomicBool,
    // The waker list doubles as the mutex paired with `cond`; the flag is only
    // ever set while it is held, which rules out lost wake-ups.
    wakers: Mutex<Vec<Waker>>,
    cond: Condvar,
}

impl ShutdownSignal {
    /// Creates an untriggered signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Triggers the signal. Idempotent; wakes every waiter and runs the
    /// registered wakers exactly once, on the calling thread.
    pub fn trigger(&self) {
        let wakers = {
            let mut guard = lock(&self.inner.wakers);
            if self.inner.triggered.swap(true, Ordering::SeqCst) {
                return;
            }
            std::mem::take(&mut *guard)
        };
        self.inner.cond.notify_all();
        for waker in wakers {
            waker();
        }
    }

    /// Returns `true` once [`trigger`](Self::trigger) has been called.
    pub fn is_triggered(&self) -> bool {
        self.inner.triggered.load(Ordering::SeqCst)
    }

    /// Blocks until the signal triggers or `timeout` elapses; returns whether
    /// it triggered.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut guard = lock(&self.inner.wakers);
        loop {
            if self.is_triggered() {
                return true;
            }
            let remaining = match deadline {
                Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                // A timeout too large to represent: wait in long slices.
                None => Duration::from_secs(3600),
            };
            if remaining.is_zero() {
                return false;
            }
            guard = match self.inner.cond.wait_timeout(guard, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Blocks until the signal triggers.
    pub fn wait(&self) {
        while !self.wait_timeout(Duration::from_secs(3600)) {}
    }

    /// Registers `waker` to run when the signal triggers; runs it right away
    /// if that already happened.
    pub(crate) fn on_trigger(&self, waker: Waker) {
        let mut guard = lock(&self.inner.wakers);
        if self.is_triggered() {
            drop(guard);
            waker();
        } else {
            guard.push(waker);
        }
    }
}
