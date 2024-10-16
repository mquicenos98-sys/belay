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
