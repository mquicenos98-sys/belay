//! `/v1/queues`: status, configuration, pausing and purging.

use belay_proto::api::{Page, PurgeResponse};
use belay_proto::{JobState, QueueConfig, QueueInfo, QueueName};

use crate::client::{segment, Call, Client};
use crate::error::Result;
use crate::pager::{ListOptions, Paginator};
use crate::retry::Safety;

/// Queue calls; see [`Client::queues`].
#[derive(Clone, Copy, Debug)]
pub struct Queues<'a> {
    client: &'a Client,
}
