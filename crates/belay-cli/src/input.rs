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
