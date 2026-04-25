//! [`EventLog`], which collects a server's event stream for assertions,
//! and [`first_gap`] for checking sequence numbers.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Client, EventOptions};
use belay_common::sync::{lock, wait_timeout};
use belay_proto::event::{Event, EventFilter};
use belay_proto::JobId;

use crate::error::{Error, Result};

#[derive(Default)]
struct Collected {
    events: Vec<Event>,
    /// Why the stream ended, if it did with an error.
    error: Option<String>,
    ended: bool,
}

#[derive(Default)]
struct Shared {
    collected: Mutex<Collected>,
    changed: Condvar,
}
