//! belay's configuration: a TOML file, overridable with `BELAY_...`
//! environment variables, read into [`Config`].
//!
//! ```
//! use belay_core::Config;
//!
//! let config = Config::from_str(r#"
//!     [server]
//!     listen = "127.0.0.1:7700"
//!
//!     [queues.emails]
//!     max_concurrency = 8
//!     default_timeout = "2m"
//!
//!     [[schedules]]
//!     id = "nightly-report"
//!     cron = "0 3 * * *"
//!     timezone = "America/Bogota"
//!     template = { queue = "reports", kind = "build_daily" }
//! "#)?;
//! assert_eq!(config.queues["emails"].max_concurrency, Some(8));
//! assert_eq!(config.schedules[0].id.as_str(), "nightly-report");
//! assert!(config.warnings().is_empty());
//! # Ok::<(), belay_core::config::ConfigError>(())
//! ```
//!
//! # Sections
//!
//! | section | type | contents |
//! |---|---|---|
//! | `[server]` | [`ServerConfig`] | listen address, threads, limits, timeouts |
//! | `[storage]` | [`StorageConfig`] | data directory or in-memory, sync policy, checkpoints |
//! | `[auth]` | [`AuthConfig`] | bearer tokens and their scopes |
//! | `[log]` | [`LogConfig`] | level directives, format |
//! | `[engine]` | [`EngineConfig`] | queue tick, fairness, worker tracking |
//! | `[queues.<name>]` | [`QueueConfig`] | per-queue limits, retries, retention |
//! | `[scheduler]` | [`SchedulerConfig`] | tick, misfire threshold, deduplication |
//! | `[[schedules]]` | [`ScheduleSpec`] | schedules created or updated at startup |
//! | `[[calendars]]` | [`CalendarSpec`] | calendars created or updated at startup |
//! | `[workflows]` | [`WorkflowsConfig`] | retention of finished runs |
//! | `[events]` | [`EventsConfig`] | replay buffer and subscriber queues |
//! | `[cluster]` | [`ClusterConfig`] | membership, seeds, advertised addresses |
//! | `[metrics]` | [`MetricsConfig`] | which metrics are registered |
//!
//! Durations are written as in the API (`"30s"`, `"1h30m"`; a bare number
//! is seconds) and byte sizes as `4096`, `"64MiB"` or `"1GB"`. Queue,
//! schedule and calendar tables take exactly the fields of their JSON
//! bodies in the HTTP API, and like the API they reject unknown fields.
//! Unknown keys anywhere else are collected as [`Config::warnings`], each
//! with a "did you mean" suggestion when one is close.
//!
//! # Environment
//!
//! [`Config::load`] applies `BELAY_<SECTION>__<KEY>` variables on top of
//! the file: `BELAY_SERVER__LISTEN=0.0.0.0:7700`,
//! `BELAY_STORAGE__SYNC=interval:10ms`,
//! `BELAY_QUEUES__EMAILS__MAX_CONCURRENCY=4`. Double underscores separate
//! path segments; values are typed like TOML (`16` is an integer, `true`
//! a boolean), except that a key the file already holds as a string stays
//! a string.
//!
//! # Errors
//!
//! Every problem is reported with the key path and, when the value came
//! from the file, the line it is on, rendered like a compiler diagnostic
//! by [`ConfigError`]'s `Display`.

mod auth;

mod example;

mod print;
