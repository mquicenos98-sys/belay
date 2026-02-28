//! A test harness: an engine without background threads on a manual
//! clock, plus shortcuts for the worker side of the protocol.

// Each test binary uses a different subset of the helpers.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use belay_common::{Clock, ManualClock, Timestamp};
use belay_core::{Config, Engine, TickReport};
use belay_json::Value;
use belay_proto::api::{CompleteRequest, FailRequest, LeaseRequest, LeasedJob};
use belay_proto::event::{Event, EventFilter};
use belay_proto::{JobSource, QueueName, WorkerId};

/// Monday 2025-03-03 08:00 UTC.
pub const START: &str = "2025-03-03T08:00:00Z";

pub struct Harness {
    pub clock: ManualClock,
    pub engine: Engine,
}

impl Harness {
    pub fn new() -> Harness {
        Harness::with_config(Config::dev())
    }

    pub fn with_config(config: Config) -> Harness {
        quiet_logs();
        let clock = ManualClock::new(START.parse::<Timestamp>().expect("valid start time"));
        let engine = Engine::builder()
            .config(config)
            .clock(clock.shared())
            .background(false)
            .seed(7)
            .start()
            .unwrap_or_else(|e| panic!("engine did not start: {e}"));
        Harness { clock, engine }
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Moves the clock and runs one round of the engine's duties.
    pub fn advance(&self, d: Duration) -> TickReport {
        let now = self.clock.advance(d);
        let report = self.engine.tick(now);
        assert!(report.errors.is_empty(), "tick failed: {:?}", report.errors);
        report
    }

    pub fn lease(&self, queue: &str, max_jobs: u32) -> Vec<LeasedJob> {
        let req = LeaseRequest::new(
            WorkerId::from_static("w1"),
            vec![QueueName::new(queue).expect("valid queue name")],
        )
        .max_jobs(max_jobs)
        .lease_for(Duration::from_secs(30));
        self.engine.lease(&req).expect("lease")
    }

    pub fn lease_one(&self, queue: &str) -> LeasedJob {
        let mut jobs = self.lease(queue, 1);
        assert_eq!(jobs.len(), 1, "expected one job in {queue}");
        jobs.remove(0)
    }

    pub fn complete(&self, leased: &LeasedJob, result: Option<Value>) {
        let mut req = CompleteRequest::new(leased.token);
        req.result = result;
        self.engine
            .complete(&leased.job.id, &req)
            .expect("complete");
    }

    pub fn fail(&self, leased: &LeasedJob, error: &str, retryable: bool) {
        let mut req = FailRequest::new(leased.token, error);
        req.retryable = retryable;
        self.engine.fail(&leased.job.id, &req).expect("fail");
    }

    /// Type names of the buffered events at or after `from`.
    pub fn event_types(&self, from: u64) -> Vec<&'static str> {
        self.engine
            .events()
            .replay(from)
            .iter()
            .map(Event::type_name)
            .collect()
    }

    pub fn all_events(&self) -> Vec<Event> {
        self.engine.events().replay(0)
    }
}

/// Keeps test output readable: only warnings and errors are logged.
pub fn quiet_logs() {
    belay_common::log::set_max_level(belay_common::log::Level::Warn);
}

/// A subscription filter replaying everything buffered.
pub fn everything() -> EventFilter {
    EventFilter::new()
}

/// Polls `condition` every few milliseconds until it holds or `timeout`
/// passes; returns whether it held.
pub fn eventually(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
