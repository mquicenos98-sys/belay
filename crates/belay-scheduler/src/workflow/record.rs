//! The stored form of a workflow run and the listing filter.

use std::collections::BTreeMap;

use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};
use belay_proto::api::{Cursor, SortOrder};
use belay_proto::limits::{DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT};
use belay_proto::workflow::WorkflowState;
use belay_proto::Workflow;

/// A workflow run with the reasons its failed steps failed.
///
/// [`Workflow`] records *that* a step failed; this adds *why*: the dead
/// job's last error, a dispatch refusal, or a placeholder that could not be
/// resolved. Stored as `{"workflow": {...}, "errors": {"<step>": "..."}}`.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowDetails {
    /// The run.
    pub workflow: Workflow,
    /// Failure messages of failed steps, by step name.
    pub errors: BTreeMap<String, String>,
}

impl WorkflowDetails {
    /// A run without failures.
    #[must_use]
    pub fn new(workflow: Workflow) -> WorkflowDetails {
        WorkflowDetails {
            workflow,
            errors: BTreeMap::new(),
        }
    }

    /// The failure message of step `name`, if it failed.
    #[must_use]
    pub fn error(&self, name: &str) -> Option<&str> {
        self.errors.get(name).map(String::as_str)
    }
}

impl ToJson for WorkflowDetails {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("workflow", &self.workflow)
            .field_if(!self.errors.is_empty(), "errors", &self.errors)
            .build()
    }
}

impl FromJson for WorkflowDetails {
    fn from_json(value: &Value) -> Result<WorkflowDetails, FromJsonError> {
        let r = ObjectReader::new(value, "WorkflowDetails")?;
        Ok(WorkflowDetails {
            workflow: r.req("workflow")?,
            errors: r.or_default("errors")?,
        })
    }
}

/// Which runs [`WorkflowEngine::list`](crate::WorkflowEngine::list)
/// returns, and in what order. Runs are listed by id, which is creation
/// order; newest first by default.
///
/// ```
/// use belay_proto::workflow::WorkflowState;
/// use belay_scheduler::WorkflowFilter;
///
/// let filter = WorkflowFilter::new().state(WorkflowState::Failed).name("nightly-etl").limit(20);
/// assert_eq!(filter.effective_limit(), 20);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkflowFilter {
    /// Only runs in this state.
    pub state: Option<WorkflowState>,
    /// Only runs of workflows with this name.
    pub name: Option<String>,
    /// Start after this run (the cursor of a previous page).
    pub cursor: Option<Cursor>,
    /// Page size; 0 means the default of 100. At most 1000.
    pub limit: u32,
    /// Listing order.
    pub order: SortOrder,
}

impl WorkflowFilter {
    /// Every run, newest first, 100 per page.
    #[must_use]
    pub fn new() -> WorkflowFilter {
        WorkflowFilter::default()
    }

    /// Only runs in `state`.
    #[must_use]
    pub fn state(mut self, state: WorkflowState) -> WorkflowFilter {
        self.state = Some(state);
        self
    }

    /// Only runs named `name`.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> WorkflowFilter {
        self.name = Some(name.into());
        self
    }

    /// Continues after `cursor`.
    #[must_use]
    pub fn after(mut self, cursor: Cursor) -> WorkflowFilter {
        self.cursor = Some(cursor);
        self
    }

    /// Sets the page size.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> WorkflowFilter {
        self.limit = limit;
        self
    }

    /// Sets the order.
    #[must_use]
    pub fn order(mut self, order: SortOrder) -> WorkflowFilter {
        self.order = order;
        self
    }

    /// The page size actually used.
    #[must_use]
    pub fn effective_limit(&self) -> u32 {
        match self.limit {
            0 => DEFAULT_PAGE_LIMIT,
            n => n.min(MAX_PAGE_LIMIT),
        }
    }

    /// Whether a run with this state and name passes the filter.
    pub(crate) fn admits(&self, state: WorkflowState, name: &str) -> bool {
        self.state.map_or(true, |s| s == state) && self.name.as_deref().map_or(true, |n| n == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::Timestamp;
    use belay_proto::workflow::{StepSpec, WorkflowSpec};
    use belay_proto::{JobSpec, WorkflowId};

    #[test]
    fn details_round_trip_and_omit_empty_errors() {
        let spec = WorkflowSpec::new("etl").step(StepSpec::new("a", JobSpec::new("q", "k")));
        let wf = Workflow::new(WorkflowId::generate(), spec, Timestamp::from_secs(10));
        let mut details = WorkflowDetails::new(wf);
        assert!(details.to_json().get("errors").is_none());
        details
            .errors
            .insert("a".into(), "job 01J... is dead: boom".into());
        let back = WorkflowDetails::from_json(&details.to_json()).unwrap();
        assert_eq!(back, details);
        assert_eq!(back.error("a"), Some("job 01J... is dead: boom"));
        assert_eq!(back.error("b"), None);
    }

    #[test]
    fn filter_limits_and_matching() {
        assert_eq!(WorkflowFilter::new().effective_limit(), 100);
        assert_eq!(WorkflowFilter::new().limit(5000).effective_limit(), 1000);
        let f = WorkflowFilter::new()
            .state(WorkflowState::Running)
            .name("etl");
        assert!(f.admits(WorkflowState::Running, "etl"));
        assert!(!f.admits(WorkflowState::Failed, "etl"));
        assert!(!f.admits(WorkflowState::Running, "other"));
        assert!(WorkflowFilter::new().admits(WorkflowState::Cancelled, "x"));
    }
}
