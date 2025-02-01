//! Structural layer of the parser: table headers, key/value pairs, arrays
//! and inline tables, and TOML's rules about which tables may be defined,
//! extended or reopened.
//!
//! The rules, in short (TOML 1.0, "Table" and "Inline Table" sections):
//! a key is defined once; a `[table]` header appears once per table, though
//! a table created implicitly as the parent of `[a.b]` may still get its own
//! `[a]` later; tables created by dotted keys cannot get a header, and
//! inline tables and static arrays are complete when closed.

pub(crate) mod lexer;

use std::collections::HashMap;

use crate::error::Error;
use crate::path::{KeyPath, Segment};
use crate::source::{LineIndex, Loc, Span};
use crate::value::{Table, Value};
use lexer::{Cursor, Diag, KeySeg, PResult};

/// Arrays and inline tables nest recursively; bound the depth so hostile
/// input cannot overflow the stack.
const MAX_DEPTH: usize = 128;

/// How a table came into existence, which decides how it may be touched later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Created as the parent in a header like `[a.b]`; may still get `[a]`.
    Implicit,
    /// Defined by its own `[header]`, or an element of an array of tables.
    Header,
    /// Created by a dotted key such as `a.b = 1`.
    Dotted,
    /// An inline table or a static array.
    Frozen,
    /// An array created with `[[header]]`, the only kind `[[header]]` extends.
    ArrayOfTables,
}

pub(crate) struct Parsed {
    pub(crate) root: Table,
    pub(crate) locs: HashMap<KeyPath, Loc>,
}

impl Diag {
    pub(crate) fn into_error(self, src: &str, lines: &LineIndex, file: Option<&str>) -> Error {
        let note = self
            .related
            .map(|(label, span)| format!("{label} on line {}", lines.line_of(span.start)));
        Error::Parse {
            message: self.message,
            at: lines.snippet(src, self.span, file),
            note,
        }
    }
}

pub(crate) fn parse_document(src: &str) -> PResult<Parsed> {
    let mut p = Parser::new(src);
    p.cur.eat('\u{feff}');
    loop {
        p.cur.skip_ws();
        match p.cur.peek() {
            None => break,
            Some('[') => p.header()?,
            Some('#') | Some('\n') | Some('\r') => p.cur.end_of_line("comment")?,
            Some(_) => p.key_value()?,
        }
    }
    Ok(Parsed {
        root: p.tree.root,
        locs: p.locs,
    })
}

/// Parses a lone value such as `[1, 2]` or `"text"`, as used for overrides.
pub(crate) fn parse_value(src: &str) -> PResult<Value> {
    let mut p = Parser::new(src);
    p.cur.skip_ws();
    let (value, _) = p.value(&KeyPath::root(), 0)?;
    p.cur.skip_ws();
    if p.cur.peek().is_some() {
        let found = p.cur.describe_next();
        return Err(p.cur.error_here(format!("unexpected {found} after value")));
    }
    Ok(value)
}

pub(crate) fn with_article(type_name: &str) -> String {
    let article = if type_name.starts_with(['a', 'i']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {type_name}")
}

fn table_label(path: &KeyPath) -> String {
    if path.is_root() {
        "the root table".to_owned()
    } else if path
        .segments()
        .iter()
        .any(|s| matches!(s, Segment::Index(_)))
    {
        format!("table `{path}`")
    } else {
        format!("table [{path}]")
    }
}

fn written(segs: &[KeySeg]) -> KeyPath {
    segs.iter().map(|s| s.name.as_str()).collect()
}

/// Location of something named by a key.
fn keyed(key: Span, value: Span) -> Loc {
    Loc::Source {
        key: Some(key),
        value,
    }
}

/// Span of the first definition of `path`, for "first defined on line N".
fn def_span(locs: &HashMap<KeyPath, Loc>, path: &KeyPath, prefer_key: bool) -> Option<Span> {
    match locs.get(path)? {
        Loc::Source { key, value } if prefer_key => Some(key.unwrap_or(*value)),
        Loc::Source { value, .. } => Some(*value),
        Loc::Env(_) => None,
    }
}
