//! The crate's error type.
//!
//! Malformed datagrams are not errors from the caller's point of view: the
//! membership state machine counts and drops them (see
//! [`MembershipStats::packets_rejected`](crate::MembershipStats)). [`WireError`]
//! surfaces only through the explicit [`decode`](crate::wire::decode) and
//! [`Packet::encode`](crate::wire::Packet::encode) calls.

use std::fmt;
use std::io;

use crate::wire::WireError;

/// Any error produced by this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A node id failed validation.
    InvalidNodeId {
        /// The rejected id.
        id: String,
        /// Why it was rejected.
        reason: String,
    },
    /// Member metadata failed validation.
    InvalidMeta(String),
    /// An advertised API address failed validation.
    InvalidAddr(String),
    /// A rendezvous weight that is not a finite positive number.
    InvalidWeight(f64),
    /// A configuration value is out of range or inconsistent.
    InvalidConfig(String),
    /// A datagram could not be encoded or decoded.
    Wire(WireError),
    /// A transport operation failed.
    Io(io::Error),
    /// The node has been shut down or has left the cluster.
    Stopped,
}

/// `Result` defaulting to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn config(message: impl Into<String>) -> Error {
        Error::InvalidConfig(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidNodeId { id, reason } => write!(f, "invalid node id {id:?}: {reason}"),
            Error::InvalidMeta(msg) => write!(f, "invalid member metadata: {msg}"),
            Error::InvalidAddr(msg) => write!(f, "invalid api address: {msg}"),
            Error::InvalidWeight(w) => {
                write!(
                    f,
                    "rendezvous weight must be a finite number above 0, got {w}"
                )
            }
            Error::InvalidConfig(msg) => write!(f, "invalid cluster config: {msg}"),
            Error::Wire(e) => write!(f, "malformed cluster packet: {e}"),
            Error::Io(e) => write!(f, "cluster transport error: {e}"),
            Error::Stopped => f.write_str("cluster node has stopped"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Wire(e) => Some(e),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}
