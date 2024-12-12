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

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(BASE32[((acc >> bits) & 31) as usize]));
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(BASE32[((acc << (5 - bits)) & 31) as usize]));
    }
    out
}

/// Case-insensitive; rejects non-zero padding bits so that every cursor
/// has exactly one spelling (modulo case).
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let digit = BASE32.iter().position(|&a| a == c.to_ascii_lowercase())?;
        acc = (acc << 5) | digit as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (bits < 5 && acc == 0).then_some(out)
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&base32_encode(&self.to_bytes()))
    }
}

impl FromStr for Cursor {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Cursor, ParseError> {
        // A name cursor is at most 131 bytes framed, 210 characters.
        const MAX_TEXT: usize = 256;
        (s.len() <= MAX_TEXT)
            .then(|| base32_decode(s))
            .flatten()
            .and_then(|bytes| Cursor::from_bytes(&bytes))
            .ok_or_else(|| ParseError::invalid("cursor", s, "not a cursor issued by this server"))
    }
}

text_json!(Cursor, "cursor");

/// Criteria for `GET /v1/jobs`. All criteria must hold; list criteria
/// (`states`) match any of their values.
///
/// Query form (see [`to_query`](Self::to_query)):
/// `queue`, `state` (comma-separated or repeated), `kind`, `tag`, `tenant`,
/// `created_after`, `created_before` (RFC 3339, exclusive), `order`
/// (`asc`/`desc`), `cursor`, `limit`. Unknown parameters are rejected with a
/// hint, single-valued ones may not repeat, and empty values count as
/// absent.
///
/// ```
/// use belay_proto::api::JobFilter;
/// use belay_proto::{JobState, QueueName};
///
/// let filter = JobFilter::new()
///     .queue(QueueName::from_static("emails"))
///     .state(JobState::Dead)
///     .state(JobState::Retrying)
///     .limit(50);
/// let pairs = filter.to_query();
/// assert_eq!(
///     pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
///     ["queue=emails", "state=dead,retrying", "limit=50"]
/// );
/// assert_eq!(JobFilter::from_query(&pairs), Ok(filter));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobFilter {
    /// Only this queue.
    pub queue: Option<QueueName>,
    /// Only these states; empty for all.
    pub states: Vec<JobState>,
    /// Only this kind.
    pub kind: Option<String>,
    /// Only jobs carrying this tag.
    pub tag: Option<String>,
    /// Only this tenant.
    pub tenant: Option<TenantId>,
    /// Only jobs created strictly after this time.
    pub created_after: Option<Timestamp>,
    /// Only jobs created strictly before this time.
    pub created_before: Option<Timestamp>,
    /// Listing order.
    pub order: SortOrder,
    /// Continue after this position.
    pub cursor: Option<Cursor>,
    /// Page size; default 100, at most 1000.
    pub limit: Option<u32>,
}

impl JobFilter {
    /// Query parameters understood by [`from_query`](Self::from_query).
    pub const QUERY_KEYS: &'static [&'static str] = &[
        "queue",
        "state",
        "kind",
        "tag",
        "tenant",
        "created_after",
        "created_before",
        "order",
        "cursor",
        "limit",
    ];

    /// Every job, newest first, default page size.
    #[must_use]
    pub fn new() -> JobFilter {
        JobFilter::default()
    }

    /// Restricts to a queue.
    #[must_use]
    pub fn queue(mut self, queue: QueueName) -> JobFilter {
        self.queue = Some(queue);
        self
    }

    /// Adds an accepted state.
    #[must_use]
    pub fn state(mut self, state: JobState) -> JobFilter {
        self.states.push(state);
        self
    }

    /// Restricts to a kind.
    #[must_use]
    pub fn kind(mut self, kind: impl Into<String>) -> JobFilter {
        self.kind = Some(kind.into());
        self
    }

    /// Restricts to jobs with a tag.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> JobFilter {
        self.tag = Some(tag.into());
        self
    }

    /// Restricts to a tenant.
    #[must_use]
    pub fn tenant(mut self, tenant: TenantId) -> JobFilter {
        self.tenant = Some(tenant);
        self
    }

    /// Restricts to creation times strictly between the bounds.
    #[must_use]
    pub fn created_between(
        mut self,
        after: Option<Timestamp>,
        before: Option<Timestamp>,
    ) -> JobFilter {
        self.created_after = after;
        self.created_before = before;
        self
    }

    /// Sets the order.
    #[must_use]
    pub fn order(mut self, order: SortOrder) -> JobFilter {
        self.order = order;
        self
    }

    /// Continues after `cursor`.
    #[must_use]
    pub fn after(mut self, cursor: Cursor) -> JobFilter {
        self.cursor = Some(cursor);
        self
    }

    /// Sets the page size.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> JobFilter {
        self.limit = Some(limit);
        self
    }

    /// The page size to use: the requested one clamped to
    /// `1..=MAX_PAGE_LIMIT`, or the default.
    #[must_use]
    pub fn effective_limit(&self) -> u32 {
        self.limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, MAX_PAGE_LIMIT)
    }

    /// Whether `job` meets every criterion (cursor and limit aside).
    #[must_use]
    pub fn matches(&self, job: &Job) -> bool {
        self.queue.as_ref().map_or(true, |q| job.queue() == q)
            && (self.states.is_empty() || self.states.contains(&job.state))
            && self.kind.as_deref().map_or(true, |k| job.kind() == k)
            && self.tag.as_deref().map_or(true, |t| job.spec.has_tag(t))
            && self
                .tenant
                .as_ref()
                .map_or(true, |t| job.spec.tenant.as_ref() == Some(t))
            && self.created_after.map_or(true, |t| job.created_at > t)
            && self.created_before.map_or(true, |t| job.created_at < t)
    }

    /// Whether a job with `id` comes after the cursor in this filter's
    /// order (always true without a cursor).
    #[must_use]
    pub fn past_cursor(&self, id: JobId) -> bool {
        match self.cursor.as_ref().and_then(Cursor::job_id) {
            None => true,
            Some(last) => match self.order {
                SortOrder::Asc => id > last,
                SortOrder::Desc => id < last,
            },
        }
    }

    /// The decoded query pairs, in [`QUERY_KEYS`](Self::QUERY_KEYS) order;
    /// unset criteria and the default order are left out.
    #[must_use]
    pub fn to_query(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut push = |key: &str, value: String| pairs.push((key.to_owned(), value));
        if let Some(q) = &self.queue {
            push("queue", q.to_string());
        }
        if !self.states.is_empty() {
            let states: Vec<&str> = self.states.iter().map(|s| s.as_str()).collect();
            push("state", states.join(","));
        }
        if let Some(kind) = &self.kind {
            push("kind", kind.clone());
        }
        if let Some(tag) = &self.tag {
            push("tag", tag.clone());
        }
        if let Some(tenant) = &self.tenant {
            push("tenant", tenant.to_string());
        }
        if let Some(t) = self.created_after {
            push("created_after", t.to_rfc3339());
        }
        if let Some(t) = self.created_before {
            push("created_before", t.to_rfc3339());
        }
        if self.order != SortOrder::default() {
            push("order", self.order.to_string());
        }
        if let Some(cursor) = &self.cursor {
            push("cursor", cursor.to_string());
        }
        if let Some(limit) = self.limit {
            push("limit", limit.to_string());
        }
        pairs
    }

    /// Parses decoded query pairs (see the type docs for the rules),
    /// reporting every bad parameter with its name as the path.
    pub fn from_query(pairs: &[(String, String)]) -> Result<JobFilter, Violations> {
        JobFilter::from_pairs(borrow_pairs(pairs))
    }

    /// Like [`from_query`](Self::from_query) for borrowed pairs, such as an
    /// HTTP server's query map yields.
    pub fn from_pairs<'a>(
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<JobFilter, Violations> {
        let mut q = QueryReader::new(pairs, JobFilter::QUERY_KEYS);
        let filter = JobFilter {
            queue: q.text("queue"),
            states: q.list("state"),
            kind: q.name("kind", LABEL_RULE.max(MAX_KIND_LEN)),
            tag: q.name("tag", LABEL_RULE.max(MAX_TAG_LEN)),
            tenant: q.text("tenant"),
            created_after: q.timestamp("created_after"),
            created_before: q.timestamp("created_before"),
            order: q.text("order").unwrap_or_default(),
            cursor: q.text("cursor"),
            limit: q.number("limit"),
        };
        if let (Some(after), Some(before)) = (filter.created_after, filter.created_before) {
            if before <= after {
                q.report(
                    "created_before",
                    format!("must be after created_after ({after})"),
                );
            }
        }
        if let Some(limit) = filter.limit {
            if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
                q.report(
                    "limit",
                    format!("must be between 1 and {MAX_PAGE_LIMIT}, got {limit}"),
                );
            }
        }
        if filter.cursor.as_ref().is_some_and(|c| c.job_id().is_none()) {
            q.report("cursor", "was not issued by a job listing");
        }
        q.finish()?;
        Ok(filter)
    }
}

/// One page of a listing: `{"items": [...], "next_cursor": "..."}`, where
/// `next_cursor` is absent on the last page.
#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    /// The items, in listing order.
    pub items: Vec<T>,
    /// Where the next page starts; `None` on the last page.
    pub next_cursor: Option<Cursor>,
}

impl<T> Page<T> {
    /// A page from its parts.
    #[must_use]
    pub fn new(items: Vec<T>, next_cursor: Option<Cursor>) -> Page<T> {
        Page { items, next_cursor }
    }

    /// A final page (or an unpaginated list).
    #[must_use]
    pub fn last(items: Vec<T>) -> Page<T> {
        Page::new(items, None)
    }

    /// Builds a page from up to `limit + 1` items fetched in listing order:
    /// fetching one extra tells whether another page exists without a
    /// second query. The cursor points at the last item kept.
    #[must_use]
    pub fn paginate(mut items: Vec<T>, limit: u32, cursor_of: impl Fn(&T) -> Cursor) -> Page<T> {
        let limit = limit as usize;
        if items.len() > limit {
            let next_cursor = items.last().map(cursor_of);
            items.truncate(limit);
            Page::new(items, next_cursor)
        } else {
            Page::last(items)
        }
    }

    /// Whether another page follows.
    #[must_use]
    pub fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }

    /// Converts the items, keeping the cursor.
    #[must_use]
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Page<U> {
        Page::new(self.items.into_iter().map(f).collect(), self.next_cursor)
    }
}

impl<T> Default for Page<T> {
    fn default() -> Page<T> {
        Page::last(Vec::new())
    }
}

impl<T: ToJson> ToJson for Page<T> {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("items", &self.items)
            .opt_field("next_cursor", &self.next_cursor)
            .build()
    }
}

impl<T: FromJson> FromJson for Page<T> {
    fn from_json(value: &Value) -> Result<Page<T>, FromJsonError> {
        let r = ObjectReader::new(value, "Page")?;
        Ok(Page {
            items: r.or_default("items")?,
            next_cursor: r.opt("next_cursor")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn lines(r: Result<JobFilter, Violations>) -> Vec<String> {
        match r {
            Ok(f) => panic!("unexpectedly parsed {f:?}"),
            Err(e) => e.iter().map(ToString::to_string).collect(),
        }
    }

    fn job(id: &str, spec: JobSpec, created_ms: i64) -> Job {
        Job::new(
            id.parse().unwrap(),
            spec,
            Timestamp::from_millis(created_ms),
        )
    }

    #[test]
    fn base32_round_trips_every_length() {
        let data: Vec<u8> = (0u8..=40).map(|i| i.wrapping_mul(37)).collect();
        for len in 0..data.len() {
            let text = base32_encode(&data[..len]);
            assert_eq!(text.len(), (len * 8).div_ceil(5));
            assert_eq!(base32_decode(&text).as_deref(), Some(&data[..len]));
            assert_eq!(
                base32_decode(&text.to_ascii_uppercase()).as_deref(),
                Some(&data[..len])
            );
        }
        // "1" carries a set padding bit: not a canonical encoding of anything.
        assert_eq!(base32_decode("01"), None);
        assert_eq!(base32_decode("0u"), None, "u is not in the alphabet");
    }

    #[test]
    fn cursors_round_trip_every_key_kind() {
        let id: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap();
        for cursor in [
            Cursor::after_job(id),
            Cursor::from_name("nightly-report"),
            Cursor::from_name(""),
            Cursor::from_seq(u64::MAX),
        ] {
            let text = cursor.to_string();
            assert!(text.bytes().all(|b| BASE32.contains(&b)), "{text}");
            assert_eq!(text.parse::<Cursor>(), Ok(cursor.clone()));
            assert_eq!(Cursor::from_json(&cursor.to_json()), Ok(cursor));
        }
        assert_eq!(Cursor::after_job(id).job_id(), Some(id));
        assert_eq!(Cursor::from_name("q").as_name(), Some("q"));
        assert_eq!(Cursor::from_seq(9).as_seq(), Some(9));
        assert_eq!(Cursor::from_seq(9).job_id(), None);
        assert_eq!(
            Cursor::from_name(&"é".repeat(100)).as_name().map(str::len),
            Some(128)
        );
    }

    #[test]
    fn tampered_cursors_are_rejected() {
        let text = Cursor::from_seq(41).to_string();
        let mut chars: Vec<char> = text.chars().collect();
        let i = chars.len() / 2;
        chars[i] = if chars[i] == '0' { '1' } else { '0' };
        let tampered: String = chars.into_iter().collect();
        assert_eq!(
            tampered.parse::<Cursor>().unwrap_err().to_string(),
            format!("invalid cursor {tampered:?}: not a cursor issued by this server")
        );
        assert!(
            text[..text.len() - 2].parse::<Cursor>().is_err(),
            "truncated"
        );
        assert!("".parse::<Cursor>().is_err());
        assert!("z".repeat(300).parse::<Cursor>().is_err());
    }

    #[test]
    fn full_filter_round_trips_through_query_pairs() {
        let filter = JobFilter::new()
            .queue(QueueName::from_static("emails"))
            .state(JobState::Ready)
            .state(JobState::Running)
            .kind("send_welcome")
            .tag("signup")
            .tenant("acme".parse().unwrap())
            .created_between(
                Some(Timestamp::from_millis(1_000)),
                Some(Timestamp::from_millis(2_000)),
            )
            .order(SortOrder::Asc)
            .after(Cursor::after_job(
                "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap(),
            ))
            .limit(25);
        let q = filter.to_query();
        let keys: Vec<&str> = q.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, JobFilter::QUERY_KEYS);
        assert_eq!(q[5].1, "1970-01-01T00:00:01.000Z");
        assert_eq!(JobFilter::from_query(&q), Ok(filter));
        assert!(JobFilter::new().to_query().is_empty());
        assert_eq!(JobFilter::from_query(&[]), Ok(JobFilter::new()));
    }

    #[test]
    fn query_accepts_repeats_blanks_and_offsets() {
        let f = JobFilter::from_query(&pairs(&[
            ("state", "dead"),
            ("state", "cancelled,"),
            ("kind", ""),
            ("created_after", "2025-01-01T05:00:00+05:00"),
        ]))
        .unwrap();
        assert_eq!(f.states, [JobState::Dead, JobState::Cancelled]);
        assert_eq!(f.kind, None);
        assert_eq!(
            f.created_after.map(|t| t.to_rfc3339()).as_deref(),
            Some("2025-01-01T00:00:00.000Z")
        );
    }

    #[test]
    fn query_errors_name_each_parameter() {
        let job_cursor = Cursor::from_name("q").to_string();
        assert_eq!(
            lines(JobFilter::from_query(&pairs(&[
                ("queue", "emails"),
                ("queue", "sms"),
                ("state", "redy"),
                ("kind", "a b"),
                ("created_after", "2025-02-01T00:00:00Z"),
                ("created_before", "2025-01-01T00:00:00Z"),
                ("order", "newest"),
                ("limit", "0"),
                ("cursor", &job_cursor),
                ("ordr", "asc"),
            ]))),
            [
                r#"$: unknown query parameter "ordr", did you mean "order"?"#,
                "$.queue: may be given only once",
                r#"$.state: unknown job state "redy", did you mean "ready"?"#,
                "$.kind: ' ' at position 1 is not allowed (use letters, digits, '_', '.', ':', '/' or '-')",
                r#"$.order: unknown sort order "newest", expected one of: asc, desc"#,
                "$.created_before: must be after created_after (2025-02-01T00:00:00.000Z)",
                "$.limit: must be between 1 and 1000, got 0",
                "$.cursor: was not issued by a job listing",
            ]
        );
        assert_eq!(
            lines(JobFilter::from_query(&pairs(&[
                ("created_after", "yesterday"),
                ("limit", "ten")
            ]))),
            [
                r#"$.created_after: invalid timestamp "yesterday": expected 4-digit year at byte 0"#,
                r#"$.limit: invalid number "ten": invalid digit found in string"#,
            ]
        );
    }

    #[test]
    fn matches_checks_every_criterion() {
        let spec = JobSpec::new("emails", "send")
            .tag("signup")
            .tenant("acme".parse().unwrap());
        let j = job("01HQ3K9Z8N6V2T4R5S7W0X1Y2Z", spec, 1_500);
        assert!(JobFilter::new().matches(&j));
        let all = JobFilter::new()
            .queue(QueueName::from_static("emails"))
            .state(JobState::Ready)
            .kind("send")
            .tag("signup")
            .tenant("acme".parse().unwrap())
            .created_between(
                Some(Timestamp::from_millis(1_000)),
                Some(Timestamp::from_millis(2_000)),
            );
        assert!(all.matches(&j));
        let misses = [
            JobFilter::new().queue(QueueName::from_static("sms")),
            JobFilter::new().state(JobState::Dead),
            JobFilter::new().kind("other"),
            JobFilter::new().tag("other"),
            JobFilter::new().tenant("globex".parse().unwrap()),
            JobFilter::new().created_between(Some(Timestamp::from_millis(1_500)), None),
            JobFilter::new().created_between(None, Some(Timestamp::from_millis(1_500))),
        ];
        for f in misses {
            assert!(!f.matches(&j), "{f:?}");
        }
    }

    #[test]
    fn cursor_position_depends_on_order() {
        let a: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2A".parse().unwrap();
        let b: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2B".parse().unwrap();
        let c: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2C".parse().unwrap();
        let desc = JobFilter::new().after(Cursor::after_job(b));
        assert!(desc.past_cursor(a) && !desc.past_cursor(b) && !desc.past_cursor(c));
        let asc = desc.clone().order(SortOrder::Asc);
        assert!(!asc.past_cursor(a) && !asc.past_cursor(b) && asc.past_cursor(c));
        assert!(JobFilter::new().past_cursor(a));
    }

    #[test]
    fn effective_limit_clamps() {
        assert_eq!(JobFilter::new().effective_limit(), DEFAULT_PAGE_LIMIT);
        assert_eq!(JobFilter::new().limit(0).effective_limit(), 1);
        assert_eq!(
            JobFilter::new().limit(5_000).effective_limit(),
            MAX_PAGE_LIMIT
        );
    }
}
