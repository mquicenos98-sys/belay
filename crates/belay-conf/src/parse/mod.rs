//! Structural layer of the parser: table headers, key/value pairs, arrays
//! and inline tables, and TOML's rules about which tables may be defined,
//! extended or reopened.
//!
//! The rules, in short (TOML 1.0, "Table" and "Inline Table" sections):
//! a key is defined once; a `[table]` header appears once per table, though
//! a table created implicitly as the parent of `[a.b]` may still get its own
//! `[a]` later; tables created by dotted keys cannot get a header, tables
//! with a header cannot be extended through dotted keys from elsewhere, and
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
                    Some(Kind::Dotted) => {}
                    Some(Kind::Implicit) => {
                        self.kinds.insert(child.clone(), Kind::Dotted);
                    }
                    Some(Kind::Header) => {
                        let msg = format!(
                            "cannot add keys to [{abs}] with dotted keys; it has its own [table] header"
                        );
                        let first = def_span(locs, &abs, false);
                        return Err(Diag::new(seg.span, msg).related("defined", first));
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

impl<'s> Parser<'s> {
    fn new(src: &'s str) -> Parser<'s> {
        Parser {
            cur: Cursor::new(src),
            tree: Tree::new(),
            locs: HashMap::new(),
            current: KeyPath::root(),
        }
    }

    fn header(&mut self) -> PResult<()> {
        let start = self.cur.pos();
        let array = self.cur.eat_str("[[");
        if !array {
            self.cur.bump();
        }
        self.cur.skip_ws();
        let segs = self.cur.key()?;
        let close = if array { "]]" } else { "]" };
        if !self.cur.eat_str(close) {
            let found = self.cur.describe_next();
            return Err(self.cur.error_here(format!(
                "expected `{close}` to close the table header, found {found}"
            )));
        }
        let span = Span::new(start, self.cur.pos());
        if array {
            self.open_array_table(&segs, span)?;
        } else {
            self.open_table(&segs, span)?;
        }
        self.cur.end_of_line("table header")
    }

    /// Walks the parent segments of a header, creating implicit tables and
    /// stepping into the last element of arrays of tables.
    fn descend(&mut self, segs: &[KeySeg]) -> PResult<KeyPath> {
        let mut path = KeyPath::root();
        for seg in segs {
            let child = path.key(&seg.name);
            path = match self.tree.existing(&path, &seg.name) {
                None => {
                    self.tree
                        .table_mut(&path)
                        .insert(seg.name.clone(), Table::new());
                    self.tree.kinds.insert(child.clone(), Kind::Implicit);
                    let loc = keyed(seg.span, seg.span);
                    self.locs.insert(child.clone(), loc);
                    child
                }
                Some(("table", _)) if self.tree.kind(&child) == Some(Kind::Frozen) => {
                    let first = def_span(&self.locs, &child, false);
                    return Err(Diag::new(
                        seg.span,
                        format!("cannot extend inline table `{child}`"),
                    )
                    .related("defined", first));
                }
                Some(("table", _)) => child,
                Some(("array", len)) if self.tree.kind(&child) == Some(Kind::ArrayOfTables) => {
                    child.index(len - 1)
                }
                Some((ty, _)) => {
                    let msg = format!(
                        "cannot use `{child}` as a table; it is already {}",
                        with_article(ty)
                    );
                    let first = def_span(&self.locs, &child, true);
                    return Err(Diag::new(seg.span, msg).related("defined", first));
                }
            };
        }
        Ok(path)
    }

    fn open_table(&mut self, segs: &[KeySeg], span: Span) -> PResult<()> {
        let (last, parents) = segs.split_last().expect("keys have at least one segment");
        let parent = self.descend(parents)?;
        let full = parent.key(&last.name);
        let name = written(segs);
        let first = def_span(&self.locs, &full, false);
        let conflict = match self.tree.existing(&parent, &last.name) {
            None => {
                self.tree
                    .table_mut(&parent)
                    .insert(last.name.clone(), Table::new());
                None
            }
            Some(("table", _)) => match self.tree.kind(&full) {
                Some(Kind::Implicit) => None,
                Some(Kind::Header) => Some(("first defined", "is defined more than once".into())),
                Some(Kind::Dotted) => {
                    Some(("defined", "was already created by dotted keys".into()))
                }
                _ => {
                    let msg = format!("cannot extend inline table `{name}`");
                    return Err(Diag::new(span, msg).related("defined", first));
                }
            },
            Some(("array", _)) if self.tree.kind(&full) == Some(Kind::ArrayOfTables) => {
                Some(("defined", "is already an array of tables".into()))
            }
            Some((ty, _)) => Some(("defined", format!("is already {}", with_article(ty)))),
        };
        if let Some((label, why)) = conflict {
            let msg = format!("table [{name}] {why}");
            return Err(Diag::new(span, msg).related(label, first));
        }
        self.tree.kinds.insert(full.clone(), Kind::Header);
        let loc = keyed(last.span, span);
        self.locs.insert(full.clone(), loc);
        self.current = full;
        Ok(())
    }

    fn open_array_table(&mut self, segs: &[KeySeg], span: Span) -> PResult<()> {
        let (last, parents) = segs.split_last().expect("keys have at least one segment");
        let parent = self.descend(parents)?;
        let full = parent.key(&last.name);
        let name = written(segs);
        let index = match self.tree.existing(&parent, &last.name) {
            None => {
                let array = Value::Array(Vec::new());
                self.tree
                    .table_mut(&parent)
                    .insert(last.name.clone(), array);
                self.tree.kinds.insert(full.clone(), Kind::ArrayOfTables);
                let loc = keyed(last.span, span);
                self.locs.insert(full.clone(), loc);
                0
            }
            Some(("array", len)) if self.tree.kind(&full) == Some(Kind::ArrayOfTables) => len,
            Some((ty, _)) => {
                let msg = if ty == "array" {
                    format!("cannot append to static array `{name}` with [[{name}]]")
                } else {
                    format!(
                        "cannot define array of tables [[{name}]]; `{name}` is already {}",
                        with_article(ty)
                    )
                };
                let first = def_span(&self.locs, &full, true);
                return Err(Diag::new(span, msg).related("defined", first));
            }
        };
        self.tree
            .table_mut(&parent)
            .get_mut(&last.name)
            .and_then(Value::as_array_mut)
            .expect("checked to be an array of tables above")
            .push(Value::Table(Table::new()));
        let element = full.index(index);
        self.tree.kinds.insert(element.clone(), Kind::Header);
        let loc = keyed(last.span, span);
        self.locs.insert(element.clone(), loc);
        self.current = element;
        Ok(())
    }

    fn key_value(&mut self) -> PResult<()> {
        let key = self.cur.key()?;
        self.expect_equals()?;
        let path = self.current.join(&written(&key));
        let value = self.value(&path, 0)?;
        let current = self.current.clone();
        self.tree
            .insert(&current, &KeyPath::root(), &key, value, &mut self.locs)?;
        self.cur.end_of_line("value")
    }

    fn expect_equals(&mut self) -> PResult<()> {
        self.cur.skip_ws();
        if !self.cur.eat('=') {
            let found = self.cur.describe_next();
            return Err(self
                .cur
                .error_here(format!("expected `=` after key, found {found}")));
        }
        self.cur.skip_ws();
        Ok(())
    }

    /// Parses the value at the cursor; `path` is its absolute location, used
    /// to record spans of nested elements and keys.
    fn value(&mut self, path: &KeyPath, depth: usize) -> PResult<(Value, Span)> {
        let start = self.cur.pos();
        let value = match self.cur.peek() {
            Some('"') | Some('\'') => Value::String(self.cur.string()?),
            Some('[') => self.array(path, depth + 1)?,
            Some('{') => self.inline_table(path, depth + 1)?,
            _ => self.cur.scalar()?,
        };
        Ok((value, Span::new(start, self.cur.pos())))
    }

    fn check_depth(&self, depth: usize) -> PResult<()> {
        if depth > MAX_DEPTH {
            return Err(self.cur.error_here(format!(
                "values are nested too deeply (the limit is {MAX_DEPTH} levels)"
            )));
        }
        Ok(())
    }

    fn array(&mut self, path: &KeyPath, depth: usize) -> PResult<Value> {
        self.check_depth(depth)?;
        let open = self.cur.pos();
        self.cur.bump();
        let mut items = Vec::new();
        loop {
            self.cur.skip_trivia()?;
            if self.cur.eat(']') {
                return Ok(Value::Array(items));
            }
            if self.cur.peek().is_none() {
                return Err(Diag::new(Span::new(open, open + 1), "unclosed array"));
            }
            let element = path.index(items.len());
            let (value, span) = self.value(&element, depth)?;
            self.locs.insert(
                element,
                Loc::Source {
                    key: None,
                    value: span,
                },
            );
            items.push(value);
            self.cur.skip_trivia()?;
            if self.cur.eat(']') {
                return Ok(Value::Array(items));
            }
            if !self.cur.eat(',') {
                if self.cur.peek().is_none() {
                    return Err(Diag::new(Span::new(open, open + 1), "unclosed array"));
                }
                let found = self.cur.describe_next();
                return Err(self
                    .cur
                    .error_here(format!("expected `,` or `]` in array, found {found}")));
            }
        }
    }

    fn inline_table(&mut self, path: &KeyPath, depth: usize) -> PResult<Value> {
        self.check_depth(depth)?;
        let open = self.cur.pos();
        self.cur.bump();
        let mut tree = Tree::new();
        self.cur.skip_ws();
        if self.cur.eat('}') {
            return Ok(Value::Table(tree.root));
        }
        loop {
            self.cur.skip_ws();
            self.inline_line_check(open)?;
            let key = self.cur.key()?;
            self.expect_equals()?;
            let value = self.value(&path.join(&written(&key)), depth)?;
            tree.insert(&KeyPath::root(), path, &key, value, &mut self.locs)?;
            self.cur.skip_ws();
            if self.cur.eat('}') {
                return Ok(Value::Table(tree.root));
            }
            if self.cur.eat(',') {
                self.cur.skip_ws();
                if self.cur.peek() == Some('}') {
                    return Err(self
                        .cur
                        .error_here("trailing comma is not allowed in an inline table"));
                }
                continue;
            }
            self.inline_line_check(open)?;
            let found = self.cur.describe_next();
            return Err(self.cur.error_here(format!(
                "expected `,` or `}}` in inline table, found {found}"
            )));
        }
    }

    /// TOML 1.0 inline tables live on a single line.
    fn inline_line_check(&self, open: usize) -> PResult<()> {
        match self.cur.peek() {
            None => Err(Diag::new(
                Span::new(open, open + 1),
                "unclosed inline table",
            )),
            Some('\n') | Some('\r') | Some('#') => Err(self.cur.error_here(
                "inline tables must be on a single line; use a [table] section for longer ones",
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Table {
        match parse_document(src) {
            Ok(p) => p.root,
            Err(d) => panic!("unexpected error: {}", d.message),
        }
    }

    fn fails(src: &str) -> String {
        match parse_document(src) {
            Ok(p) => panic!("expected an error, parsed {:?}", p.root),
            Err(d) => d.message,
        }
    }

    fn get<'t>(t: &'t Table, path: &str) -> &'t Value {
        t.lookup(&KeyPath::parse(path).unwrap())
            .unwrap_or_else(|| panic!("{path} not found"))
    }

    #[test]
    fn tables_dotted_keys_and_arrays_of_tables() {
        let t = parse(
            "title = 'x'\n[a.b]\nc = 1\n[a]\nd.e = 2\n[[q]]\nn = 1\n[q.sub]\nm = 2\n[[q]]\nn = 3\n",
        );
        assert_eq!(get(&t, "a.b.c"), &Value::Integer(1));
        assert_eq!(get(&t, "a.d.e"), &Value::Integer(2));
        assert_eq!(get(&t, "q[0].sub.m"), &Value::Integer(2));
        assert_eq!(get(&t, "q[1].n"), &Value::Integer(3));
        assert_eq!(get(&t, "q").as_array().map(<[Value]>::len), Some(2));
    }

    #[test]
    fn arrays_span_lines_with_comments_and_trailing_commas() {
        let t =
            parse("seeds = [\n  \"a:1\", # first\n  \"b:2\",\n]\nnested = [[1, 2], ['x'], []]\n");
        assert_eq!(get(&t, "seeds"), &Value::from(vec!["a:1", "b:2"]));
        assert_eq!(get(&t, "nested[0][1]"), &Value::Integer(2));
        assert_eq!(get(&t, "nested[2]"), &Value::Array(vec![]));
    }

    #[test]
    fn inline_tables_with_dotted_keys() {
        let t =
            parse("tok = { name = \"ci\", scopes = [\"work\"], meta.team = \"ops\" }\ne = {}\n");
        assert_eq!(get(&t, "tok.meta.team"), &Value::from("ops"));
        assert_eq!(get(&t, "e"), &Value::Table(Table::new()));
    }

    #[test]
    fn subtables_may_be_added_to_dotted_tables() {
        let t = parse("[fruit]\napple.color = 'red'\n[fruit.apple.texture]\nsmooth = true\n");
        assert_eq!(get(&t, "fruit.apple.texture.smooth"), &Value::Boolean(true));
    }

    #[test]
    fn nesting_depth_is_bounded() {
        let deep = format!("a = {}{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(
            fails(&deep),
            "values are nested too deeply (the limit is 128 levels)"
        );
        let ok = format!("a = {}{}", "[".repeat(100), "]".repeat(100));
        parse(&ok);
    }

    #[test]
    fn records_key_and_value_spans() {
        let src = "[server]\nlisten = \"x\"\n[[s]]\nports = [1, 22]\n";
        let p = parse_document(src).unwrap();
        let at = |path: &str| match p.locs.get(&KeyPath::parse(path).unwrap()) {
            Some(Loc::Source { key, value }) => (
                key.map(|k| &src[k.start..k.end]),
                &src[value.start..value.end],
            ),
            other => panic!("{path}: {other:?}"),
        };
        assert_eq!(at("server"), (Some("server"), "[server]"));
        assert_eq!(at("server.listen"), (Some("listen"), "\"x\""));
        assert_eq!(at("s[0]"), (Some("s"), "[[s]]"));
        assert_eq!(at("s[0].ports[1]"), (None, "22"));
    }

    #[test]
    fn parses_lone_values() {
        assert_eq!(
            parse_value(" [1, 'a'] ").unwrap(),
            Value::from(vec![Value::Integer(1), Value::from("a")])
        );
        assert_eq!(
            parse_value("x").unwrap_err().message,
            "expected a value, found `x` (strings must be quoted)"
        );
        assert_eq!(
            parse_value("1 2").unwrap_err().message,
            "unexpected `2` after value"
        );
    }
}
