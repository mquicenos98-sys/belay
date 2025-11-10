//! Helpers shared by the integration tests: a server with short timeouts and
//! a minimal raw HTTP/1.1 response reader that trusts nothing in the crate.

#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use belay_http::{Handler, Server, ServerConfig, ServerHandle};

/// Defaults scaled down so timeouts and shutdown are observable quickly.
pub fn fast_config() -> ServerConfig {
    ServerConfig {
        keep_alive_timeout: Duration::from_secs(2),
        header_timeout: Duration::from_secs(2),
        body_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_secs(5),
        poll_interval: Duration::from_millis(10),
        ..ServerConfig::default()
    }
}

pub fn serve(handler: impl Handler + 'static) -> ServerHandle {
    serve_with(fast_config(), handler)
}

pub fn serve_with(config: ServerConfig, handler: impl Handler + 'static) -> ServerHandle {
    Server::bind("127.0.0.1:0", config)
        .expect("bind an ephemeral port")
        .spawn(handler)
        .expect("spawn the server")
}
