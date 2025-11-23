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

/// Environment variable holding the bearer token.
pub const ENV_TOKEN: &str = "BELAY_TOKEN";

/// Settings for a [`Client`](crate::Client).
///
/// Build one with [`ClientConfig::new`] or [`ClientConfig::from_env`] and
/// adjust it with the builder methods; every field also has a sensible
/// default through [`Default`].
///
/// ```
/// use std::time::Duration;
/// use belay_client::ClientConfig;
///
/// let config = ClientConfig::new("http://belay.internal:7700")
///     .token("s3cret")
///     .timeout(Duration::from_secs(10))
///     .retries(5);
/// assert_eq!(config.retries, 5);
/// // The token never shows up in debug output.
/// assert!(!format!("{config:?}").contains("s3cret"));
/// ```
#[derive(Clone, PartialEq)]
pub struct ClientConfig {
    /// Base URL of the server, e.g. `http://127.0.0.1:7700`. A path prefix
    /// (`http://proxy/belay`) is kept in front of every API path. Only
    /// `http` is supported; put a TLS-terminating proxy in front of belay
    /// for encryption.
    pub base_url: String,
    /// Bearer token sent as `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// Longest wait for the server's next bytes during one attempt. Long
    /// polls add their own wait on top (see
    /// [`Jobs::lease`](crate::Jobs::lease)). Default 30 s.
    pub timeout: Duration,
    /// Time allowed to establish a TCP connection. Default 5 s.
    pub connect_timeout: Duration,
    /// Extra attempts after the first for requests that are safe to
    /// repeat; 0 disables retries. See [`retry`](crate::retry) for which
    /// requests qualify. Default 3.
    pub retries: u32,
    /// Delay before each retry. A `Retry-After` from the server lengthens
    /// it. Default `exp:100ms*2..2s~equal`.
    pub backoff: BackoffPolicy,
    /// `User-Agent` header. Default `belay-client/<version>`.
    pub user_agent: String,
    /// Redirects followed per request. In a cluster a node answers requests
    /// for queues it does not own with `307` to the owner, so this should
    /// cover at least one hop; 0 returns redirects as errors. The bearer
    /// token is forwarded to the redirect target. Default 5.
    pub max_redirects: u32,
}

impl Default for ClientConfig {
    fn default() -> ClientConfig {
        ClientConfig {
            base_url: DEFAULT_URL.to_owned(),
            token: None,
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            retries: 3,
            backoff: BackoffPolicy::exponential(
                Duration::from_millis(100),
                2.0,
                Duration::from_secs(2),
            )
            .with_jitter(Jitter::Equal),
            user_agent: concat!("belay-client/", env!("CARGO_PKG_VERSION")).to_owned(),
            max_redirects: 5,
        }
    }
}

impl ClientConfig {
    /// Defaults for a server at `base_url`.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> ClientConfig {
        ClientConfig {
            base_url: base_url.into(),
            ..ClientConfig::default()
        }
    }

    /// Reads `BELAY_URL` (default [`DEFAULT_URL`]) and `BELAY_TOKEN` from
    /// the environment; everything else takes its default. Empty values
    /// count as unset.
    #[must_use]
    pub fn from_env() -> ClientConfig {
        ClientConfig::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`from_env`](Self::from_env) over an arbitrary variable lookup, so
    /// tests need not touch the process environment.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> ClientConfig {
        let non_empty = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        ClientConfig {
            base_url: non_empty(ENV_URL).unwrap_or_else(|| DEFAULT_URL.to_owned()),
            token: non_empty(ENV_TOKEN).map(|t| t.trim().to_owned()),
            ..ClientConfig::default()
        }
    }

    /// Sets [`token`](Self::token).
    #[must_use]
    pub fn token(mut self, token: impl Into<String>) -> ClientConfig {
        self.token = Some(token.into());
        self
    }

    /// Sets [`timeout`](Self::timeout).
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> ClientConfig {
        self.timeout = timeout;
        self
    }

    /// Sets [`connect_timeout`](Self::connect_timeout).
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> ClientConfig {
        self.connect_timeout = timeout;
        self
    }

    /// Sets [`retries`](Self::retries).
    #[must_use]
    pub fn retries(mut self, retries: u32) -> ClientConfig {
        self.retries = retries;
        self
    }

    /// Sets [`backoff`](Self::backoff).
    #[must_use]
    pub fn backoff(mut self, backoff: BackoffPolicy) -> ClientConfig {
        self.backoff = backoff;
        self
    }

    /// Sets [`user_agent`](Self::user_agent).
    #[must_use]
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> ClientConfig {
        self.user_agent = user_agent.into();
        self
    }

    /// Sets [`max_redirects`](Self::max_redirects).
    #[must_use]
    pub fn max_redirects(mut self, hops: u32) -> ClientConfig {
        self.max_redirects = hops;
        self
    }

    /// Checks the settings and returns the base URL in the form requests
    /// are built from: no trailing slash, no query or fragment.
    pub(crate) fn validated_base(&self) -> Result<String> {
        let url = Url::parse(&self.base_url)
            .map_err(|e| Error::Config(format!("invalid base_url {:?}: {e}", self.base_url)))?;
        if url.scheme() != "http" {
            return Err(Error::Config(format!(
                "base_url must use http://, got {}:// (put a TLS-terminating proxy in front of belay)",
                url.scheme()
            )));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(Error::Config(format!(
                "base_url must not have a query or fragment, got {:?}",
                self.base_url
            )));
        }
        if url.userinfo().is_some() {
            return Err(Error::Config(
                "base_url must not carry credentials; set token instead".to_owned(),
            ));
        }
        if self.timeout.is_zero() {
            return Err(Error::Config(
                "timeout must be greater than zero".to_owned(),
            ));
        }
        if let Some(token) = &self.token {
            if token.is_empty() || !belay_http::is_valid_value(token) || token.contains(' ') {
                return Err(Error::Config(
                    "token must be non-empty printable ASCII without spaces".to_owned(),
                ));
            }
        }
        let path = url.path().trim_end_matches('/');
        Ok(format!("http://{}{path}", url.authority()))
    }
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("base_url", &self.base_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("retries", &self.retries)
            .field("backoff", &format_args!("{}", self.backoff))
            .field("user_agent", &self.user_agent)
            .field("max_redirects", &self.max_redirects)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn env_lookup_falls_back_to_the_default_url() {
        let config = ClientConfig::from_lookup(lookup(&[]));
        assert_eq!(config.base_url, DEFAULT_URL);
        assert_eq!(config.token, None);
        let config = ClientConfig::from_lookup(lookup(&[(ENV_URL, "  "), (ENV_TOKEN, "")]));
        assert_eq!(
            (config.base_url.as_str(), config.token),
            (DEFAULT_URL, None)
        );
    }

    #[test]
    fn env_lookup_reads_url_and_trimmed_token() {
        let config = ClientConfig::from_lookup(lookup(&[
            (ENV_URL, "http://10.0.0.5:7700"),
            (ENV_TOKEN, " tok-123\n"),
        ]));
        assert_eq!(config.base_url, "http://10.0.0.5:7700");
        assert_eq!(config.token.as_deref(), Some("tok-123"));
    }

    #[test]
    fn base_url_is_normalised() {
        let base = |url: &str| ClientConfig::new(url).validated_base().unwrap();
        assert_eq!(base("http://127.0.0.1:7700"), "http://127.0.0.1:7700");
        assert_eq!(base("http://127.0.0.1:7700/"), "http://127.0.0.1:7700");
        assert_eq!(base("HTTP://Belay.Local:80/"), "http://belay.local");
        assert_eq!(base("http://proxy/belay/"), "http://proxy/belay");
        assert_eq!(base("http://[::1]:7700"), "http://[::1]:7700");
    }

    #[test]
    fn rejects_unusable_base_urls() {
        let err = |url: &str| {
            ClientConfig::new(url)
                .validated_base()
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            err("https://belay.example.com"),
            "invalid client configuration: base_url must use http://, got https:// (put a TLS-terminating proxy in front of belay)"
        );
        assert!(
            err("belay:7700").contains("invalid base_url"),
            "{}",
            err("belay:7700")
        );
        assert!(err("http://h/?x=1").contains("must not have a query"));
        assert!(err("http://user:pw@h/").contains("must not carry credentials"));
    }

    #[test]
    fn rejects_tokens_that_cannot_be_sent_in_a_header() {
        for bad in ["", "two words", "line\nbreak"] {
            let err = ClientConfig::default()
                .token(bad)
                .validated_base()
                .unwrap_err();
            assert!(matches!(err, Error::Config(_)), "{bad:?}");
        }
        assert!(ClientConfig::default()
            .token("abc.DEF-123_~+/=")
            .validated_base()
            .is_ok());
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let err = ClientConfig::default()
            .timeout(Duration::ZERO)
            .validated_base()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid client configuration: timeout must be greater than zero"
        );
    }

    #[test]
    fn debug_output_redacts_the_token() {
        let text = format!("{:?}", ClientConfig::default().token("hunter2"));
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("exp:100ms*2..2s~equal"), "{text}");
    }
}
