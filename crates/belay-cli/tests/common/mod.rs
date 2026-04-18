//! Shared test support: running the CLI in-process from shell-like command
//! lines, a fake API server that records requests and answers from a queue
//! of canned replies, and a real server over an in-memory engine.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use belay_cli::{Exit, Runner};
use belay_common::{ManualClock, Timestamp};
use belay_core::{Config, Engine};
use belay_http::{
    MatchedRoute, Method, Request, Response, Router, Server, ServerConfig, ServerHandle, StatusCode,
};
use belay_json::{ToJson, Value};
use belay_proto::api::{encode_body, ApiError};
use belay_proto::{Job, JobId, JobSpec};

/// Splits a command line into arguments the way a POSIX shell does for
/// simple cases: whitespace separates, and single or double quotes group
/// (no escapes), so `enqueue q k -d '{"a": 1}'` is five arguments.
pub fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(c);
                in_word = true;
            }
        }
    }
    assert!(quote.is_none(), "unbalanced quote in {line:?}");
    if in_word {
        words.push(current);
    }
    words
}

/// What one invocation printed and returned.
#[derive(Debug)]
pub struct Output {
    pub exit: Exit,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Fails with both streams shown unless the command succeeded.
    #[track_caller]
    pub fn ok(self) -> Output {
        self.exits(Exit::Success)
    }

    /// Fails with both streams shown unless the command exited with `exit`.
    #[track_caller]
    pub fn exits(self, exit: Exit) -> Output {
        assert_eq!(
            self.exit, exit,
            "stdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }

    /// Stdout parsed as JSON.
    #[track_caller]
    pub fn json(&self) -> Value {
        belay_json::parse(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", self.stdout))
    }

    /// Stdout without surrounding whitespace, e.g. an id printed by `-q`.
    pub fn id(&self) -> String {
        self.stdout.trim().to_owned()
    }
}

/// The fixed "now" of test runs.
pub fn t0() -> Timestamp {
    "2025-03-04T05:06:07.089Z".parse().expect("valid timestamp")
}

/// An isolated runner with the test clock.
pub fn runner() -> Runner {
    Runner::isolated().clock(ManualClock::new(t0()).shared())
}
