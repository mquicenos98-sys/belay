//! Header maps: ordered, case-insensitive, multi-valued (RFC 9110 §5).
//!
//! [`Headers`] is a plain container and accepts any strings; validation
//! happens at the wire boundary. The server answers `500` instead of
//! emitting a response whose headers would break framing (a value with a
//! CR/LF is the classic response-splitting bug), and the client refuses to
//! send such a request.

use std::fmt;

/// Well-known header names, spelled the way most implementations send them.
///
/// Lookups are case-insensitive, so these are a convenience rather than a
/// requirement.
pub mod names {
    #![allow(missing_docs)]
    pub const ACCEPT: &str = "Accept";
    pub const ACCESS_CONTROL_ALLOW_CREDENTIALS: &str = "Access-Control-Allow-Credentials";
    pub const ACCESS_CONTROL_ALLOW_HEADERS: &str = "Access-Control-Allow-Headers";
    pub const ACCESS_CONTROL_ALLOW_METHODS: &str = "Access-Control-Allow-Methods";
    pub const ACCESS_CONTROL_ALLOW_ORIGIN: &str = "Access-Control-Allow-Origin";
    pub const ACCESS_CONTROL_EXPOSE_HEADERS: &str = "Access-Control-Expose-Headers";
    pub const ACCESS_CONTROL_MAX_AGE: &str = "Access-Control-Max-Age";
    pub const ACCESS_CONTROL_REQUEST_HEADERS: &str = "Access-Control-Request-Headers";
    pub const ACCESS_CONTROL_REQUEST_METHOD: &str = "Access-Control-Request-Method";
    pub const ALLOW: &str = "Allow";
    pub const AUTHORIZATION: &str = "Authorization";
    pub const CACHE_CONTROL: &str = "Cache-Control";
    pub const CONNECTION: &str = "Connection";
    pub const CONTENT_LENGTH: &str = "Content-Length";
    pub const CONTENT_TYPE: &str = "Content-Type";
    pub const COOKIE: &str = "Cookie";
    pub const DATE: &str = "Date";
    pub const EXPECT: &str = "Expect";
    pub const HOST: &str = "Host";
    pub const KEEP_ALIVE: &str = "Keep-Alive";
    pub const LAST_EVENT_ID: &str = "Last-Event-ID";
    pub const LOCATION: &str = "Location";
    pub const ORIGIN: &str = "Origin";
    pub const RETRY_AFTER: &str = "Retry-After";
    pub const SERVER: &str = "Server";
    pub const TRANSFER_ENCODING: &str = "Transfer-Encoding";
    pub const USER_AGENT: &str = "User-Agent";
    pub const VARY: &str = "Vary";
    pub const X_REQUEST_ID: &str = "X-Request-Id";
}

/// An ordered multi-map of header fields with case-insensitive names.
///
/// Insertion order is preserved (and is the order fields are written in);
/// names keep the spelling they were inserted with.
///
/// ```
/// use belay_http::Headers;
///
/// let mut headers = Headers::new();
/// headers.append("Accept", "text/html");
/// headers.append("accept", "application/json");
/// assert_eq!(headers.get("ACCEPT"), Some("text/html"));
/// assert_eq!(headers.get_all("accept").count(), 2);
/// headers.insert("Accept", "*/*");
/// assert_eq!(headers.get_all("accept").collect::<Vec<_>>(), ["*/*"]);
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of fields (a name appearing twice counts twice).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no fields.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The first value for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every value for `name`, in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether at least one field named `name` exists.
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Adds a field, keeping existing ones with the same name.
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries.push((name.into(), value.into()));
    }

    /// Sets `name` to a single value, replacing all existing fields of that
    /// name. The new field takes the position of the first one it replaces.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        match self
            .entries
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(&name))
        {
            Some(first) => {
                // Nothing before `first` matches, so its index survives the
                // retain unchanged.
                let mut index = 0;
                self.entries.retain(|(n, _)| {
                    let keep = index == first || !n.eq_ignore_ascii_case(&name);
                    index += 1;
                    keep
                });
                self.entries[first] = (name, value);
            }
            None => self.entries.push((name, value)),
        }
    }

    /// Removes every field named `name`, returning the first value.
    pub fn remove(&mut self, name: &str) -> Option<String> {
        let mut first = None;
        self.entries.retain_mut(|(n, v)| {
            if n.eq_ignore_ascii_case(name) {
                if first.is_none() {
                    first = Some(std::mem::take(v));
                }
                false
            } else {
                true
            }
        });
        first
    }

    /// Iterates over `(name, value)` pairs in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// The comma-separated elements of every field named `name`, trimmed,
    /// with empty elements skipped (RFC 9110 §5.6.1 list syntax).
    pub fn tokens<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.get_all(name)
            .flat_map(|v| v.split(','))
            .map(|t| t.trim_matches(|c| c == ' ' || c == '\t'))
            .filter(|t| !t.is_empty())
    }

    /// Whether the list-valued field `name` contains `token`, compared
    /// case-insensitively (e.g. `Connection: keep-alive, Upgrade`).
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.tokens(name).any(|t| t.eq_ignore_ascii_case(token))
    }

    /// Removes all fields.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Checks every field against [`is_valid_name`] and [`is_valid_value`],
    /// describing the first offender.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in &self.entries {
            if !is_valid_name(name) {
                return Err(format!("invalid header name {name:?}"));
            }
            if !is_valid_value(value) {
                return Err(format!("invalid value for header {name}: {value:?}"));
            }
        }
        Ok(())
    }
}
