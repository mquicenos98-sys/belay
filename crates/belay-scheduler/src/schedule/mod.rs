//! Cron schedules and the calendars that restrict them.
//!
//! [`Scheduler`] owns every schedule and calendar, keeps them in memory
//! (they are few and small) and persists each change as one atomic batch.
//! Firing happens only in [`Scheduler::tick`], which the caller drives:
//! belay-core runs it from a loop that sleeps until
//! [`Scheduler::next_wakeup`]; tests call it with a manual clock.
//!
//! # Semantics
//!
//! - **Fire times** are the matches of the cron expression in the
//!   schedule's time zone, inside `[starts_at, ends_at)`, that its calendar
//!   (if any) allows, evaluated in the calendar's own zone. DST follows
//!   belay-cron: a time skipped by a spring-forward fires once at the
//!   first instant after the gap, and a repeated time fires once.
//!   `Schedule::next_fire_at` is always the next such time.
//! - **On time or missed.** A due fire processed within the misfire
//!   threshold ([`SchedulerOptions::misfire_threshold`], 60s by default)
//!   is on time. Older due fires are misfires and the schedule's
//!   [`MisfirePolicy`](belay_proto::MisfirePolicy) decides: `skip` drops
//!   them, `fire_once` fires once for the lot, stamped with the *first*
//!   missed time, and `fire_all:<max>` fires the oldest `max` of them.
//! - **Overlap.** Before firing, the state of the previous fire's job is
//!   checked: `allow` ignores it, `skip` skips the fire while that job is
//!   unfinished, `cancel_previous` asks the dispatcher to cancel it first.
//! - **Paused** schedules keep moving through their fire times, reporting
//!   each as skipped, so resuming never unleashes a burst of catch-up jobs.
//! - **Jitter.** Each fire's job gets `run_at = fire_time + offset`, where
//!   the offset is derived from the schedule id and fire time
//!   ([`jitter_offset`]) and lies in `[0, jitter)`. The job is dispatched at
//!   the nominal time; the queue holds it until `run_at`.
//! - **Exactly one job per fire.** Each dispatched job carries the unique
//!   key `sched:<id>:<fire time in ms>` for
//!   [`SchedulerOptions::dedupe_window`], plus `meta["belay.schedule"]` and
//!   `meta["belay.fire_time"]`. Fire selection is deterministic from the
//!   stored `next_fire_at`, so if belay stops after dispatching but before
//!   saving, the next tick repeats the same fires and the dispatcher hands
//!   back the jobs it already has.

mod calendar;
mod plan;
mod report;
mod tick;

pub use plan::jitter_offset;
pub use report::{Fired, SkipReason, Skipped, TickError, TickErrorKind, TickReport};

pub(crate) use calendar::CompiledCalendar;
pub(crate) use plan::Plan;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::sync::lock;
use belay_common::{warn, SharedClock, Timestamp};
use belay_json::{FromJsonError, PathSegment};
use belay_proto::api::{Cursor, Page};
use belay_proto::event::EventKind;
use belay_proto::job::{META_FIRE_TIME, META_SCHEDULE};
use belay_proto::validate::{Validate, Validator, Violations};
use belay_proto::{
    CalendarSpec, EventSink, JobSpec, Schedule, ScheduleId, ScheduleSpec, Unique, UniqueScope,
};
use belay_store::{Kv, WriteBatch};

use crate::dispatch::Dispatcher;
use crate::error::{Error, Result};
use crate::options::SchedulerOptions;
use crate::store;

/// `meta` key marking jobs started by [`Scheduler::trigger_now`]; its
/// value is `manual`.
pub const META_TRIGGER: &str = "belay.trigger";

/// Most fire times [`Scheduler::preview`] returns.
pub const MAX_PREVIEW: usize = 1000;

/// The unique key of the job for schedule `id`'s fire at `fire_time`.
///
/// ```
/// use belay_common::Timestamp;
/// use belay_proto::ScheduleId;
///
/// let key = belay_scheduler::fire_key(&ScheduleId::from_static("nightly"), Timestamp::from_secs(60));
/// assert_eq!(key, "sched:nightly:60000");
/// ```
#[must_use]
pub fn fire_key(id: &ScheduleId, fire_time: Timestamp) -> String {
    format!("sched:{id}:{}", fire_time.as_millis())
}

/// What an upsert did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upserted {
    /// Nothing by that name existed; it was created.
    Created,
    /// It existed with a different definition, which was replaced.
    Updated,
    /// It existed with exactly this definition; nothing was written.
    Unchanged,
}

/// Cron schedules and calendars. See the [module docs](self) for how
/// schedules fire.
///
/// All methods take `&self`; one internal lock serialises them, and
/// [`tick`](Self::tick) holds it while it dispatches, so the dispatcher
/// must not call back into the scheduler.
pub struct Scheduler {
    kv: Arc<dyn Kv>,
    clock: SharedClock,
    dispatcher: Arc<dyn Dispatcher>,
    sink: Arc<dyn EventSink>,
    options: SchedulerOptions,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    schedules: BTreeMap<ScheduleId, Entry>,
    calendars: BTreeMap<String, Arc<CompiledCalendar>>,
}

#[derive(Clone)]
struct Entry {
    schedule: Schedule,
    /// `None` when the stored definition no longer compiles (a calendar
    /// lost to manual store surgery, a zone dropped from the table): the
    /// schedule is kept but never fires until it is updated.
    plan: Option<Arc<Plan>>,
    /// Do not try again before this time after the dispatcher was
    /// unavailable. Memory only: a restart retries at once.
    retry_at: Option<Timestamp>,
}

impl Entry {
    /// When the schedule next needs a tick, if ever.
    fn wakeup(&self) -> Option<Timestamp> {
        self.plan.as_ref()?;
        let next = self.schedule.next_fire_at?;
        Some(self.retry_at.map_or(next, |r| r.max(next)))
    }
}

impl State {
    fn entry(&self, id: &ScheduleId) -> Result<&Entry> {
        self.schedules
            .get(id)
            .ok_or_else(|| Error::not_found("schedule", id))
    }

    fn using_calendar(&self, name: &str) -> Vec<ScheduleId> {
        self.schedules
            .values()
            .filter(|e| e.schedule.spec.calendar.as_deref() == Some(name))
            .map(|e| e.schedule.spec.id.clone())
            .collect()
    }
}

impl Scheduler {
    /// Opens the scheduler with default options, loading every stored
    /// calendar and schedule.
    pub fn open(
        kv: Arc<dyn Kv>,
        clock: SharedClock,
        dispatcher: Arc<dyn Dispatcher>,
        sink: Arc<dyn EventSink>,
    ) -> Result<Scheduler> {
        Scheduler::open_with(kv, clock, dispatcher, sink, SchedulerOptions::default())
    }

    /// Opens the scheduler with `options`.
    ///
    /// Stored schedules keep their `next_fire_at`: fires that came due
    /// while belay was down are handled by the first tick according to each
    /// schedule's misfire policy. A stored schedule that no longer compiles
    /// is kept (and logged) but does not fire until updated.
    pub fn open_with(
        kv: Arc<dyn Kv>,
        clock: SharedClock,
        dispatcher: Arc<dyn Dispatcher>,
        sink: Arc<dyn EventSink>,
        options: SchedulerOptions,
    ) -> Result<Scheduler> {
        options.validate()?;
        let mut state = State::default();
        for spec in store::load_all::<CalendarSpec>(&*kv, store::calendars())? {
            let name = spec.name.clone();
            match CompiledCalendar::compile(spec) {
                Ok(calendar) => {
                    state.calendars.insert(name, Arc::new(calendar));
                }
                Err(reason) => {
                    warn!("ignoring stored calendar with an unusable time zone"; calendar = %name, reason = %reason);
                }
            }
        }
        for schedule in store::load_all::<Schedule>(&*kv, store::schedules())? {
            let plan = compile_stored(&state, &schedule.spec);
            if let Err(reason) = &plan {
                warn!("stored schedule does not compile and will not fire until updated";
                    schedule = %schedule.spec.id, reason = %reason);
            }
            state.schedules.insert(
                schedule.spec.id.clone(),
                Entry {
                    schedule,
                    plan: plan.ok().map(Arc::new),
                    retry_at: None,
                },
            );
        }
        Ok(Scheduler {
            kv,
            clock,
            dispatcher,
            sink,
            options,
            state: Mutex::new(state),
        })
    }

    /// The options the scheduler runs with.
    #[must_use]
    pub fn options(&self) -> &SchedulerOptions {
        &self.options
    }

    /// Checks `spec` as [`create`](Self::create) would, without storing
    /// anything.
    pub fn validate(&self, spec: &ScheduleSpec) -> Result<()> {
        let state = lock(&self.state);
        check_spec(&state, spec, self.clock.now()).map(|_| ())
    }

    /// Adds a schedule. Fails with [`Error::AlreadyExists`] if its id is
    /// taken and [`Error::Invalid`] if the cron expression, time zone,
    /// calendar reference or template is wrong.
    pub fn create(&self, spec: ScheduleSpec) -> Result<Schedule> {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        if state.schedules.contains_key(&spec.id) {
            return Err(Error::already_exists("schedule", &spec.id));
        }
        self.insert(&mut state, spec, None, now)
    }

    /// Replaces the definition of an existing schedule, keeping its
    /// history (fire count, last fire and job). The next fire time is
    /// recomputed from now, so fires missed before the update are not
    /// caught up.
    pub fn update(&self, spec: ScheduleSpec) -> Result<Schedule> {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let existing = state.entry(&spec.id)?.schedule.clone();
        if existing.spec == spec {
            return Ok(existing);
        }
        self.insert(&mut state, spec, Some(existing), now)
    }

    /// Creates or replaces a schedule; an identical definition is left
    /// alone, which makes it safe to apply configuration files on every
    /// start.
    pub fn upsert(&self, spec: ScheduleSpec) -> Result<(Schedule, Upserted)> {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let existing = state.schedules.get(&spec.id).map(|e| e.schedule.clone());
        match existing {
            Some(existing) if existing.spec == spec => Ok((existing, Upserted::Unchanged)),
            Some(existing) => self
                .insert(&mut state, spec, Some(existing), now)
                .map(|s| (s, Upserted::Updated)),
            None => self
                .insert(&mut state, spec, None, now)
                .map(|s| (s, Upserted::Created)),
        }
    }

    fn insert(
        &self,
        state: &mut State,
        spec: ScheduleSpec,
        existing: Option<Schedule>,
        now: Timestamp,
    ) -> Result<Schedule> {
        let plan = check_spec(state, &spec, now)?;
        let next = plan.next_fire(now);
        let schedule = match existing {
            Some(mut schedule) => {
                schedule.spec = spec;
                schedule.next_fire_at = next;
                schedule.updated_at = now;
                schedule
            }
            None => Schedule::new(spec, next, now),
        };
        self.save(&schedule)?;
        let id = schedule.spec.id.clone();
        let created = !state.schedules.contains_key(&id);
        state.schedules.insert(
            id.clone(),
            Entry {
                schedule: schedule.clone(),
                plan: Some(Arc::new(plan)),
                retry_at: None,
            },
        );
        self.sink.emit(if created {
            EventKind::ScheduleCreated { schedule: id }
        } else {
            EventKind::ScheduleUpdated { schedule: id }
        });
        Ok(schedule)
    }

    /// Removes a schedule, returning its last state. Jobs it already
    /// dispatched are not touched.
    pub fn delete(&self, id: &ScheduleId) -> Result<Schedule> {
        let mut state = lock(&self.state);
        let schedule = state.entry(id)?.schedule.clone();
        let mut batch = WriteBatch::with_capacity(1);
        batch.delete(store::schedule_key(id));
        self.kv.write(batch)?;
        state.schedules.remove(id);
        self.sink.emit(EventKind::ScheduleDeleted {
            schedule: id.clone(),
        });
        Ok(schedule)
    }

    /// The schedule `id`.
    #[must_use]
    pub fn get(&self, id: &ScheduleId) -> Option<Schedule> {
        lock(&self.state)
            .schedules
            .get(id)
            .map(|e| e.schedule.clone())
    }

    /// Every schedule, by id.
    #[must_use]
    pub fn list(&self) -> Vec<Schedule> {
        lock(&self.state)
            .schedules
            .values()
            .map(|e| e.schedule.clone())
            .collect()
    }

    /// One page of schedules by id, starting after `cursor` (a name cursor
    /// from a previous page).
    #[must_use]
    pub fn list_page(&self, cursor: Option<&Cursor>, limit: u32) -> Page<Schedule> {
        let after = cursor.and_then(Cursor::as_name);
        let state = lock(&self.state);
        let items = state
            .schedules
            .iter()
            .filter(|(id, _)| after.map_or(true, |a| id.as_str() > a))
            .take(limit as usize + 1)
            .map(|(_, e)| e.schedule.clone())
            .collect();
        Page::paginate(items, limit, |s: &Schedule| {
            Cursor::from_name(s.id().as_str())
        })
    }

    /// Number of schedules.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.state).schedules.len()
    }

    /// Whether there are no schedules.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pauses a schedule: its fire times keep passing but are reported as
    /// skipped. Pausing a paused schedule changes nothing.
    pub fn pause(&self, id: &ScheduleId) -> Result<Schedule> {
        self.set_paused(id, true)
    }

    /// Resumes a paused schedule. Fire times that came due while it was
    /// paused are not caught up; it fires next at its first fire time after
    /// now.
    pub fn resume(&self, id: &ScheduleId) -> Result<Schedule> {
        self.set_paused(id, false)
    }

    fn set_paused(&self, id: &ScheduleId, paused: bool) -> Result<Schedule> {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let entry = state.entry(id)?.clone();
        if entry.schedule.spec.paused == paused {
            return Ok(entry.schedule);
        }
        let mut schedule = entry.schedule;
        schedule.spec.paused = paused;
        schedule.updated_at = now;
        self.save(&schedule)?;
        state.schedules.insert(
            id.clone(),
            Entry {
                schedule: schedule.clone(),
                plan: entry.plan,
                retry_at: None,
            },
        );
        let id = id.clone();
        self.sink.emit(if paused {
            EventKind::SchedulePaused { schedule: id }
        } else {
            EventKind::ScheduleResumed { schedule: id }
        });
        Ok(schedule)
    }

    /// Fires schedule `id` now, whatever its cron expression, calendar,
    /// bounds, pause state or overlap policy say. The job's fire time is
    /// now, its `meta["belay.trigger"]` is `manual`, and it becomes the
    /// schedule's last job; the next regular fire time is unchanged.
    pub fn trigger_now(&self, id: &ScheduleId) -> Result<Fired> {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let entry = state.entry(id)?.clone();
        let mut job = self.job_for(&entry.schedule.spec, now, false);
        job.meta
            .insert(META_TRIGGER.to_owned(), "manual".to_owned());
        if let Some(unique) = &mut job.unique {
            // A cron fire may share this millisecond; keep the keys apart.
            unique.key.push_str(":manual");
        }
        let outcome = self.dispatcher.dispatch(&job)?;
        let mut schedule = entry.schedule;
        let next = schedule.next_fire_at;
        schedule.record_fire(now, outcome.id, next, now);
        self.save(&schedule)?;
        state.schedules.insert(
            id.clone(),
            Entry {
                schedule,
                plan: entry.plan,
                retry_at: entry.retry_at,
            },
        );
        self.sink.emit(EventKind::ScheduleFired {
            schedule: id.clone(),
            job: outcome.id,
            fire_time: now,
        });
        Ok(Fired {
            schedule: id.clone(),
            fire_time: now,
            job: outcome.id,
            run_at: None,
            duplicate: outcome.duplicate,
            catch_up: false,
        })
    }

    /// The next `n` (at most [`MAX_PREVIEW`]) fire times of schedule `id`,
    /// starting with its stored `next_fire_at` (which may already be due).
    /// Paused schedules are previewed as if they were running.
    pub fn preview(&self, id: &ScheduleId, n: usize) -> Result<Vec<Timestamp>> {
        let state = lock(&self.state);
        let entry = state.entry(id)?;
        let n = n.min(MAX_PREVIEW);
        let (Some(plan), Some(first)) = (&entry.plan, entry.schedule.next_fire_at) else {
            return Ok(Vec::new());
        };
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut times = vec![first];
        times.extend(plan.upcoming(first, n - 1));
        Ok(times)
    }

    /// The first `n` (at most [`MAX_PREVIEW`]) fire times of `spec` after
    /// `from`, validating it first. Calendars are looked up among the
    /// stored ones.
    pub fn preview_spec(
        &self,
        spec: &ScheduleSpec,
        from: Timestamp,
        n: usize,
    ) -> Result<Vec<Timestamp>> {
        let state = lock(&self.state);
        let plan = check_spec(&state, spec, from)?;
        Ok(plan.upcoming(from, n.min(MAX_PREVIEW)))
    }

    /// When the next tick has work to do: the earliest fire time of any
    /// schedule (or its retry time after a dispatch failure). `None` when
    /// nothing will ever fire. Paused schedules count, because the tick
    /// moves them past their fire times.
    #[must_use]
    pub fn next_wakeup(&self) -> Option<Timestamp> {
        lock(&self.state)
            .schedules
            .values()
            .filter_map(Entry::wakeup)
            .min()
    }

    /// Creates or replaces a calendar. Schedules that use it get their next
    /// fire time recomputed in the same atomic write.
    pub fn put_calendar(&self, spec: CalendarSpec) -> Result<Upserted> {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let existing = state.calendars.get(&spec.name);
        if existing.map_or(false, |c| *c.spec() == spec) {
            return Ok(Upserted::Unchanged);
        }
        let upserted = if existing.is_some() {
            Upserted::Updated
        } else {
            Upserted::Created
        };
        let calendar = Arc::new(check_calendar(&spec)?);
        let mut batch = WriteBatch::new();
        batch.put(store::calendar_key(&spec.name), store::encode(&spec));
        let mut changed = Vec::new();
        for id in state.using_calendar(&spec.name) {
            let mut entry = state.entry(&id)?.clone();
            let plan = Plan::compile(&entry.schedule.spec, Some(Arc::clone(&calendar))).map_err(
                |reason| Error::Corrupt {
                    key: store::printable(&store::schedule_key(&id)),
                    reason,
                },
            )?;
            // An overdue fire stays due if the new calendar still allows it.
            let from = match entry.schedule.next_fire_at {
                Some(t) if t <= now => t - Duration::from_millis(1),
                _ => now,
            };
            entry.schedule.next_fire_at = plan.next_fire(from);
            entry.schedule.updated_at = now;
            entry.plan = Some(Arc::new(plan));
            batch.put(store::schedule_key(&id), store::encode(&entry.schedule));
            changed.push((id, entry));
        }
        self.kv.write(batch)?;
        state.calendars.insert(spec.name.clone(), calendar);
        for (id, entry) in changed {
            state.schedules.insert(id, entry);
        }
        Ok(upserted)
    }

    /// The calendar `name`.
    #[must_use]
    pub fn get_calendar(&self, name: &str) -> Option<CalendarSpec> {
        lock(&self.state)
            .calendars
            .get(name)
            .map(|c| c.spec().clone())
    }

    /// Every calendar, by name.
    #[must_use]
    pub fn list_calendars(&self) -> Vec<CalendarSpec> {
        lock(&self.state)
            .calendars
            .values()
            .map(|c| c.spec().clone())
            .collect()
    }

    /// Schedules that use calendar `name`, by id.
    #[must_use]
    pub fn schedules_using(&self, name: &str) -> Vec<ScheduleId> {
        lock(&self.state).using_calendar(name)
    }

    /// Removes a calendar. Refused with [`Error::CalendarInUse`] while any
    /// schedule refers to it.
    pub fn delete_calendar(&self, name: &str) -> Result<CalendarSpec> {
        let mut state = lock(&self.state);
        let spec = state
            .calendars
            .get(name)
            .map(|c| c.spec().clone())
            .ok_or_else(|| Error::not_found("calendar", name))?;
        let users = state.using_calendar(name);
        if !users.is_empty() {
            return Err(Error::CalendarInUse {
                calendar: name.to_owned(),
                schedules: users,
            });
        }
        let mut batch = WriteBatch::with_capacity(1);
        batch.delete(store::calendar_key(name));
        self.kv.write(batch)?;
        state.calendars.remove(name);
        Ok(spec)
    }

    fn save(&self, schedule: &Schedule) -> Result<()> {
        let mut batch = WriteBatch::with_capacity(1);
        batch.put(store::schedule_key(schedule.id()), store::encode(schedule));
        self.kv.write(batch)?;
        Ok(())
    }

    /// The job for `spec`'s fire at `fire_time`: the template stamped with
    /// the schedule, the fire time and the fire's unique key, delayed by
    /// the fire's jitter when `jittered`.
    fn job_for(&self, spec: &ScheduleSpec, fire_time: Timestamp, jittered: bool) -> JobSpec {
        let mut job = spec.template.clone();
        job.meta
            .insert(META_SCHEDULE.to_owned(), spec.id.to_string());
        job.meta
            .insert(META_FIRE_TIME.to_owned(), fire_time.to_rfc3339());
        job.unique = Some(Unique::new(
            fire_key(&spec.id, fire_time),
            UniqueScope::For(self.options.get_dedupe_window()),
        ));
        let offset = if jittered {
            jitter_offset(&spec.id, fire_time, spec.jitter)
        } else {
            Duration::ZERO
        };
        if !offset.is_zero() {
            // run_at and delay are exclusive; fold a template delay in.
            let delay = job.delay.take().unwrap_or(Duration::ZERO);
            job.run_at = Some(fire_time + offset + delay);
        }
        job
    }
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("Scheduler")
            .field("schedules", &state.schedules.len())
            .field("calendars", &state.calendars.len())
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// Validates `spec` against the known calendars and compiles it.
///
/// The cron expression and zone are checked by belay-cron, whose messages
/// are more precise than the shape checks in belay-proto; everything else
/// is belay-proto's validation.
fn check_spec(state: &State, spec: &ScheduleSpec, now: Timestamp) -> Result<Plan> {
    let mut v = Validator::new();
    let expr = plan::parse_cron(&spec.cron)
        .map_err(|problem| v.report("cron", problem))
        .ok();
    let tz = plan::parse_timezone(&spec.timezone)
        .map_err(|problem| v.report("timezone", problem))
        .ok();
    let mut rest = spec.clone();
    rest.cron = "* * * * *".to_owned();
    rest.timezone = ScheduleSpec::DEFAULT_TIMEZONE.to_owned();
    rest.validate_into(&mut v);
    let calendar = match &spec.calendar {
        Some(name) => {
            let found = state.calendars.get(name).cloned();
            // Names that break the naming rule were reported above.
            if found.is_none() && ScheduleId::new(name.as_str()).is_ok() {
                let known: Vec<&str> = state.calendars.keys().map(String::as_str).collect();
                let e = FromJsonError::unknown_variant(name, "calendar", &known);
                v.report("calendar", e.message());
            }
            found
        }
        None => None,
    };
    v.finish()?;
    let expr = expr.expect("a clean validation implies the cron expression parsed");
    let tz = tz.expect("a clean validation implies the zone parsed");
    let plan = Plan::new(expr, tz, calendar, spec.starts_at, spec.ends_at);
    if let (Some(name), Some(_)) = (&spec.calendar, plan.calendar()) {
        if plan.next_fire(now).is_none() && plan.cron_matches_after(now) {
            return Err(Error::Invalid(Violations::single(
                &[PathSegment::Key("calendar".into())],
                format!("no time matching the cron expression is allowed by calendar {name:?}"),
            )));
        }
    }
    Ok(plan)
}

/// Compiles a stored schedule without re-validating it.
fn compile_stored(state: &State, spec: &ScheduleSpec) -> Result<Plan, String> {
    let calendar = match &spec.calendar {
        Some(name) => Some(
            state
                .calendars
                .get(name)
                .cloned()
                .ok_or_else(|| format!("calendar {name:?} does not exist"))?,
        ),
        None => None,
    };
    Plan::compile(spec, calendar)
}

/// Validates a calendar and parses its zone. As for schedules, the zone is
/// judged by belay-cron rather than by belay-proto's shape check.
fn check_calendar(spec: &CalendarSpec) -> Result<CompiledCalendar> {
    let mut v = Validator::new();
    let mut rest = spec.clone();
    rest.timezone = ScheduleSpec::DEFAULT_TIMEZONE.to_owned();
    rest.validate_into(&mut v);
    let compiled = CompiledCalendar::compile(spec.clone())
        .map_err(|problem| v.report("timezone", problem))
        .ok();
    v.finish()?;
    Ok(compiled.expect("a clean validation implies the zone parsed"))
}
