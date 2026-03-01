//! Loading configuration: defaults, the environment overlay, located
//! errors, warnings for unknown keys, and printing it back out.

use std::time::Duration;

use belay_common::log::LogFormat;
use belay_core::config::{ClusterProfile, ConfigError, Scope};
use belay_core::Config;
use belay_queue::QueueOrder;
use belay_store::{RecoveryMode, SyncPolicy};

fn load(src: &str) -> Config {
    Config::from_source("belay.toml", src).unwrap_or_else(|e| panic!("{e}"))
}

fn error(src: &str) -> ConfigError {
    Config::from_source("belay.toml", src).expect_err("configuration should be rejected")
}

#[test]
fn an_empty_file_gives_the_defaults() {
    let config = load("");
    assert_eq!(config, Config::default());
    assert_eq!(config.server.listen, "127.0.0.1:7700");
    assert_eq!(config.storage.sync, SyncPolicy::Always);
    assert!(!config.storage.in_memory);
    assert!(!config.auth.is_enforced());
    assert!(!config.cluster.enabled);
    assert!(Config::dev().storage.in_memory);
}

#[test]
fn every_section_is_read() {
    let config = load(
        r#"
        [server]
        listen = "0.0.0.0:8080"
        threads = 4
        max_connections = 64
        max_body = "1MiB"
        keep_alive = "5s"
        request_timeout = 10
        shutdown_grace = "2s"

        [storage]
        dir = "/var/lib/belay"
        sync = "interval:25ms"
        checkpoint_bytes = 0
        checkpoint_interval = "10m"
        segment_size = "8MiB"
        recovery = "skip-corrupt"

        [auth]
        [[auth.tokens]]
        name = "ops"
        token = "ops-token-ops-token"
        scopes = ["admin"]

        [log]
        level = "debug,belay_store=trace"
        format = "json"

        [engine]
        tick = "100ms"
        queue_order = "in_order"
        tenant_weights = { acme = 4 }

        [scheduler]
        enabled = false
        misfire_threshold = "5m"

        [workflows]
        keep_for = "2d"

        [events]
        buffer = 50
        subscriber_queue = 5

        [cluster]
        enabled = true
        node_id = "node-a"
        api_addr = "10.0.0.5:8080"
        seeds = ["10.0.0.6:7946"]
        profile = "wan"

        [metrics]
        process = false
        "#,
    );
    assert_eq!(config.server.threads, 4);
    assert_eq!(config.server.max_body, 1 << 20);
    assert_eq!(config.server.request_timeout, Duration::from_secs(10));
    assert_eq!(
        config.storage.sync,
        SyncPolicy::Interval(Duration::from_millis(25))
    );
    assert_eq!(config.storage.recovery, RecoveryMode::SkipCorrupt);
    assert_eq!(config.storage.store_options().checkpoint_bytes(), None);
    assert!(config.auth.is_enforced());
    assert!(config.auth.tokens[0].scopes.permits(Scope::Work));
    assert_eq!(config.log.format, LogFormat::Json);
    assert_eq!(config.engine.queue_order, QueueOrder::InOrder);
    assert_eq!(config.engine.tenant_weights.len(), 1);
    assert!(!config.scheduler.enabled);
    assert_eq!(config.workflows.keep_for, Duration::from_secs(2 * 86_400));
    assert_eq!(config.events.buffer, 50);
    assert_eq!(config.cluster.profile, ClusterProfile::Wan);
    assert_eq!(
        config.cluster.api_url(&config.server),
        "http://10.0.0.5:8080"
    );
    assert!(!config.metrics.process);
    assert!(config.warnings().is_empty(), "{:?}", config.warnings());
}
