//! Logging from handlers with the job attached.
//!
//! [`job_error!`](crate::job_error), [`job_warn!`](crate::job_warn),
//! [`job_info!`](crate::job_info), [`job_debug!`](crate::job_debug) and
//! [`job_trace!`](crate::job_trace) take a [`JobContext`](crate::JobContext)
//! first and otherwise follow belay-common's macro grammar
//! (`"format {}", args; key = value, other = %display`). Every record
//! gets `job`, `kind` and `attempt` fields ahead of the caller's own, so
//! one grep finds everything an attempt logged:
//!
//! ```text
//! ts=… level=info target=app::email msg="rendered template" job=01JAB… kind=send_email attempt=2 template=welcome
//! ```
//!
//! ```
//! use belay_common::log::CaptureLogger;
//! use belay_common::Timestamp;
//! use belay_proto::{Job, JobId, JobSpec};
//! use belay_worker::{job_info, job_warn, JobContext};
//!
//! let ctx = JobContext::detached(Job::new(JobId::generate(), JobSpec::new("default", "send_email"), Timestamp::now()));
//! let capture = CaptureLogger::new();
//! capture.scope(|| {
//!     let template = "welcome";
//!     job_info!(ctx, "rendered template"; template);
//!     job_warn!(ctx, "retrying smtp in {}s", 5);
//! });
//! let records = capture.records();
//! assert_eq!(records[0].field("kind"), Some("send_email"));
//! assert_eq!(records[0].field("template"), Some("welcome"));
//! assert_eq!(records[1].message, "retrying smtp in 5s");
//! assert_eq!(records[1].field("job"), Some(ctx.id().to_string().as_str()));
//! ```

/// Logs at a level chosen at run time with the job's fields attached; the
/// other `job_*` macros are shorthands for it.
///
/// ```
/// # use belay_common::Timestamp;
/// # use belay_proto::{Job, JobId, JobSpec};
/// use belay_common::log::Level;
/// use belay_worker::{job_log, JobContext};
///
/// # let ctx = JobContext::detached(Job::new(JobId::generate(), JobSpec::new("default", "k"), Timestamp::now()));
/// let slow = true;
/// job_log!(if slow { Level::Warn } else { Level::Debug }, ctx, "step finished"; slow);
/// ```
#[macro_export]
macro_rules! job_log {
    ($level:expr, $ctx:expr, $fmt:literal $(, $arg:expr)* $(,)?) => {
        $crate::__private::log!(
            $level, $fmt $(, $arg)*;
            job = %$ctx.id(), kind = $ctx.kind(), attempt = $ctx.attempt()
        )
    };
    ($level:expr, $ctx:expr, $fmt:literal $(, $arg:expr)* ; $($fields:tt)*) => {
        $crate::__private::log!(
            $level, $fmt $(, $arg)*;
            job = %$ctx.id(), kind = $ctx.kind(), attempt = $ctx.attempt(), $($fields)*
        )
    };
}

/// Logs an error with the job's fields; see [`macros`](crate::macros).
#[macro_export]
macro_rules! job_error {
    ($ctx:expr, $($rest:tt)+) => {
        $crate::job_log!($crate::__private::log::Level::Error, $ctx, $($rest)+)
    };
}

/// Logs a warning with the job's fields; see [`macros`](crate::macros).
#[macro_export]
macro_rules! job_warn {
    ($ctx:expr, $($rest:tt)+) => {
        $crate::job_log!($crate::__private::log::Level::Warn, $ctx, $($rest)+)
    };
}

/// Logs at info level with the job's fields; see [`macros`](crate::macros).
#[macro_export]
macro_rules! job_info {
    ($ctx:expr, $($rest:tt)+) => {
        $crate::job_log!($crate::__private::log::Level::Info, $ctx, $($rest)+)
    };
}

/// Logs at debug level with the job's fields; see [`macros`](crate::macros).
#[macro_export]
macro_rules! job_debug {
    ($ctx:expr, $($rest:tt)+) => {
        $crate::job_log!($crate::__private::log::Level::Debug, $ctx, $($rest)+)
    };
}

/// Logs at trace level with the job's fields; see [`macros`](crate::macros).
#[macro_export]
macro_rules! job_trace {
    ($ctx:expr, $($rest:tt)+) => {
        $crate::job_log!($crate::__private::log::Level::Trace, $ctx, $($rest)+)
    };
}

#[cfg(test)]
mod tests {
    use crate::JobContext;
    use belay_common::log::{CaptureLogger, Level, LevelFilter};
    use belay_common::Timestamp;
    use belay_proto::{Job, JobId, JobSpec};

    fn ctx() -> JobContext {
        let mut job = Job::new(
            JobId::generate(),
            JobSpec::new("default", "resize"),
            Timestamp::from_secs(1),
        );
        job.attempt = 3;
        JobContext::detached(job)
    }

    #[test]
    fn job_fields_come_first_and_caller_fields_follow() {
        let ctx = ctx();
        let capture = CaptureLogger::new();
        capture.scope(|| {
            let width = 640u32;
            job_info!(ctx, "resized to {}px", width; width, format = "png",);
        });
        let r = &capture.records()[0];
        let keys: Vec<&str> = r.fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["job", "kind", "attempt", "width", "format"]);
        assert_eq!(r.message, "resized to 640px");
        assert_eq!(r.field("attempt"), Some("3"));
        assert_eq!(r.target, module_path!());
    }

    #[test]
    fn every_level_macro_uses_its_level() {
        let ctx = ctx();
        let capture = CaptureLogger::with_level(LevelFilter::Trace);
        capture.scope(|| {
            job_error!(ctx, "e");
            job_warn!(ctx, "w");
            job_info!(ctx, "i");
            job_debug!(ctx, "d");
            job_trace!(ctx, "t");
            job_log!(Level::Info, &ctx, "via reference");
        });
        let levels: Vec<Level> = capture.records().iter().map(|r| r.level).collect();
        assert_eq!(
            levels,
            [
                Level::Error,
                Level::Warn,
                Level::Info,
                Level::Debug,
                Level::Trace,
                Level::Info
            ]
        );
    }
}
