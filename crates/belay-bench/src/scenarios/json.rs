//! `json`: belay-json on job documents, the records the queue stores and
//! the API sends, in three sizes. `serialize` is `Value` to text, `parse`
//! text to `Value`, `decode` `Value` to a typed `Job`. MB/s counts the
//! document's text length for all three.

use std::hint::black_box;

use belay_common::{Rng, Timestamp, Ulid};
use belay_json::{FromJson, ObjectBuilder, ToJson, Value};
use belay_proto::{Job, JobError, JobId, JobSpec, QueueName};

use crate::harness::{measure, Params, Plan, Work};
use crate::session::{CaseResult, Session};
use crate::units::format_size;
use crate::{Error, Result};

pub const NAME: &str = "json";

/// Distinct documents per size; calls cycle through them.
const DOCS: usize = 32;

/// Document sizes by the number of line items in the payload: about
/// 0.7 KiB, 2.4 KiB and 37 KiB of compact text (the average is in each
/// case's notes).
const SIZES: &[(&str, usize)] = &[("small", 1), ("medium", 20), ("large", 400)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Serialize,
    Parse,
    Decode,
}
