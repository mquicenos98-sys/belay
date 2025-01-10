//! Semantic validation: the rules that go beyond a value's JSON shape.
//!
//! Decoding (`FromJson`) checks structure and syntax: required fields, types,
//! identifier formats, duration strings. [`Validate`] checks policy: ranges,
//! limits from [`limits`](crate::limits), cross-field rules, workflow graphs.
//! The two are kept apart on purpose. Stored records are decoded on every
//! restart, and a limit tightened in a later release must not make old jobs
//! unreadable; it should only stop new ones from being accepted. Servers call
//! [`Validate::validate`] on every request body (see
//! [`api::decode_request`](crate::api::decode_request)); embedded callers
//! should do the same before handing a spec to the engine.
//!
//! A [`Validator`] collects every problem instead of stopping at the first,
//! so a client fixing a request sees all of them at once. Each problem has a
//! path in the same `$.jobs[2].priority` notation that belay-json uses for
//! decoding errors.

use std::fmt;
use std::time::Duration;

use belay_common::format_duration;
use belay_json::{FromJsonError, PathSegment};

use crate::ids::NameRule;

/// Problems beyond this many are counted but not kept, so a batch of a
/// thousand broken jobs produces a bounded error response.
pub const MAX_REPORTED_VIOLATIONS: usize = 100;

/// A single broken rule, with the path to the offending value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    // FromJsonError already carries a path and renders it in the notation
    // used across belay; reusing it keeps the two error kinds identical.
    inner: FromJsonError,
}

impl Violation {
    fn new(path: &[PathSegment], message: String) -> Violation {
        let mut inner = FromJsonError::new(message);
        for segment in path.iter().rev() {
            inner = match segment {
                PathSegment::Key(k) => inner.at_key(k.clone()),
                PathSegment::Index(i) => inner.at_index(*i),
            };
        }
        Violation { inner }
    }

    /// The path in `$.steps[1].depends_on[0]` notation.
    #[must_use]
    pub fn path(&self) -> String {
        self.inner.path_string()
    }

    /// The path as an RFC 6901 JSON Pointer (`/steps/1/depends_on/0`).
    #[must_use]
    pub fn pointer(&self) -> String {
        self.inner.pointer()
    }

    /// The message without the path.
    #[must_use]
    pub fn message(&self) -> &str {
        self.inner.message()
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl From<FromJsonError> for Violation {
    fn from(inner: FromJsonError) -> Violation {
        Violation { inner }
    }
}

/// One or more violations. Never empty.
///
/// `Display` shows the first problem and how many others there are:
/// `$.priority: must be between -1000 and 1000, got 5000 (and 2 more
/// problems)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violations {
    list: Vec<Violation>,
    /// Problems found beyond [`MAX_REPORTED_VIOLATIONS`].
    omitted: usize,
}

impl Violations {
    /// A single violation at `path`, for callers outside a [`Validator`]
    /// such as query-string parsers.
    #[must_use]
    pub fn single(path: &[PathSegment], message: impl fmt::Display) -> Violations {
        Violations {
            list: vec![Violation::new(path, message.to_string())],
            omitted: 0,
        }
    }

    /// The first problem found.
    #[must_use]
    pub fn first(&self) -> &Violation {
        // Invariant: constructed only with at least one entry.
        &self.list[0]
    }

    /// The kept problems, in the order they were found.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Violation> {
        self.list.iter()
    }

    /// Total number of problems, including ones not kept.
    #[must_use]
    pub fn len(&self) -> usize {
        self.list.len() + self.omitted
    }

    /// Always false; present for API symmetry with `len`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl fmt::Display for Violations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.first())?;
        match self.len() - 1 {
            0 => Ok(()),
            1 => f.write_str(" (and 1 more problem)"),
            n => write!(f, " (and {n} more problems)"),
        }
    }
}

impl std::error::Error for Violations {}

impl From<FromJsonError> for Violations {
    fn from(e: FromJsonError) -> Violations {
        Violations {
            list: vec![e.into()],
            omitted: 0,
        }
    }
}

impl<'a> IntoIterator for &'a Violations {
    type Item = &'a Violation;
    type IntoIter = std::slice::Iter<'a, Violation>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter()
    }
}

/// Collects violations while walking a value.
///
/// ```
/// use belay_proto::validate::Validator;
///
/// let mut v = Validator::new();
/// v.at_key("unique", |v| v.check(false, "key", || "must not be empty".to_owned()));
/// v.at_index("tags", 3, |v| v.report_here("duplicate tag \"eu\""));
/// let problems = v.finish().unwrap_err();
/// let lines: Vec<String> = problems.iter().map(ToString::to_string).collect();
/// assert_eq!(lines, [
///     "$.unique.key: must not be empty",
///     "$.tags[3]: duplicate tag \"eu\"",
/// ]);
/// ```
#[derive(Debug, Default)]
pub struct Validator {
    path: Vec<PathSegment>,
    found: Vec<Violation>,
    omitted: usize,
}

impl Validator {
    /// A validator at the document root.
    #[must_use]
    pub fn new() -> Validator {
        Validator::default()
    }

    /// Runs `f` with `key` appended to the current path.
    pub fn at_key<R>(&mut self, key: &str, f: impl FnOnce(&mut Validator) -> R) -> R {
        self.path.push(PathSegment::Key(key.to_owned()));
        let out = f(self);
        self.path.pop();
        out
    }

    /// Runs `f` at `key[index]`.
    pub fn at_index<R>(
        &mut self,
        key: &str,
        index: usize,
        f: impl FnOnce(&mut Validator) -> R,
    ) -> R {
        self.at_key(key, |v| {
            v.path.push(PathSegment::Index(index));
            let out = f(v);
            v.path.pop();
            out
        })
    }

    /// Records a problem at the current path.
    pub fn report_here(&mut self, message: impl fmt::Display) {
        if self.found.len() < MAX_REPORTED_VIOLATIONS {
            self.found
                .push(Violation::new(&self.path, message.to_string()));
        } else {
            self.omitted += 1;
        }
    }

    /// Records a problem with the field `key` of the current value.
    pub fn report(&mut self, key: &str, message: impl fmt::Display) {
        self.at_key(key, |v| v.report_here(message));
    }

    /// Records the message produced by `message` unless `ok` holds. The
    /// message is only built on failure.
    pub fn check(&mut self, ok: bool, key: &str, message: impl FnOnce() -> String) {
        if !ok {
            self.report(key, message());
        }
    }

    /// Validates a nested value under `key`.
    pub fn nested<T: Validate + ?Sized>(&mut self, key: &str, value: &T) {
        self.at_key(key, |v| value.validate_into(v));
    }

    /// Whether nothing has been reported so far.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.found.is_empty()
    }

    /// Number of problems reported so far.
    #[must_use]
    pub fn count(&self) -> usize {
        self.found.len() + self.omitted
    }

    /// `Ok` if nothing was reported.
    pub fn finish(self) -> Result<(), Violations> {
        if self.found.is_empty() {
            Ok(())
        } else {
            Err(Violations {
                list: self.found,
                omitted: self.omitted,
            })
        }
    }
}

/// Checks shared by the `Validate` impls in this crate.
impl Validator {
    /// `key` must satisfy a name rule.
    pub(crate) fn check_name(&mut self, key: &str, value: &str, rule: NameRule) {
        if let Some(problem) = rule.problem(value) {
            self.report(key, problem);
        }
    }

    /// `key` must lie in `min..=max`.
    pub(crate) fn check_duration(&mut self, key: &str, d: Duration, min: Duration, max: Duration) {
        if d < min || d > max {
            let message = if min.is_zero() {
                format!(
                    "must be at most {}, got {}",
                    format_duration(max),
                    format_duration(d)
                )
            } else {
                format!(
                    "must be between {} and {}, got {}",
                    format_duration(min),
                    format_duration(max),
                    format_duration(d)
                )
            };
            self.report(key, message);
        }
    }

    /// `key` (a count, length or number) must lie in `min..=max`.
    pub(crate) fn check_range<T>(&mut self, key: &str, value: T, min: T, max: T)
    where
        T: PartialOrd + fmt::Display,
    {
        if value < min || value > max {
            self.report(key, format!("must be between {min} and {max}, got {value}"));
        }
    }

    /// `key` may hold at most `max` bytes of text.
    pub(crate) fn check_bytes(&mut self, key: &str, value: &str, max: usize) {
        if value.len() > max {
            self.report(
                key,
                format!("must be at most {max} bytes, got {}", value.len()),
            );
        }
    }
}

/// Types with semantic rules beyond their JSON shape.
pub trait Validate {
    /// Reports every problem into `v`, relative to its current path.
    fn validate_into(&self, v: &mut Validator);

    /// Checks all rules, returning every problem found.
    fn validate(&self) -> Result<(), Violations> {
        let mut v = Validator::new();
        self.validate_into(&mut v);
        v.finish()
    }
}

impl<T: Validate + ?Sized> Validate for &T {
    fn validate_into(&self, v: &mut Validator) {
        (**self).validate_into(v);
    }
}

impl<T: Validate> Validate for [T] {
    fn validate_into(&self, v: &mut Validator) {
        for (i, item) in self.iter().enumerate() {
            v.path.push(PathSegment::Index(i));
            item.validate_into(v);
            v.path.pop();
        }
    }
}

impl<T: Validate> Validate for Vec<T> {
    fn validate_into(&self, v: &mut Validator) {
        self.as_slice().validate_into(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Range(i32);

    impl Validate for Range {
        fn validate_into(&self, v: &mut Validator) {
            v.check((0..=10).contains(&self.0), "n", || {
                format!("must be between 0 and 10, got {}", self.0)
            });
        }
    }

    #[test]
    fn nested_paths_and_list_indices_compose() {
        let items = vec![Range(1), Range(11), Range(-1)];
        let mut v = Validator::new();
        v.nested("items", &items);
        assert_eq!(v.count(), 2);
        let err = v.finish().unwrap_err();
        let lines: Vec<String> = err.iter().map(ToString::to_string).collect();
        assert_eq!(
            lines,
            [
                "$.items[1].n: must be between 0 and 10, got 11",
                "$.items[2].n: must be between 0 and 10, got -1"
            ]
        );
        assert_eq!(err.first().pointer(), "/items/1/n");
        assert_eq!(err.first().path(), "$.items[1].n");
        assert_eq!(err.first().message(), "must be between 0 and 10, got 11");
        assert_eq!(
            err.to_string(),
            "$.items[1].n: must be between 0 and 10, got 11 (and 1 more problem)"
        );
    }

    #[test]
    fn keys_that_are_not_identifiers_are_quoted() {
        let mut v = Validator::new();
        v.at_key("meta", |v| v.report("belay.schedule", "reserved key"));
        assert_eq!(
            v.finish().unwrap_err().to_string(),
            r#"$.meta["belay.schedule"]: reserved key"#
        );
    }

    #[test]
    fn report_count_is_capped_but_total_is_kept() {
        let many: Vec<Range> = (0..250).map(|_| Range(99)).collect();
        let err = many.validate().unwrap_err();
        assert_eq!(err.iter().len(), MAX_REPORTED_VIOLATIONS);
        assert_eq!(err.len(), 250);
        assert!(err.to_string().ends_with("(and 249 more problems)"));
    }

    #[test]
    fn clean_values_pass() {
        assert!(Range(5).validate().is_ok());
        assert!(Vec::<Range>::new().validate().is_ok());
        let v = Validator::new();
        assert!(v.is_clean());
        assert!(v.finish().is_ok());
    }

    #[test]
    fn single_builds_a_violation_outside_a_validator() {
        let err = Violations::single(&[PathSegment::Key("limit".into())], "must be at least 1");
        assert_eq!(err.to_string(), "$.limit: must be at least 1");
        assert_eq!(err.len(), 1);
    }
}
