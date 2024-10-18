//! A fixed-size pool of named worker threads.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use super::{lock, wait};

type Task = Box<dyn FnOnce() + Send + 'static>;

struct State {
    queue: VecDeque<Task>,
    active: usize,
    closing: bool,
}
