//! A worker process of your own: it leases jobs from a belay server over
//! HTTP and runs them with a custom handler and the built-in shell
//! handler.
//!
//! The server is taken from `BELAY_URL` (default `http://127.0.0.1:7700`)
//! and the token, which needs the `work` scope, from `BELAY_TOKEN`. With
//! `belay dev --no-worker` running in another terminal:
//!
//! ```text
//! cargo run -p belay-worker --example worker
//! belay enqueue media thumbnail --payload '{"image": "cat.png", "width": 320}'
//! belay enqueue media shell --payload '{"cmd": ["echo", "hello from a shell job"]}'
//! ```
//!
//! Press Enter to stop: the worker stops leasing, lets running jobs
//! finish (or hands them back), and prints what it did. The standard
//! library cannot catch Ctrl-C; killing the process is safe too, but its
//! jobs then return to the queue only when their leases expire.

use std::error::Error;
use std::io::BufRead;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use belay_client::{Client, ClientConfig};
use belay_common::log::{Filter, Level, LogFormat};
use belay_common::Shutdown;
use belay_json::json;
use belay_proto::{JobSource, WorkerId};
use belay_worker::builtin::{self, ShellHandler};
use belay_worker::middleware::LoggingMiddleware;
use belay_worker::{
    handler_fn, HandlerError, JobContext, KindOptions, Outcome, Registry, Worker, WorkerConfig,
};

/// Pretends to render a thumbnail: checks the payload, reports progress,
/// and stops early if the job is cancelled or times out.
fn thumbnail(ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
    let image = ctx.payload()["image"]
        .as_str()
        .ok_or_else(|| HandlerError::invalid_payload("expected {\"image\": \"<path>\"}"))?
        .to_owned();
    let width = ctx.payload()["width"].as_u64().unwrap_or(256);
    if width == 0 || width > 4096 {
        // Retrying cannot fix a bad request: the job goes straight to dead.
        return Err(HandlerError::fatal(format!(
            "width {width} is out of range"
        )));
    }

    for step in 1..=4u8 {
        // Wakes early when the job is cancelled, times out, or the worker
        // shuts down.
        if !ctx.sleep_or_cancel(Duration::from_millis(250)) {
            return Err(HandlerError::cancelled());
        }
        ctx.progress(f32::from(step) * 25.0, format!("rendering {image}"));
    }

    // A real handler would upload somewhere and fail with
    // `HandlerError::retry("...")` if the upload did not go through.
    let output = format!("{image}.{width}px.png");
    Ok(Outcome::with(json!({"thumbnail": output, "width": width})))
}
