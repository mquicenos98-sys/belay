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

impl ApiToken {
    /// A token for the clear-text `secret`.
    #[must_use]
    pub fn new(name: impl Into<String>, secret: &str, scopes: Scopes) -> ApiToken {
        ApiToken {
            name: name.into(),
            digest: sha256(secret.as_bytes()),
            scopes,
        }
    }
}

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiToken")
            .field("name", &self.name)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// Who made a request, as established by [`AuthConfig::authorize`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// The token's name, or `anonymous` when authentication is off.
    pub name: String,
    /// What the caller may do.
    pub scopes: Scopes,
}

impl Principal {
    /// The caller when authentication is not enforced: allowed everything.
    #[must_use]
    pub fn anonymous() -> Principal {
        Principal {
            name: "anonymous".to_owned(),
            scopes: Scopes::all(),
        }
    }
}

/// `[auth]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthConfig {
    /// Serve without authentication even if tokens are configured.
    pub disabled: bool,
    /// The accepted tokens, `[[auth.tokens]]`.
    pub tokens: Vec<ApiToken>,
}

impl AuthConfig {
    /// Whether requests must present a token.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        !self.disabled && !self.tokens.is_empty()
    }

    /// The token whose secret is `secret`, compared by digest in constant
    /// time. Every configured token is compared, match or not, so the time
    /// taken does not reveal which one matched.
    #[must_use]
    pub fn authenticate(&self, secret: &str) -> Option<&ApiToken> {
        let digest = sha256(secret.as_bytes());
        let mut found = None;
        for token in &self.tokens {
            if constant_time_eq(&token.digest, &digest) && found.is_none() {
                found = Some(token);
            }
        }
        found
    }

    /// Checks an `Authorization` header value for `scope`, returning the
    /// caller or the error to answer with (`unauthenticated` for a missing
    /// or unknown token, `permission_denied` for a missing scope).
    ///
    /// ```
    /// use belay_core::config::{ApiToken, AuthConfig, Scope, Scopes};
    /// use belay_proto::api::ErrorCode;
    ///
    /// let auth = AuthConfig {
    ///     disabled: false,
    ///     tokens: vec![ApiToken::new("ci", "0123456789abcdef", [Scope::Enqueue].into_iter().collect())],
    /// };
    /// let caller = auth.authorize(Some("Bearer 0123456789abcdef"), Scope::Enqueue).unwrap();
    /// assert_eq!(caller.name, "ci");
    /// let denied = auth.authorize(Some("Bearer 0123456789abcdef"), Scope::Admin).unwrap_err();
    /// assert_eq!(denied.code, ErrorCode::PermissionDenied);
    /// assert_eq!(auth.authorize(None, Scope::Read).unwrap_err().code, ErrorCode::Unauthenticated);
    /// ```
    pub fn authorize(
        &self,
        authorization: Option<&str>,
        scope: Scope,
    ) -> std::result::Result<Principal, ApiError> {
        if !self.is_enforced() {
            return Ok(Principal::anonymous());
        }
        let Some(header) = authorization.map(str::trim) else {
            return Err(ApiError::unauthenticated(
                "missing bearer token (send an Authorization: Bearer <token> header)",
            ));
        };
        let secret = match header.split_once(' ') {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
            _ => {
                return Err(ApiError::unauthenticated(
                    "unsupported authorization scheme; expected Bearer",
                ))
            }
        };
        let token = self
            .authenticate(secret)
            .ok_or_else(|| ApiError::unauthenticated("invalid bearer token"))?;
        if !token.scopes.permits(scope) {
            return Err(ApiError::permission_denied(format!(
                "token {} lacks the {scope} scope",
                token.name
            )));
        }
        Ok(Principal {
            name: token.name.clone(),
            scopes: token.scopes,
        })
    }

    pub(crate) fn read(r: &Reader<'_>) -> Result<AuthConfig> {
        let disabled = r.or("auth.disabled", false)?;
        let mut tokens: Vec<ApiToken> = Vec::new();
        let mut names = BTreeSet::new();
        for t in r.array_of_tables("auth.tokens")? {
            let token = read_token(&t)?;
            if !names.insert(token.name.clone()) {
                return Err(t.invalid("name", format!("token name {:?} is used twice", token.name)));
            }
            if let Some(other) = tokens.iter().find(|o| o.digest == token.digest) {
                return Err(t.invalid("", format!("has the same secret as token {:?}", other.name)));
            }
            tokens.push(token);
        }
        Ok(AuthConfig { disabled, tokens })
    }
}

fn read_token(t: &Reader<'_>) -> Result<ApiToken> {
    let name = t.string("name")?;
    if name.is_empty() || name.len() > MAX_TOKEN_NAME_LEN {
        return Err(t.invalid(
            "name",
            format!(
                "must be 1-{MAX_TOKEN_NAME_LEN} characters, got {}",
                name.len()
            ),
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(t.invalid("name", format!("contains invalid character {bad:?}")));
    }
    let clear: Option<String> = t.opt("token")?;
    let hashed: Option<String> = t.opt("token_sha256")?;
    let digest = match (clear, hashed) {
        (Some(_), Some(_)) => {
            return Err(t.invalid("token_sha256", "set either token or token_sha256, not both"))
        }
        (None, None) => {
            return Err(t.invalid("", "needs a token or a token_sha256"));
        }
        (Some(secret), None) => {
            if secret.chars().count() < MIN_TOKEN_LEN {
                return Err(t.invalid(
                    "token",
                    format!("must be at least {MIN_TOKEN_LEN} characters"),
                ));
            }
            sha256(secret.as_bytes())
        }
        (None, Some(hex)) => parse_hex_digest(&hex).map_err(|e| t.invalid("token_sha256", e))?,
    };
    let names: Vec<String> = t.req("scopes")?;
    let mut scopes = Scopes::NONE;
    for (i, name) in names.iter().enumerate() {
        let scope = name
            .parse::<Scope>()
            .map_err(|e| t.invalid(&format!("scopes[{i}]"), e))?;
        scopes.insert(scope);
    }
    if scopes.is_empty() {
        return Err(t.invalid("scopes", "must name at least one scope"));
    }
    Ok(ApiToken {
        name,
        digest,
        scopes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_conf::Document;
    use belay_proto::api::ErrorCode;

    fn auth(src: &str) -> Result<AuthConfig> {
        let doc = Document::parse(src).expect("valid TOML");
        AuthConfig::read(&Reader::new(&doc))
    }

    const SECRET: &str = "s3cret-s3cret-s3cret";

    #[test]
    fn clear_and_hashed_tokens_authenticate_alike() {
        let hashed = crate::sha256::to_hex(&sha256(b"another-long-secret"));
        let config = auth(&format!(
            "[[auth.tokens]]\nname = 'ci'\ntoken = '{SECRET}'\nscopes = ['enqueue']\n\
             [[auth.tokens]]\nname = 'ops'\ntoken_sha256 = '{hashed}'\nscopes = ['admin']\n"
        ))
        .unwrap();
        assert!(config.is_enforced());
        assert_eq!(config.authenticate(SECRET).unwrap().name, "ci");
        assert_eq!(
            config.authenticate("another-long-secret").unwrap().name,
            "ops"
        );
        assert!(config.authenticate("s3cret").is_none());
        // Debug output never shows the digest.
        assert!(!format!("{config:?}").contains("digest"));
    }

    #[test]
    fn admin_permits_every_scope_and_others_only_their_own() {
        let admin: Scopes = [Scope::Admin].into_iter().collect();
        let worker: Scopes = [Scope::Work, Scope::Read].into_iter().collect();
        assert!(Scope::ALL.iter().all(|s| admin.permits(*s)));
        assert!(worker.permits(Scope::Work) && worker.permits(Scope::Read));
        assert!(!worker.permits(Scope::Enqueue));
        assert_eq!(worker.to_string(), "work,read");
    }

    #[test]
    fn authorize_distinguishes_missing_invalid_and_insufficient() {
        let config = AuthConfig {
            disabled: false,
            tokens: vec![ApiToken::new(
                "w",
                SECRET,
                [Scope::Work].into_iter().collect(),
            )],
        };
        let bearer = format!("Bearer {SECRET}");
        assert_eq!(
            config.authorize(Some(&bearer), Scope::Work).unwrap().name,
            "w"
        );
        let lowercase = format!("bearer {SECRET}");
        assert!(config.authorize(Some(&lowercase), Scope::Work).is_ok());
        let cases = [
            (None, Scope::Work, ErrorCode::Unauthenticated),
            (
                Some("Basic dXNlcjpwYXNz"),
                Scope::Work,
                ErrorCode::Unauthenticated,
            ),
            (
                Some("Bearer wrong-wrong-wrong"),
                Scope::Work,
                ErrorCode::Unauthenticated,
            ),
            (
                Some(bearer.as_str()),
                Scope::Enqueue,
                ErrorCode::PermissionDenied,
            ),
        ];
        for (header, scope, code) in cases {
            assert_eq!(
                config.authorize(header, scope).unwrap_err().code,
                code,
                "{header:?}"
            );
        }
        let disabled = AuthConfig {
            disabled: true,
            ..config
        };
        assert_eq!(
            disabled.authorize(None, Scope::Admin).unwrap(),
            Principal::anonymous()
        );
    }

    #[test]
    fn bad_tokens_are_located_errors() {
        let cases = [
            (
                "[[auth.tokens]]\nname = 'a'\ntoken = 'short'\nscopes = ['read']\n",
                "invalid value for auth.tokens[0].token: must be at least 16 characters",
                Some((3, 9)),
            ),
            (
                "[[auth.tokens]]\nname = 'a'\nscopes = ['read']\n",
                "invalid value for auth.tokens[0]: needs a token or a token_sha256",
                Some((1, 1)),
            ),
            (
                "[[auth.tokens]]\nname = 'a'\ntoken_sha256 = 'abc'\nscopes = ['read']\n",
                "invalid value for auth.tokens[0].token_sha256: expected 64 hexadecimal digits, got 3",
                Some((3, 16)),
            ),
            (
                "[[auth.tokens]]\nname = 'a'\ntoken = 'not-a-real-token'\nscopes = ['reed']\n",
                "invalid value for auth.tokens[0].scopes[0]: unknown scope \"reed\", expected admin, enqueue, work or read",
                Some((4, 11)),
            ),
            (
                "[[auth.tokens]]\nname = 'a'\ntoken = 'not-a-real-token'\nscopes = []\n",
                "invalid value for auth.tokens[0].scopes: must name at least one scope",
                Some((4, 10)),
            ),
        ];
        for (src, message, at) in cases {
            let err = auth(src).unwrap_err();
            assert_eq!(err.message(), message);
            assert_eq!(err.line_col(), at, "{message}");
        }
    }

    #[test]
    fn duplicate_names_and_secrets_are_rejected() {
        let token = |name: &str, secret: &str| {
            format!("[[auth.tokens]]\nname = '{name}'\ntoken = '{secret}'\nscopes = ['read']\n")
        };
        let twice = format!("{}{}", token("a", SECRET), token("a", "0123456789abcdef"));
        assert_eq!(
            auth(&twice).unwrap_err().message(),
            "invalid value for auth.tokens[1].name: token name \"a\" is used twice"
        );
        let same_secret = format!("{}{}", token("a", SECRET), token("b", SECRET));
        assert_eq!(
            auth(&same_secret).unwrap_err().message(),
            "invalid value for auth.tokens[1]: has the same secret as token \"a\""
        );
    }
}
