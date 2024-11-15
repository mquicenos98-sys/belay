//! Syntax of the logging macros.
//!
//! [`error!`](crate::error), [`warn!`](crate::warn), [`info!`](crate::info),
//! [`debug!`](crate::debug) and [`trace!`](crate::trace) share one grammar;
//! [`log!`](crate::log!) takes the level as its first argument.
//!
//! ```text
//! info!( [target: <&str expr>,] "<format string>" [, <format args>]* [; <fields>] )
//!
//! fields := field [, field]* [,]
//! field  := key = value      value converted with ToValue (numbers, strings, bool,
//!                            Duration, Timestamp, Ulid, Option<_>, ...)
//!         | key = %value     value formatted with Display
//!         | key = ?value     value formatted with Debug
//!         | key | %key | ?key   shorthand for `key = key`, `key = %key`, `key = ?key`
//! key    := identifier | "string literal"
//! ```
//!
//! The message is an ordinary `format!` string, including inline captures
//! (`"leased {count} jobs"`). A semicolon separates it from the structured
//! fields, which loggers keep as typed key/value pairs instead of baking
//! them into the text.
//!
//! When the level is disabled nothing after the level check is evaluated:
//! neither the format arguments nor the field expressions.
//!
//! ```
//! use belay_common::{debug, error, info, warn};
//! use belay_common::log::CaptureLogger;
//! use std::time::Duration;
//!
//! let capture = CaptureLogger::new();
//! capture.scope(|| {
//!     let job = "01JAB5ZK0GQX4T0C9Y6F3N2M8R";
//!     let worker = String::from("w1");
//!     let attempt = 3u32;
//!     let backoff = Duration::from_secs(90);
//!
//!     info!("lease expired"; job, worker = %worker);
//!     warn!("retrying job {} (attempt {attempt})", job; delay = backoff, attempt);
//!     debug!(target: "belay::lease", "renewed"; "lease.token" = 42u64, ok = true);
//!     error!("handler failed"; error = ?std::io::ErrorKind::TimedOut);
//! });
//!
//! let records = capture.records();
//! assert_eq!(records[0].message, "lease expired");
//! assert_eq!(records[0].field("worker"), Some("w1"));
//! assert_eq!(records[1].message, "retrying job 01JAB5ZK0GQX4T0C9Y6F3N2M8R (attempt 3)");
//! assert_eq!(records[1].field("delay"), Some("1m30s"));
//! assert_eq!(records[2].target, "belay::lease");
//! assert_eq!(records[2].field("lease.token"), Some("42"));
//! assert_eq!(records[3].field("error"), Some("TimedOut"));
//! ```

/// Logs a record at [`Level::Error`](crate::log::Level::Error).
///
/// See [`log::macros`](crate::log::macros) for the syntax.
///
/// ```
/// # let path = std::path::Path::new("/var/lib/belay");
/// belay_common::error!("checkpoint failed"; dir = %path.display(), retry = true);
/// ```
#[macro_export]
macro_rules! error {
    ($($arg:tt)+) => {
        $crate::__log!($crate::log::Level::Error, $($arg)+)
    };
}

/// Logs a record at [`Level::Warn`](crate::log::Level::Warn).
///
/// See [`log::macros`](crate::log::macros) for the syntax.
///
/// ```
/// let lag = 1_200u64;
/// belay_common::warn!("subscriber lagging, dropping {lag} events"; subscriber = "sse-7");
/// ```
#[macro_export]
macro_rules! warn {
    ($($arg:tt)+) => {
        $crate::__log!($crate::log::Level::Warn, $($arg)+)
    };
}

/// Logs a record at [`Level::Info`](crate::log::Level::Info).
///
/// See [`log::macros`](crate::log::macros) for the syntax.
///
/// ```
/// use belay_common::info;
///
/// let queue = "emails";
/// info!("queue paused"; queue, by = "admin");
/// info!(target: "belay::audit", "token rotated");
/// ```
#[macro_export]
macro_rules! info {
    ($($arg:tt)+) => {
        $crate::__log!($crate::log::Level::Info, $($arg)+)
    };
}

/// Logs a record at [`Level::Debug`](crate::log::Level::Debug).
///
/// See [`log::macros`](crate::log::macros) for the syntax.
///
/// ```
/// let batch = vec![1, 2, 3];
/// belay_common::debug!("wal batch appended"; records = batch.len(), ids = ?batch);
/// ```
#[macro_export]
macro_rules! debug {
    ($($arg:tt)+) => {
        $crate::__log!($crate::log::Level::Debug, $($arg)+)
    };
}

/// Logs a record at [`Level::Trace`](crate::log::Level::Trace).
///
/// See [`log::macros`](crate::log::macros) for the syntax.
///
/// ```
/// belay_common::trace!("poll tick");
/// ```
#[macro_export]
macro_rules! trace {
    ($($arg:tt)+) => {
        $crate::__log!($crate::log::Level::Trace, $($arg)+)
    };
}

/// Logs a record at a level chosen at run time.
///
/// ```
/// use belay_common::log::Level;
///
/// let status = 503;
/// let level = if status >= 500 { Level::Warn } else { Level::Debug };
/// belay_common::log!(level, "request finished"; status);
/// ```
#[macro_export]
macro_rules! log {
    ($level:expr, $($arg:tt)+) => {
        $crate::__log!($level, $($arg)+)
    };
}

/// Whether a record at the given level (and optionally `target:`) would be
/// logged from here; for guarding expensive preparation of log data.
///
/// ```
/// use belay_common::log::Level;
///
/// if belay_common::log_enabled!(Level::Trace) {
///     let dump = format!("{:?}", vec![0u8; 16]);
///     belay_common::trace!("state"; dump);
/// }
/// ```
#[macro_export]
macro_rules! log_enabled {
    (target: $target:expr, $level:expr $(,)?) => {
        $crate::log::enabled($level, $target)
    };
    ($level:expr $(,)?) => {
        $crate::log::enabled($level, ::core::module_path!())
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __log {
    ($level:expr, target: $target:expr, $($rest:tt)+) => {
        $crate::__log_message!(($level, $target), $($rest)+)
    };
    ($level:expr, $($rest:tt)+) => {
        $crate::__log_message!(($level, ::core::module_path!()), $($rest)+)
    };
}

/// Splits the format string and its arguments from the fields.
#[doc(hidden)]
#[macro_export]
macro_rules! __log_message {
    ($ctx:tt, $fmt:literal $(, $arg:expr)* $(,)?) => {
        $crate::__log_emit!($ctx, ($fmt $(, $arg)*), [])
    };
    ($ctx:tt, $fmt:literal $(, $arg:expr)* ; $($fields:tt)*) => {
        $crate::__log_fields!($ctx, ($fmt $(, $arg)*), [] $($fields)*)
    };
}

/// Munches `key = value` pairs into `Field` constructors.
#[doc(hidden)]
#[macro_export]
macro_rules! __log_fields {
    // Done.
    ($ctx:tt, $msg:tt, [$($out:tt)*] $(,)?) => {
        $crate::__log_emit!($ctx, $msg, [$($out)*])
    };

    // key = %value / key = ?value / key = value, with identifier keys.
    ($ctx:tt, $msg:tt, [$($out:tt)*] $key:ident = % $value:expr $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!(::core::stringify!($key), % $value),] $($($rest)*)?)
    };
    ($ctx:tt, $msg:tt, [$($out:tt)*] $key:ident = ? $value:expr $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!(::core::stringify!($key), ? $value),] $($($rest)*)?)
    };
    ($ctx:tt, $msg:tt, [$($out:tt)*] $key:ident = $value:expr $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!(::core::stringify!($key), $value),] $($($rest)*)?)
    };

    // The same with string-literal keys.
    ($ctx:tt, $msg:tt, [$($out:tt)*] $key:literal = % $value:expr $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!($key, % $value),] $($($rest)*)?)
    };
    ($ctx:tt, $msg:tt, [$($out:tt)*] $key:literal = ? $value:expr $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!($key, ? $value),] $($($rest)*)?)
    };
    ($ctx:tt, $msg:tt, [$($out:tt)*] $key:literal = $value:expr $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!($key, $value),] $($($rest)*)?)
    };

    // Shorthands: `%key`, `?key`, `key`.
    ($ctx:tt, $msg:tt, [$($out:tt)*] % $key:ident $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!(::core::stringify!($key), % $key),] $($($rest)*)?)
    };
    ($ctx:tt, $msg:tt, [$($out:tt)*] ? $key:ident $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!(::core::stringify!($key), ? $key),] $($($rest)*)?)
    };
    ($ctx:tt, $msg:tt, [$($out:tt)*] $key:ident $(, $($rest:tt)*)?) => {
        $crate::__log_fields!($ctx, $msg, [$($out)* $crate::__log_field!(::core::stringify!($key), $key),] $($($rest)*)?)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __log_field {
    ($key:expr, % $value:expr) => {
        $crate::log::Field::new($key, $crate::log::Value::Display(&$value))
    };
    ($key:expr, ? $value:expr) => {
        $crate::log::Field::new($key, $crate::log::Value::Debug(&$value))
    };
    ($key:expr, $value:expr) => {
        $crate::log::Field::new($key, $crate::log::ToValue::to_value(&$value))
    };
}

/// The level check and the single dispatch statement. Everything is one
/// expression statement so temporaries in the arguments live long enough.
#[doc(hidden)]
#[macro_export]
macro_rules! __log_emit {
    (($level:expr, $target:expr), ($($msg:tt)+), [$($field:expr,)*]) => {{
        let __belay_level: $crate::log::Level = $level;
        if $crate::log::__private::enabled_fast(__belay_level) {
            let __belay_target: &str = $target;
            if let ::core::option::Option::Some(__belay_logger) =
                $crate::log::__private::logger_for(__belay_level, __belay_target)
            {
                $crate::log::__private::dispatch(
                    &*__belay_logger,
                    __belay_level,
                    __belay_target,
                    ::core::format_args!($($msg)+),
                    &[$($field,)*],
                    ::core::module_path!(),
                    ::core::file!(),
                    ::core::line!(),
                );
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use crate::log::{CaptureLogger, Level, LevelFilter};
    use std::cell::Cell;
    use std::fmt;
    use std::time::Duration;

    #[derive(Debug)]
    struct Point {
        x: i32,
        y: i32,
    }

    impl fmt::Display for Point {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "({}, {})", self.x, self.y)
        }
    }

    #[test]
    fn message_only_with_and_without_arguments() {
        let c = CaptureLogger::new();
        c.scope(|| {
            info!("plain");
            info!("with {} args", 2);
            let n = 7;
            info!("inline {n}");
            info!("trailing comma {}", 1,);
        });
        assert_eq!(
            c.messages(),
            vec!["plain", "with 2 args", "inline 7", "trailing comma 1"]
        );
    }

    #[test]
    fn field_sigils_select_the_formatting() {
        let c = CaptureLogger::new();
        let p = Point { x: 1, y: 2 };
        c.scope(|| {
            info!("point"; plain = 5u8, shown = %p, debugged = ?p, text = "a b");
        });
        let r = &c.records()[0];
        assert_eq!(
            r.fields,
            vec![
                ("plain".to_owned(), "5".to_owned()),
                ("shown".to_owned(), "(1, 2)".to_owned()),
                ("debugged".to_owned(), "Point { x: 1, y: 2 }".to_owned()),
                ("text".to_owned(), "a b".to_owned()),
            ]
        );
    }

    #[test]
    fn shorthand_fields_use_the_variable_name() {
        let c = CaptureLogger::new();
        let job = "j1";
        let p = Point { x: 0, y: 0 };
        let delay = Duration::from_millis(1500);
        c.scope(|| info!("s"; job, %p, ?delay));
        let r = &c.records()[0];
        assert_eq!(r.field("job"), Some("j1"));
        assert_eq!(r.field("p"), Some("(0, 0)"));
        assert_eq!(r.field("delay"), Some("1.5s"));
    }

    #[test]
    fn literal_keys_and_trailing_commas() {
        let c = CaptureLogger::new();
        c.scope(|| {
            info!("k"; "http.status" = 200, "peer" = %"10.0.0.1", "raw" = ?'x',);
            info!("empty fields";);
        });
        let r = c.records();
        assert_eq!(r[0].field("http.status"), Some("200"));
        assert_eq!(r[0].field("peer"), Some("10.0.0.1"));
        assert_eq!(r[0].field("raw"), Some("'x'"));
        assert!(r[1].fields.is_empty());
    }

    #[test]
    fn format_arguments_and_fields_together() {
        let c = CaptureLogger::new();
        c.scope(|| {
            let queue = "emails";
            warn!("queue {} is {}", queue, "paused"; queue, depth = 12i64);
        });
        let r = &c.records()[0];
        assert_eq!(r.level, Level::Warn);
        assert_eq!(r.message, "queue emails is paused");
        assert_eq!(r.field("depth"), Some("12"));
    }

    #[test]
    fn temporaries_in_fields_live_long_enough() {
        let c = CaptureLogger::new();
        c.scope(|| {
            info!("t"; owned = format!("{}-{}", 1, 2), shown = %String::from("s"), n = vec![1, 2].len());
        });
        let r = &c.records()[0];
        assert_eq!(r.field("owned"), Some("1-2"));
        assert_eq!(r.field("shown"), Some("s"));
        assert_eq!(r.field("n"), Some("2"));
    }

    #[test]
    fn default_target_is_the_module_path_and_can_be_overridden() {
        let c = CaptureLogger::new();
        c.scope(|| {
            debug!("default");
            let t = String::from("custom::target");
            error!(target: &t, "custom"; code = 1);
        });
        let r = c.records();
        assert_eq!(r[0].target, module_path!());
        assert_eq!(r[1].target, "custom::target");
        assert_eq!(r[1].level, Level::Error);
    }

    #[test]
    fn disabled_levels_do_not_evaluate_arguments() {
        let c = CaptureLogger::with_level(LevelFilter::Info);
        let evaluated = Cell::new(0);
        let touch = || {
            evaluated.set(evaluated.get() + 1);
            1
        };
        c.scope(|| {
            debug!("hidden {}", touch(); field = touch());
            trace!("hidden"; field = %touch());
            info!("shown {}", touch(); field = touch());
        });
        assert_eq!(evaluated.get(), 2);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn runtime_level_macro_and_enabled_check() {
        let c = CaptureLogger::with_level(LevelFilter::Debug);
        c.scope(|| {
            for level in Level::ALL {
                log!(level, "at {level}");
            }
            assert!(log_enabled!(Level::Debug));
            assert!(!log_enabled!(Level::Trace));
            assert!(log_enabled!(target: "any", Level::Error));
        });
        assert_eq!(
            c.messages(),
            vec!["at error", "at warn", "at info", "at debug"]
        );
    }

    #[test]
    fn option_values_render_as_null_when_absent() {
        let c = CaptureLogger::new();
        let missing: Option<u32> = None;
        let present = Some("x");
        c.scope(|| info!("o"; missing, present));
        let r = &c.records()[0];
        assert_eq!(r.field("missing"), Some("null"));
        assert_eq!(r.field("present"), Some("x"));
    }
}
