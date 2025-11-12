//! Server-Sent Events end to end: the server's `SseWriter` streaming to the
//! client's `EventStream`, reconnection with `Last-Event-ID`, and
//! disconnect detection on the server side.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_http::{
    Client, Drain, Error, Request, Response, Router, SseEvent, SseOptions, StatusCode,
};
use common::{serve, url};

fn fast_options() -> SseOptions {
    SseOptions::default()
        .reconnect_delay(Duration::from_millis(10))
        .read_timeout(Some(Duration::from_secs(5)))
}
