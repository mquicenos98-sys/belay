//! [`Engine`]: storage, queue, scheduler, workflows, events, metrics and
//! clustering, started together and driven by background threads.
//!
//! The engine is split by concern: opening everything and applying the
//! configuration (`builder`), the periodic duties and their threads
//! (`duties`), the request-facing methods (`api`) and the worker protocol
//! (`source`). How the parts are wired is described on [`Engine`].

mod api;

mod builder;

mod duties;

mod source;

pub use api::{Paging, StoreReport, VerifyOutcome};
pub use builder::EngineBuilder;
pub use duties::TickReport;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use belay_common::sync::lock;
use belay_common::{info, warn, SharedClock, Shutdown, Timestamp};
use belay_metrics::Registry;
use belay_proto::api::ServerInfo;
use belay_proto::event::EventFilter;
use belay_queue::QueueEngine;
use belay_scheduler::Scheduler;
use belay_store::{Kv, LogKv};

use crate::cluster::Cluster;
use crate::config::Config;
use crate::dispatch::WorkflowDriver;
use crate::events::{EventBus, Subscription};
use crate::metrics::Metrics;

/// How the engine's store was provided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StorageKind {
    /// `storage.in_memory`.
    Memory,
    /// A log-structured store in `storage.dir`.
    Disk,
    /// A store handed to the builder.
    Custom,
}

impl StorageKind {
    fn as_str(self) -> &'static str {
        match self {
            StorageKind::Memory => "memory",
            StorageKind::Disk => "disk",
            StorageKind::Custom => "custom",
        }
    }
}
