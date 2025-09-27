//! HTTP/1.1 wire format: head parsing, body framing, the chunked coding and
//! head serialization. Shared by the server and the client; nothing in here
//! touches sockets directly.

pub(crate) mod chunked;
pub(crate) mod framing;
pub(crate) mod parse;
pub(crate) mod write;

pub use parse::ParseError;
