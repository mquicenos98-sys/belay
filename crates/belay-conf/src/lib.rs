//! Configuration files for belay: a TOML subset, environment overlays and
//! typed extraction that notices typos.
//!
//! Loading a configuration is four steps:
//!
//! 1. [`Document::parse`] (or [`Document::from_file`]) parses the text and
//!    remembers where every value was written.
//! 2. [`Overlay`] applies `BELAY_SERVER__LISTEN`-style environment variables.
//! 3. A [`Reader`] extracts typed values and records which keys were read.
//! 4. [`Reader::unused_keys`] lists keys nobody read, with suggestions.
//!
//! ```
//! use belay_conf::{Document, Overlay, Reader};
//!
//! let mut doc = Document::parse_named("belay.toml", r#"
//! [server]
//! listen = "0.0.0.0:7700"
//!
//! [storage]
//! dir = "/var/lib/belay"
//! sync = "interval:10ms"
//!
//! [[schedules]]
//! id = "nightly-report"
//! cron = "0 3 * * *"
//! "#)?;
//! Overlay::new("BELAY").apply(&mut doc, [("BELAY_SERVER__THREADS", "16")])?;
//!
//! let r = Reader::new(&doc);
//! let listen = r.string_or("server.listen", "127.0.0.1:7700")?;
//! let threads: u32 = r.or("server.threads", 4)?;
//! let sync = r.string_or("storage.sync", "always")?;
//! let schedules = r
//!     .array_of_tables("schedules")?
//!     .into_iter()
//!     .map(|s| Ok((s.string("id")?, s.string("cron")?)))
//!     .collect::<belay_conf::Result<Vec<_>>>()?;
//!
//! assert_eq!((listen.as_str(), threads, sync.as_str()), ("0.0.0.0:7700", 16, "interval:10ms"));
//! assert_eq!(schedules.len(), 1);
//! // `storage.dir` was never read:
//! assert_eq!(r.unused_keys()[0].path, "storage.dir");
//! # Ok::<(), belay_conf::Error>(())
//! ```
//!
//! # Supported syntax
//!
//! The TOML 1.0 language: comments; bare, quoted and dotted keys; basic and
//! literal strings, single- and multi-line, with all escapes; integers with
//! `_` separators and `0x`/`0o`/`0b` prefixes; floats including `inf` and
//! `nan`; booleans; arrays (multi-line, trailing comma, mixed types); inline
//! tables; `[tables]` and `[[arrays.of.tables]]`. Date-times of all four
//! kinds are validated but kept as text ([`Value::Datetime`]); like
//! durations and addresses, interpreting them is up to the consumer.
//! Duplicate keys and table redefinitions are errors, following the TOML
//! rules for implicit, dotted and inline tables.
//!
//! Not supported: TOML 1.1 relaxations (inline tables spanning lines or
//! with trailing commas, `\e` and `\xHH` escapes, seconds-less times), and
//! preserving comments or formatting when writing a document back out.
//!
//! Errors render like compiler diagnostics:
//!
//! ```text
//! error: duplicate key `listen` in table [server]
//!  --> line 7, column 1
//!   |
//! 7 | listen = "0.0.0.0:7700"
//!   | ^^^^^^
//!   = note: first defined on line 3
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod convert;

mod document;

mod error;

mod overlay;

pub(crate) mod parse;

mod path;

mod reader;

mod ser;
