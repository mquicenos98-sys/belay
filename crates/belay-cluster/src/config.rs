//! Identity and protocol tunables for one cluster node.
//!
//! The defaults ([`Config::new`]) are for nodes on one LAN or cloud region
//! and follow the values that have worked well for SWIM implementations in
//! practice: a 1 s protocol period, 500 ms probe timeout and 3 indirect
//! probers. [`Config::wan`] relaxes them for links with higher latency and
//! [`Config::local`] tightens them for processes on one machine (tests and
//! development), where round trips take microseconds.
//!
//! Several timings scale with the cluster size `n` (the number of live
//! members, including the local node); the formulas are on the methods
//! that compute them.

use std::net::SocketAddr;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::member::{validate_addr, validate_meta, Meta, NodeId};
use crate::wire::{cluster_tag, MAX_DATAGRAM, MIN_PACKET_SIZE};

/// Cluster names are at most this long.
pub const MAX_CLUSTER_NAME_LEN: usize = 64;

/// Membership configuration.
///
/// Fields are public so that configuration loaders can fill them directly;
/// [`validate`](Self::validate) (called by
/// [`Membership::new`](crate::Membership::new)) checks them together.
///
/// ```
/// use belay_cluster::{Config, NodeId};
/// use std::time::Duration;
///
/// let config = Config::new(NodeId::new("node-a").unwrap())
///     .with_api_addr("10.0.0.5:7070")
///     .with_seeds(["10.0.0.2:7946".parse().unwrap()])
///     .with_meta("zone", "eu-west-1a");
/// assert_eq!(config.probe_interval, Duration::from_secs(1));
/// assert_eq!(config.suspicion_timeout(10), Duration::from_secs(4));
/// config.validate().unwrap();
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// This node's identity; must be unique in the cluster.
    pub node_id: NodeId,
    /// Name shared by every node of one cluster. Packets from a cluster
    /// with another name are dropped, which keeps two clusters on one
    /// network apart when a seed list is wrong. It is not a secret and
    /// provides no security. Default `"belay"`.
    pub cluster_name: String,
    /// The API address other nodes should use for this node (see
    /// [`Member::addr`](crate::Member::addr)). Default empty.
    pub api_addr: String,
    /// The gossip address to advertise. `None` means "the transport's
    /// local address", which [`Node`](crate::Node) fills in; the pure state
    /// machine requires it to be set.
    pub advertise: Option<SocketAddr>,
    /// Gossip addresses of existing members to join through. Empty for the
    /// first node of a cluster.
    pub seeds: Vec<SocketAddr>,
    /// Metadata published with this node's `Alive` record.
    pub meta: Meta,
    /// The incarnation to start from. A node that restarts with the same id
    /// refutes whatever the cluster remembers about its previous run in any
    /// case; starting higher (e.g. from a persisted counter) just skips that
    /// round trip. Default 0.
    pub incarnation: u64,
    /// Protocol period: one member is probed per period. Default 1 s.
    pub probe_interval: Duration,
    /// How long to wait for a direct ack before asking other members to
    /// probe indirectly. Must be below `probe_interval`. Default 500 ms.
    pub probe_timeout: Duration,
    /// How many members are asked to probe a silent member on our behalf.
    /// Default 3.
    pub indirect_checks: usize,
    /// Scales the minimum suspicion timeout (see
    /// [`suspicion_timeout`](Self::suspicion_timeout)). Default 4.
    pub suspicion_mult: u32,
    /// The maximum suspicion timeout as a multiple of the minimum; a
    /// suspicion starts at the maximum and shrinks towards the minimum as
    /// other members independently confirm it. Default 6.
    pub suspicion_max_mult: u32,
    /// Scales how many times each update is retransmitted (see
    /// [`retransmit_limit`](Self::retransmit_limit)). Default 4.
    pub retransmit_mult: u32,
    /// Period of the dedicated gossip rounds that complement piggybacking.
    /// Default 200 ms.
    pub gossip_interval: Duration,
    /// Members sent to in each gossip round, at least 1. Gossip rounds are
    /// also what carries updates too large to piggyback on a probe.
    /// Default 3.
    pub gossip_fanout: usize,
    /// For how long members declared dead are still included in gossip
    /// rounds, so that a live node wrongly declared dead hears about it and
    /// refutes. Default 30 s.
    pub gossip_to_dead: Duration,
    /// Period of full-state anti-entropy exchanges with a random member
    /// (scaled for large clusters, see
    /// [`push_pull_period`](Self::push_pull_period)). Zero disables them.
    /// Default 30 s.
    pub push_pull_interval: Duration,
    /// Period of attempts to reach a random dead member (to heal
    /// partitions) and, while this node is alone, its seeds. Zero disables
    /// them. Default 10 s.
    pub reconnect_interval: Duration,
    /// Period of join attempts through the seeds until one answers.
    /// Default 1 s.
    pub join_retry_interval: Duration,
    /// How long [`Node::leave`](crate::Node::leave) keeps gossiping the
    /// departure before giving up. Default 5 s.
    pub leave_timeout: Duration,
    /// How long dead and departed members are remembered (and dead ones
    /// retried by reconnect attempts) before being forgotten. Default 24 h.
    pub tombstone_ttl: Duration,
    /// Largest datagram this node sends, in bytes. Piggybacked gossip and
    /// state transfers are packed up to it. The default of 1400 fits an
    /// Ethernet MTU with room for tunnel headers. Nodes accept datagrams of
    /// any size up to the UDP maximum, so the setting need not match across
    /// the cluster.
    pub max_packet_size: usize,
}

impl Config {
    /// LAN defaults for `node_id`.
    #[must_use]
    pub fn new(node_id: NodeId) -> Config {
        Config {
            node_id,
            cluster_name: "belay".to_owned(),
            api_addr: String::new(),
            advertise: None,
            seeds: Vec::new(),
            meta: Meta::new(),
            incarnation: 0,
            probe_interval: Duration::from_secs(1),
            probe_timeout: Duration::from_millis(500),
            indirect_checks: 3,
            suspicion_mult: 4,
            suspicion_max_mult: 6,
            retransmit_mult: 4,
            gossip_interval: Duration::from_millis(200),
            gossip_fanout: 3,
            gossip_to_dead: Duration::from_secs(30),
            push_pull_interval: Duration::from_secs(30),
            reconnect_interval: Duration::from_secs(10),
            join_retry_interval: Duration::from_secs(1),
            leave_timeout: Duration::from_secs(5),
            tombstone_ttl: Duration::from_secs(24 * 3600),
            max_packet_size: 1400,
        }
    }

    /// Defaults for members spread across regions: slower probes, longer
    /// timeouts and suspicion, wider gossip.
    #[must_use]
    pub fn wan(node_id: NodeId) -> Config {
        Config {
            probe_interval: Duration::from_secs(5),
            probe_timeout: Duration::from_secs(3),
            suspicion_mult: 6,
            retransmit_mult: 4,
            gossip_interval: Duration::from_millis(500),
            gossip_fanout: 4,
            gossip_to_dead: Duration::from_secs(60),
            push_pull_interval: Duration::from_secs(60),
            reconnect_interval: Duration::from_secs(30),
            join_retry_interval: Duration::from_secs(3),
            leave_timeout: Duration::from_secs(10),
            ..Config::new(node_id)
        }
    }

    /// Defaults for processes on one machine (tests, development): failures
    /// are detected within about a second.
    #[must_use]
    pub fn local(node_id: NodeId) -> Config {
        Config {
            probe_interval: Duration::from_millis(100),
            probe_timeout: Duration::from_millis(50),
            suspicion_mult: 3,
            gossip_interval: Duration::from_millis(20),
            gossip_to_dead: Duration::from_secs(5),
            push_pull_interval: Duration::from_secs(2),
            reconnect_interval: Duration::from_millis(500),
            join_retry_interval: Duration::from_millis(100),
            leave_timeout: Duration::from_secs(1),
            ..Config::new(node_id)
        }
    }

    /// Sets [`cluster_name`](Self::cluster_name).
    #[must_use]
    pub fn with_cluster_name(mut self, name: impl Into<String>) -> Config {
        self.cluster_name = name.into();
        self
    }

    /// Sets [`api_addr`](Self::api_addr).
    #[must_use]
    pub fn with_api_addr(mut self, addr: impl Into<String>) -> Config {
        self.api_addr = addr.into();
        self
    }

    /// Sets [`advertise`](Self::advertise).
    #[must_use]
    pub fn with_advertise(mut self, addr: SocketAddr) -> Config {
        self.advertise = Some(addr);
        self
    }

    /// Replaces [`seeds`](Self::seeds).
    #[must_use]
    pub fn with_seeds(mut self, seeds: impl IntoIterator<Item = SocketAddr>) -> Config {
        self.seeds = seeds.into_iter().collect();
        self
    }

    /// Adds one metadata entry.
    #[must_use]
    pub fn with_meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Config {
        self.meta.insert(key.into(), value.into());
        self
    }

    /// Sets [`incarnation`](Self::incarnation).
    #[must_use]
    pub fn with_incarnation(mut self, incarnation: u64) -> Config {
        self.incarnation = incarnation;
        self
    }

    /// Checks every field and the relations between them.
    pub fn validate(&self) -> Result<()> {
        if self.cluster_name.is_empty() || self.cluster_name.len() > MAX_CLUSTER_NAME_LEN {
            return Err(Error::config(format!(
                "cluster_name must be 1-{MAX_CLUSTER_NAME_LEN} bytes, got {}",
                self.cluster_name.len()
            )));
        }
        validate_addr(&self.api_addr)?;
        validate_meta(&self.meta)?;
        if self.probe_interval < Duration::from_millis(1) {
            return Err(Error::config("probe_interval must be at least 1ms"));
        }
        if self.probe_timeout.is_zero() || self.probe_timeout >= self.probe_interval {
            return Err(Error::config(format!(
                "probe_timeout ({}) must be above zero and below probe_interval ({})",
                fmt_ms(self.probe_timeout),
                fmt_ms(self.probe_interval)
            )));
        }
        for (name, value) in [
            ("suspicion_mult", self.suspicion_mult),
            ("suspicion_max_mult", self.suspicion_max_mult),
            ("retransmit_mult", self.retransmit_mult),
        ] {
            if value == 0 {
                return Err(Error::config(format!("{name} must be at least 1")));
            }
        }
        if self.gossip_fanout == 0 {
            return Err(Error::config("gossip_fanout must be at least 1"));
        }
        if self.gossip_interval < Duration::from_millis(1) {
            return Err(Error::config("gossip_interval must be at least 1ms"));
        }
        if self.join_retry_interval < Duration::from_millis(1) {
            return Err(Error::config("join_retry_interval must be at least 1ms"));
        }
        if !(MIN_PACKET_SIZE..=MAX_DATAGRAM).contains(&self.max_packet_size) {
            return Err(Error::config(format!(
                "max_packet_size must be {MIN_PACKET_SIZE}-{MAX_DATAGRAM} bytes, got {}",
                self.max_packet_size
            )));
        }
        if let Some(addr) = self.advertise {
            if addr.ip().is_unspecified() || addr.port() == 0 {
                return Err(Error::config(format!(
                    "advertise address {addr} is not reachable by other nodes"
                )));
            }
        }
        Ok(())
    }

    /// The tag carried by every packet: a hash of the cluster name.
    #[must_use]
    pub fn cluster_tag(&self) -> u32 {
        cluster_tag(&self.cluster_name)
    }

    /// Minimum suspicion timeout in a cluster of `n` live members:
    /// `suspicion_mult * max(1, log10(n)) * probe_interval`.
    ///
    /// The logarithm gives gossip about the suspicion, which needs O(log n)
    /// rounds to reach the suspect, time to do so in larger clusters.
    #[must_use]
    pub fn suspicion_timeout(&self, n: usize) -> Duration {
        let scale = (n.max(1) as f64).log10().max(1.0);
        self.probe_interval
            .mul_f64(f64::from(self.suspicion_mult) * scale)
    }

    /// Maximum suspicion timeout: `suspicion_max_mult` times the minimum.
    #[must_use]
    pub fn suspicion_max_timeout(&self, n: usize) -> Duration {
        self.suspicion_timeout(n)
            .saturating_mul(self.suspicion_max_mult)
    }

    /// Independent confirmations after which a suspicion runs for only the
    /// minimum timeout: `suspicion_mult - 2`, at least 1. With the default
    /// multiplier of 4 that is two members (besides the first suspecter)
    /// failing to reach the suspect on their own.
    #[must_use]
    pub fn suspicion_confirmations(&self) -> u32 {
        self.suspicion_mult.saturating_sub(2).max(1)
    }

    /// How many times each update is sent before it is dropped from the
    /// gossip queue: `retransmit_mult * ceil(log10(n + 1))`.
    ///
    /// Epidemic dissemination reaches every member with high probability
    /// after O(log n) rounds; the multiplier buys margin against loss.
    #[must_use]
    pub fn retransmit_limit(&self, n: usize) -> u32 {
        let rounds = ((n as f64) + 1.0).log2().ceil().max(1.0) as u32;
        self.retransmit_mult.saturating_mul(rounds)
    }

    /// The anti-entropy period for `n` members: `push_pull_interval` up to
    /// 32 members, then one more multiple per doubling, so the total
    /// full-state traffic grows like n log n rather than n squared.
    #[must_use]
    pub fn push_pull_period(&self, n: usize) -> Duration {
        const SCALE_THRESHOLD: usize = 32;
        if n <= SCALE_THRESHOLD {
            return self.push_pull_interval;
        }
        let doublings = ((n as f64).log2() - (SCALE_THRESHOLD as f64).log2()).ceil() as u32;
        self.push_pull_interval.saturating_mul(doublings + 1)
    }

    /// An upper bound on the time from a member crashing until every other
    /// member of an `n`-member cluster has declared it dead, assuming no
    /// message loss:
    ///
    /// 1. **Detection.** Randomized round-robin probing reaches every
    ///    member at most `2(n - 1) - 1` periods apart (SWIM, section 4.3),
    ///    and the failed probe completes one period later: `2(n - 1)`
    ///    periods.
    /// 2. **Suspicion.** At most the maximum suspicion timeout.
    /// 3. **Dissemination.** The `Dead` update is gossiped in rounds of
    ///    `gossip_fanout` members every `gossip_interval`, for up to
    ///    `retransmit_limit(n)` rounds, plus one probe period for updates
    ///    that travel only piggybacked.
    ///
    /// Typical detection is much faster: the first of `n - 1` probers to
    /// reach the crashed member suspects it, and confirmations from the
    /// others shrink the suspicion towards the minimum timeout.
    #[must_use]
    pub fn failure_detection_bound(&self, n: usize) -> Duration {
        let periods = 2 * n.saturating_sub(1).max(1) as u32;
        let detect = self.probe_interval.saturating_mul(periods);
        let disseminate = self
            .gossip_interval
            .saturating_mul(self.retransmit_limit(n))
            .saturating_add(self.probe_interval);
        detect
            .saturating_add(self.suspicion_max_timeout(n))
            .saturating_add(disseminate)
    }
}

fn fmt_ms(d: Duration) -> String {
    belay_common::format_duration(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::new(NodeId::new("n1").unwrap())
    }

    #[test]
    fn defaults_match_the_documented_values_and_validate() {
        let c = config();
        assert_eq!(c.probe_interval, Duration::from_secs(1));
        assert_eq!(c.probe_timeout, Duration::from_millis(500));
        assert_eq!(c.indirect_checks, 3);
        assert_eq!(c.suspicion_mult, 4);
        assert_eq!(c.max_packet_size, 1400);
        assert_eq!(c.cluster_name, "belay");
        c.validate().unwrap();
        Config::wan(c.node_id.clone()).validate().unwrap();
        Config::local(c.node_id.clone()).validate().unwrap();
    }

    #[test]
    fn probe_timeout_must_fit_in_the_protocol_period() {
        let mut c = config();
        c.probe_timeout = c.probe_interval;
        assert_eq!(
            c.validate().unwrap_err().to_string(),
            "invalid cluster config: probe_timeout (1s) must be above zero and below probe_interval (1s)"
        );
        c.probe_timeout = Duration::ZERO;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_out_of_range_values() {
        let cases: Vec<(fn(&mut Config), &str)> = vec![
            (
                |c| c.cluster_name.clear(),
                "cluster_name must be 1-64 bytes, got 0",
            ),
            (
                |c| c.suspicion_mult = 0,
                "suspicion_mult must be at least 1",
            ),
            (
                |c| c.retransmit_mult = 0,
                "retransmit_mult must be at least 1",
            ),
            (
                |c| c.max_packet_size = 500,
                "max_packet_size must be 1200-65507 bytes, got 500",
            ),
            (|c| c.max_packet_size = 70_000, "got 70000"),
            (
                |c| c.gossip_interval = Duration::ZERO,
                "gossip_interval must be at least 1ms",
            ),
            (
                |c| c.join_retry_interval = Duration::ZERO,
                "join_retry_interval",
            ),
            (
                |c| c.probe_interval = Duration::ZERO,
                "probe_interval must be at least 1ms",
            ),
            (
                |c| c.advertise = Some("0.0.0.0:7946".parse().unwrap()),
                "advertise address 0.0.0.0:7946 is not reachable",
            ),
            (|c| c.api_addr = "has space".into(), "invalid api address"),
            (
                |c| {
                    c.meta.insert("weight".into(), "0".into());
                },
                "weight must be a number",
            ),
        ];
        for (mutate, expected) in cases {
            let mut c = config();
            mutate(&mut c);
            let err = c.validate().unwrap_err().to_string();
            assert!(
                err.contains(expected),
                "{err:?} should contain {expected:?}"
            );
        }
        let mut c = config();
        c.gossip_fanout = 0;
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("gossip_fanout must be at least 1"), "{err}");
    }

    #[test]
    fn suspicion_timeout_scales_with_log_of_cluster_size() {
        let c = config();
        assert_eq!(c.suspicion_timeout(0), Duration::from_secs(4));
        assert_eq!(c.suspicion_timeout(1), Duration::from_secs(4));
        assert_eq!(c.suspicion_timeout(10), Duration::from_secs(4));
        assert_eq!(c.suspicion_timeout(100), Duration::from_secs(8));
        let t20 = c.suspicion_timeout(20).as_millis();
        assert!((5_200..=5_205).contains(&t20), "{t20}");
        assert_eq!(c.suspicion_max_timeout(10), Duration::from_secs(24));
        assert_eq!(c.suspicion_confirmations(), 2);
        let mut c = config();
        c.suspicion_mult = 1;
        assert_eq!(c.suspicion_confirmations(), 1);
    }

    #[test]
    fn push_pull_period_scales_above_32_members() {
        let c = config();
        assert_eq!(c.push_pull_period(1), Duration::from_secs(30));
        assert_eq!(c.push_pull_period(32), Duration::from_secs(30));
        assert_eq!(c.push_pull_period(33), Duration::from_secs(60));
        assert_eq!(c.push_pull_period(64), Duration::from_secs(60));
        assert_eq!(c.push_pull_period(65), Duration::from_secs(90));
        assert_eq!(c.push_pull_period(1000), Duration::from_secs(180));
    }

    #[test]
    fn failure_detection_bound_adds_detection_suspicion_and_dissemination() {
        let c = config();
        // 2 * 9 periods + 24 s suspicion + 16 * 200 ms + 1 s.
        assert_eq!(
            c.failure_detection_bound(10),
            Duration::from_millis(18_000 + 24_000 + 3_200 + 1_000)
        );
        assert!(c.failure_detection_bound(20) > c.failure_detection_bound(10));
        assert!(c.failure_detection_bound(1) > Duration::ZERO);
    }

    #[test]
    fn builders_set_fields() {
        let c = config()
            .with_cluster_name("prod")
            .with_api_addr("10.0.0.1:7070")
            .with_advertise("10.0.0.1:7946".parse().unwrap())
            .with_seeds(["10.0.0.2:7946".parse().unwrap()])
            .with_meta("zone", "a")
            .with_incarnation(7);
        assert_eq!(c.cluster_name, "prod");
        assert_eq!(c.seeds.len(), 1);
        assert_eq!(c.meta["zone"], "a");
        assert_eq!(c.incarnation, 7);
        assert_ne!(c.cluster_tag(), config().cluster_tag());
        c.validate().unwrap();
    }
}
