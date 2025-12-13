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
