//! Identifiers. ULID-based ids ([`JobId`], [`WorkflowId`]) sort by creation
//! time; textual ids are validated on construction, so holding a
//! [`QueueName`] means holding a valid one.
//!
//! | type | form |
//! |---|---|
//! | [`JobId`], [`WorkflowId`] | 26-character ULID, e.g. `01HQ3K9Z8N6V2T4R5S7W0X1Y2Z` |
//! | [`QueueName`] | `[a-z0-9][a-z0-9_.:-]{0,63}` |
//! | [`ScheduleId`] | `[a-z0-9][a-z0-9_.-]{0,127}` |
//! | [`WorkerId`] | `[A-Za-z0-9][A-Za-z0-9_.:@/-]{0,127}` |
//! | [`TenantId`] | `[A-Za-z0-9][A-Za-z0-9_.:-]{0,63}` |
//! | [`LeaseToken`] | 16 lowercase hex digits |

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

use belay_common::{Rng, Timestamp, Ulid};

use crate::error::ParseError;
use crate::wire::text_json;

/// Character rules for a textual identifier or label. All allowed
/// characters are ASCII, so lengths in characters and bytes agree for valid
/// names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NameRule {
    max: usize,
    first: fn(u8) -> bool,
    first_desc: &'static str,
    rest: fn(u8) -> bool,
    allowed: &'static str,
}

fn lower_or_digit(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit()
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
