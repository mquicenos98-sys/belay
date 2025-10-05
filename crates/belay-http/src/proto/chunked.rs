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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Size { value: u64, digits: u32 },
    Extension { value: u64 },
    SizeLf { value: u64 },
    Data { remaining: u64 },
    DataCr,
    DataLf,
    TrailerStart,
    TrailerLine,
    TrailerLf,
    FinalLf,
    Done,
}

/// Incremental decoder for one chunked body.
#[derive(Debug)]
pub(crate) struct ChunkedDecoder {
    state: State,
    limit: u64,
    total: u64,
    line_bytes: usize,
    trailer_bytes: usize,
}

impl ChunkedDecoder {
    /// A decoder that rejects bodies longer than `limit` decoded bytes.
    pub(crate) fn new(limit: u64) -> Self {
        ChunkedDecoder {
            state: State::Size {
                value: 0,
                digits: 0,
            },
            limit,
            total: 0,
            line_bytes: 0,
            trailer_bytes: 0,
        }
    }

    /// Whether the terminating chunk and trailers have been consumed.
    pub(crate) fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Decodes as much of `input` as possible, appending body bytes to
    /// `out`. Returns how many input bytes were consumed; once the body is
    /// complete, bytes after it are left unconsumed.
    pub(crate) fn decode(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize, ParseError> {
        let mut i = 0;
        while i < input.len() {
            match self.state {
                State::Done => break,
                State::Data { remaining } => {
                    let available = (input.len() - i) as u64;
                    // Bounded by the input slice length, so it fits in usize.
                    let take = remaining.min(available) as usize;
                    out.extend_from_slice(&input[i..i + take]);
                    i += take;
                    let left = remaining - take as u64;
                    self.state = if left == 0 {
                        State::DataCr
                    } else {
                        State::Data { remaining: left }
                    };
                }
                _ => {
                    self.step(input[i])?;
                    i += 1;
                }
            }
        }
        Ok(i)
    }

    fn step(&mut self, b: u8) -> Result<(), ParseError> {
        self.state = match self.state {
            State::Size { value, digits } => {
                self.count_line_byte()?;
                match (hex_value(b), b) {
                    (Some(d), _) => {
                        if digits == 16 {
                            return Err(ParseError::InvalidChunk("chunk size overflows 64 bits"));
                        }
                        State::Size {
                            value: value << 4 | u64::from(d),
                            digits: digits + 1,
                        }
                    }
                    (None, _) if digits == 0 => {
                        return Err(ParseError::InvalidChunk("missing chunk size"))
                    }
                    (None, b'\r') => State::SizeLf { value },
                    (None, b'\n') => {
                        return Err(ParseError::InvalidChunk("bare lf after chunk size"))
                    }
                    (None, b';' | b' ' | b'\t') => State::Extension { value },
                    (None, _) => {
                        return Err(ParseError::InvalidChunk("invalid character in chunk size"))
                    }
                }
            }
            State::Extension { value } => {
                self.count_line_byte()?;
                match b {
                    b'\r' => State::SizeLf { value },
                    b'\n' | 0 => {
                        return Err(ParseError::InvalidChunk(
                            "invalid character in chunk extension",
                        ))
                    }
                    _ => State::Extension { value },
                }
            }
            State::SizeLf { value } => {
                if b != b'\n' {
                    return Err(ParseError::InvalidChunk("expected lf after chunk size"));
                }
                self.line_bytes = 0;
                if value == 0 {
                    State::TrailerStart
                } else {
                    if value > self.limit.saturating_sub(self.total) {
                        return Err(ParseError::BodyTooLarge { limit: self.limit });
                    }
                    self.total += value;
                    State::Data { remaining: value }
                }
            }
            State::DataCr => match b {
                b'\r' => State::DataLf,
                _ => return Err(ParseError::InvalidChunk("missing crlf after chunk data")),
            },
            State::DataLf => match b {
                b'\n' => State::Size {
                    value: 0,
                    digits: 0,
                },
                _ => return Err(ParseError::InvalidChunk("missing crlf after chunk data")),
            },
            State::TrailerStart => match b {
                b'\r' => State::FinalLf,
                b'\n' => return Err(ParseError::InvalidChunk("bare lf in trailer section")),
                _ => {
                    self.count_trailer_byte()?;
                    State::TrailerLine
                }
            },
            State::TrailerLine => {
                self.count_trailer_byte()?;
                match b {
                    b'\r' => State::TrailerLf,
                    b'\n' => return Err(ParseError::InvalidChunk("bare lf in trailer section")),
                    _ => State::TrailerLine,
                }
            }
            State::TrailerLf => match b {
                b'\n' => State::TrailerStart,
                _ => return Err(ParseError::InvalidChunk("expected lf after trailer field")),
            },
            State::FinalLf => match b {
                b'\n' => State::Done,
                _ => return Err(ParseError::InvalidChunk("expected lf after last chunk")),
            },
            State::Data { .. } | State::Done => self.state,
        };
        Ok(())
    }

    fn count_line_byte(&mut self) -> Result<(), ParseError> {
        self.line_bytes += 1;
        if self.line_bytes > MAX_SIZE_LINE {
            return Err(ParseError::InvalidChunk("chunk size line too long"));
        }
        Ok(())
    }

    fn count_trailer_byte(&mut self) -> Result<(), ParseError> {
        self.trailer_bytes += 1;
        if self.trailer_bytes > MAX_TRAILERS {
            return Err(ParseError::InvalidChunk("trailer section too large"));
        }
        Ok(())
    }
}

/// Appends one chunk carrying `data` to `out`. Empty data writes nothing,
/// since an empty chunk would terminate the body.
pub(crate) fn encode_chunk(out: &mut Vec<u8>, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    out.extend_from_slice(format!("{:X}\r\n", data.len()).as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
}

/// The last chunk with an empty trailer section.
pub(crate) const LAST_CHUNK: &[u8] = b"0\r\n\r\n";

/// A chunked body read through an [`InBuf`] that may already hold some of
/// its bytes; decoded data is staged in `pending` between reads.
#[derive(Debug)]
pub(crate) struct ChunkedBody {
    decoder: ChunkedDecoder,
    pending: Vec<u8>,
    pos: usize,
}
