//! Listing: the job filter behind `GET /v1/jobs?...`, opaque pagination
//! cursors, and the [`Page`] every list endpoint returns.

use std::fmt;
use std::str::FromStr;

use belay_common::hash::fnv1a64;
use belay_common::{Timestamp, Ulid};
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::error::ParseError;
use crate::ids::{JobId, QueueName, TenantId, LABEL_RULE};
use crate::job::{Job, JobState};
use crate::limits::{DEFAULT_PAGE_LIMIT, MAX_KIND_LEN, MAX_PAGE_LIMIT, MAX_TAG_LEN};
use crate::query::{borrow_pairs, QueryReader};
use crate::validate::Violations;
use crate::wire::{text_enum, text_json};

text_enum! {
    /// Listing order by id, which for jobs is creation order.
    #[derive(Default)]
    SortOrder, "sort order" {
        /// Oldest first.
        Asc = "asc",
        /// Newest first (the default).
        #[default]
        Desc = "desc",
    }
}

const CURSOR_VERSION: u8 = 1;

const TAG_ULID: u8 = b'u';

const TAG_NAME: u8 = b'n';

const TAG_SEQ: u8 = b's';

/// Longest name a cursor carries; every listed name type is shorter.
const MAX_CURSOR_NAME: usize = 128;

const BASE32: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
