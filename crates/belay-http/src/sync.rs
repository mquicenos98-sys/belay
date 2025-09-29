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

impl fmt::Debug for ShutdownSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShutdownSignal")
            .field("triggered", &self.is_triggered())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn wait_timeout_returns_false_when_not_triggered() {
        let signal = ShutdownSignal::new();
        let start = Instant::now();
        assert!(!signal.wait_timeout(Duration::from_millis(20)));
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn trigger_wakes_waiters_on_other_threads() {
        let signal = ShutdownSignal::new();
        let waiter = {
            let signal = signal.clone();
            std::thread::spawn(move || signal.wait_timeout(Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(10));
        signal.trigger();
        assert!(waiter.join().expect("waiter thread panicked"));
        assert!(signal.is_triggered());
    }

    #[test]
    fn wakers_run_exactly_once() {
        let signal = ShutdownSignal::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        signal.on_trigger(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        signal.trigger();
        signal.trigger();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn waker_registered_after_trigger_runs_immediately() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        signal.on_trigger(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lock_recovers_from_poisoning() {
        let mutex = Arc::new(Mutex::new(5));
        let m = Arc::clone(&mutex);
        let _ = std::thread::spawn(move || {
            let _guard = m.lock().expect("fresh mutex");
            panic!("poison the mutex");
        })
        .join();
        assert!(mutex.is_poisoned());
        assert_eq!(*lock(&mutex), 5);
    }
}
