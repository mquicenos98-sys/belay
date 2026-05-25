//! `GET /v1/events` over the network: the typed client resuming after a
//! dropped connection without gaps or repeats, `Last-Event-ID` choosing
//! where a new stream starts (and winning over `from`), filters, and the
//! lag frame for a subscriber that fell behind the replay buffer.

use std::time::Duration;

use belay_client::EventOptions;
use belay_proto::event::EventPattern;
use belay_testkit::prelude::*;
use belay_testkit::TcpProxy;

fn enqueue_feed(client: &Client, n: usize) -> Result<JobId> {
    Ok(client
        .jobs()
        .enqueue(&job("feed", "item").payload(numbered(n)))?
        .id)
}
