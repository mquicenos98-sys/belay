//! The benchmark scenarios. Each is a function that runs its cases
//! (skipping those the filters exclude) and records a result per case.
//!
//! | scenario | measures |
//! |---|---|
//! | `store-write` | [`LogKv`](belay_store::LogKv) writes on a real directory by sync policy, batch size, value size and writer threads |
//! | `store-recover` | opening a store: replaying the log versus loading a snapshot, by data size |
//! | `queue-embedded` | [`QueueEngine`](belay_queue::QueueEngine) enqueue, lease and complete on `MemKv` and `LogKv`, by thread count |
//! | `queue-http` | the same operations through `belay-server` and `belay-client` over loopback, by client count |
//! | `worker-e2e` | the worker runtime draining a backlog, and how fast an idle worker picks up a new job |
//! | `cron` | `CronExpr::next_after` for typical expressions, in UTC and in zones with daylight saving time |
//! | `json` | serializing, parsing and decoding job documents of three sizes |

use crate::session::Session;
use crate::Result;

mod cron;

mod json;

mod queue;

mod store;
