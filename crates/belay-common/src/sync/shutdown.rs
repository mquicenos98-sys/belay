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
