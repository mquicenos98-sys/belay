//! The commented example printed by `belay config default`.
//!
//! Lines of the form `#key = value` (no space after the `#`) are optional
//! settings; the tests remove those `#`s and check that the result still
//! loads, so the example cannot drift out of date.

pub(super) const EXAMPLE: &str = r##"# belay configuration.
#
# Settings are shown with their defaults unless a comment says otherwise.
# Durations are written "30s", "1h30m" or "250ms"; sizes 4096, "64MiB" or
# "1GB". Any setting can be overridden from the environment, e.g.
# BELAY_SERVER__LISTEN=0.0.0.0:7700 or BELAY_STORAGE__SYNC=never.
# A line starting with '#' directly followed by a key is an optional
# setting: remove the '#' to use it.

[server]
# Address of the HTTP API. Listening on anything but a loopback address
# requires [[auth.tokens]], or auth.disabled = true.
listen = "127.0.0.1:7700"
# Threads handling requests, and the most connections kept open.
threads = 16
max_connections = 1024
# Largest request body accepted.
max_body = "4MiB"
# Idle keep-alive connections are closed after this long.
keep_alive = "1m"
# Budget for reading one request.
request_timeout = "30s"
# On shutdown, requests in flight get this long to finish.
shutdown_grace = "30s"

[storage]
# Where jobs, schedules and workflows are kept.
dir = "belay-data"
# Keep everything in memory instead; it is lost on exit.
in_memory = false
# When the write-ahead log reaches the disk: "always" (every write is
# durable once acknowledged), "interval:10ms" (a power failure loses at
# most the last interval), or "never" (left to the operating system).
sync = "always"
# A checkpoint (snapshot) is written when the log reaches this size, and
# at least this often while anything changes. 0 disables a trigger.
checkpoint_bytes = "128MiB"
checkpoint_interval = "1h"
# Size of one log segment file.
segment_size = "32MiB"
# Damage a crash cannot explain: "strict" refuses to start,
# "skip-corrupt" recovers everything around it.
recovery = "strict"

[auth]
# Tokens are required as soon as one is configured; this switches the
# check off anyway, for development.
disabled = false

# A token in the clear, with the scopes it grants: admin, enqueue, work,
# read.
#[[auth.tokens]]
#name = "ci"
#token = "replace-with-a-long-random-secret"
#scopes = ["enqueue", "read"]

# The same as the SHA-256 of the secret, which keeps the secret itself
# out of this file: printf %s "$SECRET" | sha256sum
#[[auth.tokens]]
#name = "workers"
#token_sha256 = "87cbebfeebc05f7c54ac9336c4b4bbec831227a641951a4bde7edd56020f8590"  # gitleaks:allow
#scopes = ["work"]

[log]
# A level, optionally refined per module: "debug,belay_store=trace".
level = "info"
# "logfmt" or "json".
format = "logfmt"

[engine]
# How often leases are checked for expiry and delayed jobs promoted.
tick = "250ms"
# A lease naming several queues takes from them in turn ("round_robin")
# or strictly in the order listed ("in_order").
queue_order = "round_robin"
# A worker not heard from for this long is reported lost, and forgotten
# after worker_retention.
worker_timeout = "2m"
worker_retention = "1h"
# Job changes written per batch by bulk operations.
batch_size = 256
# Fair share between the tenants of a queue: jobs per turn, and per-tenant
# multipliers.
tenant_quantum = 1
#tenant_weights = { acme = 3 }

# Queue settings, applied at startup; these are an example, not defaults.
# Queues not configured here are created with the defaults on first use.
[queues.emails]
# Jobs running at once across all workers.
max_concurrency = 16
# Leases granted per second, and the burst allowed after a quiet spell.
rate_limit = { per_second = 50.0, burst = 100 }
# For jobs that do not set their own.
default_timeout = "5m"
default_retry = "exp:1s*2..10m~full"
default_max_attempts = 10
# How long finished jobs are kept.
retention = { succeeded = "1d", dead = "7d", cancelled = "1d" }

[scheduler]
# Set to false to stop this node from firing schedules.
enabled = true
# Longest sleep between checks for due schedules.
tick = "1s"
# A fire processed later than this counts as missed, and the schedule's
# misfire policy decides what to do about it.
misfire_threshold = "1m"
# How long a fire's job is deduplicated against a repeated fire.
dedupe_window = "1d"
# Wait before retrying a fire the queue could not take.
retry_delay = "5s"

# Calendars restrict when schedules may fire; applied at startup.
[[calendars]]
name = "business-hours"
timezone = "America/Bogota"
exclude_dates = ["2025-12-25", "2026-01-01"]
windows = [{ days = ["mon-fri"], start = "08:00", end = "18:00" }]

# Schedules, created or updated at startup. Schedules created through the
# API live alongside them; removing one from this file does not delete it.
[[schedules]]
id = "nightly-report"
cron = "0 3 * * *"
timezone = "America/Bogota"
description = "Daily sales report"
# Fires missed while belay was down: "skip", "fire_once" or
# "fire_all:<max>".
misfire = "fire_once"
# While the previous job still runs: "allow", "skip" or "cancel_previous".
overlap = "skip"
# Random delay of up to this much, so schedules sharing a minute spread out.
jitter = "30s"
template = { queue = "reports", kind = "build_daily", payload = { format = "pdf" } }

[[schedules]]
id = "hourly-sync"
cron = "0 * * * *"
calendar = "business-hours"
[schedules.template]
queue = "sync"
kind = "sync_accounts"
max_attempts = 3
timeout = "10m"

[workflows]
# Finished workflow runs are deleted after this long.
keep_for = "7d"
# How often old runs are deleted and running steps reconciled with their
# jobs.
sweep_every = "30s"
# How long a step's job is deduplicated against a repeated dispatch.
dedupe_window = "1d"

[events]
# Events kept for subscribers that reconnect with Last-Event-ID.
buffer = 10000
# Events queued per subscriber before it is switched to catching up from
# the buffer.
subscriber_queue = 1024

[cluster]
# Join other belay nodes. Each queue and schedule is then owned by one
# live node, and requests for another node's queues are sent there.
enabled = false
# Defaults to the host name.
#node_id = "belay-1"
#name = "belay"
#gossip_listen = "0.0.0.0:7946"
# The gossip address other nodes use, when gossip_listen is a wildcard.
#advertise = "10.0.0.5:7946"
# This node's API as other nodes redirect clients to it.
#api_addr = "http://10.0.0.5:7700"
#seeds = ["10.0.0.6:7946", "10.0.0.7:7946"]
# Timing preset: "lan", "wan", or "local" for processes on one machine.
#profile = "lan"
# A node fires no schedules until it has joined, or for this long.
#join_grace = "10s"
#meta = { zone = "eu-west-1a" }

[metrics]
enabled = true
# Also export process_* metrics.
process = true
# How often lease latencies are folded into the histogram.
refresh = "5s"
"##;

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::EXAMPLE;

    /// The example with every optional setting switched on.
    fn uncommented() -> String {
        EXAMPLE
            .lines()
            .map(|line| match line.strip_prefix('#') {
                Some(rest) if rest.starts_with(|c: char| c.is_ascii_lowercase() || c == '[') => {
                    rest
                }
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn example_loads_without_warnings_and_documents_the_defaults() {
        let config = Config::from_source("belay.toml", Config::example_toml()).unwrap();
        assert!(config.warnings().is_empty(), "{:?}", config.warnings());
        let defaults = Config::default();
        assert_eq!(config.server, defaults.server);
        assert_eq!(config.storage, defaults.storage);
        assert_eq!(config.auth, defaults.auth);
        assert_eq!(config.log, defaults.log);
        assert_eq!(config.engine, defaults.engine);
        assert_eq!(config.scheduler, defaults.scheduler);
        assert_eq!(config.workflows, defaults.workflows);
        assert_eq!(config.events, defaults.events);
        assert_eq!(config.metrics, defaults.metrics);
        assert_eq!(config.queues.len(), 1);
        assert_eq!(config.calendars[0].name, "business-hours");
        let ids: Vec<&str> = config.schedules.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["nightly-report", "hourly-sync"]);
        assert_eq!(config.schedules[1].template.max_attempts, Some(3));
    }

    #[test]
    fn optional_settings_in_the_example_are_valid() {
        let text = uncommented();
        assert!(text.contains("\n[[auth.tokens]]\nname = \"ci\""));
        let config = Config::from_source("belay.toml", &text).unwrap();
        assert!(config.warnings().is_empty(), "{:?}", config.warnings());
        assert_eq!(config.auth.tokens.len(), 2);
        assert!(config
            .auth
            .authenticate("correct-horse-battery-staple")
            .is_some_and(|t| t.name == "workers"));
        assert_eq!(config.cluster.seeds.len(), 2);
        assert_eq!(
            config.cluster.api_addr.as_deref(),
            Some("http://10.0.0.5:7700")
        );
        assert_eq!(config.engine.tenant_weights.len(), 1);
    }
}
