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
