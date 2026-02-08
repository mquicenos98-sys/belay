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
pub(crate) mod read;
mod sections;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

use belay_conf::{Document, KeyPath, Overlay, Reader};
use belay_cron::{CronExpr, Tz};
use belay_proto::{CalendarSpec, QueueConfig, QueueName, ScheduleSpec};

pub use auth::{ApiToken, AuthConfig, Principal, Scope, Scopes, MIN_TOKEN_LEN};
pub use belay_conf::{Error as ConfigError, UnusedKey};
pub use sections::{
    default_node_id, ClusterConfig, ClusterProfile, EngineConfig, EventsConfig, LogConfig,
    MetricsConfig, SchedulerConfig, ServerConfig, StorageConfig, WorkflowsConfig,
};

use read::located;

/// The environment variable prefix: `BELAY_SERVER__LISTEN`.
pub const ENV_PREFIX: &str = "BELAY";

/// A complete belay configuration. See the [module docs](self).
///
/// `Config::default()` is what an empty file gives: a server on
/// `127.0.0.1:7700` without authentication, storing data under
/// `./belay-data`. [`Config::dev`] keeps everything in memory instead.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// `[server]`.
    pub server: ServerConfig,
    /// `[storage]`.
    pub storage: StorageConfig,
    /// `[auth]`.
    pub auth: AuthConfig,
    /// `[log]`.
    pub log: LogConfig,
    /// `[engine]`.
    pub engine: EngineConfig,
    /// `[queues.<name>]`: configurations applied at startup.
    pub queues: BTreeMap<QueueName, QueueConfig>,
    /// `[scheduler]`.
    pub scheduler: SchedulerConfig,
    /// `[[schedules]]`: created or updated at startup.
    pub schedules: Vec<ScheduleSpec>,
    /// `[[calendars]]`: created or updated at startup.
    pub calendars: Vec<CalendarSpec>,
    /// `[workflows]`.
    pub workflows: WorkflowsConfig,
    /// `[events]`.
    pub events: EventsConfig,
    /// `[cluster]`.
    pub cluster: ClusterConfig,
    /// `[metrics]`.
    pub metrics: MetricsConfig,
    warnings: Vec<UnusedKey>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            server: ServerConfig::default(),
            storage: StorageConfig::default(),
            auth: AuthConfig::default(),
            log: LogConfig::default(),
            engine: EngineConfig::default(),
            queues: BTreeMap::new(),
            scheduler: SchedulerConfig::default(),
            schedules: Vec::new(),
            calendars: Vec::new(),
            workflows: WorkflowsConfig::default(),
            events: EventsConfig::default(),
            cluster: ClusterConfig::default(),
            metrics: MetricsConfig::default(),
            warnings: Vec::new(),
        }
    }
}

impl Config {
    /// The defaults with in-memory storage: what `belay dev` runs with.
    #[must_use]
    pub fn dev() -> Config {
        let mut config = Config::default();
        config.storage.in_memory = true;
        config
    }

    /// Parses configuration text, without applying the environment.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(src: &str) -> Result<Config, ConfigError> {
        Config::from_document(&Document::parse(src)?)
    }

    /// Parses configuration text named `name` (a file name) in
    /// diagnostics, without applying the environment.
    pub fn from_source(name: &str, src: &str) -> Result<Config, ConfigError> {
        Config::from_document(&Document::parse_named(name, src)?)
    }

    /// Reads a configuration file, without applying the environment.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        Config::from_document(&Document::from_file(path)?)
    }

    /// What the server loads: the file at `path` (defaults if `None`) with
    /// `BELAY_...` variables from the process environment applied.
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let mut doc = match path {
            Some(path) => Document::from_file(path)?,
            None => Document::new(),
        };
        Overlay::new(ENV_PREFIX).apply_env(&mut doc)?;
        Config::from_document(&doc)
    }

    /// Like [`load`](Self::load) with explicit variables instead of the
    /// process environment.
    pub fn from_source_with_env<I, K, V>(
        name: &str,
        src: &str,
        vars: I,
    ) -> Result<Config, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut doc = Document::parse_named(name, src)?;
        Overlay::new(ENV_PREFIX).apply(&mut doc, vars)?;
        Config::from_document(&doc)
    }

    /// Reads a parsed (and possibly overlaid) document.
    pub fn from_document(doc: &Document) -> Result<Config, ConfigError> {
        let r = Reader::new(doc);
        let server = ServerConfig::read(&r)?;
        let auth = AuthConfig::read(&r)?;
        if !auth.disabled && auth.tokens.is_empty() && !server.is_loopback() {
            return Err(r.invalid(
                "server.listen",
                format!(
                    "{} accepts connections from other hosts, but no [[auth.tokens]] are \
                     configured; add a token, or set auth.disabled = true to serve without \
                     authentication",
                    server.listen
                ),
            ));
        }
        let cluster = ClusterConfig::read(&r, &server)?;
        let config = Config {
            storage: StorageConfig::read(&r)?,
            log: LogConfig::read(&r)?,
            engine: EngineConfig::read(&r)?,
            queues: read_queues(&r)?,
            scheduler: SchedulerConfig::read(&r)?,
            calendars: read_calendars(&r)?,
            schedules: read_schedules(&r)?,
            workflows: WorkflowsConfig::read(&r)?,
            events: EventsConfig::read(&r)?,
            metrics: MetricsConfig::read(&r)?,
            server,
            auth,
            cluster,
            warnings: Vec::new(),
        };
        Ok(Config {
            warnings: r.unused_keys(),
            ..config
        })
    }

    /// Keys in the source that nothing reads, most likely typos. `Display`
    /// renders each as a warning with its location and a suggestion.
    #[must_use]
    pub fn warnings(&self) -> &[UnusedKey] {
        &self.warnings
    }

    /// A commented example configuration covering every section, as
    /// printed by `belay config default`. It parses into a valid
    /// configuration, as do the optional settings it shows commented out.
    #[must_use]
    pub fn example_toml() -> &'static str {
        example::EXAMPLE
    }

    /// The configuration as TOML, defaults included, for
    /// `belay config print`. Token secrets are printed as `token_sha256`.
    /// Parsing the text gives back an equal configuration.
    #[must_use]
    pub fn to_toml(&self) -> String {
        belay_conf::to_string(&print::to_table(self))
    }
}

impl FromStr for Config {
    type Err = ConfigError;

    fn from_str(src: &str) -> Result<Config, ConfigError> {
        Config::from_document(&Document::parse(src)?)
    }
}

fn read_queues(r: &Reader<'_>) -> Result<BTreeMap<QueueName, QueueConfig>, ConfigError> {
    let mut queues = BTreeMap::new();
    for (name, _) in r.tables("queues")? {
        let path = KeyPath::root().key("queues").key(&name);
        let queue = QueueName::new(name.as_str()).map_err(|e| located(r, &path, e.to_string()))?;
        let config = read::domain::<QueueConfig>(r, &path.to_string())?
            .expect("tables() only lists present entries");
        queues.insert(queue, config);
    }
    Ok(queues)
}

fn read_calendars(r: &Reader<'_>) -> Result<Vec<CalendarSpec>, ConfigError> {
    let mut calendars: Vec<CalendarSpec> = Vec::new();
    let mut names = BTreeSet::new();
    for (i, _) in r.array_of_tables("calendars")?.iter().enumerate() {
        let path = KeyPath::root().key("calendars").index(i);
        let spec = read::domain::<CalendarSpec>(r, &path.to_string())?
            .expect("array_of_tables() only lists present elements");
        if let Err(e) = Tz::parse(&spec.timezone) {
            return Err(located(r, &path.key("timezone"), e.to_string()));
        }
        if !names.insert(spec.name.clone()) {
            return Err(located(
                r,
                &path.key("name"),
                format!("calendar {:?} is defined twice", spec.name),
            ));
        }
        calendars.push(spec);
    }
    Ok(calendars)
}

fn read_schedules(r: &Reader<'_>) -> Result<Vec<ScheduleSpec>, ConfigError> {
    let mut schedules: Vec<ScheduleSpec> = Vec::new();
    let mut ids = BTreeSet::new();
    for (i, _) in r.array_of_tables("schedules")?.iter().enumerate() {
        let path = KeyPath::root().key("schedules").index(i);
        let spec = read::domain::<ScheduleSpec>(r, &path.to_string())?
            .expect("array_of_tables() only lists present elements");
        // The spec's own validation checks the shape of the expression;
        // parsing it here reports field-level problems at load time
        // instead of when the engine starts.
        if let Err(e) = CronExpr::parse(&spec.cron) {
            return Err(located(r, &path.key("cron"), e.to_string()));
        }
        if let Err(e) = Tz::parse(&spec.timezone) {
            return Err(located(r, &path.key("timezone"), e.to_string()));
        }
        if !ids.insert(spec.id.clone()) {
            return Err(located(
                r,
                &path.key("id"),
                format!("schedule {} is defined twice", spec.id),
            ));
        }
        schedules.push(spec);
    }
    Ok(schedules)
}
