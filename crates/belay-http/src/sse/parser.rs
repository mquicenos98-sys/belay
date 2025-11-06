//! Incremental `text/event-stream` parsing, following the WHATWG HTML
//! "event stream interpretation" algorithm.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::sse::SseEvent;

/// Default cap on a single line or event's data, 1 MiB.
const DEFAULT_MAX_EVENT_BYTES: usize = 1 << 20;
