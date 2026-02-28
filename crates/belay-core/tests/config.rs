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
