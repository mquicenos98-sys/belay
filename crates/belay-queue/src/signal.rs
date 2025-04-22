//! Wakeups for long-polling lease requests.
//!
//! A [`Signal`] is a generation counter behind its own mutex and condition
//! variable, deliberately separate from the engine lock: a waiting lease
//! request holds only this small mutex while it sleeps, never the engine
//! lock, so it blocks nobody.
//!
//! The protocol avoids lost wakeups without holding both locks: a waiter
//! reads the generation *before* it looks for work, and after finding none
//! sleeps only while the generation is unchanged. Anything that could make
//! work available bumps the generation after committing it, so a change
//! between the look and the sleep is always noticed.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, wait_timeout};

#[derive(Debug, Default)]
pub(crate) struct Signal {
    generation: Mutex<u64>,
    changed: Condvar,
}

impl Signal {
    pub(crate) fn new() -> Signal {
        Signal::default()
    }

    /// The current generation, to pass to [`wait`](Self::wait).
    pub(crate) fn current(&self) -> u64 {
        *lock(&self.generation)
    }

    /// Wakes every waiter.
    ///
    /// All of them, not one: waiters ask for different queues and only
    /// each of them knows whether the change concerns it. Long-poll counts
    /// are modest (one per idle worker), so the extra wakeups are cheap.
    pub(crate) fn notify(&self) {
        let mut generation = lock(&self.generation);
        *generation = generation.wrapping_add(1);
        drop(generation);
        self.changed.notify_all();
    }

    /// Sleeps until the generation differs from `seen` or `timeout`
    /// passes. Returns whether it changed.
    pub(crate) fn wait(&self, seen: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut generation = lock(&self.generation);
        while *generation == seen {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            generation = wait_timeout(&self.changed, generation, deadline - now).0;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn a_change_before_the_wait_is_not_lost() {
        let s = Signal::new();
        let seen = s.current();
        s.notify();
        let start = Instant::now();
        assert!(s.wait(seen, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn times_out_without_a_change() {
        let s = Signal::new();
        let seen = s.current();
        assert!(!s.wait(seen, Duration::from_millis(20)));
    }

    #[test]
    fn wakes_a_waiter_on_another_thread() {
        let s = Arc::new(Signal::new());
        let seen = s.current();
        let waiter = {
            let s = Arc::clone(&s);
            thread::spawn(move || s.wait(seen, Duration::from_secs(10)))
        };
        thread::sleep(Duration::from_millis(10));
        s.notify();
        assert!(waiter.join().expect("waiter thread"));
    }
}
