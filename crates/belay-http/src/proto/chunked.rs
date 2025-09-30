//! The chunked transfer coding (RFC 9112 §7.1).
//!
//! The decoder is a byte-level state machine that can be fed arbitrary
//! slices, so it works the same whether a body arrives in one read or one
//! byte at a time. It never allocates based on a declared chunk size: the
//! size is checked against the body limit before any data is accepted, and
//! output grows only with bytes that actually arrived. Chunk extensions and
//! trailer fields are skipped (within fixed size bounds); nothing in belay
//! uses them.
//!
//! [`ChunkedBody`] wraps the decoder for the client, which streams response
//! bodies; the encoder side is two helpers used by the server for streaming
//! responses.

use std::io::{self, Read};

use crate::buf::InBuf;
use crate::percent::hex_value;
use crate::proto::parse::ParseError;

/// Maximum bytes on one chunk-size line, extensions included.
const MAX_SIZE_LINE: usize = 1024;

/// Maximum bytes of trailer section.
const MAX_TRAILERS: usize = 8 * 1024;
