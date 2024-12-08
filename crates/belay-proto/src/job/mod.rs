//! Jobs: the submitted [`JobSpec`], the stored [`Job`] record and the
//! [`JobState`] machine that governs it.

mod record;
mod spec;
mod state;

pub(crate) use record::validate_error_kind;
pub use record::{Job, JobError, Lease, Progress};
pub(crate) use spec::validate_meta;
pub use spec::{
    JobSpec, OnConflict, Unique, UniqueScope, WorkflowRef, META_FIRE_TIME, META_SCHEDULE,
    RESERVED_META_PREFIX,
};
pub use state::{JobState, TransitionError};
