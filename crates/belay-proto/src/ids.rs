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

impl NameRule {
    /// The same rule with another length limit.
    pub(crate) const fn max(self, max: usize) -> NameRule {
        NameRule { max, ..self }
    }

    /// Why `s` breaks the rule, if it does.
    pub(crate) fn problem(&self, s: &str) -> Option<String> {
        let len = s.chars().count();
        if len == 0 {
            return Some("must not be empty".to_owned());
        }
        if len > self.max {
            return Some(format!("must be 1-{} characters, got {len}", self.max));
        }
        let mut chars = s.chars();
        if let Some(c) = chars.next() {
            if !(c.is_ascii() && (self.first)(c as u8)) {
                return Some(format!("must start with {}, got {c:?}", self.first_desc));
            }
        }
        for (pos, c) in chars.enumerate() {
            if !(c.is_ascii() && (self.rest)(c as u8)) {
                return Some(format!(
                    "{c:?} at position {} is not allowed (use {})",
                    pos + 1,
                    self.allowed
                ));
            }
        }
        None
    }

    /// Checks `s`, naming it `what` in the error.
    pub(crate) fn check(&self, what: &'static str, s: &str) -> Result<(), ParseError> {
        match self.problem(s) {
            None => Ok(()),
            Some(reason) => Err(ParseError::invalid(what, s, reason)),
        }
    }
}

/// Queue names: `[a-z0-9][a-z0-9_.:-]{0,63}`.
pub(crate) const QUEUE_RULE: NameRule = NameRule {
    max: 64,
    first: lower_or_digit,
    first_desc: "a lowercase letter or digit",
    rest: |b| lower_or_digit(b) || matches!(b, b'_' | b'.' | b':' | b'-'),
    allowed: "a-z, 0-9, '_', '.', ':' or '-'",
};

/// Slugs for schedules and calendars: `[a-z0-9][a-z0-9_.-]{0,127}`.
pub(crate) const SLUG_RULE: NameRule = NameRule {
    max: 128,
    first: lower_or_digit,
    first_desc: "a lowercase letter or digit",
    rest: |b| lower_or_digit(b) || matches!(b, b'_' | b'.' | b'-'),
    allowed: "a-z, 0-9, '_', '.' or '-'",
};
