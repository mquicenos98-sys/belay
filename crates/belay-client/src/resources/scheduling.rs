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
