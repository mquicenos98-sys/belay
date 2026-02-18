//! `[auth]`: bearer tokens and the scopes they grant.
//!
//! ```toml
//! [[auth.tokens]]
//! name = "ci"
//! token_sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"  # gitleaks:allow
//! scopes = ["enqueue", "read"]
//! ```
//!
//! A token is given either in the clear (`token`) or as the SHA-256 of the
//! clear text (`token_sha256`, 64 hex digits), which keeps the secret out
//! of the configuration file. Either way only the digest is kept in
//! memory, and presented tokens are hashed and compared with every
//! configured digest in constant time.
//!
//! Authentication is enforced as soon as one token is configured. Without
//! tokens the API is open, which is refused for a server listening on
//! anything but a loopback address unless `auth.disabled = true` says so
//! explicitly; `disabled` also switches checks off when tokens exist, for
//! development.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use belay_conf::{Reader, Result};
use belay_proto::api::ApiError;

use crate::sha256::{constant_time_eq, parse_hex_digest, sha256, Digest};

/// Shortest clear-text token accepted.
pub const MIN_TOKEN_LEN: usize = 16;

/// Longest token name.
pub const MAX_TOKEN_NAME_LEN: usize = 64;

/// What a token may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Everything, including queue, schedule and store administration.
    Admin,
    /// Enqueue jobs and submit workflows.
    Enqueue,
    /// Lease jobs and report on them: what workers need.
    Work,
    /// Read jobs, queues, schedules, workflows, events and metrics.
    Read,
}

impl Scope {
    /// Every scope.
    pub const ALL: [Scope; 4] = [Scope::Admin, Scope::Enqueue, Scope::Work, Scope::Read];

    /// Lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Scope::Admin => "admin",
            Scope::Enqueue => "enqueue",
            Scope::Work => "work",
            Scope::Read => "read",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Scope, String> {
        Scope::ALL
            .into_iter()
            .find(|scope| scope.as_str() == s)
            .ok_or_else(|| format!("unknown scope {s:?}, expected admin, enqueue, work or read"))
    }
}

/// A set of scopes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Scopes(u8);

impl Scopes {
    /// No scope at all.
    pub const NONE: Scopes = Scopes(0);

    /// Every scope.
    #[must_use]
    pub fn all() -> Scopes {
        Scope::ALL.into_iter().collect()
    }

    /// Whether `scope` is in the set itself.
    #[must_use]
    pub fn contains(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// Whether the set permits `scope`: it contains it, or `admin`.
    #[must_use]
    pub fn permits(self, scope: Scope) -> bool {
        self.contains(scope) || self.contains(Scope::Admin)
    }

    /// Adds `scope`.
    pub fn insert(&mut self, scope: Scope) {
        self.0 |= scope.bit();
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The scopes in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = Scope> {
        Scope::ALL.into_iter().filter(move |s| self.contains(*s))
    }
}

impl FromIterator<Scope> for Scopes {
    fn from_iter<I: IntoIterator<Item = Scope>>(iter: I) -> Scopes {
        let mut scopes = Scopes::NONE;
        for scope in iter {
            scopes.insert(scope);
        }
        scopes
    }
}

impl fmt::Display for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.iter().map(Scope::as_str).collect();
        f.write_str(&names.join(","))
    }
}

impl fmt::Debug for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Scopes({self})")
    }
}

/// A configured token: its name, the SHA-256 of its secret, its scopes.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiToken {
    /// Name, for logs and errors; never the secret.
    pub name: String,
    /// SHA-256 of the secret.
    pub digest: Digest,
    /// What it may do.
    pub scopes: Scopes,
}
