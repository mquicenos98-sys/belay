//! An elastic thread pool with a hard cap on concurrent jobs.
//!
//! Each job here is a whole connection, which may live for minutes (keep-
//! alive, long polls, event streams). A fixed pool smaller than the number
//! of connections would let a few idle clients starve everyone else, so
//! instead the pool grows up to `max_active` threads, reuses idle threads,
//! and lets threads that have been idle for `thread_idle` exit. Admission
//! is decided up front: when `max_active` jobs are queued or running,
//! [`WorkerPool::try_execute`] hands the job back so the caller can refuse
//! it cheaply.
//!
//! A new thread is spawned exactly when queued jobs outnumber threads that
//! are not busy, so a queued job never waits behind a long-running one.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::sync::lock;

/// Why a job was handed back.
#[derive(Debug)]
pub(crate) enum RejectReason {
    /// `max_active` jobs are already queued or running.
    Saturated,
    /// The pool is shutting down.
    Closed,
    /// No thread was available and a new one could not be spawned.
    Spawn(io::Error),
}

#[derive(Debug)]
pub(crate) struct Rejected<J> {
    pub(crate) job: J,
    pub(crate) reason: RejectReason,
}

struct State<J> {
    queue: VecDeque<J>,
    /// Jobs queued or running.
    active: usize,
    threads: usize,
    busy: usize,
    closing: bool,
    handles: Vec<JoinHandle<()>>,
    spawned: usize,
}

struct Inner<J> {
    state: Mutex<State<J>>,
    work: Condvar,
    finished: Condvar,
    run: Box<dyn Fn(J) + Send + Sync>,
    max_active: usize,
    thread_idle: Duration,
    name: String,
}
