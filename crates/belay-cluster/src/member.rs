//! Node identities and the member records that gossip spreads.
//!
//! Every field of a [`Member`] travels in datagrams, so identities,
//! addresses and metadata are bounded here, at construction time, rather
//! than discovered to be too large when a packet is being assembled. The
//! limits guarantee that one member record always fits in the smallest
//! permitted packet ([`MIN_PACKET_SIZE`](crate::wire::MIN_PACKET_SIZE)).

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use belay_common::Timestamp;

use crate::error::{Error, Result};

/// Longest node id, in bytes.
pub const MAX_NODE_ID_LEN: usize = 128;

/// Longest advertised API address, in bytes.
pub const MAX_ADDR_LEN: usize = 255;

/// Most metadata entries a member may carry.
pub const MAX_META_ENTRIES: usize = 32;

/// Longest metadata key, in bytes.
pub const MAX_META_KEY_LEN: usize = 64;

/// Upper bound on the encoded size of a member's metadata (keys, values
/// and their length prefixes).
pub const MAX_META_BYTES: usize = 512;

/// Metadata key holding a member's rendezvous weight (see
/// [`Member::weight`]).
pub const WEIGHT_KEY: &str = "weight";

/// Largest accepted rendezvous weight.
pub const MAX_WEIGHT: f64 = 1_000.0;

/// Free-form member metadata, gossiped with the member's `Alive` record.
///
/// Validated by [`validate_meta`]; changing it at run time costs an
/// incarnation bump (see [`Membership::update_meta`](crate::Membership::update_meta)).
pub type Meta = BTreeMap<String, String>;

/// A node's identity: 1 to 128 bytes of ASCII letters, digits, `-`, `_`,
/// `.`, `:` or `@`, starting with a letter or digit.
///
/// Ordering is bytewise, which is what [`leader`](crate::Membership::leader)
/// election ("lowest live id wins") relies on to agree across nodes.
///
/// ```
/// use belay_cluster::NodeId;
///
/// let id: NodeId = "node-1.eu-west".parse().unwrap();
/// assert_eq!(id.as_str(), "node-1.eu-west");
/// assert!(NodeId::new("node 1").is_err());
/// assert!(NodeId::new("n1").unwrap() < NodeId::new("n2").unwrap());
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// Validates and wraps `id`.
    pub fn new(id: impl Into<String>) -> Result<NodeId> {
        let id = id.into();
        match check_node_id(&id) {
            Ok(()) => Ok(NodeId(id)),
            Err(reason) => Err(Error::InvalidNodeId { id, reason }),
        }
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwraps the id.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

fn check_node_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_NODE_ID_LEN {
        return Err(format!(
            "must be 1-{MAX_NODE_ID_LEN} bytes, got {}",
            id.len()
        ));
    }
    if let Some(bad) = id
        .chars()
        .find(|&c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@')))
    {
        return Err(format!(
            "contains invalid character {bad:?} (allowed: letters, digits, '-', '_', '.', ':', '@')"
        ));
    }
    if !id.as_bytes()[0].is_ascii_alphanumeric() {
        return Err("must start with a letter or digit".to_owned());
    }
    Ok(())
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}
