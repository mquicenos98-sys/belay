//! Thread coordination: poison-tolerant locking, shutdown signalling, a
//! wait group, a fixed-size thread pool and a drift-free ticker.
//!
//! Lock poisoning is treated as recoverable throughout belay: a panic while
//! holding a lock is already reported by the panic hook, and taking the guard
//! anyway beats cascading that panic into every thread that touches the lock.
//! The flip side is that code must not rely on poisoning to hide data it may
//! have left half-updated: mutate under a lock only in steps that cannot panic
//! midway, or restore consistency before anything that can.
//! Use [`lock`], [`read`], [`write()`], [`wait`] and [`wait_timeout`] instead of
//! calling `.unwrap()` on the standard results.

mod pool;
mod shutdown;
mod ticker;
mod wait_group;

use std::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

pub use pool::ThreadPool;
pub use shutdown::Shutdown;
pub use ticker::{spawn_periodic, Tick, Ticker};
pub use wait_group::WaitGroup;

/// Locks `mutex`, recovering the guard if a previous holder panicked.
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read-locks `rwlock`, recovering from poisoning.
pub fn read<T: ?Sized>(rwlock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    rwlock.read().unwrap_or_else(|e| e.into_inner())
}

/// Write-locks `rwlock`, recovering from poisoning.
pub fn write<T: ?Sized>(rwlock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    rwlock.write().unwrap_or_else(|e| e.into_inner())
}

/// Blocks on `condvar`, recovering from poisoning. Spurious wakeups are
/// possible, so callers re-check their condition in a loop.
pub fn wait<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar.wait(guard).unwrap_or_else(|e| e.into_inner())
}

/// Blocks on `condvar` for at most `timeout`, recovering from poisoning.
/// The flag is `true` if the wait timed out.
pub fn wait_timeout<'a, T>(
    condvar: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
) -> (MutexGuard<'a, T>, bool) {
    let (guard, result) = condvar
        .wait_timeout(guard, timeout)
        .unwrap_or_else(|e| e.into_inner());
    (guard, result.timed_out())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn poison<T: Send + 'static>(m: &Arc<Mutex<T>>) {
        let m2 = Arc::clone(m);
        let result = thread::spawn(move || {
            let _guard = m2.lock();
            panic!("poisoning the mutex on purpose");
        })
        .join();
        assert!(result.is_err());
        assert!(m.is_poisoned());
    }

    #[test]
    fn lock_recovers_poisoned_mutex() {
        let m = Arc::new(Mutex::new(5));
        poison(&m);
        *lock(&m) += 1;
        assert_eq!(*lock(&m), 6);
    }

    #[test]
    fn rwlock_helpers_recover_from_poisoning() {
        let l = Arc::new(RwLock::new(vec![1]));
        let l2 = Arc::clone(&l);
        let _ = thread::spawn(move || {
            let _guard = l2.write();
            panic!("poisoning the rwlock on purpose");
        })
        .join();
        assert!(l.is_poisoned());
        write(&l).push(2);
        assert_eq!(*read(&l), vec![1, 2]);
    }

    #[test]
    fn wait_timeout_reports_timeouts() {
        let m = Mutex::new(());
        let cv = Condvar::new();
        let (_guard, timed_out) = wait_timeout(&cv, lock(&m), Duration::from_millis(1));
        assert!(timed_out);
    }

    #[test]
    fn wait_wakes_on_notify() {
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let pair2 = Arc::clone(&pair);
        let setter = thread::spawn(move || {
            *lock(&pair2.0) = true;
            pair2.1.notify_all();
        });
        let mut ready = lock(&pair.0);
        while !*ready {
            ready = wait(&pair.1, ready);
        }
        drop(ready);
        setter.join().expect("setter panicked");
    }
}
