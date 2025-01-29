//! Environment overlays: `BELAY_SERVER__LISTEN=0.0.0.0:7700` overrides
//! `server.listen` in a [`Document`].
//!
//! After the prefix and one `_`, double underscores separate path segments
//! and single underscores stay part of the key, so
//! `BELAY_STORAGE__CHECKPOINT_BYTES` is `storage.checkpoint_bytes` and
//! `BELAY_QUEUES__EMAILS__MAX_CONCURRENCY` is
//! `queues.emails.max_concurrency`. Segments are lowercased; a segment of
//! digits selects an existing array element (`BELAY_SCHEDULES__0__PAUSED`).
//!
//! Variables with a single segment (`BELAY_URL`, `BELAY_TOKEN`) are ignored
//! by default: such names are commonly used by clients rather than being
//! configuration, and belay's configuration has no top-level scalars.

use crate::error::{Error, Result};
use crate::parse::lexer::classify_scalar;
use crate::path::{KeyPath, Segment};
use crate::source::Loc;
use crate::value::Value;
use crate::Document;

/// Applies environment variables with a common prefix onto a document.
///
/// ```
/// use belay_conf::{Document, Overlay};
///
/// let mut doc = Document::parse("[server]\nthreads = 4\n")?;
/// let vars = [("BELAY_SERVER__THREADS", "16"), ("BELAY_SERVER__LISTEN", "0.0.0.0:7700")];
/// let applied = Overlay::new("BELAY").apply(&mut doc, vars)?;
/// assert_eq!(applied.len(), 2);
/// assert_eq!(doc.get_int("server.threads")?, 16);
/// assert_eq!(doc.get_str("server.listen")?, "0.0.0.0:7700");
/// # Ok::<(), belay_conf::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    prefix: String,
    top_level: bool,
}

/// One variable that was applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The variable name.
    pub var: String,
    /// The path it set.
    pub path: KeyPath,
    /// The value it replaced, if any.
    pub previous: Option<Value>,
}
