//! The tick: turning due fire times into jobs.
//!
//! A tick looks at every schedule whose `next_fire_at` has come, sorts its
//! due fire times into stale (vetoed by a calendar that changed), paused,
//! missed and on-time ones, applies the misfire and overlap policies,
//! dispatches, and moves the schedule to its next fire time. All the
//! bookkeeping of one tick is saved in a single batch at the end; events
//! are emitted only once it is saved. If the save fails nothing is
//! recorded, and the next tick repeats the same fires, which the
//! dispatcher deduplicates by their unique keys.

use std::time::Duration;

use belay_common::sync::lock;
use belay_common::{debug, warn, Timestamp};
use belay_proto::event::{EventKind, SkipReason as EventSkipReason};
use belay_proto::{MisfirePolicy, OverlapPolicy, Schedule, ScheduleId};
use belay_store::WriteBatch;

use super::plan::{Next, Plan, Span, COUNT_LIMIT};
use super::{Entry, Fired, Scheduler, SkipReason, Skipped, TickError, TickErrorKind, TickReport};
use crate::store;

/// The due fire times of one schedule, sorted by what happens to them.
struct Due {
    /// Times vetoed by a calendar that changed after they were computed.
    stale: Option<Span>,
    /// Times that came due while the schedule was paused.
    paused: Option<Span>,
    /// Times missed by more than the misfire threshold.
    missed: Option<Span>,
    /// The oldest missed times: as many as the policy may fire, plus one.
    missed_head: Vec<Timestamp>,
    /// Times due within the threshold, oldest first.
    on_time: Vec<Timestamp>,
    /// The first fire time after the due ones.
    next: Next,
}

/// What processing one schedule produced.
struct Processed {
    entry: Entry,
    events: Vec<EventKind>,
}

impl Scheduler {
    /// Fires every schedule that is due at `now`.
    ///
    /// Call it whenever [`next_wakeup`](Self::next_wakeup) has passed
    /// (calling it more often is harmless). `now` is normally the clock's
    /// current time; passing it explicitly keeps tests exact.
    pub fn tick(&self, now: Timestamp) -> TickReport {
        self.tick_owned(now, &|_| true)
    }

    /// Like [`tick`](Self::tick) on a node that shares its schedules with
    /// other nodes: only the schedules `owned` accepts fire. The others
    /// move past their due fire times as if paused, without dispatching,
    /// reporting or emitting anything, so that a node taking one over
    /// continues from the present instead of catching up on fires that
    /// another node already made.
    pub fn tick_owned(&self, now: Timestamp, owned: &dyn Fn(&ScheduleId) -> bool) -> TickReport {
        let mut state = lock(&self.state);
        let mut report = TickReport::default();
        let mut due: Vec<(Timestamp, ScheduleId)> = state
            .schedules
            .iter()
            .filter_map(|(id, e)| e.wakeup().filter(|t| *t <= now).map(|t| (t, id.clone())))
            .collect();
        due.sort();

        let mut results = Vec::with_capacity(due.len());
        for (_, id) in due {
            let Some(mut entry) = state.schedules.get(&id).cloned() else {
                continue;
            };
            let paused = entry.schedule.spec.paused;
            let foreign = !owned(&id);
            entry.schedule.spec.paused |= foreign;
            let mark = report.skipped.len();
            if let Some(mut done) = self.process(entry, now, &mut report) {
                if foreign {
                    done.entry.schedule.spec.paused = paused;
                    done.events.clear();
                    report.skipped.truncate(mark);
                }
                results.push(done);
            }
        }

        let mut batch = WriteBatch::new();
        for done in &results {
            let before = state.schedules.get(done.entry.schedule.id());
            if before.map_or(true, |b| b.schedule != done.entry.schedule) {
                batch.put(
                    store::schedule_key(done.entry.schedule.id()),
                    store::encode(&done.entry.schedule),
                );
            }
        }
        let saved = if batch.is_empty() {
            Ok(())
        } else {
            self.kv.write(batch).map(|_| ())
        };
        match saved {
            Ok(()) => {
                for done in results {
                    for event in done.events {
                        self.sink.emit(event);
                    }
                    let id = done.entry.schedule.spec.id.clone();
                    state.schedules.insert(id, done.entry);
                }
            }
            Err(e) => {
                warn!("saving the tick's schedule updates failed; the fires will be repeated"; error = %e);
                report.errors.push(TickError {
                    schedule: None,
                    fire_time: None,
                    kind: TickErrorKind::Store,
                    message: e.to_string(),
                });
                let retry_at = now + self.options.get_retry_delay();
                for done in results {
                    if let Some(entry) = state.schedules.get_mut(done.entry.schedule.id()) {
                        entry.retry_at = Some(retry_at);
                    }
                }
            }
        }
        if !report.is_empty() {
            debug!("scheduler tick: {report}"; at = now);
        }
        report
    }

    /// Handles the due fire times of one schedule. Returns the updated
    /// entry and the events to emit once it is saved.
    fn process(
        &self,
        mut entry: Entry,
        now: Timestamp,
        report: &mut TickReport,
    ) -> Option<Processed> {
        let plan = entry.plan.clone()?;
        let first = entry.schedule.next_fire_at?;
        let spec = entry.schedule.spec.clone();
        let id = spec.id.clone();
        let mut sched = entry.schedule.clone();
        let mut events = Vec::new();

        let keep = match spec.misfire {
            MisfirePolicy::Skip => 1,
            MisfirePolicy::FireOnce => 2,
            MisfirePolicy::FireAll { max } => max as usize + 1,
        };
        let due = collect_due(
            &plan,
            first,
            now,
            spec.paused,
            self.options.get_misfire_threshold(),
            keep,
        );

        if let Some(span) = due.stale {
            skip(report, &id, SkipReason::Calendar, span);
            events.push(skipped_event(&id, span.first, EventSkipReason::Calendar));
        }
        if let Some(span) = due.paused {
            skip(report, &id, SkipReason::Paused, span);
        }

        let mut fires: Vec<(Timestamp, bool)> = Vec::new();
        if let Some(missed) = due.missed {
            events.push(EventKind::ScheduleMisfired {
                schedule: id.clone(),
                fire_time: missed.first,
                missed: missed.count,
                policy: spec.misfire,
            });
            let allowed = match spec.misfire {
                MisfirePolicy::Skip => 0,
                MisfirePolicy::FireOnce => 1,
                MisfirePolicy::FireAll { max } => max as usize,
            };
            let n = allowed.min(due.missed_head.len());
            let caught_up: Vec<Timestamp> = match spec.misfire {
                // One job stands for the whole lot, as of the latest miss.
                MisfirePolicy::FireOnce => vec![missed.last],
                _ => due.missed_head[..n].to_vec(),
            };
            fires.extend(caught_up.into_iter().map(|t| (t, true)));
            if missed.count as usize > n {
                let first_dropped = due.missed_head.get(n).copied().unwrap_or(missed.last);
                let dropped = Span {
                    first: first_dropped,
                    last: missed.last,
                    count: missed.count - n as u32,
                };
                skip(report, &id, SkipReason::Misfire, dropped);
            }
        }
        fires.extend(due.on_time.iter().map(|t| (*t, false)));

        for (fire_time, catch_up) in fires {
            if self.overlap_blocks(&sched, fire_time, report, &mut events) {
                continue;
            }
            let job = self.job_for(&spec, fire_time, true);
            match self.dispatcher.dispatch(&job) {
                Ok(outcome) => {
                    let next = sched.next_fire_at;
                    sched.record_fire(fire_time, outcome.id, next, now);
                    events.push(EventKind::ScheduleFired {
                        schedule: id.clone(),
                        job: outcome.id,
                        fire_time,
                    });
                    report.fired.push(Fired {
                        schedule: id.clone(),
                        fire_time,
                        job: outcome.id,
                        run_at: job.run_at,
                        duplicate: outcome.duplicate,
                        catch_up,
                    });
                }
                Err(e) if e.is_transient() => {
                    warn!("dispatcher unavailable, the fire will be retried"; schedule = %id, fire_time, error = %e);
                    report.errors.push(TickError {
                        schedule: Some(id.clone()),
                        fire_time: Some(fire_time),
                        kind: TickErrorKind::DispatchUnavailable,
                        message: e.message().to_owned(),
                    });
                    // Stay at this fire; everything after it is redone
                    // once the dispatcher is back.
                    sched.next_fire_at = Some(fire_time);
                    entry.retry_at = Some(now + self.options.get_retry_delay());
                    return Some(finish(entry, sched, events));
                }
                Err(e) => {
                    warn!("dispatch rejected, the fire is dropped"; schedule = %id, fire_time, error = %e);
                    report.errors.push(TickError {
                        schedule: Some(id.clone()),
                        fire_time: Some(fire_time),
                        kind: TickErrorKind::DispatchRejected,
                        message: e.message().to_owned(),
                    });
                }
            }
        }

        sched.next_fire_at = due.next.at;
        sched.updated_at = now;
        if let (Some(span), false) = (due.next.vetoed, spec.paused) {
            skip(report, &id, SkipReason::Calendar, span);
            events.push(skipped_event(&id, span.first, EventSkipReason::Calendar));
        }
        entry.retry_at = None;
        Some(finish(entry, sched, events))
    }

    /// Applies the overlap policy before a fire. Returns whether the fire
    /// must be skipped.
    fn overlap_blocks(
        &self,
        sched: &Schedule,
        fire_time: Timestamp,
        report: &mut TickReport,
        events: &mut Vec<EventKind>,
    ) -> bool {
        let policy = sched.spec.overlap;
        if policy == OverlapPolicy::Allow {
            return false;
        }
        let Some(previous) = sched.last_job else {
            return false;
        };
        match self.dispatcher.job_state(&previous) {
            Some(state) if !state.is_terminal() => {}
            // Finished, or gone (deleted by retention): nothing overlaps.
            _ => return false,
        }
        let id = &sched.spec.id;
        match policy {
            OverlapPolicy::Allow => false,
            OverlapPolicy::Skip => {
                skip(report, id, SkipReason::Overlap, Span::new(fire_time));
                events.push(skipped_event(id, fire_time, EventSkipReason::Overlap));
                true
            }
            OverlapPolicy::CancelPrevious => {
                if let Err(e) = self.dispatcher.cancel(&previous) {
                    warn!("cancelling the previous job failed; firing anyway";
                        schedule = %id, job = %previous, error = %e);
                    report.errors.push(TickError {
                        schedule: Some(id.clone()),
                        fire_time: Some(fire_time),
                        kind: TickErrorKind::CancelFailed,
                        message: e.message().to_owned(),
                    });
                }
                false
            }
        }
    }
}

fn finish(mut entry: Entry, sched: Schedule, events: Vec<EventKind>) -> Processed {
    entry.schedule = sched;
    Processed { entry, events }
}

/// Walks the due fire times of a schedule, from `first` up to `now`.
///
/// After [`COUNT_LIMIT`] times the rest of the missed backlog is jumped
/// over (its count saturates, its last time stays exact); fire times
/// within the misfire threshold of `now` are still visited one by one, so
/// they fire on time however long the outage was.
fn collect_due(
    plan: &Plan,
    first: Timestamp,
    now: Timestamp,
    paused: bool,
    threshold: Duration,
    keep_missed: usize,
) -> Due {
    let mut due = Due {
        stale: None,
        paused: None,
        missed: None,
        missed_head: Vec::new(),
        on_time: Vec::new(),
        next: Next {
            at: Some(first),
            vetoed: None,
        },
    };
    // Fire times before the cutoff are missed.
    let cutoff = now - threshold;
    let mut jumped = false;
    let mut examined = 0u32;
    while let Some(t) = due.next.at.filter(|t| *t <= now) {
        if !plan.allows(t) {
            Span::extend(&mut due.stale, t);
        } else if paused {
            Span::extend(&mut due.paused, t);
        } else if now.saturating_duration_since(t) > threshold {
            Span::extend(&mut due.missed, t);
            if due.missed_head.len() < keep_missed {
                due.missed_head.push(t);
            }
        } else {
            due.on_time.push(t);
        }
        examined += 1;
        due.next = if examined >= COUNT_LIMIT && !jumped && t < cutoff {
            jumped = true;
            // The backlog jumped over counts as more of the same, up to the
            // last fire time before the cutoff.
            let before_cutoff = cutoff - Duration::from_millis(1);
            let last = plan.last_fire_until(before_cutoff);
            for span in [&mut due.missed, &mut due.paused].into_iter().flatten() {
                span.count = COUNT_LIMIT;
                span.last = last.map_or(span.last, |l| l.max(span.last));
            }
            plan.next_after(before_cutoff)
        } else {
            plan.next_after(t)
        };
    }
    due
}

/// Adds a skip to the report, merging it into the previous entry when both
/// are the same kind of skip of the same schedule (consecutive overlaps).
fn skip(report: &mut TickReport, id: &ScheduleId, reason: SkipReason, span: Span) {
    if let Some(last) = report.skipped.last_mut() {
        if last.schedule == *id && last.reason == reason && reason == SkipReason::Overlap {
            last.last_fire_time = span.last;
            last.count = (last.count + span.count).min(COUNT_LIMIT);
            return;
        }
    }
    report.skipped.push(Skipped {
        schedule: id.clone(),
        reason,
        fire_time: span.first,
        last_fire_time: span.last,
        count: span.count,
    });
}

fn skipped_event(id: &ScheduleId, fire_time: Timestamp, reason: EventSkipReason) -> EventKind {
    EventKind::ScheduleSkipped {
        schedule: id.clone(),
        fire_time,
        reason,
    }
}
