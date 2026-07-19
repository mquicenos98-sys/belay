//! Two nodes gossiping over an in-memory network, each with a real HTTP
//! server: requests for queues and schedules owned by the other node are
//! redirected there, and the typed client follows.

mod common;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use belay_cluster::{MemNetwork, Transport};
use belay_common::ManualClock;
use belay_core::config::ClusterProfile;
use belay_core::{Engine, Ownership};
use belay_http::Method;
use belay_proto::api::{ErrorCode, MemberState};
use belay_proto::{JobSpec, QueueName, ScheduleId};
use belay_server::{ServerHandle, ServerOptions, REMOTE_QUEUES_HEADER};
use common::{api_error, eventually, json, quiet_logs, test_config, START};

struct Node {
    engine: Arc<Engine>,
    handle: ServerHandle,
    url: String,
}

impl Node {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    fn client(&self) -> belay_client::Client {
        belay_client::Client::new(common::client_config(&self.url)).expect("client")
    }

    fn send(&self, method: Method, path: &str, body: &str) -> belay_http::ClientResponse {
        let req = belay_http::Request::new(method, self.url(path)).with_body(body);
        common::http().request(req).expect("the node answers")
    }
}

/// Starts nodes `a` and `b`; `b` joins through `a`.
fn cluster() -> (Node, Node) {
    quiet_logs();
    let net = MemNetwork::new(3);
    let clock = ManualClock::new(START.parse().expect("valid start"));
    let mut seeds: Vec<String> = Vec::new();
    let mut nodes = Vec::new();
    for id in ["node-a", "node-b"] {
        // Bind first: the API address is part of what the node gossips.
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
        let addr = listener.local_addr().expect("bound");
        let transport = net.bind_next();
        let gossip = transport.local_addr();
        let mut config = test_config();
        config.cluster.enabled = true;
        config.cluster.node_id = id.to_owned();
        config.cluster.profile = ClusterProfile::Local;
        config.cluster.api_addr = Some(format!("http://{addr}"));
        config.cluster.seeds = seeds.clone();
        let engine = Engine::builder()
            .config(config)
            .clock(clock.shared())
            .cluster_transport(Arc::new(transport))
            .background(false)
            .start()
            .unwrap_or_else(|e| panic!("{id} did not start: {e}"));
        let engine = Arc::new(engine);
        let handle =
            belay_server::serve_on_with(Arc::clone(&engine), listener, ServerOptions::default())
                .expect("server starts");
        seeds.push(gossip.to_string());
        nodes.push(Node {
            engine,
            url: format!("http://{addr}"),
            handle,
        });
    }
    let b = nodes.pop().expect("two nodes");
    let a = nodes.pop().expect("two nodes");
    let converged = eventually(Duration::from_secs(10), || {
        [&a, &b].iter().all(|n| {
            n.engine
                .members()
                .iter()
                .filter(|m| m.state == MemberState::Alive)
                .count()
                == 2
        })
    });
    assert!(converged, "the nodes did not see each other");
    (a, b)
}

/// A name `make(i)` for which both nodes agree that `owner` owns it.
fn owned_by<T>(
    owner: &Node,
    other: &Node,
    make: impl Fn(usize) -> T,
    ownership: impl Fn(&Engine, &T) -> Ownership,
) -> T {
    (0..200)
        .map(make)
        .find(|key| {
            ownership(&owner.engine, key).is_local()
                && matches!(ownership(&other.engine, key), Ownership::Remote(ref r) if r.node == owner.engine.node_id())
        })
        .expect("rendezvous hashing spreads keys over both nodes")
}

fn queue(i: usize) -> QueueName {
    QueueName::new(format!("q{i}")).expect("valid queue name")
}

#[test]
fn requests_follow_queue_and_schedule_ownership() {
    let (a, b) = cluster();
    let on_a = owned_by(&a, &b, queue, |e, q| e.owner_of(q));
    let on_b = owned_by(&b, &a, queue, |e, q| e.owner_of(q));

    // An enqueue into b's queue, sent to a, is redirected to b.
    let body = format!(r#"{{"queue": "{on_b}", "kind": "send"}}"#);
    let redirected = a.send(Method::Post, "/v1/jobs", &body);
    assert_eq!(redirected.status, 307);
    assert_eq!(
        redirected.header("location"),
        Some(b.url("/v1/jobs").as_str())
    );
    assert_eq!(json(&redirected)["redirect"]["node"], "node-b");

    // The typed client follows it; the job lives on b only.
    let created = a
        .client()
        .jobs()
        .enqueue(&JobSpec::new(on_b.clone(), "send"))
        .expect("enqueued through the redirect");
    assert!(b.engine.get_job(&created.id).is_ok());
    let missing = api_error(&a.send(Method::Get, &format!("/v1/jobs/{}", created.id), ""));
    assert_eq!(missing.code, ErrorCode::NotFound);
    assert!(
        missing.message.contains("on node node-a"),
        "{}",
        missing.message
    );

    // Queue-scoped reads keep their query string.
    let listing = a.send(Method::Get, &format!("/v1/jobs?queue={on_b}&limit=5"), "");
    assert_eq!(listing.status, 307);
    assert_eq!(
        listing.header("location"),
        Some(b.url(&format!("/v1/jobs?queue={on_b}&limit=5")).as_str())
    );
    let info = a.send(Method::Get, &format!("/v1/queues/{on_b}"), "");
    assert_eq!(info.status, 307);
    let local = a.send(Method::Post, &format!("/v1/queues/{on_a}/pause"), "");
    assert_eq!(local.status, 200);

    // Schedules go to the node that fires them.
    let schedule = owned_by(
        &b,
        &a,
        |i| ScheduleId::new(format!("s{i}")).expect("valid id"),
        |e, id| e.owner_of_schedule(id),
    );
    let spec = format!(
        r#"{{"id": "{schedule}", "cron": "0 * * * *", "template": {{"queue": "{on_b}", "kind": "k"}}}}"#
    );
    let create = a.send(Method::Post, "/v1/schedules", &spec);
    assert_eq!(create.status, 307);
    assert_eq!(
        create.header("location"),
        Some(b.url("/v1/schedules").as_str())
    );
    let preview = a.send(
        Method::Get,
        &format!("/v1/schedules/{schedule}/preview?n=2"),
        "",
    );
    assert_eq!(preview.status, 307);
}

#[test]
fn batches_and_leases_are_split_by_owner() {
    let (a, b) = cluster();
    let on_a = owned_by(&a, &b, queue, |e, q| e.owner_of(q));
    let on_b = owned_by(&b, &a, queue, |e, q| e.owner_of(q));

    let batch = format!(
        r#"{{"jobs": [{{"queue": "{on_a}", "kind": "k"}}, {{"queue": "{on_b}", "kind": "k"}}]}}"#
    );
    let refused = api_error(&a.send(Method::Post, "/v1/jobs", &batch));
    assert_eq!(refused.code, ErrorCode::InvalidArgument);
    assert!(
        refused.message.contains(&format!("{on_b} on node-b")),
        "{}",
        refused.message
    );

    a.engine.enqueue(JobSpec::new(on_a.clone(), "k")).unwrap();
    // A lease naming both queues is served from a's, skipping b's.
    let lease = format!(r#"{{"worker": "w1", "queues": ["{on_b}", "{on_a}"]}}"#);
    let served = a.send(Method::Post, "/v1/lease", &lease);
    assert_eq!(served.status, 200);
    assert_eq!(served.header(REMOTE_QUEUES_HEADER), Some(on_b.as_str()));
    assert_eq!(json(&served)["jobs"].as_array().map(Vec::len), Some(1));
    // Only b's queue: redirected to b.
    let only_b = format!(r#"{{"worker": "w1", "queues": ["{on_b}"]}}"#);
    let redirected = a.send(Method::Post, "/v1/lease", &only_b);
    assert_eq!(redirected.status, 307);
    assert_eq!(
        redirected.header("location"),
        Some(b.url("/v1/lease").as_str())
    );
}

#[test]
fn every_node_lists_the_members_and_is_ready() {
    let (a, b) = cluster();
    for node in [&a, &b] {
        let members = node.client().members().expect("members");
        let ids: Vec<&str> = members.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["node-a", "node-b"]);
        assert_eq!(members.iter().filter(|m| m.leader).count(), 1);
        assert!(eventually(Duration::from_secs(10), || node
            .client()
            .ready()
            .unwrap_or(false)));
    }
    assert_eq!(a.client().info().expect("info").cluster_size, 2);
    a.handle.shutdown_and_join();
    b.handle.shutdown_and_join();
}
