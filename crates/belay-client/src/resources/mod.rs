//! Typed wrappers for the API's collections, one handle per collection.
//! Each handle borrows the [`Client`](crate::Client) it came from, so they
//! are free to create per call: `client.jobs().get(id)`.

mod jobs;
mod queues;
mod scheduling;
mod system;

pub use jobs::Jobs;
pub use queues::Queues;
pub use scheduling::{Calendars, Schedules, Workflows};
pub use system::Admin;
