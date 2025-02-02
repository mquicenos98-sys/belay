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

/// A table under construction plus the [`Kind`] of every table in it.
/// Paths are relative to `root`, which is the document root or, while an
/// inline table is being parsed, that inline table.
struct Tree {
    root: Table,
    kinds: HashMap<KeyPath, Kind>,
}

impl Tree {
    fn new() -> Tree {
        let kinds = HashMap::from([(KeyPath::root(), Kind::Header)]);
        Tree {
            root: Table::new(),
            kinds,
        }
    }

    fn table_mut(&mut self, path: &KeyPath) -> &mut Table {
        if path.is_root() {
            return &mut self.root;
        }
        self.root
            .lookup_mut(path)
            .and_then(Value::as_table_mut)
            .expect("the parser only records paths of tables it created")
    }

    /// The type name of the existing entry `key` in the table at `path`,
    /// plus its length when it is an array.
    fn existing(&mut self, path: &KeyPath, key: &str) -> Option<(&'static str, usize)> {
        let v = self.table_mut(path).get(key)?;
        Some((v.type_name(), v.as_array().map_or(0, <[Value]>::len)))
    }

    fn kind(&self, path: &KeyPath) -> Option<Kind> {
        self.kinds.get(path).copied()
    }

    /// Inserts `key = value` into the table at `at`, creating tables for
    /// the dotted prefix. `base` is the absolute path of this tree's root,
    /// used for locations and messages.
    fn insert(
        &mut self,
        at: &KeyPath,
        base: &KeyPath,
        key: &[KeySeg],
        (value, span): (Value, Span),
        locs: &mut HashMap<KeyPath, Loc>,
    ) -> PResult<()> {
        let (last, parents) = key.split_last().expect("keys have at least one segment");
        let mut path = at.clone();
        for seg in parents {
            let child = path.key(&seg.name);
            let abs = base.join(&child);
            match self.existing(&path, &seg.name) {
                None => {
                    self.table_mut(&path).insert(seg.name.clone(), Table::new());
                    self.kinds.insert(child.clone(), Kind::Dotted);
                    let loc = keyed(seg.span, seg.span);
                    locs.insert(abs, loc);
                }
                Some(("table", _)) => match self.kind(&child) {
                    Some(Kind::Dotted) | Some(Kind::Header) => {}
                    Some(Kind::Implicit) => {
                        self.kinds.insert(child.clone(), Kind::Dotted);
                    }
                    _ => {
                        let first = def_span(locs, &abs, false);
                        return Err(Diag::new(
                            seg.span,
                            format!("cannot extend inline table `{abs}`"),
                        )
                        .related("defined", first));
                    }
                },
                Some((ty, _)) => {
                    let msg = format!(
                        "cannot use `{abs}` as a table; it is already {}",
                        with_article(ty)
                    );
                    return Err(
                        Diag::new(seg.span, msg).related("defined", def_span(locs, &abs, true))
                    );
                }
            }
            path = child;
        }
        let full = path.key(&last.name);
        let abs = base.join(&full);
        if self.table_mut(&path).contains_key(&last.name) {
            let msg = format!(
                "duplicate key `{}` in {}",
                last.name,
                table_label(&base.join(&path))
            );
            let first = def_span(locs, &abs, true);
            return Err(Diag::new(last.span, msg).related("first defined", first));
        }
        if matches!(value, Value::Table(_) | Value::Array(_)) {
            self.kinds.insert(full, Kind::Frozen);
        }
        self.table_mut(&path).insert(last.name.clone(), value);
        let loc = keyed(last.span, span);
        locs.insert(abs, loc);
        Ok(())
    }
}

struct Parser<'s> {
    cur: Cursor<'s>,
    tree: Tree,
    locs: HashMap<KeyPath, Loc>,
    /// The table that key/value lines go into: the latest header's table.
    current: KeyPath,
}
