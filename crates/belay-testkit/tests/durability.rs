//! Durability through the whole stack: jobs acknowledged over HTTP survive
//! a graceful restart on a real disk and a power loss on a simulated one;
//! without syncs a power loss keeps a prefix of what was acknowledged; a
//! lease survives a crash and its worker finishes the job afterwards.

use std::collections::BTreeSet;
use std::thread;

use belay_store::SyncPolicy;
use belay_testkit::prelude::*;

fn queue_counts(client: &Client, name: &'static str) -> Result<belay_proto::StateCounts> {
    Ok(client.queues().get(&QueueName::from_static(name))?.counts)
}
