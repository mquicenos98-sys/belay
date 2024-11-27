//! RFC 6901 JSON Pointers (`/steps/0/result`).
//!
//! [`Value::pointer`](crate::Value::pointer) resolves a pointer string on the
//! fly without allocating unless a token contains an escape. [`Pointer`] is
//! the parsed form, for pointers that are validated once (e.g. workflow
//! templates at submit time) and resolved many times.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use crate::Value;

/// A parsed JSON Pointer: a sequence of unescaped reference tokens.
///
/// ```
/// use belay_json::{json, Pointer};
///
/// let p: Pointer = "/steps/fetch/result/url".parse().unwrap();
/// let doc = json!({"steps": {"fetch": {"result": {"url": "http://x"}}}});
/// assert_eq!(p.get(&doc), Some(&json!("http://x")));
///
/// let built = Pointer::root().child("a/b").child(0);
/// assert_eq!(built.to_string(), "/a~1b/0");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Pointer {
    tokens: Vec<String>,
}

/// A string that is not a valid JSON Pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerError {
    offset: usize,
    reason: &'static str,
}

impl PointerError {
    /// Byte offset of the problem within the pointer string.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl fmt::Display for PointerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid JSON pointer at offset {}: {}",
            self.offset, self.reason
        )
    }
}
