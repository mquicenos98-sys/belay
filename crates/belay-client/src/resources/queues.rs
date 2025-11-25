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

fn queue_path(name: &QueueName, action: &str) -> String {
    let name = segment(name.as_str());
    if action.is_empty() {
        format!("/v1/queues/{name}")
    } else {
        format!("/v1/queues/{name}/{action}")
    }
}

impl<'a> Queues<'a> {
    pub(crate) fn new(client: &'a Client) -> Queues<'a> {
        Queues { client }
    }

    /// One page of queues (`GET /v1/queues`), by name.
    pub fn list(&self, options: &ListOptions) -> Result<Page<QueueInfo>> {
        self.client
            .fetch(Call::get("/v1/queues".to_owned()).query(options.to_query()))
    }

    /// Every queue, following cursors as the iterator is consumed.
    #[must_use]
    pub fn list_all(&self) -> Paginator<QueueInfo> {
        Paginator::new(
            self.client.clone(),
            "/v1/queues".to_owned(),
            Vec::new(),
            None,
        )
    }

    /// The status of queue `name` (`GET /v1/queues/:name`): configuration,
    /// counts per state, age of the oldest ready job, throughput.
    pub fn get(&self, name: &QueueName) -> Result<QueueInfo> {
        self.client.fetch(Call::get(queue_path(name, "")))
    }

    /// Replaces the configuration of `name` (`PUT /v1/queues/:name/config`)
    /// and returns the updated status. Jobs already enqueued keep the
    /// defaults they were given.
    pub fn set_config(&self, name: &QueueName, config: &QueueConfig) -> Result<QueueInfo> {
        self.client
            .fetch(Call::put(queue_path(name, "config")).json(config))
    }

    /// Stops leasing from `name` (`POST /v1/queues/:name/pause`); enqueueing
    /// still works and running jobs finish.
    pub fn pause(&self, name: &QueueName) -> Result<()> {
        self.client
            .execute(Call::post(queue_path(name, "pause"), Safety::Repeatable))
    }

    /// Resumes leasing from `name` (`POST /v1/queues/:name/resume`).
    pub fn resume(&self, name: &QueueName) -> Result<()> {
        self.client
            .execute(Call::post(queue_path(name, "resume"), Safety::Repeatable))
    }

    /// Removes the jobs of `name` that are in `state`, or all of its jobs
    /// when `state` is `None` (`POST /v1/queues/:name/purge?state=`).
    /// Returns how many were removed. Not retried: a repeat would also
    /// remove jobs that arrived in between.
    pub fn purge(&self, name: &QueueName, state: Option<JobState>) -> Result<u64> {
        let query = state
            .map(|s| vec![("state".to_owned(), s.to_string())])
            .unwrap_or_default();
        let call = Call::post(queue_path(name, "purge"), Safety::NotRepeatable).query(query);
        Ok(self.client.fetch::<PurgeResponse>(call)?.purged)
    }

    /// Deletes queue `name` and its configuration (`DELETE /v1/queues/:name`).
    pub fn delete(&self, name: &QueueName) -> Result<()> {
        self.client.execute(Call::delete(queue_path(name, "")))
    }
}
