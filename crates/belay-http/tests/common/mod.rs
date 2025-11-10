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

pub fn url(handle: &ServerHandle, path: &str) -> String {
    format!("http://{}{}", handle.local_addr(), path)
}

/// A parsed response as seen on the wire.
#[derive(Debug)]
pub struct RawResponse {
    pub status: u16,
    pub head: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A client socket that reads responses one at a time, keeping bytes that
/// belong to the next response.
pub struct Conn {
    pub stream: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    pub fn open(addr: SocketAddr) -> Conn {
        let stream = TcpStream::connect(addr).expect("connect to the test server");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        stream.set_nodelay(true).expect("set nodelay");
        Conn {
            stream,
            buf: Vec::new(),
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).expect("write request bytes");
    }

    fn fill(&mut self) -> usize {
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                n
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset => 0,
            Err(e) => panic!("reading the response failed: {e}"),
        }
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        while self.buf.len() < n {
            assert!(self.fill() > 0, "connection closed mid-body");
        }
        self.buf.drain(..n).collect()
    }

    fn line(&mut self) -> String {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..pos + 2).collect();
                return String::from_utf8_lossy(&line[..pos]).into_owned();
            }
            assert!(self.fill() > 0, "connection closed mid-line");
        }
    }

    /// Reads a status line and header section.
    pub fn head(&mut self) -> RawResponse {
        let status_line = self.line();
        let status = status_line
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("bad status line {status_line:?}"));
        let mut head = status_line.clone();
        let mut headers = Vec::new();
        loop {
            let line = self.line();
            if line.is_empty() {
                break;
            }
            head.push_str("\r\n");
            head.push_str(&line);
            let (name, value) = line.split_once(':').expect("header line has a colon");
            headers.push((name.to_owned(), value.trim().to_owned()));
        }
        RawResponse {
            status,
            head,
            headers,
            body: Vec::new(),
        }
    }

    /// Reads a complete response (not to a HEAD request).
    pub fn response(&mut self) -> RawResponse {
        let mut response = self.head();
        if response.status == 204 || response.status == 304 || response.status < 200 {
            return response;
        }
        if let Some(len) = response.header("content-length") {
            let len: usize = len.parse().expect("numeric content-length");
            response.body = self.take(len);
        } else if response
            .header("transfer-encoding")
            .is_some_and(|te| te.eq_ignore_ascii_case("chunked"))
        {
            loop {
                let size_line = self.line();
                let size = usize::from_str_radix(size_line.split(';').next().unwrap_or(""), 16)
                    .expect("hex chunk size");
                if size == 0 {
                    while !self.line().is_empty() {}
                    break;
                }
                let data = self.take(size);
                response.body.extend_from_slice(&data);
                assert_eq!(self.take(2), b"\r\n", "chunk data ends with crlf");
            }
        } else {
            while self.fill() > 0 {}
            response.body = std::mem::take(&mut self.buf);
        }
        response
    }

    /// Whether the server has closed the connection (EOF or reset), waiting
    /// up to the read timeout.
    pub fn closed_by_server(&mut self) -> bool {
        if !self.buf.is_empty() {
            return false;
        }
        let mut probe = [0u8; 1];
        match self.stream.read(&mut probe) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => matches!(
                e.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
            ),
        }
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}
