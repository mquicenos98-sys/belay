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

/// An opaque position in a listing: the last item of a page, after which
/// the next page starts.
///
/// Clients treat it as an opaque string. Inside it is a key (a ULID for
/// jobs and workflows, a name for schedules, calendars and queues, a
/// sequence number for events) framed as `version, tag, key, check byte`
/// and written in lowercase Crockford base32. Jobs are listed by id, and
/// ids are ULIDs that begin with their creation time, so the id alone pins
/// a position in creation order. The check byte (low byte of FNV-1a over
/// the rest) catches truncated or hand-edited cursors, which are rejected
/// as `invalid_argument`.
///
/// ```
/// use belay_proto::api::Cursor;
/// use belay_proto::JobId;
///
/// let id: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap();
/// let cursor = Cursor::after_job(id);
/// let text = cursor.to_string();
/// assert_eq!(text.parse::<Cursor>().unwrap().job_id(), Some(id));
/// assert!("not-a-cursor".parse::<Cursor>().is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cursor(Key);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Ulid(Ulid),
    Name(String),
    Seq(u64),
}

impl Cursor {
    /// A position after the item with this ULID.
    #[must_use]
    pub fn from_ulid(id: Ulid) -> Cursor {
        Cursor(Key::Ulid(id))
    }

    /// A position after the job `id`.
    #[must_use]
    pub fn after_job(id: JobId) -> Cursor {
        Cursor::from_ulid(id.ulid())
    }

    /// A position after the item named `name` (schedules, calendars,
    /// queues). Names longer than 128 bytes are cut on a character boundary;
    /// no listed name type is that long.
    #[must_use]
    pub fn from_name(name: &str) -> Cursor {
        let mut end = name.len().min(MAX_CURSOR_NAME);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        Cursor(Key::Name(name[..end].to_owned()))
    }

    /// A position after sequence number `seq`.
    #[must_use]
    pub fn from_seq(seq: u64) -> Cursor {
        Cursor(Key::Seq(seq))
    }

    /// The ULID key, if this is a ULID cursor.
    #[must_use]
    pub fn as_ulid(&self) -> Option<Ulid> {
        match self.0 {
            Key::Ulid(id) => Some(id),
            _ => None,
        }
    }

    /// The key as a job id, if this is a ULID cursor.
    #[must_use]
    pub fn job_id(&self) -> Option<JobId> {
        self.as_ulid().map(JobId::from_ulid)
    }

    /// The name key, if this is a name cursor.
    #[must_use]
    pub fn as_name(&self) -> Option<&str> {
        match &self.0 {
            Key::Name(name) => Some(name),
            _ => None,
        }
    }

    /// The sequence key, if this is a sequence cursor.
    #[must_use]
    pub fn as_seq(&self) -> Option<u64> {
        match self.0 {
            Key::Seq(seq) => Some(seq),
            _ => None,
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![CURSOR_VERSION];
        match &self.0 {
            Key::Ulid(id) => {
                bytes.push(TAG_ULID);
                bytes.extend_from_slice(&id.to_bytes());
            }
            Key::Name(name) => {
                bytes.push(TAG_NAME);
                bytes.extend_from_slice(name.as_bytes());
            }
            Key::Seq(seq) => {
                bytes.push(TAG_SEQ);
                bytes.extend_from_slice(&seq.to_be_bytes());
            }
        }
        bytes.push(fnv1a64(&bytes) as u8);
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Option<Cursor> {
        let (&check, framed) = bytes.split_last()?;
        if fnv1a64(framed) as u8 != check {
            return None;
        }
        match framed {
            [CURSOR_VERSION, TAG_ULID, key @ ..] => {
                let raw: [u8; 16] = key.try_into().ok()?;
                Some(Cursor::from_ulid(Ulid::from_bytes(raw)))
            }
            [CURSOR_VERSION, TAG_NAME, key @ ..] if key.len() <= MAX_CURSOR_NAME => {
                let name = std::str::from_utf8(key).ok()?;
                Some(Cursor(Key::Name(name.to_owned())))
            }
            [CURSOR_VERSION, TAG_SEQ, key @ ..] => {
                let raw: [u8; 8] = key.try_into().ok()?;
                Some(Cursor::from_seq(u64::from_be_bytes(raw)))
            }
            _ => None,
        }
    }
}
