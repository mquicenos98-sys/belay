//! Reading what users hand the CLI besides flags: job payloads (inline
//! JSON, `@file` or `-` for stdin) and spec files for schedules, calendars,
//! workflows and queue configurations, in JSON or TOML.
//!
//! Spec files are decoded with the same `FromJson` and `Validate` code the
//! server uses, so a file that passes here is accepted by the API. TOML is
//! converted to JSON first (tables become objects, date-times strings),
//! which is how the configuration file reads the same types.

use std::path::Path;

use belay_json::{FromJson, Map, Number, Value};
use belay_proto::validate::Validate;

use crate::commands::Ctx;
use crate::error::{Error, Result};

/// Where a document comes from, for error messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Stdin,
    File(String),
    Inline,
}

impl Origin {
    fn describe(&self) -> String {
        match self {
            Origin::Stdin => "stdin".to_owned(),
            Origin::File(path) => path.clone(),
            Origin::Inline => "the command line".to_owned(),
        }
    }
}

/// Reads `path`, or stdin when it is `-`.
pub(crate) fn read_source(ctx: &Ctx<'_>, path: &str) -> Result<(Origin, String)> {
    let (origin, bytes) = if path == "-" {
        let bytes = ctx
            .read_stdin()
            .map_err(|e| Error::io("reading stdin", e))?;
        (Origin::Stdin, bytes)
    } else {
        let bytes = std::fs::read(path).map_err(|e| Error::io(format!("reading {path}"), e))?;
        (Origin::File(path.to_owned()), bytes)
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Input(format!("{} is not valid UTF-8", origin.describe())))?;
    Ok((origin, text))
}

/// A job payload from `--payload`: absent is `null`; `-` reads stdin;
/// `@path` reads a file; anything else is inline JSON.
pub(crate) fn payload(ctx: &Ctx<'_>, flag: Option<&str>) -> Result<Value> {
    let Some(text) = flag else {
        return Ok(Value::Null);
    };
    let (origin, source) = if text == "-" {
        read_source(ctx, "-")?
    } else if let Some(path) = text.strip_prefix('@') {
        read_source(ctx, path)?
    } else {
        (Origin::Inline, text.to_owned())
    };
    belay_json::parse(&source).map_err(|e| match origin {
        Origin::Inline => Error::Usage(
            ctx.usage(format!("invalid value for --payload: invalid JSON: {e}"))
                .with_hint(
                    "payloads are JSON: quote strings ('\"hello\"'), or use @file or - for stdin",
                ),
        ),
        other => Error::Input(format!(
            "payload from {} is not valid JSON: {e}",
            other.describe()
        )),
    })
}

/// Whether text from `origin` should be read as TOML: by extension for
/// files, by sniffing for stdin (JSON documents start with `{` or `[`).
fn is_toml(origin: &Origin, text: &str) -> bool {
    match origin {
        Origin::File(path) => Path::new(path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("toml")),
        Origin::Stdin | Origin::Inline => {
            !matches!(text.trim_start().chars().next(), Some('{' | '['))
        }
    }
}

/// Reads a JSON or TOML document as a JSON value.
pub(crate) fn document(ctx: &Ctx<'_>, path: &str) -> Result<(Origin, Value)> {
    let (origin, text) = read_source(ctx, path)?;
    let value = parse_document(&origin, &text)?;
    Ok((origin, value))
}

fn parse_document(origin: &Origin, text: &str) -> Result<Value> {
    if is_toml(origin, text) {
        let doc = belay_conf::Document::parse_named(&origin.describe(), text)
            .map_err(|e| Error::Input(e.to_string()))?;
        Ok(table_to_json(doc.root()))
    } else {
        belay_json::parse(text)
            .map_err(|e| Error::Input(format!("{} is not valid JSON: {e}", origin.describe())))
    }
}

/// Decodes and validates a spec of type `T` from a JSON value.
pub(crate) fn decode<T: FromJson + Validate>(origin: &Origin, value: &Value) -> Result<T> {
    let spec =
        T::from_json(value).map_err(|e| Error::Input(format!("{}: {e}", origin.describe())))?;
    spec.validate()
        .map_err(|e| Error::Input(format!("{}: {e}", origin.describe())))?;
    Ok(spec)
}

/// Reads, decodes and validates a spec file.
pub(crate) fn spec_file<T: FromJson + Validate>(ctx: &Ctx<'_>, path: &str) -> Result<T> {
    let (origin, value) = document(ctx, path)?;
    decode(&origin, &value)
}

fn table_to_json(table: &belay_conf::Table) -> Value {
    let mut map = Map::with_capacity(table.len());
    for (key, value) in table.iter() {
        if let Some(value) = toml_to_json(value) {
            map.insert(key, value);
        }
    }
    Value::Object(map)
}

/// TOML to JSON; `None` for floats JSON cannot hold (`nan`, `inf`), which
/// no belay spec uses.
fn toml_to_json(value: &belay_conf::Value) -> Option<Value> {
    use belay_conf::Value as T;
    Some(match value {
        T::String(s) | T::Datetime(s) => Value::String(s.clone()),
        T::Integer(i) => Value::Number(Number::from(*i)),
        T::Float(f) => Value::Number(Number::from_f64(*f)?),
        T::Boolean(b) => Value::Bool(*b),
        T::Array(items) => Value::Array(items.iter().filter_map(toml_to_json).collect()),
        T::Table(table) => table_to_json(table),
    })
}

/// Sets `path` (dotted, e.g. `retention.dead`) in a JSON object to a value
/// typed from `raw`: `null`/`none` removes the key, `true`/`false` are
/// booleans, numbers are numbers, JSON objects and arrays are parsed, and
/// anything else is a string. Missing intermediate objects are created.
pub(crate) fn set_path(root: &mut Value, path: &str, raw: &str) -> std::result::Result<(), String> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.iter().any(|s| s.trim().is_empty()) {
        return Err(format!("{path:?} is not a valid key path"));
    }
    let typed = infer(raw);
    let (last, parents) = segments
        .split_last()
        .expect("split always yields at least one segment");
    let mut node = root;
    for segment in parents {
        let map = match node {
            Value::Object(map) => map,
            _ => return Err(format!("cannot set {path}: {segment} is not a table")),
        };
        let child = map
            .entry(*segment)
            .or_insert_with(|| Value::Object(Map::new()));
        if child.is_null() {
            *child = Value::Object(Map::new());
        }
        node = child;
    }
    let Value::Object(map) = node else {
        return Err(format!("cannot set {path}: its parent is not a table"));
    };
    match typed {
        Value::Null => {
            map.remove(last);
        }
        value => {
            map.insert(*last, value);
        }
    }
    Ok(())
}

fn infer(raw: &str) -> Value {
    let trimmed = raw.trim();
    match trimmed {
        "null" | "none" => return Value::Null,
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        return Value::Number(Number::from(i));
    }
    if trimmed.contains('.') {
        if let Some(n) = trimmed.parse::<f64>().ok().and_then(Number::from_f64) {
            return Value::Number(n);
        }
    }
    if trimmed.starts_with(['{', '[']) {
        if let Ok(v) = belay_json::parse(trimmed) {
            return v;
        }
    }
    Value::String(raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;
    use belay_proto::QueueConfig;

    #[test]
    fn toml_documents_become_json_objects() {
        let origin = Origin::File("spec.toml".into());
        let text = "id = \"nightly\"\ncron = \"0 2 * * *\"\nstarts_at = 2025-01-01T00:00:00Z\n\
                    [template]\nqueue = \"reports\"\nkind = \"build\"\npriority = 5\n";
        let value = parse_document(&origin, text).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "nightly",
                "cron": "0 2 * * *",
                "starts_at": "2025-01-01T00:00:00Z",
                "template": {"queue": "reports", "kind": "build", "priority": 5}
            })
        );
    }

    #[test]
    fn stdin_is_sniffed_for_json() {
        assert!(!is_toml(&Origin::Stdin, "  {\"a\": 1}"));
        assert!(is_toml(&Origin::Stdin, "a = 1"));
        assert!(is_toml(&Origin::File("x.TOML".into()), "{"));
        assert!(!is_toml(&Origin::File("x.json".into()), "a = 1"));
    }

    #[test]
    fn syntax_errors_name_the_source() {
        let err = parse_document(&Origin::File("bad.json".into()), "{").unwrap_err();
        assert!(
            err.to_string().starts_with("bad.json is not valid JSON"),
            "{err}"
        );
        let err = parse_document(&Origin::File("bad.toml".into()), "a = ").unwrap_err();
        assert!(err.to_string().contains("bad.toml"), "{err}");
    }

    #[test]
    fn decoding_reports_the_json_path() {
        let origin = Origin::File("q.json".into());
        let err = decode::<QueueConfig>(&origin, &json!({"max_concurency": 3})).unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("q.json: "), "{text}");
        assert!(text.contains("max_concurrency"), "{text}");
        let err = decode::<QueueConfig>(&origin, &json!({"max_concurrency": 0})).unwrap_err();
        assert!(err.to_string().contains("must be at least 1"), "{err}");
    }

    #[test]
    fn set_path_types_values_and_creates_tables() {
        let mut v = json!({"retention": {"dead": "7d"}, "rate_limit": null});
        set_path(&mut v, "max_concurrency", "8").unwrap();
        set_path(&mut v, "retention.dead", "30d").unwrap();
        set_path(&mut v, "rate_limit.per_second", "2.5").unwrap();
        set_path(&mut v, "flag", "true").unwrap();
        set_path(&mut v, "list", "[1, 2]").unwrap();
        assert_eq!(
            v,
            json!({
                "retention": {"dead": "30d"},
                "rate_limit": {"per_second": 2.5},
                "max_concurrency": 8,
                "flag": true,
                "list": [1, 2]
            })
        );
        set_path(&mut v, "rate_limit", "none").unwrap();
        assert!(v.get("rate_limit").is_none());
        assert!(set_path(&mut v, "max_concurrency.x", "1")
            .unwrap_err()
            .contains("not a table"));
        assert!(set_path(&mut v, "a..b", "1").is_err());
    }

    #[test]
    fn inference_keeps_ambiguous_text_as_strings() {
        assert_eq!(infer("30s"), json!("30s"));
        assert_eq!(infer("exp:1s*2..10m"), json!("exp:1s*2..10m"));
        assert_eq!(infer("-3"), json!(-3));
        assert_eq!(infer("{broken"), json!("{broken"));
    }
}
