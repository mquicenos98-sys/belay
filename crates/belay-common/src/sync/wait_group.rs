//! Waiting for a set of threads to finish without collecting join handles.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{lock, wait, wait_timeout};

struct Inner {
    count: Mutex<usize>,
    condvar: Condvar,
}

/// Each clone is one participant; [`wait`](Self::wait) blocks until every
/// other clone has been dropped.
///
/// Participation ends on drop rather than through an explicit `done()` call,
/// so a thread that panics still releases the group.
///
/// ```
/// use belay_common::sync::WaitGroup;
///
/// let wg = WaitGroup::new();
/// for _ in 0..4 {
///     let member = wg.clone();
///     std::thread::spawn(move || {
///         // ... work ...
///         drop(member);
///     });
/// }
/// wg.wait();
/// ```
pub struct WaitGroup {
    inner: Arc<Inner>,
}

impl WaitGroup {
    /// A group whose only participant is the returned handle.
    #[must_use]
    pub fn new() -> WaitGroup {
        WaitGroup {
            inner: Arc::new(Inner {
                count: Mutex::new(1),
                condvar: Condvar::new(),
            }),
        }
    }

    /// Leaves the group and blocks until all other participants have left.
    pub fn wait(self) {
        let inner = Arc::clone(&self.inner);
        drop(self);
        let mut count = lock(&inner.count);
        while *count > 0 {
            count = wait(&inner.condvar, count);
        }
    }

    /// Like [`wait`](Self::wait) with a limit. Returns `true` if everyone
    /// left in time.
    pub fn wait_timeout(self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let inner = Arc::clone(&self.inner);
        drop(self);
        let mut count = lock(&inner.count);
        while *count > 0 {
            let remaining = match deadline {
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(r) if !r.is_zero() => r,
                    _ => return false,
                },
                None => Duration::from_secs(3600),
            };
            count = wait_timeout(&inner.condvar, count, remaining).0;
        }
        true
    }

    /// Current number of participants, including this one.
    #[must_use]
    pub fn count(&self) -> usize {
        *lock(&self.inner.count)
    }
}
