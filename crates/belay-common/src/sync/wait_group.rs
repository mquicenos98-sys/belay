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

impl Default for WaitGroup {
    fn default() -> Self {
        WaitGroup::new()
    }
}

impl Clone for WaitGroup {
    fn clone(&self) -> Self {
        *lock(&self.inner.count) += 1;
        WaitGroup {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for WaitGroup {
    fn drop(&mut self) {
        let mut count = lock(&self.inner.count);
        *count -= 1;
        if *count == 0 {
            self.inner.condvar.notify_all();
        }
    }
}

impl fmt::Debug for WaitGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitGroup")
            .field("count", &self.count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn wait_returns_after_all_members_drop() {
        let wg = WaitGroup::new();
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..8 {
            let member = wg.clone();
            let done = Arc::clone(&done);
            thread::spawn(move || {
                done.fetch_add(1, Ordering::SeqCst);
                drop(member);
            });
        }
        wg.wait();
        assert_eq!(done.load(Ordering::SeqCst), 8);
    }

    #[test]
    fn wait_with_no_other_members_returns_immediately() {
        WaitGroup::new().wait();
        assert!(WaitGroup::default().wait_timeout(Duration::ZERO));
    }

    #[test]
    fn panicking_member_still_releases_the_group() {
        let wg = WaitGroup::new();
        let member = wg.clone();
        let handle = thread::spawn(move || {
            let _member = member;
            panic!("worker failed on purpose");
        });
        assert!(wg.wait_timeout(Duration::from_secs(30)));
        assert!(handle.join().is_err());
    }

    #[test]
    fn wait_timeout_gives_up_while_members_remain() {
        let wg = WaitGroup::new();
        let straggler = wg.clone();
        assert_eq!(wg.count(), 2);
        assert!(!wg.wait_timeout(Duration::from_millis(5)));
        assert_eq!(straggler.count(), 1);
        assert_eq!(format!("{straggler:?}"), "WaitGroup { count: 1 }");
    }
}
