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

/// Workflow step names: `[a-z0-9][a-z0-9_-]{0,63}`. No dots, because step
/// names appear in `{{steps.<name>.result}}` placeholders.
pub(crate) const STEP_RULE: NameRule = NameRule {
    max: 64,
    first: lower_or_digit,
    first_desc: "a lowercase letter or digit",
    rest: |b| lower_or_digit(b) || matches!(b, b'_' | b'-'),
    allowed: "a-z, 0-9, '_' or '-'",
};

/// Free-form labels (job kinds, tags, meta keys, workflow names, error
/// kinds); the length limit is set per use with [`NameRule::max`].
pub(crate) const LABEL_RULE: NameRule = NameRule {
    max: 64,
    first: alnum,
    first_desc: "a letter or digit",
    rest: |b| alnum(b) || matches!(b, b'_' | b'.' | b':' | b'/' | b'-'),
    allowed: "letters, digits, '_', '.', ':', '/' or '-'",
};

const WORKER_RULE: NameRule = NameRule {
    max: 128,
    first: alnum,
    first_desc: "a letter or digit",
    rest: |b| alnum(b) || matches!(b, b'_' | b'.' | b':' | b'@' | b'/' | b'-'),
    allowed: "letters, digits, '_', '.', ':', '@', '/' or '-'",
};

const TENANT_RULE: NameRule = NameRule {
    max: 64,
    first: alnum,
    first_desc: "a letter or digit",
    rest: |b| alnum(b) || matches!(b, b'_' | b'.' | b':' | b'-'),
    allowed: "letters, digits, '_', '.', ':' or '-'",
};

macro_rules! ulid_id {
    ($(#[$meta:meta])* $name:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(Ulid);

        impl $name {
            /// Wraps a ULID.
            #[must_use]
            pub const fn from_ulid(id: Ulid) -> $name {
                $name(id)
            }

            /// The underlying ULID.
            #[must_use]
            pub const fn ulid(self) -> Ulid {
                self.0
            }

            /// When the id was minted: the ULID's millisecond timestamp.
            #[must_use]
            pub fn timestamp(self) -> Timestamp {
                self.0.timestamp()
            }

            /// A fresh id from the process-wide monotonic generator.
            /// Engines driven by a manual clock mint ids from their own
            /// generator so that ids follow the test clock.
            #[must_use]
            pub fn generate() -> $name {
                $name(Ulid::generate())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = ParseError;

            fn from_str(s: &str) -> Result<$name, ParseError> {
                Ulid::parse(s).map($name).map_err(|e| {
                    let detail = e.to_string();
                    let detail = detail.strip_prefix("invalid ULID: ").unwrap_or(&detail);
                    ParseError::invalid($what, s, detail)
                })
            }
        }

        impl From<Ulid> for $name {
            fn from(id: Ulid) -> $name {
                $name(id)
            }
        }

        impl From<$name> for Ulid {
            fn from(id: $name) -> Ulid {
                id.0
            }
        }

        text_json!($name, $what);
    };
}

ulid_id!(
    /// Identifies a job. Ids are ULIDs, so they sort in creation order and a
    /// store scan over them walks jobs oldest first.
    JobId,
    "job id"
);

ulid_id!(
    /// Identifies a workflow run.
    WorkflowId,
    "workflow id"
);

macro_rules! name_id {
    ($(#[$meta:meta])* $name:ident, $what:literal, $rule:expr) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps `name`.
            pub fn new(name: impl Into<String>) -> Result<$name, ParseError> {
                let name = name.into();
                $rule.check($what, &name)?;
                Ok($name(name))
            }

            /// Wraps a literal.
            ///
            /// # Panics
            ///
            /// If `name` is invalid. Meant for names written in source code;
            /// use [`new`](Self::new) or `parse` for anything read at runtime.
            #[must_use]
            pub fn from_static(name: &'static str) -> $name {
                match $name::new(name) {
                    Ok(id) => id,
                    Err(e) => panic!("{e}"),
                }
            }

            /// The name as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The name as an owned string.
            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = ParseError;

            fn from_str(s: &str) -> Result<$name, ParseError> {
                $name::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ParseError;

            fn try_from(s: String) -> Result<$name, ParseError> {
                $name::new(s)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ParseError;

            fn try_from(s: &str) -> Result<$name, ParseError> {
                $name::new(s)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        /// Lets maps keyed by this type be queried with a `&str`.
        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        text_json!($name, $what);
    };
}

name_id!(
    /// A queue name: 1-64 characters from `a-z 0-9 _ . : -`, starting with a
    /// letter or digit. Colons allow namespacing (`billing:invoices`).
    ///
    /// ```
    /// use belay_proto::QueueName;
    ///
    /// let q: QueueName = "billing:invoices".parse().unwrap();
    /// assert_eq!(q, "billing:invoices");
    /// let err = "Emails".parse::<QueueName>().unwrap_err();
    /// assert_eq!(
    ///     err.to_string(),
    ///     r#"invalid queue name "Emails": must start with a lowercase letter or digit, got 'E'"#
    /// );
    /// ```
    QueueName,
    "queue name",
    QUEUE_RULE
);

name_id!(
    /// A schedule id: a slug of 1-128 characters from `a-z 0-9 _ . -`.
    ScheduleId,
    "schedule id",
    SLUG_RULE
);

name_id!(
    /// A worker id, typically `host:pid` or a pod name: 1-128 characters from
    /// letters, digits and `_ . : @ / -`.
    WorkerId,
    "worker id",
    WORKER_RULE
);

name_id!(
    /// A tenant id used for fair sharing within a queue: 1-64 characters
    /// from letters, digits and `_ . : -`.
    TenantId,
    "tenant id",
    TENANT_RULE
);

/// Proof of holding a job's current lease. Every worker call about a leased
/// job presents it; a stale token (the lease expired and the job was handed
/// to someone else) is rejected with `lease_lost`.
///
/// Tokens are random, never zero, and written as 16 lowercase hex digits
/// (a JSON string, since JSON numbers above 2^53 do not survive every
/// parser).
///
/// Zero is [`LeaseToken::REDACTED`]: read APIs replace the token with it
/// (see [`Job::redacted`](crate::Job::redacted)) so that clients with read
/// access cannot act on another worker's lease, and it is omitted from JSON.
/// Because fresh tokens are never zero, a redacted token never matches
/// a live lease.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LeaseToken(u64);

impl LeaseToken {
    /// The placeholder for a token that has been withheld.
    pub const REDACTED: LeaseToken = LeaseToken(0);

    /// Wraps a raw value.
    #[must_use]
    pub const fn new(value: u64) -> LeaseToken {
        LeaseToken(value)
    }

    /// A random, non-zero token.
    pub fn generate(rng: &mut Rng) -> LeaseToken {
        loop {
            let value = rng.next_u64();
            if value != 0 {
                return LeaseToken(value);
            }
        }
    }

    /// The raw value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Whether this is [`REDACTED`](Self::REDACTED).
    #[must_use]
    pub const fn is_redacted(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for LeaseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl fmt::Debug for LeaseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LeaseToken({self})")
    }
}

impl FromStr for LeaseToken {
    type Err = ParseError;

    /// Exactly 16 hex digits, either case.
    fn from_str(s: &str) -> Result<LeaseToken, ParseError> {
        if s.len() != 16 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseError::invalid(
                "lease token",
                s,
                "expected 16 hex digits",
            ));
        }
        u64::from_str_radix(s, 16)
            .map(LeaseToken)
            .map_err(|e| ParseError::invalid("lease token", s, e))
    }
}

text_json!(LeaseToken, "lease token");

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::QueueName {}
    impl Sealed for &crate::QueueName {}
    impl Sealed for &'static str {}
}

/// Arguments accepted where a queue name is expected in builders:
/// a [`QueueName`], a reference to one, or a string literal.
///
/// String literals are checked when converted and panic if invalid, like
/// [`QueueName::from_static`]. Only `&'static str` is accepted so that names
/// read at runtime go through `parse` and get a proper error.
pub trait IntoQueueName: sealed::Sealed {
    /// Performs the conversion.
    fn into_queue_name(self) -> QueueName;
}

impl IntoQueueName for QueueName {
    fn into_queue_name(self) -> QueueName {
        self
    }
}

impl IntoQueueName for &QueueName {
    fn into_queue_name(self) -> QueueName {
        self.clone()
    }
}

impl IntoQueueName for &'static str {
    fn into_queue_name(self) -> QueueName {
        QueueName::from_static(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::{json, FromJson, ToJson};
    use std::collections::HashMap;

    fn queue_err(s: &str) -> String {
        s.parse::<QueueName>().unwrap_err().to_string()
    }

    #[test]
    fn queue_names_follow_the_documented_charset() {
        for ok in [
            "emails",
            "0",
            "billing:invoices",
            "a.b_c-d",
            &"q".repeat(64),
        ] {
            assert!(ok.parse::<QueueName>().is_ok(), "{ok}");
        }
        assert_eq!(queue_err(""), r#"invalid queue name "": must not be empty"#);
        assert_eq!(
            queue_err(&"q".repeat(71)),
            format!(
                r#"invalid queue name "{}...": must be 1-64 characters, got 71"#,
                "q".repeat(61)
            )
        );
        assert_eq!(
            queue_err("-emails"),
            r#"invalid queue name "-emails": must start with a lowercase letter or digit, got '-'"#
        );
        assert_eq!(
            queue_err("em ails"),
            r#"invalid queue name "em ails": ' ' at position 2 is not allowed (use a-z, 0-9, '_', '.', ':' or '-')"#
        );
        assert!(queue_err("emAils").contains("'A' at position 2"));
        assert!(queue_err("emé").contains("'é' at position 2"));
    }

    #[test]
    fn rejects_queue_names_with_uppercase() {
        assert!("Emails".parse::<QueueName>().is_err());
        assert!("emails".parse::<QueueName>().is_ok());
    }

    #[test]
    fn schedule_ids_disallow_colons_but_worker_ids_allow_hosts() {
        assert!("nightly.report-v2".parse::<ScheduleId>().is_ok());
        assert!("nightly:report".parse::<ScheduleId>().is_err());
        assert!("web-7f9c:4121".parse::<WorkerId>().is_ok());
        assert!("svc@Host.example/7".parse::<WorkerId>().is_ok());
        assert!("worker 1".parse::<WorkerId>().is_err());
        assert!("Acme_Corp".parse::<TenantId>().is_ok());
        assert!("acme/corp".parse::<TenantId>().is_err());
    }

    #[test]
    fn name_ids_behave_like_strings() {
        let q = QueueName::from_static("emails");
        assert_eq!(q.as_str(), "emails");
        assert_eq!(format!("[{q:>8}]"), "[  emails]");
        assert_eq!(format!("{q:?}"), r#"QueueName("emails")"#);
        let mut depth: HashMap<QueueName, u32> = HashMap::new();
        depth.insert(q.clone(), 3);
        assert_eq!(depth.get("emails"), Some(&3));
        assert_eq!(String::from(q.clone()), "emails");
        assert_eq!(QueueName::try_from("emails".to_owned()), Ok(q));
    }

    #[test]
    #[should_panic(expected = "invalid queue name \"Bad\"")]
    fn from_static_panics_on_invalid_literals() {
        let _ = QueueName::from_static("Bad");
    }

    #[test]
    fn ulid_ids_round_trip_and_explain_errors() {
        let id: JobId = "01ARYZ6S41TSV4RRFFQ69G5FAV".parse().unwrap();
        assert_eq!(id.to_string(), "01ARYZ6S41TSV4RRFFQ69G5FAV");
        assert_eq!(format!("{id:?}"), "JobId(01ARYZ6S41TSV4RRFFQ69G5FAV)");
        assert_eq!(id.timestamp().to_rfc3339(), "2016-07-30T22:36:16.385Z");
        assert_eq!(JobId::from_json(&id.to_json()), Ok(id));
        assert_eq!(
            "nope".parse::<JobId>().unwrap_err().to_string(),
            r#"invalid job id "nope": expected 26 characters, got 4"#
        );
        let err = WorkflowId::from_json(&json!(42)).unwrap_err();
        assert_eq!(err.to_string(), "$: expected workflow id string, found 42");
        assert!(JobId::generate() < JobId::generate());
    }

    #[test]
    fn lease_tokens_are_sixteen_hex_digits() {
        let t = LeaseToken::new(0xdead_beef);
        assert_eq!(t.to_string(), "00000000deadbeef");
        assert_eq!("00000000DEADBEEF".parse::<LeaseToken>(), Ok(t));
        assert_eq!(t.to_json(), json!("00000000deadbeef"));
        assert_eq!(
            "deadbeef".parse::<LeaseToken>().unwrap_err().to_string(),
            r#"invalid lease token "deadbeef": expected 16 hex digits"#
        );
        assert!("+000000000000001".parse::<LeaseToken>().is_err());
        assert!(LeaseToken::REDACTED.is_redacted());
    }

    #[test]
    fn fresh_lease_tokens_are_never_redacted() {
        let mut rng = Rng::seed_from_u64(7);
        for _ in 0..1000 {
            assert!(!LeaseToken::generate(&mut rng).is_redacted());
        }
    }

    #[test]
    fn into_queue_name_accepts_names_and_literals() {
        fn take(q: impl IntoQueueName) -> QueueName {
            q.into_queue_name()
        }
        let q = QueueName::from_static("a");
        assert_eq!(take(&q), q);
        assert_eq!(take("a"), q);
        assert_eq!(take(q.clone()), q);
    }

    #[test]
    fn json_errors_carry_the_parse_message() {
        let err = TenantId::from_json(&json!("")).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$: invalid tenant id "": must not be empty"#
        );
    }
}
