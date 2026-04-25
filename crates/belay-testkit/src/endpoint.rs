//! Where a harness's API is reachable, and how the kit's clients talk to
//! it.
//!
//! A [`TestServer`](crate::TestServer) keeps its port across restarts
//! when it can; when the port was taken in the meantime it moves, and the
//! shared [`Endpoint`] lets workers built from the server follow it.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use belay_client::{Client, ClientConfig};
use belay_common::sync::{read, write};
use belay_common::BackoffPolicy;

use crate::error::Result;

/// How long the kit's clients wait for a TCP connection. Harness servers
/// are local, where a connection is made in well under a millisecond; but
/// on Windows a refused connection to a loopback port only fails when the
/// connect timeout runs out, so a long one would make every call to a
/// stopped server (a restart in progress, a killed node) crawl.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// The client settings the kit uses: near-instant retries (a harness
/// server is local), a generous timeout for loaded machines, and a short
/// connect timeout so that talking to a stopped server fails fast.
#[must_use]
pub fn client_config(url: &str) -> ClientConfig {
    ClientConfig::new(url)
        .timeout(Duration::from_secs(15))
        .connect_timeout(CONNECT_TIMEOUT)
        .retries(3)
        .backoff(BackoffPolicy::constant(Duration::from_millis(5)))
}

/// A client for `url` presenting `token`, with [`client_config`].
pub fn connect(url: &str, token: Option<&str>) -> Result<Client> {
    let mut config = client_config(url);
    if let Some(token) = token {
        config = config.token(token);
    }
    Ok(Client::new(config)?)
}

/// The current base URL of a harness server, shared with whatever talks
/// to it.
#[derive(Clone, Debug)]
pub(crate) struct Endpoint(Arc<RwLock<String>>);

impl Endpoint {
    pub(crate) fn new(url: impl Into<String>) -> Endpoint {
        Endpoint(Arc::new(RwLock::new(url.into())))
    }

    pub(crate) fn url(&self) -> String {
        read(&self.0).clone()
    }

    pub(crate) fn set(&self, url: String) {
        *write(&self.0) = url;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_follow_a_moved_endpoint() {
        let endpoint = Endpoint::new("http://127.0.0.1:7700");
        let seen_by_worker = endpoint.clone();
        endpoint.set("http://127.0.0.1:7701".to_owned());
        assert_eq!(seen_by_worker.url(), "http://127.0.0.1:7701");
    }

    #[test]
    fn clients_are_configured_for_local_servers() {
        let config = client_config("http://127.0.0.1:1");
        assert_eq!(config.connect_timeout, CONNECT_TIMEOUT);
        assert!(connect("http://127.0.0.1:1", Some("0123456789abcdef")).is_ok());
        assert!(connect("ftp://127.0.0.1:1", None).is_err());
    }
}
