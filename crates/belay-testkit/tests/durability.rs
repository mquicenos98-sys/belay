//! Durability through the whole stack: jobs acknowledged over HTTP survive
//! a graceful restart on a real disk and a power loss on a simulated one;
//! without syncs a power loss keeps a prefix of what was acknowledged; a
//! lease survives a crash and its worker finishes the job afterwards.

use std::collections::BTreeSet;
use std::thread;
use std::time::Duration;

use belay_store::SyncPolicy;
use belay_testkit::fixtures::number_of;
use belay_testkit::prelude::*;

fn queue_counts(client: &Client, name: &'static str) -> Result<belay_proto::StateCounts> {
    Ok(client.queues().get(&QueueName::from_static(name))?.counts)
}

#[test]
fn jobs_survive_a_graceful_restart_of_a_disk_store() -> Result<()> {
    // Group commit keeps the test fast on slow disks; a graceful stop syncs
    // the log whatever the policy.
    let mut server = TestServer::builder()
        .storage(Storage::Disk {
            sync: SyncPolicy::Interval(Duration::from_millis(10)),
        })
        .start()?;
    let client = server.client();
    let quick = server.worker().queue("quick").recording("ping").start()?;

    let mut backlog: Vec<JobId> = (0..40)
        .map(|n| {
            let spec = job("backlog", "load").payload(numbered(n));
            Ok(client.jobs().enqueue(&spec)?.id)
        })
        .collect::<Result<_>>()?;
    let batch = client.jobs().enqueue_batch(&numbered_jobs(
        &QueueName::from_static("archive"),
        "load",
        160,
    ))?;
    backlog.extend(batch.iter().map(|r| r.id));
    let pinged: Vec<JobId> = (0..5)
        .map(|n| {
            let spec = job("quick", "ping").payload(numbered(n));
            Ok(client.jobs().enqueue(&spec)?.id)
        })
        .collect::<Result<_>>()?;
    for id in &pinged {
        wait_for_state(&client, *id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    }

    server.restart()?;
    let client = server.client();
    assert_eq!(server.engine().store_report().storage, "disk");
    assert!(server.engine().verify()?.is_consistent());
    for (n, id) in backlog.iter().enumerate() {
        let job = client.jobs().get(*id)?;
        assert_eq!(job.state, JobState::Ready, "{id}");
        let expected = if n < 40 { n } else { n - 40 };
        assert_eq!(number_of(&job.spec.payload), Some(expected as u64));
    }
    for (n, id) in pinged.iter().enumerate() {
        let job = client.jobs().get(*id)?;
        assert_eq!(job.state, JobState::Succeeded);
        assert_eq!(job.result, Some(numbered(n)));
    }
    assert_eq!(queue_counts(&client, "backlog")?.get(JobState::Ready), 40);
    assert_eq!(queue_counts(&client, "archive")?.get(JobState::Ready), 160);

    // The worker started before the restart follows the server back.
    let after = client.jobs().enqueue(&job("quick", "ping"))?.id;
    wait_for_state(&client, after, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(quick.recorder().len(), 6);

    // And the backlog drains on the restarted server.
    let _loader = server
        .worker()
        .queues(["backlog", "archive"])
        .concurrency(8)
        .recording("load")
        .start()?;
    eventually(DEFAULT_TIMEOUT, || {
        ["backlog", "archive"].iter().all(|q| {
            queue_counts(&client, q)
                .is_ok_and(|c| c.pending() == 0 && c.get(JobState::Running) == 0)
        })
    })?;
    assert_eq!(
        queue_counts(&client, "archive")?.get(JobState::Succeeded),
        160
    );
    Ok(())
}

#[test]
fn acknowledged_jobs_survive_a_power_loss() -> Result<()> {
    let mut server = TestServer::builder()
        .storage(Storage::simulated_disk())
        .start()?;
    // Four producers enqueue at once; every id they get back was
    // acknowledged, so it must survive.
    let producers: Vec<_> = (0..4)
        .map(|p| {
            let url = server.url();
            thread::spawn(move || -> Result<Vec<JobId>> {
                let client = belay_testkit::connect(&url, None)?;
                (0..25)
                    .map(|n| {
                        let spec = job("orders", "charge").payload(json!({"producer": p, "n": n}));
                        Ok(client.jobs().enqueue(&spec)?.id)
                    })
                    .collect()
            })
        })
        .collect();
    let mut acknowledged = Vec::new();
    for producer in producers {
        acknowledged.extend(producer.join().expect("a producer panicked")?);
    }
    // Some are finished too, and a finish is as acknowledged as an enqueue.
    let worker = server
        .worker()
        .queue("receipts")
        .recording("print")
        .start()?;
    let client = server.client();
    let printed: Vec<JobId> = (0..10)
        .map(|n| {
            Ok(client
                .jobs()
                .enqueue(&job("receipts", "print").payload(numbered(n)))?
                .id)
        })
        .collect::<Result<_>>()?;
    for id in &printed {
        wait_for_state(&client, *id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    }
    drop(worker);

    server.crash()?;
    let client = server.client();
    for id in &acknowledged {
        assert_eq!(client.jobs().get(*id)?.state, JobState::Ready, "{id}");
    }
    for (n, id) in printed.iter().enumerate() {
        let job = client.jobs().get(*id)?;
        assert_eq!(job.state, JobState::Succeeded, "{id}");
        assert_eq!(job.result, Some(numbered(n)));
    }
    assert_eq!(queue_counts(&client, "orders")?.get(JobState::Ready), 100);
    assert!(server.engine().verify()?.is_consistent());
    let disk = server.simulated_disk().expect("a simulated disk");
    assert_eq!(disk.stats().crashes, 1);
    Ok(())
}

#[test]
fn without_syncs_a_power_loss_keeps_a_prefix_of_what_was_acknowledged() -> Result<()> {
    let mut server = TestServer::builder()
        .storage(Storage::SimulatedDisk {
            sync: SyncPolicy::Never,
        })
        .start()?;
    let client = server.client();
    let mut ids = Vec::new();
    for n in 0..30 {
        ids.push(
            client
                .jobs()
                .enqueue(&job("log", "append").payload(numbered(n)))?
                .id,
        );
        if n == 9 {
            // A checkpoint writes a synced snapshot: the first ten are safe.
            client.admin().checkpoint()?;
        }
    }

    server.crash()?;
    let client = server.client();
    let survivors: BTreeSet<JobId> = client
        .jobs()
        .list_all(JobFilter::new().queue(QueueName::from_static("log")))
        .map(|j| j.map(|j| j.id))
        .collect::<std::result::Result<_, _>>()?;
    let kept = ids.iter().take_while(|id| survivors.contains(id)).count();
    assert!(kept >= 10, "only {kept} of the checkpointed jobs survived");
    assert!(
        kept < ids.len(),
        "nothing unsynced was lost: the crash did not bite"
    );
    assert_eq!(
        survivors.len(),
        kept,
        "the survivors are exactly the first {kept} jobs acknowledged, with no holes"
    );
    assert!(server.engine().verify()?.is_consistent());
    Ok(())
}

#[test]
fn a_lease_survives_a_crash_and_its_worker_finishes_the_job() -> Result<()> {
    let mut server = TestServer::builder()
        .storage(Storage::simulated_disk())
        .start()?;
    let gate = Gate::new();
    let worker = server
        .worker()
        .queue("exports")
        .gated("export", &gate)
        .start()?;
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&job("exports", "export").payload(json!({"table": "orders"})))?
        .id;
    worker.recorder().wait_for_start(id, DEFAULT_TIMEOUT)?;
    let before = client.jobs().get(id)?;

    server.crash()?;
    let client = server.client();
    let after = client.jobs().get(id)?;
    assert_eq!(after.state, JobState::Running);
    assert_eq!(
        after.lease.as_ref().map(|l| &l.worker),
        before.lease.as_ref().map(|l| &l.worker)
    );

    // The worker never noticed more than a few failed heartbeats; its
    // lease token is still good, so its result is accepted.
    gate.open();
    let done = wait_for_state(&client, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(done.attempt, 1);
    assert_eq!(done.result, Some(json!({"table": "orders"})));
    assert_eq!(worker.recorder().len(), 1);
    Ok(())
}
