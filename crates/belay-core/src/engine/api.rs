//! The request-facing methods of [`Engine`], one per endpoint of the HTTP
//! API (the crate docs map endpoints to methods). Every failure is an
//! [`ApiError`] the server can send as is; list endpoints take a
//! [`Paging`] and return a [`Page`].

use std::path::PathBuf;
use std::time::Instant;

use belay_json::{json, ObjectBuilder, ToJson, Value};
use belay_proto::api::{
    ApiError, CheckpointResponse, Cursor, EnqueueRequest, EnqueueResponse, EnqueueResult,
    JobFilter, MemberInfo, MemberState, Page, PreviewResponse, PurgeResponse, TriggerResponse,
    WorkerInfo,
};
use belay_proto::limits::{DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT};
use belay_proto::{
    CalendarSpec, Job, JobId, JobSpec, JobState, QueueConfig, QueueInfo, QueueName, Schedule,
    ScheduleId, ScheduleSpec, Workflow, WorkflowId, WorkflowSpec,
};
use belay_queue::VerifyReport;
use belay_scheduler::{Upserted, WorkflowDetails, WorkflowFilter};
use belay_store::KvStats;

use super::Engine;
use crate::cluster::{queue_key, schedule_key, Ownership};

/// Where a listing starts and how much it returns: the `cursor` and
/// `limit` query parameters of the list endpoints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Paging {
    /// Continue after this position, from a previous page's
    /// `next_cursor`.
    pub cursor: Option<Cursor>,
    /// Page size, 1 to 1000; default 100.
    pub limit: Option<u32>,
}

impl Paging {
    /// The first page with the default size.
    #[must_use]
    pub fn new() -> Paging {
        Paging::default()
    }

    /// Continues after `cursor`.
    #[must_use]
    pub fn after(mut self, cursor: Cursor) -> Paging {
        self.cursor = Some(cursor);
        self
    }

    /// Sets the page size.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Paging {
        self.limit = Some(limit);
        self
    }

    /// Reads `cursor` and `limit` from decoded query pairs; other keys are
    /// left to the caller.
    ///
    /// ```
    /// use belay_core::Paging;
    ///
    /// let paging = Paging::from_pairs([("limit", "20"), ("state", "ready")]).unwrap();
    /// assert_eq!(paging.limit, Some(20));
    /// assert!(Paging::from_pairs([("limit", "0")]).is_err());
    /// assert!(Paging::from_pairs([("cursor", "garbage")]).is_err());
    /// ```
    pub fn from_pairs<'a>(
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Paging, ApiError> {
        let mut paging = Paging::new();
        for (key, value) in pairs {
            match key {
                "cursor" if !value.is_empty() => {
                    let cursor = value
                        .parse::<Cursor>()
                        .map_err(|e| ApiError::invalid_argument(format!("$.cursor: {e}")))?;
                    paging.cursor = Some(cursor);
                }
                "limit" => {
                    let limit = value.parse::<u32>().map_err(|_| {
                        ApiError::invalid_argument(format!(
                            "$.limit: expected a number, got {value:?}"
                        ))
                    })?;
                    paging.limit = Some(limit);
                }
                _ => {}
            }
        }
        paging.effective_limit()?;
        Ok(paging)
    }

    fn effective_limit(&self) -> Result<u32, ApiError> {
        match self.limit {
            None => Ok(DEFAULT_PAGE_LIMIT),
            Some(n) if (1..=MAX_PAGE_LIMIT).contains(&n) => Ok(n),
            Some(n) => Err(ApiError::invalid_argument(format!(
                "$.limit: must be between 1 and {MAX_PAGE_LIMIT}, got {n}"
            ))),
        }
    }

    /// The name a name-ordered listing continues after.
    fn after_name(&self) -> Result<Option<&str>, ApiError> {
        match &self.cursor {
            None => Ok(None),
            Some(cursor) => cursor.as_name().map(Some).ok_or_else(|| {
                ApiError::invalid_argument("$.cursor: was not issued by this listing")
            }),
        }
    }
}

/// One page of `items` ordered by `name`, after `paging`'s cursor.
fn page_by_name<T>(
    mut items: Vec<T>,
    name: impl Fn(&T) -> &str,
    paging: &Paging,
) -> Result<Page<T>, ApiError> {
    let limit = paging.effective_limit()?;
    let after = paging.after_name()?;
    items.retain(|item| after.map_or(true, |a| name(item) > a));
    items.sort_by(|a, b| name(a).cmp(name(b)));
    items.truncate(limit as usize + 1);
    Ok(Page::paginate(items, limit, |item| {
        Cursor::from_name(name(item))
    }))
}

/// `GET /v1/admin/store`: what the store is and its figures.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreReport {
    /// `memory`, `disk` or `custom`.
    pub storage: String,
    /// The data directory, for `disk`.
    pub dir: Option<PathBuf>,
    /// The store's own statistics.
    pub stats: KvStats,
}
