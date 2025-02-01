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
