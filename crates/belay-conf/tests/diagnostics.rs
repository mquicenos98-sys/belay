//! Every parse error names the problem and points at the right line and
//! column; the rendered diagnostic shows the source line with a caret.

use belay_conf::{Document, Error};

fn parse_error(src: &str) -> Error {
    match Document::parse(src) {
        Ok(doc) => panic!("expected an error for {src:?}, parsed {:?}", doc.root()),
        Err(err) => err,
    }
}

fn check(cases: &[(&str, usize, usize, &str)]) {
    for &(src, line, column, message) in cases {
        let err = parse_error(src);
        assert_eq!(err.message(), message, "{src:?}");
        assert_eq!(err.line_col(), Some((line, column)), "{src:?}: {message}");
    }
}

#[test]
fn duplicate_key_renders_like_a_compiler_diagnostic() {
    let src = "# belay\n[server]\nlisten = \"127.0.0.1:7700\"\nthreads = 4\nmax_body = 1024\n\
               # public interface\nlisten = \"0.0.0.0:7700\"\n";
    assert_eq!(
        parse_error(src).to_string(),
        "error: duplicate key `listen` in table [server]\n \
         --> line 7, column 1\n  |\n7 | listen = \"0.0.0.0:7700\"\n  | ^^^^^^\n  \
         = note: first defined on line 3"
    );
}

#[test]
fn named_documents_show_the_file_and_align_tabs() {
    let err = Document::parse_named("belay.toml", "[server]\n\tbad key = 1\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        "error: expected `=` after key, found `k`\n --> belay.toml:2:6\n  |\n\
         2 | \tbad key = 1\n  | \t    ^"
    );
}

#[test]
fn redefinitions_are_rejected() {
    check(&[
        ("a = 1\na = 2", 2, 1, "duplicate key `a` in the root table"),
        (
            "[t]\na.b = 1\na.b = 2",
            3,
            3,
            "duplicate key `b` in table [t.a]",
        ),
        (
            "[t]\nx = 1\n\n[t]",
            4,
            1,
            "table [t] is defined more than once",
        ),
        (
            "[t]\nx.y = 1\n[t.x]",
            3,
            1,
            "table [t.x] was already created by dotted keys",
        ),
        ("a = 1\n[a]", 2, 1, "table [a] is already an integer"),
        (
            "[[a]]\n[a]",
            2,
            1,
            "table [a] is already an array of tables",
        ),
        (
            "[a]\n[[a]]",
            2,
            1,
            "cannot define array of tables [[a]]; `a` is already a table",
        ),
        (
            "a = [1]\n[[a]]",
            2,
            1,
            "cannot append to static array `a` with [[a]]",
        ),
        (
            "a = 1\n[a.b]",
            2,
            2,
            "cannot use `a` as a table; it is already an integer",
        ),
        (
            "a = 'x'\na.b = 1",
            2,
            1,
            "cannot use `a` as a table; it is already a string",
        ),
        (
            "x = { a = 1, a = 2 }",
            1,
            14,
            "duplicate key `a` in table [x]",
        ),
        (
            "[[s]]\nk = 1\nk = 2",
            3,
            1,
            "duplicate key `k` in table `s[0]`",
        ),
    ]);
}

#[test]
fn notes_point_at_the_first_definition() {
    let err = parse_error("[queues.emails]\nmax = 1\n\n[queues.emails]\n");
    match err {
        Error::Parse { note, .. } => assert_eq!(note.as_deref(), Some("first defined on line 1")),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn structural_syntax_errors() {
    check(&[
        ("a = ", 1, 5, "expected a value, found end of file"),
        ("a = \n", 1, 5, "expected a value, found end of line"),
        ("a 1", 1, 3, "expected `=` after key, found `1`"),
        ("= 1", 1, 1, "expected a key, found `=`"),
        (
            "a = 1 2",
            1,
            7,
            "expected end of line after value, found `2`",
        ),
        (
            "[a\nb = 1",
            1,
            3,
            "expected `]` to close the table header, found end of line",
        ),
        (
            "[[a]\n",
            1,
            4,
            "expected `]]` to close the table header, found `]`",
        ),
        (
            "[a] x",
            1,
            5,
            "expected end of line after table header, found `x`",
        ),
        ("a = [1 2]", 1, 8, "expected `,` or `]` in array, found `2`"),
        ("ports = [\n  1,\n  2,\n", 1, 9, "unclosed array"),
        (
            "a = { b = 1, }",
            1,
            14,
            "trailing comma is not allowed in an inline table",
        ),
        (
            "a = { b = 1\n}",
            1,
            12,
            "inline tables must be on a single line; use a [table] section for longer ones",
        ),
        (
            "a = { b = 1 c = 2 }",
            1,
            13,
            "expected `,` or `}` in inline table, found `c`",
        ),
        (
            "a = 1 # bell \u{7}",
            1,
            14,
            "control character U+0007 is not allowed in comments",
        ),
        (
            "a = 1\rb = 2",
            1,
            6,
            "carriage return must be followed by a newline",
        ),
    ]);
}

#[test]
fn value_syntax_errors() {
    check(&[
        ("s = \"open\nx = 1", 1, 5, "unterminated string"),
        ("s = '''never closed\n", 1, 5, "unterminated string"),
        (
            "s = \"bad \\q escape\"",
            1,
            10,
            "invalid escape sequence `\\q`",
        ),
        (
            "n = 0123",
            1,
            5,
            "invalid number `0123`: leading zeros are not allowed",
        ),
        (
            "n = 9_223_372_036_854_775_808",
            1,
            5,
            "integer `9_223_372_036_854_775_808` does not fit in 64 bits",
        ),
        (
            "enabled = yes",
            1,
            11,
            "expected a value, found `yes` (strings must be quoted)",
        ),
        (
            "d = 2023-02-29",
            1,
            5,
            "invalid date-time `2023-02-29`: day 29 is out of range for 2023-02",
        ),
        (
            "[server]\n  listen = \"é\" # ok\n  threads = 1x",
            3,
            13,
            "invalid number `1x`: unexpected character",
        ),
        (
            "k = \"日本\" x",
            1,
            10,
            "expected end of line after value, found `x`",
        ),
    ]);
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    let src = format!("a = {}", "[".repeat(10_000));
    let err = parse_error(&src);
    assert_eq!(
        err.message(),
        "values are nested too deeply (the limit is 128 levels)"
    );
    assert_eq!(err.line_col(), Some((1, 133)));
}

#[test]
fn lone_value_errors_render_against_the_value_text() {
    let err = belay_conf::parse_value("[1, 2").unwrap_err();
    assert_eq!(
        err.to_string(),
        "error: unclosed array\n --> line 1, column 1\n  |\n1 | [1, 2\n  | ^"
    );
}
