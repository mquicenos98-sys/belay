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

impl Default for Shutdown {
    fn default() -> Self {
        Shutdown::new()
    }
}

impl fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shutdown")
            .field("triggered", &self.is_triggered())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    #[test]
    fn trigger_is_visible_to_all_clones() {
        let a = Shutdown::new();
        let b = a.clone();
        assert!(!b.is_triggered());
        assert!(a.trigger());
        assert!(b.is_triggered());
        assert!(!b.trigger(), "second trigger reports false");
    }

    #[test]
    fn wait_timeout_returns_false_when_not_triggered() {
        let s = Shutdown::new();
        let start = Instant::now();
        assert!(!s.wait_timeout(Duration::from_millis(10)));
        assert!(start.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn waiters_wake_promptly_on_trigger() {
        let s = Shutdown::new();
        let waiters: Vec<_> = (0..3)
            .map(|i| {
                let s = s.clone();
                thread::spawn(move || {
                    if i == 0 {
                        s.wait();
                        true
                    } else {
                        s.wait_timeout(Duration::from_secs(60))
                    }
                })
            })
            .collect();
        let start = Instant::now();
        s.trigger();
        for w in waiters {
            assert!(w.join().expect("waiter panicked"));
        }
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn waiting_after_trigger_returns_immediately() {
        let s = Shutdown::new();
        s.trigger();
        s.wait();
        assert!(s.wait_timeout(Duration::from_secs(60)));
        assert!(s.wait_timeout(Duration::MAX));
    }

    #[test]
    fn callbacks_run_once_and_late_registrations_run_immediately() {
        let s = Shutdown::new();
        let count = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let c = Arc::clone(&count);
            s.on_trigger(move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
        s.trigger();
        s.trigger();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let c = Arc::clone(&count);
        s.on_trigger(move || {
            c.fetch_add(10, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 12);
    }

    #[test]
    fn callbacks_may_use_the_signal_without_deadlocking() {
        let s = Shutdown::new();
        let s2 = s.clone();
        let observed = Arc::new(AtomicBool::new(false));
        let o = Arc::clone(&observed);
        s.on_trigger(move || {
            o.store(
                s2.is_triggered() && s2.wait_timeout(Duration::ZERO),
                Ordering::SeqCst,
            );
        });
        s.trigger();
        assert!(observed.load(Ordering::SeqCst));
    }

    #[test]
    fn child_follows_parent_but_not_the_reverse() {
        let parent = Shutdown::new();
        let child = parent.child();
        let grandchild = child.child();
        child.trigger();
        assert!(child.is_triggered() && grandchild.is_triggered());
        assert!(!parent.is_triggered());

        let other = parent.child();
        parent.trigger();
        assert!(other.is_triggered());
        assert!(parent.child().is_triggered(), "child of a triggered parent");
    }

    #[test]
    fn dropped_children_are_not_kept_alive() {
        let parent = Shutdown::new();
        let child = parent.child();
        let weak = Arc::downgrade(&child.inner);
        drop(child);
        assert!(weak.upgrade().is_none());
        parent.trigger();
    }

    #[test]
    fn debug_shows_state() {
        let s = Shutdown::default();
        assert_eq!(format!("{s:?}"), "Shutdown { triggered: false }");
    }
}
