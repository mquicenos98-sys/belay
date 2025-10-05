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

impl ChunkedBody {
    pub(crate) fn new(limit: u64) -> Self {
        ChunkedBody {
            decoder: ChunkedDecoder::new(limit),
            pending: Vec::new(),
            pos: 0,
        }
    }

    /// Whether the whole body has been decoded and handed out.
    pub(crate) fn is_done(&self) -> bool {
        self.decoder.is_done() && self.pos >= self.pending.len()
    }

    /// Reads decoded body bytes into `out`, refilling `buf` from `src` as
    /// needed. Returns 0 once the body is complete; a stream that ends
    /// early is an `UnexpectedEof` error.
    pub(crate) fn read<R: Read>(
        &mut self,
        src: &mut R,
        buf: &mut InBuf,
        out: &mut [u8],
    ) -> io::Result<usize> {
        loop {
            if self.pos < self.pending.len() {
                let n = out.len().min(self.pending.len() - self.pos);
                out[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            self.pending.clear();
            self.pos = 0;
            if self.decoder.is_done() {
                return Ok(0);
            }
            if !buf.is_empty() {
                let used = self
                    .decoder
                    .decode(buf.as_slice(), &mut self.pending)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                buf.consume(used);
                if !self.pending.is_empty() || self.decoder.is_done() {
                    continue;
                }
            }
            if buf.read_from(src, 16 * 1024)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(input: &[u8], limit: u64) -> Result<(Vec<u8>, usize), ParseError> {
        let mut decoder = ChunkedDecoder::new(limit);
        let mut out = Vec::new();
        let used = decoder.decode(input, &mut out)?;
        assert!(decoder.is_done(), "body should be complete");
        Ok((out, used))
    }

    #[test]
    fn decodes_chunks_and_stops_after_the_body() {
        let input = b"4\r\nWiki\r\n5\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\n\r\nNEXT";
        let (body, used) = decode_all(input, 1024).expect("valid body");
        assert_eq!(body, b"Wikipedia in\r\n\r\nchunks.");
        assert_eq!(&input[used..], b"NEXT");
    }

    #[test]
    fn byte_by_byte_matches_one_shot() {
        let input = b"a;name=value\r\n0123456789\r\n1\r\nX\r\n0\r\nExpires: never\r\n\r\n";
        let mut decoder = ChunkedDecoder::new(1024);
        let mut out = Vec::new();
        for byte in input.chunks(1) {
            assert_eq!(decoder.decode(byte, &mut out).expect("valid"), 1);
        }
        assert!(decoder.is_done());
        assert_eq!(out, b"0123456789X");
    }

    #[test]
    fn accepts_uppercase_hex_and_leading_zeros() {
        let (body, _) = decode_all(b"000A\r\n0123456789\r\n0\r\n\r\n", 64).expect("valid");
        assert_eq!(body.len(), 10);
    }

    #[test]
    fn enforces_the_limit_before_reading_data() {
        let mut decoder = ChunkedDecoder::new(8);
        let mut out = Vec::new();
        // Only the size line arrives; the limit must trip without the data.
        let err = decoder.decode(b"FFFFFFFF\r\n", &mut out).unwrap_err();
        assert_eq!(err, ParseError::BodyTooLarge { limit: 8 });
        assert!(out.is_empty());
    }

    #[test]
    fn limit_counts_across_chunks() {
        let mut decoder = ChunkedDecoder::new(5);
        let mut out = Vec::new();
        let err = decoder
            .decode(b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n", &mut out)
            .unwrap_err();
        assert_eq!(err, ParseError::BodyTooLarge { limit: 5 });
    }

    #[test]
    fn rejects_malformed_framing() {
        let cases: [(&[u8], &str); 8] = [
            (b"\r\n", "missing chunk size"),
            (b"g\r\n", "missing chunk size"),
            (b"5x\r\n", "invalid character in chunk size"),
            (b"3\nabc\r\n", "bare lf after chunk size"),
            (b"3\rabc\r\n", "expected lf after chunk size"),
            (b"3\r\nabcd\r\n", "missing crlf after chunk data"),
            (b"11111111111111111\r\n", "chunk size overflows 64 bits"),
            (b"0\r\nX: y\n\r\n", "bare lf in trailer section"),
        ];
        for (input, reason) in cases {
            let mut decoder = ChunkedDecoder::new(u64::MAX);
            let err = decoder.decode(input, &mut Vec::new()).unwrap_err();
            assert_eq!(
                err,
                ParseError::InvalidChunk(reason),
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn bounds_extension_and_trailer_sizes() {
        let mut long_ext = b"1;".to_vec();
        long_ext.extend(std::iter::repeat(b'x').take(MAX_SIZE_LINE));
        let err = ChunkedDecoder::new(10)
            .decode(&long_ext, &mut Vec::new())
            .unwrap_err();
        assert_eq!(err, ParseError::InvalidChunk("chunk size line too long"));

        let mut trailers = b"0\r\n".to_vec();
        for _ in 0..1000 {
            trailers.extend_from_slice(b"X-Trailer: value\r\n");
        }
        let err = ChunkedDecoder::new(10)
            .decode(&trailers, &mut Vec::new())
            .unwrap_err();
        assert_eq!(err, ParseError::InvalidChunk("trailer section too large"));
    }

    #[test]
    fn encoder_output_decodes() {
        let mut wire = Vec::new();
        encode_chunk(&mut wire, b"hello ");
        encode_chunk(&mut wire, b"");
        encode_chunk(&mut wire, &[b'z'; 300]);
        wire.extend_from_slice(LAST_CHUNK);
        assert!(wire.starts_with(b"6\r\nhello \r\n12C\r\n"));
        let (body, used) = decode_all(&wire, 1024).expect("valid");
        assert_eq!(used, wire.len());
        assert_eq!(body.len(), 306);
    }

    #[test]
    fn reader_streams_a_body_split_across_reads() {
        let wire = b"3\r\nabc\r\n4\r\ndefg\r\n0\r\n\r\nrest";
        // A reader that returns one byte per call exercises every refill path.
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() || out.is_empty() {
                    return Ok(0);
                }
                out[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        let mut src = Trickle(wire);
        let mut buf = InBuf::new();
        let mut chunked = ChunkedBody::new(100);
        let mut body = Vec::new();
        let mut scratch = [0u8; 3];
        loop {
            let n = chunked
                .read(&mut src, &mut buf, &mut scratch)
                .expect("valid body");
            if n == 0 {
                break;
            }
            body.extend_from_slice(&scratch[..n]);
        }
        assert_eq!(body, b"abcdefg");
        assert!(buf.is_empty(), "no bytes past the body were buffered");
        let mut rest = Vec::new();
        src.read_to_end(&mut rest).expect("slice read");
        assert_eq!(rest, b"rest");
    }

    #[test]
    fn reader_reports_truncated_bodies() {
        let mut src: &[u8] = b"5\r\nab";
        let mut buf = InBuf::new();
        let mut chunked = ChunkedBody::new(100);
        let mut scratch = [0u8; 16];
        let n = chunked
            .read(&mut src, &mut buf, &mut scratch)
            .expect("partial data is delivered");
        assert_eq!(&scratch[..n], b"ab");
        let err = chunked.read(&mut src, &mut buf, &mut scratch).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
