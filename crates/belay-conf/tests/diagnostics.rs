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
