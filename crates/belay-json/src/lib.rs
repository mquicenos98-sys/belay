//! JSON for belay: a value model, a strict RFC 8259 parser, a serializer, and
//! the [`ToJson`]/[`FromJson`] traits that every API and storage type
//! implements by hand.
//!
//! # Values
//!
//! [`Value`] is the usual six-variant tree. [`Number`] keeps integers exact
//! as `u64`/`i64` and everything else as a finite `f64`. [`Map`] keeps
//! objects in insertion order.
//!
//! ```
//! use belay_json::{json, Value};
//!
//! let v: Value = belay_json::parse(r#"{"id": 9007199254740993, "tags": ["a"]}"#)?;
//! assert_eq!(v["id"].as_u64(), Some(9_007_199_254_740_993));
//! assert_eq!(v.pointer("/tags/0"), Some(&json!("a")));
//! assert_eq!(v.to_string(), r#"{"id":9007199254740993,"tags":["a"]}"#);
//! # Ok::<(), belay_json::ParseError>(())
//! ```
//!
//! # Typed conversion
//!
//! Types implement [`ToJson`] with an [`ObjectBuilder`] and [`FromJson`] with
//! an [`ObjectReader`]. Decoding errors say where in the document the problem
//! is:
//!
//! ```
//! use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};
//!
//! #[derive(Debug, PartialEq)]
//! struct Job { queue: String, priority: i16, tags: Vec<String> }
//!
//! impl ToJson for Job {
//!     fn to_json(&self) -> Value {
//!         ObjectBuilder::new()
//!             .field("queue", &self.queue)
//!             .field("priority", self.priority)
//!             .field_if(!self.tags.is_empty(), "tags", &self.tags)
//!             .build()
//!     }
//! }
//!
//! impl FromJson for Job {
//!     fn from_json(value: &Value) -> Result<Self, FromJsonError> {
//!         let r = ObjectReader::new(value, "Job")?;
//!         let job = Job {
//!             queue: r.req("queue")?,
//!             priority: r.or("priority", 0)?,
//!             tags: r.or_default("tags")?,
//!         };
//!         r.deny_unknown_fields()?;
//!         Ok(job)
//!     }
//! }
//!
//! let jobs: Vec<Job> = belay_json::from_str(r#"[{"queue": "a"}, {"queue": "b", "priority": 7}]"#)?;
//! assert_eq!(jobs[1].priority, 7);
//! assert_eq!(belay_json::to_string_of(&jobs), r#"[{"queue":"a","priority":0},{"queue":"b","priority":7}]"#);
//!
//! let err = belay_json::from_str::<Vec<Job>>(r#"[{"queue": "a", "priority": 40000}]"#).unwrap_err();
//! assert_eq!(
//!     err.to_string(),
//!     "$[0].priority: expected integer in range -32768..=32767, found 40000"
//! );
//! # Ok::<(), belay_json::Error>(())
//! ```
//!
//! # Limitations
//!
//! - Whole documents only: there is no streaming/incremental parser, and the
//!   parsed tree is held in memory.
//! - Integers beyond 64 bits are parsed as (rounded) `f64`; there is no
//!   arbitrary-precision number type. `i128`/`u128` have no conversions.
//! - Numbers that overflow `f64` (e.g. `1e400`) are rejected rather than
//!   mapped to infinity.
//! - Drop, clone, comparison and serialization recurse; values nested far
//!   deeper than [`ParseOptions::DEFAULT_MAX_DEPTH`] can exhaust the stack,
//!   so keep the depth limit if parsing untrusted input.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

mod convert;

mod macros;

pub mod map;

mod number;

mod parse;

mod pointer;

mod value;

mod write;

use std::fmt;
use std::io;

pub use convert::{
    parse_str_as, FromJson, FromJsonError, ObjectBuilder, ObjectReader, PathSegment, ToJson,
};
pub use map::Map;
pub use number::Number;
pub use parse::{
    parse, parse_bytes, parse_bytes_with, parse_with, ParseError, ParseErrorKind, ParseOptions,
};
pub use pointer::{escape_token, Pointer, PointerError};
pub use value::{Kind, Value, ValueIndex};
pub use write::{
    to_string, to_string_pretty, to_string_with, write_pretty_to, write_string, write_to,
    write_with, WriteOptions,
};

/// Everything that can go wrong going from text to a typed value.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The text is not valid JSON.
    Parse(ParseError),
    /// The JSON does not have the expected shape.
    Convert(FromJsonError),
    /// Reading the input failed.
    Io(io::Error),
}

/// `Result` with this crate's [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
