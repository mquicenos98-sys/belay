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

impl fmt::Display for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}, {}, incarnation {})",
            self.id, self.gossip_addr, self.state, self.incarnation
        )
    }
}

/// Parses a rendezvous weight: a finite number in `(0, MAX_WEIGHT]`.
pub(crate) fn parse_weight(s: &str) -> Result<f64> {
    let w: f64 = s.trim().parse().map_err(|_| {
        Error::InvalidMeta(format!(
            "{WEIGHT_KEY} must be a number in (0, {MAX_WEIGHT}], got {s:?}"
        ))
    })?;
    if w.is_finite() && w > 0.0 && w <= MAX_WEIGHT {
        Ok(w)
    } else {
        Err(Error::InvalidMeta(format!(
            "{WEIGHT_KEY} must be a number in (0, {MAX_WEIGHT}], got {s:?}"
        )))
    }
}

/// Checks `meta` against the entry, key and size limits, and checks the
/// [`WEIGHT_KEY`] entry if present.
///
/// ```
/// use belay_cluster::{validate_meta, Meta};
///
/// let mut meta = Meta::new();
/// meta.insert("zone".into(), "eu-west-1a".into());
/// meta.insert("weight".into(), "2".into());
/// assert!(validate_meta(&meta).is_ok());
///
/// meta.insert("weight".into(), "-1".into());
/// assert!(validate_meta(&meta).is_err());
/// ```
pub fn validate_meta(meta: &Meta) -> Result<()> {
    if meta.len() > MAX_META_ENTRIES {
        return Err(Error::InvalidMeta(format!(
            "at most {MAX_META_ENTRIES} entries are allowed, got {}",
            meta.len()
        )));
    }
    let mut encoded = belay_common::codec::uvarint_len(meta.len() as u64);
    for (key, value) in meta {
        if key.is_empty() || key.len() > MAX_META_KEY_LEN {
            return Err(Error::InvalidMeta(format!(
                "key {key:?} must be 1-{MAX_META_KEY_LEN} bytes"
            )));
        }
        if let Some(bad) = key
            .chars()
            .find(|&c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        {
            return Err(Error::InvalidMeta(format!(
                "key {key:?} contains invalid character {bad:?}"
            )));
        }
        if value.chars().any(char::is_control) {
            return Err(Error::InvalidMeta(format!(
                "value of {key:?} contains control characters"
            )));
        }
        encoded += str_encoded_len(key) + str_encoded_len(value);
    }
    if encoded > MAX_META_BYTES {
        return Err(Error::InvalidMeta(format!(
            "encodes to {encoded} bytes, the limit is {MAX_META_BYTES}"
        )));
    }
    if let Some(w) = meta.get(WEIGHT_KEY) {
        parse_weight(w)?;
    }
    Ok(())
}

/// Checks an advertised API address: at most [`MAX_ADDR_LEN`] bytes, no
/// whitespace or control characters. Its form (`host:port`, a URL) is up
/// to the application.
pub fn validate_addr(addr: &str) -> Result<()> {
    if addr.len() > MAX_ADDR_LEN {
        return Err(Error::InvalidAddr(format!(
            "must be at most {MAX_ADDR_LEN} bytes, got {}",
            addr.len()
        )));
    }
    if addr.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::InvalidAddr(format!(
            "{addr:?} must not contain whitespace or control characters"
        )));
    }
    Ok(())
}

pub(crate) fn str_encoded_len(s: &str) -> usize {
    belay_common::codec::uvarint_len(s.len() as u64) + s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn member(id: &str, meta: &[(&str, &str)]) -> Member {
        Member {
            id: NodeId::new(id).unwrap(),
            addr: "10.0.0.1:7070".into(),
            gossip_addr: "10.0.0.1:7946".parse().unwrap(),
            state: MemberState::Alive,
            incarnation: 3,
            meta: meta
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            since: Timestamp::EPOCH,
        }
    }

    #[test]
    fn node_ids_accept_hostnames_and_reject_junk() {
        for ok in [
            "a",
            "node-1",
            "worker_7.eu-west",
            "10.0.0.5:7946",
            "svc@host",
            "Z9",
        ] {
            assert!(NodeId::new(ok).is_ok(), "{ok}");
        }
        let err = NodeId::new("").unwrap_err().to_string();
        assert_eq!(err, "invalid node id \"\": must be 1-128 bytes, got 0");
        let long = "a".repeat(MAX_NODE_ID_LEN + 1);
        assert!(NodeId::new(long)
            .unwrap_err()
            .to_string()
            .contains("got 129"));
        assert!(NodeId::new("a".repeat(MAX_NODE_ID_LEN)).is_ok());
        let err = NodeId::new("node 1").unwrap_err().to_string();
        assert!(err.contains("invalid character ' '"), "{err}");
        assert!(NodeId::new("nöde").is_err());
        assert!(NodeId::new("a/b").is_err());
        let err = NodeId::new("-node").unwrap_err().to_string();
        assert!(err.ends_with("must start with a letter or digit"), "{err}");
        assert!(NodeId::new(".node").is_err());
    }

    #[test]
    fn node_ids_order_bytewise_and_borrow_as_str() {
        let ids: BTreeSet<NodeId> = ["b", "a10", "a2", "B"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let order: Vec<&str> = ids.iter().map(NodeId::as_str).collect();
        assert_eq!(order, ["B", "a10", "a2", "b"]);
        assert!(ids.contains("a2"));
        let id = NodeId::new("n1").unwrap();
        assert_eq!(id, "n1");
        assert_eq!(format!("{id:>4}|{id:?}"), "  n1|\"n1\"");
        assert_eq!(NodeId::try_from(String::from("n1")).unwrap(), id);
    }

    #[test]
    fn member_states_round_trip_and_classify_liveness() {
        for state in MemberState::ALL {
            assert_eq!(state.as_str().parse::<MemberState>().unwrap(), state);
            assert_eq!(MemberState::from_code(state.code()), Some(state));
        }
        assert_eq!(
            "SUSPECT".parse::<MemberState>().unwrap(),
            MemberState::Suspect
        );
        assert!("gone".parse::<MemberState>().is_err());
        assert_eq!(MemberState::from_code(4), None);
        assert!(MemberState::Alive.is_live());
        assert!(MemberState::Suspect.is_live());
        assert!(!MemberState::Dead.is_live());
        assert!(!MemberState::Left.is_live());
    }

    #[test]
    fn weight_defaults_to_one_and_tolerates_bad_peer_values() {
        assert_eq!(member("a", &[]).weight(), 1.0);
        assert_eq!(member("a", &[("weight", "2.5")]).weight(), 2.5);
        assert_eq!(member("a", &[("weight", "lots")]).weight(), 1.0);
        assert_eq!(member("a", &[("weight", "0")]).weight(), 1.0);
        assert_eq!(member("a", &[("weight", "inf")]).weight(), 1.0);
    }

    #[test]
    fn meta_limits_are_enforced() {
        let mut meta = Meta::new();
        meta.insert("zone".into(), "eu".into());
        validate_meta(&meta).unwrap();

        let too_many: Meta = (0..=MAX_META_ENTRIES)
            .map(|i| (format!("k{i}"), String::new()))
            .collect();
        let err = validate_meta(&too_many).unwrap_err().to_string();
        assert!(err.contains("at most 32 entries"), "{err}");

        let mut bad_key = Meta::new();
        bad_key.insert("has space".into(), "v".into());
        assert!(validate_meta(&bad_key).is_err());
        bad_key.clear();
        bad_key.insert(String::new(), "v".into());
        assert!(validate_meta(&bad_key).is_err());

        let mut control = Meta::new();
        control.insert("k".into(), "line\nbreak".into());
        assert!(validate_meta(&control).is_err());

        let mut big = Meta::new();
        big.insert("blob".into(), "x".repeat(MAX_META_BYTES));
        let err = validate_meta(&big).unwrap_err().to_string();
        assert!(err.contains("the limit is 512"), "{err}");

        for bad in ["0", "-2", "NaN", "1e9", ""] {
            let mut w = Meta::new();
            w.insert(WEIGHT_KEY.into(), bad.into());
            assert!(validate_meta(&w).is_err(), "{bad}");
        }
    }

    #[test]
    fn api_addresses_reject_whitespace_and_oversize() {
        validate_addr("").unwrap();
        validate_addr("http://10.0.0.1:7070").unwrap();
        assert!(validate_addr("10.0.0.1 :7070").is_err());
        assert!(validate_addr(&"a".repeat(MAX_ADDR_LEN + 1)).is_err());
    }

    #[test]
    fn member_display_is_compact() {
        assert_eq!(
            member("node-a", &[]).to_string(),
            "node-a (10.0.0.1:7946, alive, incarnation 3)"
        );
    }
}
