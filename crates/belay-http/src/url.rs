//! Absolute `scheme://authority/path?query#fragment` URLs (RFC 3986), with
//! reference resolution for redirects and base-URL joining.
//!
//! Only hierarchical URLs with an authority are supported; that covers every
//! URL an HTTP client dereferences. Characters that are not allowed
//! literally (spaces, non-ASCII, `"<>{}` …) are percent-encoded while
//! parsing, so the path and query accessors always return wire-ready text.
//! Internationalised host names are rejected rather than punycode-encoded.

use std::fmt;
use std::net::Ipv6Addr;
use std::str::FromStr;

use crate::percent::{hex_value, push_escape};
use crate::query::Query;

/// A parsed absolute URL.
///
/// ```
/// use belay_http::Url;
///
/// let url = Url::parse("http://Example.com:8080/v1/jobs?state=ready#top")?;
/// assert_eq!(url.host(), "example.com");
/// assert_eq!(url.port(), Some(8080));
/// assert_eq!(url.path(), "/v1/jobs");
/// assert_eq!(url.query(), Some("state=ready"));
/// assert_eq!(url.request_target(), "/v1/jobs?state=ready");
/// assert_eq!(url.join("../queues")?.to_string(), "http://example.com:8080/queues");
/// # Ok::<(), belay_http::UrlError>(())
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Url {
    scheme: String,
    userinfo: Option<String>,
    host: String,
    port: Option<u16>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}

/// Why a URL failed to parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlError {
    /// The input was empty or all whitespace.
    Empty,
    /// A control character at this byte offset.
    InvalidCharacter {
        /// Byte offset of the character.
        position: usize,
    },
    /// No `scheme:` prefix.
    MissingScheme,
    /// The scheme has characters outside `ALPHA *( ALPHA / DIGIT / + / - / . )`.
    InvalidScheme(String),
    /// The scheme is not followed by `//authority`.
    MissingAuthority,
    /// The host is empty or malformed.
    InvalidHost(String),
    /// The port is not a number in `0..=65535`.
    InvalidPort(String),
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UrlError::Empty => f.write_str("url is empty"),
            UrlError::InvalidCharacter { position } => {
                write!(f, "control character at byte {position}")
            }
            UrlError::MissingScheme => f.write_str("missing scheme (expected e.g. http://)"),
            UrlError::InvalidScheme(s) => write!(f, "invalid scheme {s:?}"),
            UrlError::MissingAuthority => f.write_str("missing // and host after the scheme"),
            UrlError::InvalidHost(h) => write!(f, "invalid host {h:?}"),
            UrlError::InvalidPort(p) => write!(f, "invalid port {p:?}"),
        }
    }
}

impl std::error::Error for UrlError {}

impl Url {
    /// Parses an absolute URL. Surrounding ASCII whitespace is ignored.
    pub fn parse(input: &str) -> Result<Url, UrlError> {
        let input = trim(input);
        if input.is_empty() {
            return Err(UrlError::Empty);
        }
        check_controls(input)?;
        let scheme_len = match scheme_end(input) {
            Some(end) => end,
            None => {
                return Err(match input.find("://") {
                    Some(end) => UrlError::InvalidScheme(input[..end].to_owned()),
                    None => UrlError::MissingScheme,
                })
            }
        };
        let scheme = input[..scheme_len].to_ascii_lowercase();
        let rest = input[scheme_len + 1..]
            .strip_prefix("//")
            .ok_or(UrlError::MissingAuthority)?;

        let authority_end = rest.find(&['/', '?', '#'][..]).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(authority_end);
        let (userinfo, host_port) = match authority.rfind('@') {
            Some(at) => (Some(normalize(&authority[..at])), &authority[at + 1..]),
            None => (None, authority),
        };
        let (host, port) = parse_host_port(host_port)?;

        let (before_fragment, fragment) = split_off(tail, '#');
        let (path, query) = split_off(before_fragment, '?');
        let mut path = normalize(path);
        if path.is_empty() {
            path.push('/');
        }
        Ok(Url {
            scheme,
            userinfo,
            host,
            port,
            path,
            query: query.map(normalize),
            fragment: fragment.map(normalize),
        })
    }

    /// The lowercased scheme.
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// The `user:password` part before the host, if any (still encoded).
    pub fn userinfo(&self) -> Option<&str> {
        self.userinfo.as_deref()
    }

    /// The lowercased host; IPv6 addresses come without brackets.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The explicit port, if the URL has one.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// The explicit port, or the scheme's default (`http` 80, `https` 443).
    pub fn port_or_default(&self) -> Option<u16> {
        self.port.or_else(|| default_port(&self.scheme))
    }

    /// The path, always starting with `/`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The raw query string without `?`.
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// The fragment without `#`.
    pub fn fragment(&self) -> Option<&str> {
        self.fragment.as_deref()
    }

    /// The decoded query parameters.
    pub fn query_pairs(&self) -> Query {
        Query::parse(self.query().unwrap_or(""))
    }

    /// `host[:port]` as it belongs in a `Host` header: IPv6 hosts are
    /// bracketed and the scheme's default port is omitted.
    pub fn authority(&self) -> String {
        let mut out = self.bracketed_host();
        if let Some(port) = self.port {
            if Some(port) != default_port(&self.scheme) {
                out.push(':');
                out.push_str(&port.to_string());
            }
        }
        out
    }

    /// The origin-form request target: path plus `?query`.
    pub fn request_target(&self) -> String {
        match &self.query {
            Some(query) => format!("{}?{}", self.path, query),
            None => self.path.clone(),
        }
    }

    /// Replaces the path; a missing leading `/` is added.
    pub fn set_path(&mut self, path: &str) {
        let mut path = normalize(path);
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
        self.path = path;
    }

    /// Replaces the raw query string (`None` removes it).
    pub fn set_query(&mut self, query: Option<&str>) {
        self.query = query.map(normalize);
    }

    /// Replaces the query with the encoded form of `query` (removed when
    /// `query` is empty).
    #[must_use]
    pub fn with_query(mut self, query: &Query) -> Url {
        self.query = if query.is_empty() {
            None
        } else {
            Some(query.to_query_string())
        };
        self
    }

    /// Replaces the fragment (`None` removes it).
    pub fn set_fragment(&mut self, fragment: Option<&str>) {
        self.fragment = fragment.map(normalize);
    }

    /// Resolves `reference` against this URL (RFC 3986 §5.2), as a browser
    /// does for links and `Location` headers.
    pub fn join(&self, reference: &str) -> Result<Url, UrlError> {
        let reference = trim(reference);
        check_controls(reference)?;
        if scheme_end(reference).is_some() {
            let mut url = Url::parse(reference)?;
            url.path = remove_dot_segments(&url.path);
            return Ok(url);
        }
        if reference.starts_with("//") {
            let mut url = Url::parse(&format!("{}:{}", self.scheme, reference))?;
            url.path = remove_dot_segments(&url.path);
            return Ok(url);
        }
        let (before_fragment, fragment) = split_off(reference, '#');
        let (path, query) = split_off(before_fragment, '?');
        let mut url = self.clone();
        url.fragment = fragment.map(normalize);
        if path.is_empty() {
            if let Some(query) = query {
                url.query = Some(normalize(query));
            }
        } else {
            let merged = if path.starts_with('/') {
                path.to_owned()
            } else {
                match self.path.rfind('/') {
                    Some(slash) => format!("{}{}", &self.path[..=slash], path),
                    None => format!("/{path}"),
                }
            };
            url.path = normalize(&remove_dot_segments(&merged));
            if url.path.is_empty() {
                url.path.push('/');
            }
            url.query = query.map(normalize);
        }
        Ok(url)
    }

    fn bracketed_host(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://", self.scheme)?;
        if let Some(userinfo) = &self.userinfo {
            write!(f, "{userinfo}@")?;
        }
        f.write_str(&self.bracketed_host())?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        f.write_str(&self.path)?;
        if let Some(query) = &self.query {
            write!(f, "?{query}")?;
        }
        if let Some(fragment) = &self.fragment {
            write!(f, "#{fragment}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Url({self})")
    }
}

impl FromStr for Url {
    type Err = UrlError;

    fn from_str(s: &str) -> Result<Self, UrlError> {
        Url::parse(s)
    }
}

/// The well-known port for `scheme`.
pub fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
}

fn check_controls(s: &str) -> Result<(), UrlError> {
    match s.bytes().position(|b| b < 0x20 || b == 0x7f) {
        Some(position) => Err(UrlError::InvalidCharacter { position }),
        None => Ok(()),
    }
}

/// Byte index of the `:` ending a valid scheme, if `s` starts with one.
fn scheme_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if !bytes.first()?.is_ascii_alphabetic() {
        return None;
    }
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        match b {
            b':' => return Some(i),
            b if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.') => {}
            _ => return None,
        }
    }
    None
}

fn split_off(s: &str, delimiter: char) -> (&str, Option<&str>) {
    match s.split_once(delimiter) {
        Some((before, after)) => (before, Some(after)),
        None => (s, None),
    }
}

fn parse_host_port(input: &str) -> Result<(String, Option<u16>), UrlError> {
    let (host, port) = if let Some(bracketed) = input.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or_else(|| UrlError::InvalidHost(input.to_owned()))?;
        let literal = &bracketed[..close];
        let addr: Ipv6Addr = literal
            .parse()
            .map_err(|_| UrlError::InvalidHost(input.to_owned()))?;
        let after = &bracketed[close + 1..];
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| UrlError::InvalidPort(after.to_owned()))?,
            ),
        };
        (addr.to_string(), port)
    } else {
        let (host, port) = split_off(input, ':');
        if host.is_empty() || !host.bytes().all(is_reg_name_byte) {
            return Err(UrlError::InvalidHost(host.to_owned()));
        }
        (host.to_ascii_lowercase(), port)
    };
    let port = match port {
        None | Some("") => None,
        Some(digits) => {
            if !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(UrlError::InvalidPort(digits.to_owned()));
            }
            Some(
                digits
                    .parse::<u16>()
                    .map_err(|_| UrlError::InvalidPort(digits.to_owned()))?,
            )
        }
    };
    Ok((host, port))
}

fn is_reg_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=%".contains(&b)
}

fn is_literal_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?".contains(&b)
}

/// Percent-encodes everything that may not appear literally, keeping
/// well-formed `%XX` escapes as they are.
fn normalize(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let valid_escape = b == b'%'
            && bytes.get(i + 1).copied().and_then(hex_value).is_some()
            && bytes.get(i + 2).copied().and_then(hex_value).is_some();
        if is_literal_byte(b) || valid_escape {
            out.push(b as char);
        } else {
            push_escape(&mut out, b);
        }
        i += 1;
    }
    out
}

/// RFC 3986 §5.2.4.
fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output: Vec<&str> = Vec::new();
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../") {
            input = rest;
        } else if let Some(rest) = input.strip_prefix("./") {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") {
            input = &input[3..];
            output.pop();
        } else if input == "/.." {
            input = "/";
            output.pop();
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let skip = usize::from(input.starts_with('/'));
            let end = input[skip..].find('/').map_or(input.len(), |i| i + skip);
            output.push(&input[..end]);
            input = &input[end..];
        }
    }
    output.concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|e| panic!("{s:?} should parse: {e}"))
    }

    #[test]
    fn parses_all_components() {
        let u = url("HTTP://user:pw@Host.Example:81/a/b?x=1&y#frag");
        assert_eq!(u.scheme(), "http");
        assert_eq!(u.userinfo(), Some("user:pw"));
        assert_eq!(u.host(), "host.example");
        assert_eq!(u.port(), Some(81));
        assert_eq!(u.path(), "/a/b");
        assert_eq!(u.query(), Some("x=1&y"));
        assert_eq!(u.fragment(), Some("frag"));
        assert_eq!(
            u.to_string(),
            "http://user:pw@host.example:81/a/b?x=1&y#frag"
        );
    }

    #[test]
    fn empty_path_becomes_root() {
        assert_eq!(url("http://h").path(), "/");
        assert_eq!(url("http://h?q").request_target(), "/?q");
        assert_eq!(url("http://h:").port(), None);
    }

    #[test]
    fn ipv6_hosts_are_bracketed_on_output() {
        let u = url("http://[::1]:9000/x");
        assert_eq!(u.host(), "::1");
        assert_eq!(u.authority(), "[::1]:9000");
        assert_eq!(u.to_string(), "http://[::1]:9000/x");
        assert_eq!(url("http://[2001:DB8::1]/").host(), "2001:db8::1");
    }

    #[test]
    fn authority_omits_default_port() {
        assert_eq!(url("http://h:80/").authority(), "h");
        assert_eq!(url("http://h:8080/").authority(), "h:8080");
        assert_eq!(url("https://h/").port_or_default(), Some(443));
        assert_eq!(url("ftp://h/").port_or_default(), None);
    }

    #[test]
    fn disallowed_characters_are_encoded() {
        let u = url("http://h/a b/\u{e9}?q=a b&r=\"x\"");
        assert_eq!(u.path(), "/a%20b/%C3%A9");
        assert_eq!(u.query(), Some("q=a%20b&r=%22x%22"));
        assert_eq!(url("http://h/100%").path(), "/100%25");
        assert_eq!(url("http://h/%41").path(), "/%41");
    }

    #[test]
    fn rejects_malformed_urls() {
        let cases = [
            ("", UrlError::Empty),
            ("   ", UrlError::Empty),
            ("/relative/path", UrlError::MissingScheme),
            ("1http://h/", UrlError::InvalidScheme("1http".into())),
            ("mailto:someone", UrlError::MissingAuthority),
            ("http:///path", UrlError::InvalidHost("".into())),
            ("http://h:99999/", UrlError::InvalidPort("99999".into())),
            ("http://h:8o/", UrlError::InvalidPort("8o".into())),
            ("http://[::1/", UrlError::InvalidHost("[::1".into())),
            ("http://[zz]/", UrlError::InvalidHost("[zz]".into())),
            ("http://ho st/", UrlError::InvalidHost("ho st".into())),
            (
                "http://h/\ninjected",
                UrlError::InvalidCharacter { position: 9 },
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(Url::parse(input), Err(expected), "{input:?}");
        }
    }

    #[test]
    fn resolves_rfc_3986_normal_examples() {
        let base = url("http://a/b/c/d;p?q");
        let cases = [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            // RFC result is "http://g"; this type always has a non-empty path.
            ("//g", "http://g/"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q#s"),
            ("g#s", "http://a/b/c/g#s"),
            ("g?y#s", "http://a/b/c/g?y#s"),
            (";x", "http://a/b/c/;x"),
            ("g;x", "http://a/b/c/g;x"),
            ("g;x?y#s", "http://a/b/c/g;x?y#s"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../", "http://a/"),
            ("../../g", "http://a/g"),
        ];
        for (reference, expected) in cases {
            let joined = base.join(reference).expect("valid reference");
            assert_eq!(joined.to_string(), expected, "reference {reference:?}");
        }
    }

    #[test]
    fn resolves_rfc_3986_abnormal_examples() {
        let base = url("http://a/b/c/d;p?q");
        let cases = [
            ("../../../g", "http://a/g"),
            ("../../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            (".g", "http://a/b/c/.g"),
            ("g..", "http://a/b/c/g.."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("./g/.", "http://a/b/c/g/"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
            ("g;x=1/../y", "http://a/b/c/y"),
            ("http://other:81/x/../y", "http://other:81/y"),
        ];
        for (reference, expected) in cases {
            let joined = base.join(reference).expect("valid reference");
            assert_eq!(joined.to_string(), expected, "reference {reference:?}");
        }
    }

    #[test]
    fn with_query_encodes_pairs() {
        let query: Query = [("state", "ready"), ("tag", "a b")].into_iter().collect();
        let u = url("http://h/v1/jobs").with_query(&query);
        assert_eq!(u.to_string(), "http://h/v1/jobs?state=ready&tag=a%20b");
        assert_eq!(u.query_pairs().get("tag"), Some("a b"));
        assert_eq!(u.with_query(&Query::new()).query(), None);
    }

    #[test]
    fn set_path_adds_leading_slash() {
        let mut u = url("http://h/old?q=1");
        u.set_path("v1/new path");
        assert_eq!(u.to_string(), "http://h/v1/new%20path?q=1");
    }
}
