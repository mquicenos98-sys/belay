//! What belay-scheduler manages: cron schedules (`/v1/schedules`), the
//! calendars that restrict when they fire (`/v1/calendars`), and workflow
//! runs (`/v1/workflows`).

use belay_common::Timestamp;
use belay_proto::api::{Page, PreviewResponse, TriggerResponse};
use belay_proto::{
    CalendarSpec, JobId, Schedule, ScheduleId, ScheduleSpec, Workflow, WorkflowId, WorkflowSpec,
};

use crate::client::{segment, Call, Client};
use crate::error::Result;
use crate::pager::{ListOptions, Paginator};
use crate::retry::Safety;

/// Schedule calls; see [`Client::schedules`].
#[derive(Clone, Copy, Debug)]
pub struct Schedules<'a> {
    client: &'a Client,
}

fn schedule_path(id: &ScheduleId, action: &str) -> String {
    let id = segment(id.as_str());
    if action.is_empty() {
        format!("/v1/schedules/{id}")
    } else {
        format!("/v1/schedules/{id}/{action}")
    }
}

impl<'a> Schedules<'a> {
    pub(crate) fn new(client: &'a Client) -> Schedules<'a> {
        Schedules { client }
    }

    /// One page of schedules (`GET /v1/schedules`), by id.
    pub fn list(&self, options: &ListOptions) -> Result<Page<Schedule>> {
        self.client
            .fetch(Call::get("/v1/schedules".to_owned()).query(options.to_query()))
    }

    /// Every schedule, following cursors as the iterator is consumed.
    #[must_use]
    pub fn list_all(&self) -> Paginator<Schedule> {
        Paginator::new(
            self.client.clone(),
            "/v1/schedules".to_owned(),
            Vec::new(),
            None,
        )
    }

    /// The schedule `id` (`GET /v1/schedules/:id`), with its next and last
    /// fire times.
    pub fn get(&self, id: &ScheduleId) -> Result<Schedule> {
        self.client.fetch(Call::get(schedule_path(id, "")))
    }

    /// Creates a schedule (`POST /v1/schedules`); an existing id fails with
    /// `already_exists`. Not retried, so that `already_exists` always means
    /// someone else created it.
    pub fn create(&self, spec: &ScheduleSpec) -> Result<Schedule> {
        self.client
            .fetch(Call::post("/v1/schedules".to_owned(), Safety::NotRepeatable).json(spec))
    }

    /// Creates or replaces the schedule `spec.id`
    /// (`PUT /v1/schedules/:id`).
    pub fn put(&self, spec: &ScheduleSpec) -> Result<Schedule> {
        self.client
            .fetch(Call::put(schedule_path(&spec.id, "")).json(spec))
    }

    /// Deletes schedule `id` (`DELETE /v1/schedules/:id`); jobs it already
    /// enqueued are not touched.
    pub fn delete(&self, id: &ScheduleId) -> Result<()> {
        self.client.execute(Call::delete(schedule_path(id, "")))
    }

    /// Stops `id` from firing (`POST /v1/schedules/:id/pause`).
    pub fn pause(&self, id: &ScheduleId) -> Result<()> {
        self.client
            .execute(Call::post(schedule_path(id, "pause"), Safety::Repeatable))
    }

    /// Lets `id` fire again (`POST /v1/schedules/:id/resume`).
    pub fn resume(&self, id: &ScheduleId) -> Result<()> {
        self.client
            .execute(Call::post(schedule_path(id, "resume"), Safety::Repeatable))
    }

    /// Fires `id` now, outside its cron expression
    /// (`POST /v1/schedules/:id/trigger`), and returns the enqueued job.
    /// Not retried: each call enqueues a job.
    pub fn trigger(&self, id: &ScheduleId) -> Result<JobId> {
        let call = Call::post(schedule_path(id, "trigger"), Safety::NotRepeatable);
        Ok(self.client.fetch::<TriggerResponse>(call)?.job)
    }

    /// The next `n` fire times of `id`, earliest first, before jitter and
    /// calendar filtering (`GET /v1/schedules/:id/preview?n=`).
    pub fn preview(&self, id: &ScheduleId, n: u32) -> Result<Vec<Timestamp>> {
        let call =
            Call::get(schedule_path(id, "preview")).query(vec![("n".to_owned(), n.to_string())]);
        Ok(self.client.fetch::<PreviewResponse>(call)?.times)
    }
}

/// Calendar calls; see [`Client::calendars`].
#[derive(Clone, Copy, Debug)]
pub struct Calendars<'a> {
    client: &'a Client,
}

fn calendar_path(name: &str) -> String {
    format!("/v1/calendars/{}", segment(name))
}

impl<'a> Calendars<'a> {
    pub(crate) fn new(client: &'a Client) -> Calendars<'a> {
        Calendars { client }
    }

    /// One page of calendars (`GET /v1/calendars`), by name.
    pub fn list(&self, options: &ListOptions) -> Result<Page<CalendarSpec>> {
        self.client
            .fetch(Call::get("/v1/calendars".to_owned()).query(options.to_query()))
    }

    /// Every calendar, following cursors as the iterator is consumed.
    #[must_use]
    pub fn list_all(&self) -> Paginator<CalendarSpec> {
        Paginator::new(
            self.client.clone(),
            "/v1/calendars".to_owned(),
            Vec::new(),
            None,
        )
    }

    /// The calendar `name` (`GET /v1/calendars/:name`).
    pub fn get(&self, name: &str) -> Result<CalendarSpec> {
        self.client.fetch(Call::get(calendar_path(name)))
    }

    /// Creates or replaces the calendar `spec.name`
    /// (`PUT /v1/calendars/:name`) and returns it as stored.
    pub fn put(&self, spec: &CalendarSpec) -> Result<CalendarSpec> {
        self.client
            .fetch(Call::put(calendar_path(&spec.name)).json(spec))
    }

    /// Deletes calendar `name` (`DELETE /v1/calendars/:name`).
    pub fn delete(&self, name: &str) -> Result<()> {
        self.client.execute(Call::delete(calendar_path(name)))
    }
}

/// Workflow calls; see [`Client::workflows`].
#[derive(Clone, Copy, Debug)]
pub struct Workflows<'a> {
    client: &'a Client,
}

fn workflow_path(id: WorkflowId) -> String {
    format!("/v1/workflows/{}", segment(&id.to_string()))
}

impl<'a> Workflows<'a> {
    pub(crate) fn new(client: &'a Client) -> Workflows<'a> {
        Workflows { client }
    }

    /// Starts a run of `spec` (`POST /v1/workflows`). The server checks the
    /// step graph (unique names, known dependencies, no cycles) and
    /// enqueues the root steps. Not retried: each call starts a run.
    pub fn submit(&self, spec: &WorkflowSpec) -> Result<Workflow> {
        self.client
            .fetch(Call::post("/v1/workflows".to_owned(), Safety::NotRepeatable).json(spec))
    }

    /// The run `id` with the state of every step (`GET /v1/workflows/:id`).
    pub fn get(&self, id: WorkflowId) -> Result<Workflow> {
        self.client.fetch(Call::get(workflow_path(id)))
    }

    /// One page of runs (`GET /v1/workflows`).
    pub fn list(&self, options: &ListOptions) -> Result<Page<Workflow>> {
        self.client
            .fetch(Call::get("/v1/workflows".to_owned()).query(options.to_query()))
    }

    /// Every run, following cursors as the iterator is consumed.
    #[must_use]
    pub fn list_all(&self) -> Paginator<Workflow> {
        Paginator::new(
            self.client.clone(),
            "/v1/workflows".to_owned(),
            Vec::new(),
            None,
        )
    }

    /// Cancels run `id` and its unfinished steps
    /// (`POST /v1/workflows/:id/cancel`).
    pub fn cancel(&self, id: WorkflowId) -> Result<()> {
        self.client.execute(Call::post(
            format!("{}/cancel", workflow_path(id)),
            Safety::Repeatable,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_encode_ids_and_names() {
        let id = ScheduleId::from_static("nightly.report");
        assert_eq!(schedule_path(&id, ""), "/v1/schedules/nightly.report");
        assert_eq!(
            schedule_path(&id, "preview"),
            "/v1/schedules/nightly.report/preview"
        );
        assert_eq!(
            calendar_path("business hours/eu"),
            "/v1/calendars/business%20hours%2Feu"
        );
    }
}
