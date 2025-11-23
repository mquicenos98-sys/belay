//! Client settings: where the server is, how to authenticate, and how
//! patient to be. [`ClientConfig::from_env`] reads the same `BELAY_URL` and
//! `BELAY_TOKEN` variables as the `belay` command.

use std::fmt;
use std::time::Duration;

use belay_common::backoff::Jitter;
use belay_common::BackoffPolicy;
use belay_http::Url;

use crate::error::{Error, Result};

/// The server address used when none is configured.
pub const DEFAULT_URL: &str = "http://127.0.0.1:7700";

/// Environment variable holding the server's base URL.
pub const ENV_URL: &str = "BELAY_URL";
