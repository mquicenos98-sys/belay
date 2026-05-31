//! `GET /v1/events` over the network: the typed client resuming after a
//! dropped connection without gaps or repeats, `Last-Event-ID` choosing
//! where a new stream starts (and winning over `from`), filters, and the
//! lag frame for a subscriber that fell behind the replay buffer.

use std::time::Duration;

use belay_client::EventOptions;
use belay_proto::event::EventPattern;
use belay_testkit::prelude::*;
use belay_testkit::raw::{RawEvent, RawEventStream};
use belay_testkit::TcpProxy;

fn enqueue_feed(client: &Client, n: usize) -> Result<JobId> {
    Ok(client
        .jobs()
        .enqueue(&job("feed", "item").payload(numbered(n)))?
        .id)
}

fn next_raw(stream: &mut RawEventStream) -> Result<RawEvent> {
    stream
        .next_event()?
        .ok_or_else(|| belay_testkit::Error::Invalid("the stream ended early".to_owned()))
}

#[test]
fn a_stream_resumes_after_a_dropped_connection_without_gaps_or_repeats() -> Result<()> {
    let server = TestServer::start()?;
    let proxy = TcpProxy::start(server.addr())?;
    let through_proxy = belay_testkit::connect(&proxy.url(), None)?;
    let direct = server.client();

    let options = EventOptions::default()
        .idle_timeout(Duration::from_secs(10))
        .max_reconnects(10);
    let filter = EventFilter::new().for_queue(QueueName::from_static("feed"));
    let mut stream = through_proxy.events_with(&filter, options)?;

    let mut enqueued = Vec::new();
    let mut received = Vec::new();
    for n in 0..5 {
        enqueued.push(enqueue_feed(&direct, n)?);
    }
    for _ in 0..5 {
        received.push(stream.next().expect("the stream is open")?);
    }
    let last_before_cut = stream.last_seq().expect("events were delivered");

    // The network drops; events keep happening meanwhile.
    assert!(proxy.cut() >= 1);
    for n in 5..10 {
        enqueued.push(enqueue_feed(&direct, n)?);
    }
    for _ in 0..5 {
        received.push(stream.next().expect("the stream reconnects")?);
    }
    assert!(stream.reconnects() >= 1);

    let seen: Vec<JobId> = received.iter().filter_map(|e| e.kind.job()).collect();
    assert_eq!(seen, enqueued, "every event once, in order");
    assert!(received.windows(2).all(|w| w[0].seq < w[1].seq));
    assert!(received.iter().all(|e| e.type_name() == "job.enqueued"));

    // The reconnection asked to resume right after the last event it had.
    let resumed = proxy
        .request_heads()
        .into_iter()
        .filter(|head| head.starts_with("GET /v1/events"))
        .last()
        .expect("the stream reconnected through the proxy");
    let wanted = format!("last-event-id: {last_before_cut}");
    assert!(resumed.to_ascii_lowercase().contains(&wanted), "{resumed}");
    Ok(())
}

#[test]
fn last_event_id_picks_where_a_new_stream_starts_and_wins_over_from() -> Result<()> {
    let server = TestServer::start()?;
    let client = server.client();
    let ids: Vec<JobId> = (0..6)
        .map(|n| enqueue_feed(&client, n))
        .collect::<Result<_>>()?;
    let url = format!("{}/v1/events", server.url());
    let timeout = Duration::from_secs(10);

    let mut everything = RawEventStream::open(&url, &[], timeout)?;
    assert_eq!(everything.status, 200);
    let first: Vec<RawEvent> = (0..6)
        .map(|_| next_raw(&mut everything))
        .collect::<Result<_>>()?;
    for (event, id) in first.iter().zip(&ids) {
        assert_eq!(event.event.as_deref(), Some("job.enqueued"));
        let data = belay_json::parse(&event.data)?;
        assert_eq!(data["job"], id.to_string().as_str());
    }
    let seq = |i: usize| first[i].id.clone().expect("events carry ids");

    // A client that saw the third event resumes after it.
    let third = seq(2);
    let mut resumed = RawEventStream::open(&url, &[("Last-Event-ID", &third)], timeout)?;
    let next: Vec<Option<String>> = (0..3)
        .map(|_| next_raw(&mut resumed).map(|e| e.id))
        .collect::<Result<_>>()?;
    assert_eq!(next, [Some(seq(3)), Some(seq(4)), Some(seq(5))]);

    // `from` asks for everything, Last-Event-ID says four were seen.
    let with_from = format!("{url}?from={}", seq(0));
    let mut both = RawEventStream::open(&with_from, &[("Last-Event-ID", &seq(4))], timeout)?;
    assert_eq!(next_raw(&mut both)?.id, Some(seq(5)));

    // Live events keep flowing on the resumed stream.
    let seventh = enqueue_feed(&client, 6)?;
    let live = next_raw(&mut resumed)?;
    assert_eq!(
        belay_json::parse(&live.data)?["job"],
        seventh.to_string().as_str()
    );
    Ok(())
}

#[test]
fn filters_select_types_and_queues_over_http() -> Result<()> {
    let server = TestServer::start()?;
    let client = server.client();
    let only_successes = EventFilter::new()
        .with_type("job.succeeded".parse::<EventPattern>()?)
        .for_queue(QueueName::from_static("wanted"));
    let events = EventLog::start(&client, &only_successes)?;
    let _worker = server
        .worker()
        .queues(["wanted", "other"])
        .recording("k")
        .start()?;
    let wanted = client.jobs().enqueue(&job("wanted", "k"))?.id;
    let other = client.jobs().enqueue(&job("other", "k"))?.id;
    wait_for_state(&client, other, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    events.wait_for_job_event(wanted, "job.succeeded", DEFAULT_TIMEOUT)?;
    // Everything the log received passed the filter.
    let seen = events.events();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].kind.job(), Some(wanted));

    let bad = belay_testkit::raw::request(
        "GET",
        &format!("{}/v1/events?types=job.succeded", server.url()),
        &[],
        b"",
    )?;
    assert_eq!(bad.status, 400);
    let message = bad.json()?["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("did you mean \"job.succeeded\""),
        "{message}"
    );
    Ok(())
}

