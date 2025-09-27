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
