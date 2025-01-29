//! Environment overlays: `BELAY_SERVER__LISTEN=0.0.0.0:7700` overrides
//! `server.listen` in a [`Document`].
//!
//! After the prefix and one `_`, double underscores separate path segments
//! and single underscores stay part of the key, so
//! `BELAY_STORAGE__CHECKPOINT_BYTES` is `storage.checkpoint_bytes` and
//! `BELAY_QUEUES__EMAILS__MAX_CONCURRENCY` is
//! `queues.emails.max_concurrency`. Segments are lowercased; a segment of
//! digits selects an existing array element (`BELAY_SCHEDULES__0__PAUSED`).
//!
//! Variables with a single segment (`BELAY_URL`, `BELAY_TOKEN`) are ignored
//! by default: such names are commonly used by clients rather than being
//! configuration, and belay's configuration has no top-level scalars.

use crate::error::{Error, Result};
use crate::parse::lexer::classify_scalar;
use crate::path::{KeyPath, Segment};
use crate::source::Loc;
use crate::value::Value;
use crate::Document;

/// Applies environment variables with a common prefix onto a document.
///
/// ```
/// use belay_conf::{Document, Overlay};
///
/// let mut doc = Document::parse("[server]\nthreads = 4\n")?;
/// let vars = [("BELAY_SERVER__THREADS", "16"), ("BELAY_SERVER__LISTEN", "0.0.0.0:7700")];
/// let applied = Overlay::new("BELAY").apply(&mut doc, vars)?;
/// assert_eq!(applied.len(), 2);
/// assert_eq!(doc.get_int("server.threads")?, 16);
/// assert_eq!(doc.get_str("server.listen")?, "0.0.0.0:7700");
/// # Ok::<(), belay_conf::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    prefix: String,
    top_level: bool,
}

/// One variable that was applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The variable name.
    pub var: String,
    /// The path it set.
    pub path: KeyPath,
    /// The value it replaced, if any.
    pub previous: Option<Value>,
}

impl Overlay {
    /// An overlay for variables named `{prefix}_...`.
    pub fn new(prefix: &str) -> Overlay {
        Overlay {
            prefix: format!("{prefix}_"),
            top_level: false,
        }
    }

    /// Also apply single-segment variables (`{prefix}_NAME` sets `name`).
    #[must_use]
    pub fn allow_top_level(mut self, allow: bool) -> Overlay {
        self.top_level = allow;
        self
    }

    /// The path a variable maps to, or `None` if the overlay ignores it.
    pub fn path_for(&self, var: &str) -> Option<KeyPath> {
        self.map_var(var).ok().flatten()
    }

    fn map_var(&self, var: &str) -> std::result::Result<Option<KeyPath>, String> {
        let Some(rest) = var.strip_prefix(&self.prefix) else {
            return Ok(None);
        };
        if rest.is_empty() || (!self.top_level && !rest.contains("__")) {
            return Ok(None);
        }
        let mut path = KeyPath::root();
        for (i, seg) in rest.split("__").enumerate() {
            if seg.is_empty() {
                return Err("empty key segment in variable name".into());
            }
            match seg.parse::<usize>() {
                Ok(n) if i > 0 => path.push(Segment::Index(n)),
                _ => path.push(Segment::Key(seg.to_ascii_lowercase())),
            }
        }
        Ok(Some(path))
    }

    /// Applies matching variables from the process environment.
    pub fn apply_env(&self, doc: &mut Document) -> Result<Vec<Applied>> {
        let mut vars = Vec::new();
        for (name, value) in std::env::vars_os() {
            let Ok(name) = name.into_string() else {
                continue;
            };
            if !name.starts_with(&self.prefix) {
                continue;
            }
            let value = value.into_string().map_err(|_| Error::Env {
                var: name.clone(),
                message: "value is not valid UTF-8".into(),
            })?;
            vars.push((name, value));
        }
        self.apply(doc, vars)
    }

    /// Applies matching `(name, value)` pairs; others are ignored.
    ///
    /// Variables are applied in name order, so conflicting ones
    /// (`BELAY_A__B` and `BELAY_A__B__C`) fail the same way on every run
    /// regardless of environment order.
    pub fn apply<I, K, V>(&self, doc: &mut Document, vars: I) -> Result<Vec<Applied>>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut matched = Vec::new();
        for (name, raw) in vars {
            let var = name.as_ref().to_owned();
            match self.map_var(&var) {
                Ok(Some(path)) => matched.push((var, path, raw.as_ref().to_owned())),
                Ok(None) => {}
                Err(message) => return Err(Error::Env { var, message }),
            }
        }
        matched.sort_by(|a, b| a.0.cmp(&b.0));
        let mut applied = Vec::with_capacity(matched.len());
        for (var, path, raw) in matched {
            match apply_one(doc, &var, &path, &raw) {
                Ok(previous) => applied.push(Applied {
                    var,
                    path,
                    previous,
                }),
                Err(message) => return Err(Error::Env { var, message }),
            }
        }
        Ok(applied)
    }
}

fn apply_one(
    doc: &mut Document,
    var: &str,
    path: &KeyPath,
    raw: &str,
) -> std::result::Result<Option<Value>, String> {
    let existing = doc.lookup(path).map_err(|e| e.message())?;
    let value = infer(raw, existing);
    if matches!(existing, Some(Value::Table(_))) && !matches!(value, Value::Table(_)) {
        return Err(format!(
            "`{path}` is a table; only an inline table (`{{ ... }}`) can replace it"
        ));
    }
    doc.set_at(path, value, Some(Loc::Env(var.to_owned())))
}

/// Where the document already holds a string, unquoted text stays a
/// string, so `BELAY_SERVER__LISTEN=7700` cannot turn an address into an
/// integer. Otherwise see [`infer_value`].
fn infer(raw: &str, existing: Option<&Value>) -> Value {
    if raw.trim_start().starts_with(['[', '{', '"', '\'']) {
        if let Ok(value) = crate::parse_value(raw) {
            return value;
        }
    }
    if matches!(existing, Some(Value::String(_))) {
        return Value::String(raw.to_owned());
    }
    match classify_scalar(raw) {
        // `inf` and `nan` are far more likely meant as words.
        Ok(Value::Float(f)) if !f.is_finite() => Value::String(raw.to_owned()),
        Ok(value) => value,
        Err(_) => Value::String(raw.to_owned()),
    }
}

/// Infers a value from environment or command-line text, as [`Overlay`]
/// does for keys the document does not have yet: text that parses as a
/// TOML array, inline table or quoted string is that value; `true`/`false`,
/// integers, floats and date-times in TOML syntax are recognised; anything
/// else (including `[::1]:7700`) is a string.
///
/// ```
/// use belay_conf::{infer_value, Value};
///
/// assert_eq!(infer_value("16"), Value::Integer(16));
/// assert_eq!(infer_value("[::1]:7700"), Value::from("[::1]:7700"));
/// assert_eq!(infer_value("[\"a\", \"b\"]"), Value::from(vec!["a", "b"]));
/// ```
pub fn infer_value(raw: &str) -> Value {
    infer(raw, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str =
        "[server]\nlisten = \"127.0.0.1:7700\"\nmax_body = 1024\n[[schedules]]\nid = \"a\"\n";

    fn overlay(doc: &mut Document, vars: &[(&str, &str)]) -> Result<Vec<Applied>> {
        Overlay::new("BELAY").apply(doc, vars.iter().copied())
    }

    #[test]
    fn maps_variable_names_to_paths() {
        let o = Overlay::new("BELAY");
        let path = |v: &str| o.path_for(v).map(|p| p.to_string());
        let queues = path("BELAY_QUEUES__EMAILS__MAX_CONCURRENCY");
        assert_eq!(queues.as_deref(), Some("queues.emails.max_concurrency"));
        let schedule = path("BELAY_SCHEDULES__2__PAUSED");
        assert_eq!(schedule.as_deref(), Some("schedules[2].paused"));
        assert_eq!(path("BELAY_URL"), None);
        assert_eq!(path("OTHER_SERVER__LISTEN"), None);
        let top = o.clone().allow_top_level(true).path_for("BELAY_URL");
        assert_eq!(top.map(|p| p.to_string()).as_deref(), Some("url"));
    }

    #[test]
    fn infers_types_but_keeps_existing_strings_as_strings() {
        let mut doc = Document::parse(SRC).unwrap();
        let vars = [
            ("BELAY_SERVER__LISTEN", "7700"),
            ("BELAY_SERVER__THREADS", "8"),
            ("BELAY_STORAGE__IN_MEMORY", "true"),
            (
                "BELAY_CLUSTER__SEEDS",
                "[\"10.0.0.1:7946\", \"10.0.0.2:7946\"]",
            ),
            ("BELAY_METRICS__LABEL", "\"123\""),
        ];
        overlay(&mut doc, &vars).unwrap();
        assert_eq!(doc.get("server.listen"), Some(&Value::from("7700")));
        assert_eq!(doc.get("server.threads"), Some(&Value::Integer(8)));
        assert_eq!(doc.get("storage.in_memory"), Some(&Value::Boolean(true)));
        assert_eq!(doc.get_array("cluster.seeds").unwrap().len(), 2);
        assert_eq!(doc.get("metrics.label"), Some(&Value::from("123")));
    }

    #[test]
    fn records_the_variable_as_origin_and_reports_previous_values() {
        let mut doc = Document::parse(SRC).unwrap();
        let vars = [
            ("BELAY_SERVER__MAX_BODY", "2048"),
            ("BELAY_SCHEDULES__0__ID", "b"),
        ];
        let applied = overlay(&mut doc, &vars).unwrap();
        assert_eq!(applied[0].path.to_string(), "schedules[0].id");
        assert_eq!(applied[1].previous, Some(Value::Integer(1024)));
        let err = doc.get_bool("server.max_body").unwrap_err();
        assert_eq!(
            err.to_string(),
            "error: expected boolean at server.max_body, found integer\n \
             --> environment variable BELAY_SERVER__MAX_BODY"
        );
    }

    #[test]
    fn rejects_conflicting_and_malformed_variables() {
        let cases: [(&[(&str, &str)], &str); 4] = [
            (
                &[("BELAY_A__B", "1"), ("BELAY_A__B__C", "2")],
                "BELAY_A__B__C: expected table at a.b, found integer",
            ),
            (
                &[("BELAY_SERVER____LISTEN", "x")],
                "BELAY_SERVER____LISTEN: empty key segment in variable name",
            ),
            (
                &[("BELAY_SCHEDULES__0", "x")],
                "BELAY_SCHEDULES__0: `schedules[0]` is a table; only an inline table (`{ ... }`) can replace it",
            ),
            (
                &[("BELAY_SCHEDULES__5__ID", "x")],
                "BELAY_SCHEDULES__5__ID: index 5 is out of bounds; `schedules` has 1 element",
            ),
        ];
        for (vars, message) in cases {
            let mut doc = Document::parse(SRC).unwrap();
            let err = overlay(&mut doc, vars).unwrap_err();
            assert_eq!(
                err.message(),
                format!("cannot apply environment variable {message}")
            );
        }
    }

    #[test]
    fn infer_value_rules() {
        let cases = [
            ("false", Value::Boolean(false)),
            ("1_000", Value::Integer(1000)),
            ("-2.5", Value::Float(-2.5)),
            ("007", Value::from("007")),
            ("nan", Value::from("nan")),
            ("30s", Value::from("30s")),
            ("\"42\"", Value::from("42")),
            ("[1,", Value::from("[1,")),
            ("{ a = 1 }", Value::Table([("a", 1)].into_iter().collect())),
            ("2025-01-01", Value::Datetime("2025-01-01".into())),
        ];
        for (raw, value) in cases {
            assert_eq!(infer_value(raw), value, "{raw}");
        }
    }

    // Regression: bracketed IPv6 listen addresses were rejected as malformed arrays.
    #[test]
    fn bracketed_addresses_stay_strings() {
        let mut doc = Document::parse(SRC).unwrap();
        overlay(&mut doc, &[("BELAY_SERVER__ADMIN", "[::1]:7701")]).unwrap();
        assert_eq!(doc.get_str("server.admin").unwrap(), "[::1]:7701");
    }
}
