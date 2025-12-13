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
