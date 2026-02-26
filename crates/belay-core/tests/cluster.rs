//! Three engines on an in-memory network: they agree on who owns each
//! queue and schedule, and each schedule fires on exactly one of them,
//! also after a node leaves.

mod common;

use std::sync::Arc;
use std::time::Duration;

use belay_cluster::MemNetwork;
use belay_common::{Clock, ManualClock, Timestamp};
use belay_core::config::ClusterProfile;
use belay_core::{Config, Engine, Ownership};
use belay_proto::api::MemberState;
use belay_proto::{QueueName, ScheduleId};
use common::{eventually, quiet_logs, START};

const NODES: [&str; 3] = ["node-a", "node-b", "node-c"];
const SCHEDULES: usize = 12;

fn config(node: &str, port: u16, seeds: &[String]) -> Config {
    let schedules: String = (0..SCHEDULES)
        .map(|i| {
            format!(
                "[[schedules]]\nid = \"s{i}\"\ncron = \"0 * * * *\"\n\
                 template = {{ queue = \"cron\", kind = \"k{i}\" }}\n"
            )
        })
        .collect();
    let mut config = Config::from_str(&schedules).unwrap();
    config.storage.in_memory = true;
    config.cluster.enabled = true;
    config.cluster.node_id = node.to_owned();
    config.cluster.profile = ClusterProfile::Local;
    config.cluster.api_addr = Some(format!("http://127.0.0.1:{port}"));
    config.cluster.seeds = seeds.to_vec();
    config
}

struct Cluster {
    clock: ManualClock,
    engines: Vec<Option<Engine>>,
}

impl Cluster {
    fn start() -> Cluster {
        quiet_logs();
        let net = MemNetwork::new(11);
        let clock = ManualClock::new(START.parse::<Timestamp>().unwrap());
        let mut engines: Vec<Option<Engine>> = Vec::new();
        let mut seeds: Vec<String> = Vec::new();
        for (i, node) in NODES.iter().enumerate() {
            let transport = net.bind_next();
            let addr = belay_cluster::Transport::local_addr(&transport);
            let engine = Engine::builder()
                .config(config(node, 7700 + i as u16, &seeds))
                .clock(clock.shared())
                .cluster_transport(Arc::new(transport))
                .background(false)
                .start()
                .unwrap_or_else(|e| panic!("{node} did not start: {e}"));
            seeds.push(addr.to_string());
            engines.push(Some(engine));
        }
        Cluster { clock, engines }
    }

    fn live(&self) -> impl Iterator<Item = &Engine> {
        self.engines.iter().flatten()
    }

    fn converged(&self, members: usize) -> bool {
        self.live().all(|e| {
            e.members()
                .iter()
                .filter(|m| m.state == MemberState::Alive)
                .count()
                == members
        })
    }

    /// The node every live engine names as the owner of `queue`, if they
    /// agree.
    fn agreed_owner(&self, owner: impl Fn(&Engine) -> Ownership) -> Option<String> {
        let mut named = self.live().map(|e| match owner(e) {
            Ownership::Local => e.node_id().to_owned(),
            Ownership::Remote(remote) => remote.node,
        });
        let first = named.next()?;
        named.all(|n| n == first).then_some(first)
    }

    /// Advances an hour, ticks every live engine and returns how many
    /// schedules each fired.
    fn fire_hour(&self) -> Vec<(String, usize)> {
        let now = self.clock.advance(Duration::from_secs(3600));
        self.live()
            .map(|e| (e.node_id().to_owned(), e.tick(now).schedules.fired.len()))
            .collect()
    }
}

#[test]
fn engines_agree_on_ownership_and_fire_each_schedule_once() {
    let mut cluster = Cluster::start();
    assert!(
        eventually(Duration::from_secs(10), || cluster.converged(3)),
        "the cluster did not converge"
    );
    for engine in cluster.live() {
        assert!(engine.is_clustered() && engine.is_joined() && engine.is_ready());
        assert_eq!(engine.info().cluster_size, 3);
        assert_eq!(engine.members().iter().filter(|m| m.leader).count(), 1);
    }

    // Queues spread over the nodes, and every node names the same owner.
    let mut owners = std::collections::BTreeMap::new();
    for i in 0..60 {
        let queue = QueueName::new(format!("q{i}")).unwrap();
        let owner = cluster
            .agreed_owner(|e| e.owner_of(&queue))
            .unwrap_or_else(|| panic!("nodes disagree about {queue}"));
        *owners.entry(owner).or_insert(0) += 1;
    }
    assert_eq!(owners.len(), 3, "{owners:?}");

    // Redirects point at the owner's API address.
    let queue = QueueName::from_static("emails");
    let owner = cluster.agreed_owner(|e| e.owner_of(&queue)).unwrap();
    let other = cluster.live().find(|e| e.node_id() != owner).unwrap();
    match other.owner_of(&queue) {
        Ownership::Remote(remote) => {
            let index = NODES.iter().position(|n| *n == owner).unwrap();
            assert_eq!(
                remote.url("/v1/lease"),
                format!("http://127.0.0.1:{}/v1/lease", 7700 + index)
            );
        }
        Ownership::Local => panic!("{} should not own {queue}", other.node_id()),
    }

    // Each schedule fires on its owner and nowhere else.
    let fired = cluster.fire_hour();
    let total: usize = fired.iter().map(|(_, n)| n).sum();
    assert_eq!(total, SCHEDULES, "{fired:?}");
    for (node, count) in &fired {
        let expected = (0..SCHEDULES)
            .filter(|i| {
                let id = ScheduleId::new(format!("s{i}")).unwrap();
                cluster
                    .agreed_owner(|e| e.owner_of_schedule(&id))
                    .as_deref()
                    == Some(node.as_str())
            })
            .count();
        assert_eq!(*count, expected, "{node}: {fired:?}");
    }

    // A node leaves; the others take over its schedules from the present
    // without catching up fires it already made.
    let leaving = cluster.engines[2].take().unwrap();
    leaving.shutdown();
    assert!(
        eventually(Duration::from_secs(10), || cluster.converged(2)),
        "the remaining nodes did not notice the departure"
    );
    let fired = cluster.fire_hour();
    let total: usize = fired.iter().map(|(_, n)| n).sum();
    assert_eq!(total, SCHEDULES, "{fired:?}");
    assert!(cluster.clock.now() > START.parse::<Timestamp>().unwrap());
    let states: Vec<MemberState> = cluster.engines[0]
        .as_ref()
        .unwrap()
        .members()
        .iter()
        .map(|m| m.state)
        .collect();
    assert!(states.contains(&MemberState::Left), "{states:?}");
}
