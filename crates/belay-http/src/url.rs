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
