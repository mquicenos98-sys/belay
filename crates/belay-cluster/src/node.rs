//! Runs the membership state machine over a transport on two threads.
//!
//! `belay-gossip-rx` blocks on the transport and feeds each datagram to the
//! state machine; `belay-gossip-tick` sleeps until the machine's next
//! deadline and ticks it. Both send what the machine returns. The machine
//! sits behind one mutex, held only while it computes (never across I/O),
//! so the two threads never wait on each other for long.
//!
//! Readers (`members`, `is_owner`, `leader`...) never touch that mutex:
//! after every change the runner publishes an immutable snapshot of the
//! member list and its ring, and readers clone an `Arc` to it. Ownership
//! checks on hot request paths are therefore a read lock and a handful of
//! hashes.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, read, write};
use belay_common::time::system_clock;
use belay_common::{debug, info, warn, Clock, Rng, SharedClock, Shutdown};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::event::MembershipEvent;
use crate::member::{Member, Meta, NodeId};
use crate::membership::{LocalState, Membership, MembershipStats, Outbound};
use crate::ring::Ring;
use crate::transport::{Datagram, Transport};

/// How long the receive thread blocks before re-checking for shutdown.
const RECV_POLL: Duration = Duration::from_millis(50);

/// Longest sleep of the tick thread, bounding how late it notices
/// deadlines created by incoming datagrams.
const MAX_TICK_SLEEP: Duration = Duration::from_millis(50);

/// Pause after a transport receive error, to avoid spinning on a broken
/// socket.
const ERROR_BACKOFF: Duration = Duration::from_millis(100);
