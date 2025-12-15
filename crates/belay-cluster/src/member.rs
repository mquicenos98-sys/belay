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

impl FromStr for NodeId {
    type Err = Error;

    fn from_str(s: &str) -> Result<NodeId> {
        NodeId::new(s)
    }
}

impl TryFrom<&str> for NodeId {
    type Error = Error;

    fn try_from(s: &str) -> Result<NodeId> {
        NodeId::new(s)
    }
}

impl TryFrom<String> for NodeId {
    type Error = Error;

    fn try_from(s: String) -> Result<NodeId> {
        NodeId::new(s)
    }
}

impl AsRef<str> for NodeId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Lets maps keyed by `NodeId` be queried with a `&str`.
impl Borrow<str> for NodeId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for NodeId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for NodeId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// Where a member stands, as far as the local node knows.
///
/// `Alive` and `Suspect` members are *live*: they are probed, gossiped to
/// and own keys. Suspicion deliberately does not move ownership, because
/// under packet loss suspicions come and go and ownership would flap with
/// them; a suspect either refutes or is declared `Dead` within the
/// suspicion timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberState {
    /// Answering probes, or at least not yet suspected.
    Alive,
    /// Missed a probe (directly and through the indirect probers) and has
    /// not yet refuted it.
    Suspect,
    /// Declared failed after its suspicion timed out.
    Dead,
    /// Announced a graceful departure.
    Left,
}

impl MemberState {
    /// Every state, in wire-code order.
    pub const ALL: [MemberState; 4] = [
        MemberState::Alive,
        MemberState::Suspect,
        MemberState::Dead,
        MemberState::Left,
    ];

    /// Lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            MemberState::Alive => "alive",
            MemberState::Suspect => "suspect",
            MemberState::Dead => "dead",
            MemberState::Left => "left",
        }
    }

    /// Whether members in this state take part in the cluster: `Alive` or
    /// `Suspect`.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, MemberState::Alive | MemberState::Suspect)
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            MemberState::Alive => 0,
            MemberState::Suspect => 1,
            MemberState::Dead => 2,
            MemberState::Left => 3,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<MemberState> {
        MemberState::ALL.get(usize::from(code)).copied()
    }
}

impl fmt::Display for MemberState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Case-insensitive.
impl FromStr for MemberState {
    type Err = Error;

    fn from_str(s: &str) -> Result<MemberState> {
        MemberState::ALL
            .into_iter()
            .find(|state| state.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| {
                Error::config(format!(
                    "unknown member state {s:?} (expected alive, suspect, dead or left)"
                ))
            })
    }
}

/// One node as seen by the local node.
///
/// A snapshot: the membership state machine hands out clones, so holding a
/// `Member` never blocks gossip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The node's identity.
    pub id: NodeId,
    /// Address of the node's HTTP API, as advertised by the node (empty if
    /// it serves none). Other nodes redirect requests for keys it owns here.
    pub addr: String,
    /// UDP address the node gossips on.
    pub gossip_addr: SocketAddr,
    /// Current state.
    pub state: MemberState,
    /// The node's incarnation number: only the node itself increments it,
    /// to refute suspicion or to publish new metadata. Higher incarnations
    /// supersede lower ones.
    pub incarnation: u64,
    /// Metadata published by the node.
    pub meta: Meta,
    /// When the local node observed the member enter its current state
    /// (local clock, not comparable across nodes).
    pub since: Timestamp,
}

impl Member {
    /// Whether the member is `Alive` or `Suspect`.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.state.is_live()
    }

    /// The member's rendezvous weight: the [`WEIGHT_KEY`] metadata entry,
    /// or 1.0 when absent. Weights are validated before they are
    /// published, so an unparsable value (from a peer running different
    /// code) also counts as 1.0 rather than removing the member from the
    /// ring.
    #[must_use]
    pub fn weight(&self) -> f64 {
        self.meta
            .get(WEIGHT_KEY)
            .and_then(|w| parse_weight(w).ok())
            .unwrap_or(1.0)
    }
}
