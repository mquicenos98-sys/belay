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

impl fmt::Debug for Headers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for &'a Headers {
    type Item = (&'a str, &'a str);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a str)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Headers {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut headers = Headers::new();
        headers.extend(iter);
        headers
    }
}

impl<K: Into<String>, V: Into<String>> Extend<(K, V)> for Headers {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (name, value) in iter {
            self.append(name, value);
        }
    }
}

/// `tchar` from RFC 9110 §5.6.2: the characters allowed in tokens such as
/// header names and methods.
pub(crate) fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Whether `name` is a syntactically valid field name (a non-empty token).
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_tchar)
}

/// Whether `value` can be written as a field value: no control characters
/// other than horizontal tab, which in particular rules out CR, LF and NUL.
pub fn is_valid_value(value: &str) -> bool {
    value.bytes().all(is_field_byte)
}

pub(crate) fn is_field_byte(b: u8) -> bool {
    b == b'\t' || (b >= 0x20 && b != 0x7f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_ignore_ascii_case() {
        let headers: Headers = [("Content-Type", "text/plain")].into_iter().collect();
        assert_eq!(headers.get("content-type"), Some("text/plain"));
        assert_eq!(headers.get("CONTENT-TYPE"), Some("text/plain"));
        assert!(headers.contains("Content-type"));
        assert_eq!(headers.get("content-length"), None);
    }

    #[test]
    fn insert_replaces_all_values_in_place() {
        let mut headers = Headers::new();
        headers.append("A", "1");
        headers.append("X-Tag", "a");
        headers.append("B", "2");
        headers.append("x-tag", "b");
        headers.insert("X-TAG", "c");
        let pairs: Vec<_> = headers.iter().collect();
        assert_eq!(pairs, [("A", "1"), ("X-TAG", "c"), ("B", "2")]);
    }

    #[test]
    fn remove_returns_first_value_and_drops_all() {
        let mut headers = Headers::new();
        headers.append("Via", "a");
        headers.append("Host", "h");
        headers.append("via", "b");
        assert_eq!(headers.remove("VIA"), Some("a".into()));
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.remove("via"), None);
    }

    #[test]
    fn tokens_split_lists_across_fields() {
        let mut headers = Headers::new();
        headers.append("Connection", "keep-alive, Upgrade");
        headers.append("connection", " ,close ,");
        let tokens: Vec<_> = headers.tokens("connection").collect();
        assert_eq!(tokens, ["keep-alive", "Upgrade", "close"]);
        assert!(headers.has_token("Connection", "CLOSE"));
        assert!(!headers.has_token("Connection", "clos"));
    }

    #[test]
    fn name_validation_requires_tokens() {
        assert!(is_valid_name("X-Request-Id"));
        assert!(is_valid_name("weird!#$%&'*+.^_`|~"));
        for bad in ["", "Bad Name", "Colon:", "Tab\t", "Non-\u{e9}"] {
            assert!(!is_valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn value_validation_rejects_line_breaks() {
        assert!(is_valid_value("text/plain; charset=utf-8"));
        assert!(is_valid_value("tab\tseparated"));
        assert!(is_valid_value("caf\u{e9}"));
        for bad in ["a\r\nInjected: yes", "nul\0", "bell\x07", "del\x7f"] {
            assert!(!is_valid_value(bad), "{bad:?}");
        }
    }

    #[test]
    fn validate_names_the_offending_header() {
        let mut headers = Headers::new();
        headers.append("Ok", "fine");
        headers.append("X-Evil", "a\nb");
        let err = headers.validate().expect_err("newline must be rejected");
        assert!(err.contains("X-Evil"), "{err}");
    }

    #[test]
    fn debug_renders_as_map() {
        let headers: Headers = [("A", "1")].into_iter().collect();
        assert_eq!(format!("{headers:?}"), r#"{"A": "1"}"#);
    }
}
