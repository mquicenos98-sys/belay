//! Three nodes on an in-memory gossip network, each with its own HTTP
//! server, store and worker: requests are redirected to the owning node,
//! each queue is processed where it is owned, schedules fire once across
//! the cluster, and a crashed node's queues move to the survivors and come
//! back when it returns.

use std::collections::BTreeMap;
use std::time::Duration;

use belay_proto::api::MemberState;
use belay_proto::job::META_SCHEDULE;
use belay_testkit::prelude::*;
use belay_testkit::raw;

const MEMBERSHIP_TIMEOUT: Duration = Duration::from_secs(30);

fn worker_name(cluster: &TestCluster, i: usize) -> String {
    format!("w-{}", cluster.node_id(i))
}

#[test]
fn requests_are_redirected_to_the_owner_and_each_queue_is_processed_there() -> Result<()> {
    let cluster = TestCluster::start(3)?;
    let queues: Vec<QueueName> = (0..3)
        .map(|i| cluster.queue_owned_by(i, "work"))
        .collect::<Result<_>>()?;

    // Asked about a queue it does not own, a node answers with a redirect
    // to the owner and keeps nothing.
    let body = format!(r#"{{"queue": "{}", "kind": "task"}}"#, queues[1]);
    let redirect = raw::request(
        "POST",
        &format!("{}/v1/jobs", cluster.node(0).url()),
        &[],
        body.as_bytes(),
    )?;
    assert_eq!(redirect.status, 307);
    let expected = format!("{}/v1/jobs", cluster.node(1).url());
    assert_eq!(redirect.header("location"), Some(expected.as_str()));

    // The typed client follows: everything goes in through node 0.
    let entry = cluster.node(0).client();
    let mut placed: Vec<(usize, JobId)> = Vec::new();
    for (owner, queue) in queues.iter().enumerate() {
        for n in 0..5 {
            let spec = job(queue, "task").payload(numbered(n));
            placed.push((owner, entry.jobs().enqueue(&spec)?.id));
        }
    }

    // A worker per node, each offered every queue; each node hands out
    // only the queue it owns.
    let recorder = Recorder::new();
    let _workers: Vec<TestWorker> = (0..3)
        .map(|i| {
            cluster
                .node(i)
                .worker()
                .id(&worker_name(&cluster, i))
                .queues(queues.iter().map(QueueName::as_str))
                .recorder(&recorder)
                .recording("task")
                .start()
        })
        .collect::<Result<_>>()?;
    for (owner, id) in &placed {
        let client = cluster.node(*owner).client();
        wait_for_state(&client, *id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    }
    let executions = recorder.executions();
    assert_eq!(executions.len(), placed.len());
    for execution in &executions {
        let owner = queues
            .iter()
            .position(|q| *q == execution.queue)
            .expect("only the test's queues are served");
        assert_eq!(execution.worker.as_str(), worker_name(&cluster, owner));
    }

    // Jobs live on their owner only; other nodes say where they looked.
    let (_, on_b) = placed[5];
    let elsewhere = cluster.node(0).client().jobs().get(on_b);
    let error = elsewhere.expect_err("node 0 does not store node 1's jobs");
    assert!(error.is_not_found());
    Ok(())
}

#[test]
fn schedules_fire_exactly_once_across_the_cluster() -> Result<()> {
    const SCHEDULES: usize = 12;
    // Every node is configured with the same schedules; only the owner of
    // each fires it.
    let cluster = TestCluster::builder(3)
        .configure(|_, config| {
            config.schedules = (0..SCHEDULES)
                .map(|i| {
                    let id = ScheduleId::new(format!("hourly-{i}")).expect("valid schedule id");
                    ScheduleSpec::new(id, "0 * * * *", job("cron", format!("report-{i}")))
                })
                .collect();
        })
        .start()?;
    cluster.wait_converged(MEMBERSHIP_TIMEOUT)?;

    for hour in 1..=2 {
        let ticks = cluster.tick(Duration::from_secs(3600))?;
        let mut fired: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (node, report) in &ticks {
            for fire in &report.schedules.fired {
                fired
                    .entry(fire.schedule.to_string())
                    .or_default()
                    .push(*node);
            }
        }
        assert_eq!(fired.len(), SCHEDULES, "hour {hour}: {fired:?}");
        for (id, nodes) in &fired {
            assert_eq!(nodes.len(), 1, "{id} fired on {nodes:?}");
            let owner = cluster.owner_of_schedule(&ScheduleId::new(id.as_str())?);
            assert_eq!(owner, Some(nodes[0]), "{id} fired off its owner");
        }
    }

    // Over HTTP: each node holds the jobs of the schedules it owns, two
    // per schedule, stamped with the schedule's id. (The listing has no
    // queue filter: one would be redirected to the queue's owner, while a
    // fired job stays on the node that fired it.)
    let mut total = 0;
    let mut nodes_with_jobs = 0;
    for i in cluster.live() {
        let jobs: Vec<Job> = cluster
            .node(i)
            .client()
            .jobs()
            .list_all(JobFilter::new())
            .collect::<std::result::Result<_, _>>()?;
        for job in &jobs {
            let schedule = job.spec.meta.get(META_SCHEDULE).expect("stamped");
            let owner = cluster.owner_of_schedule(&ScheduleId::new(schedule.as_str())?);
            assert_eq!(owner, Some(i));
        }
        total += jobs.len();
        nodes_with_jobs += usize::from(!jobs.is_empty());
    }
    assert_eq!(total, 2 * SCHEDULES);
    assert!(nodes_with_jobs >= 2, "rendezvous hashing spreads schedules");
    Ok(())
}
