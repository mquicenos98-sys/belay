//! Who may call what: each route declares an [`Access`], and the endpoint
//! checks the bearer token against it before running.
//!
//! The comparison itself lives in belay-core
//! ([`AuthConfig::authorize`]): tokens are hashed and compared with every
//! configured digest in constant time. This module turns its verdicts into
//! responses, with the `WWW-Authenticate` challenge RFC 6750 asks for, so
//! a client can tell a missing token (`401`, no error), a wrong one (`401`,
//! `invalid_token`) and a missing scope (`403`, `insufficient_scope`) apart
//! without parsing the message.

use std::fmt;

use belay_core::config::{AuthConfig, Principal, Scope};
use belay_http::header::AUTHORIZATION;
use belay_http::{Request, Response};
use belay_proto::api::{ApiError, ErrorCode};

use crate::reply;

/// The `WWW-Authenticate` header.
pub(crate) const WWW_AUTHENTICATE: &str = "WWW-Authenticate";

/// What a route requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Nothing: probes a load balancer or orchestrator must reach without
    /// credentials (`/healthz`, `/readyz`).
    Public,
    /// A token granting this scope (or `admin`), when authentication is
    /// enforced.
    Token(Scope),
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Access::Public => f.write_str("public"),
            Access::Token(scope) => scope.fmt(f),
        }
    }
}

/// Checks `req` against `scope`: the caller, or the response refusing it.
///
/// With authentication off every caller is [`Principal::anonymous`],
/// allowed everything.
pub(crate) fn authorize(
    auth: &AuthConfig,
    req: &Request,
    scope: Scope,
) -> Result<Principal, Response> {
    let header = req.header(AUTHORIZATION);
    auth.authorize(header, scope)
        .map_err(|e| refusal(&e, header.is_some(), scope))
}

/// The response to a failed check, with its challenge.
fn refusal(error: &ApiError, presented: bool, scope: Scope) -> Response {
    let challenge = match error.code {
        ErrorCode::PermissionDenied => {
            format!(r#"Bearer realm="belay", error="insufficient_scope", scope="{scope}""#)
        }
        _ if presented => r#"Bearer realm="belay", error="invalid_token""#.to_owned(),
        _ => r#"Bearer realm="belay""#.to_owned(),
    };
    reply::error(error).header(WWW_AUTHENTICATE, challenge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_core::config::{ApiToken, Scopes};
    use belay_http::Method;
    use belay_proto::api::decode_error;

    const SECRET: &str = "worker-secret-0123456789";

    fn auth() -> AuthConfig {
        AuthConfig {
            disabled: false,
            tokens: vec![ApiToken::new(
                "fleet",
                SECRET,
                [Scope::Work, Scope::Read].into_iter().collect::<Scopes>(),
            )],
        }
    }

    fn request(authorization: Option<&str>) -> Request {
        let req = Request::new(Method::Post, "/v1/lease");
        match authorization {
            Some(value) => req.with_header("Authorization", value),
            None => req,
        }
    }

    fn refused(authorization: Option<&str>, scope: Scope) -> (u16, String, ErrorCode) {
        let response = authorize(&auth(), &request(authorization), scope).unwrap_err();
        let challenge = response
            .headers
            .get(WWW_AUTHENTICATE)
            .expect("refusals carry a challenge")
            .to_owned();
        let body = response.body_bytes().expect("json body");
        let status = response.status.as_u16();
        (status, challenge, decode_error(status, body).code)
    }

    #[test]
    fn a_granted_scope_yields_the_principal() {
        let bearer = format!("Bearer {SECRET}");
        let principal = authorize(&auth(), &request(Some(&bearer)), Scope::Work).unwrap();
        assert_eq!(principal.name, "fleet");
        assert!(principal.scopes.permits(Scope::Read));
    }

    #[test]
    fn missing_invalid_and_insufficient_tokens_are_told_apart() {
        assert_eq!(
            refused(None, Scope::Work),
            (
                401,
                r#"Bearer realm="belay""#.to_owned(),
                ErrorCode::Unauthenticated
            )
        );
        assert_eq!(
            refused(Some("Bearer not-the-secret-at-all"), Scope::Work),
            (
                401,
                r#"Bearer realm="belay", error="invalid_token""#.to_owned(),
                ErrorCode::Unauthenticated
            )
        );
        assert_eq!(
            refused(Some("Basic dXNlcjpwYXNz"), Scope::Work),
            (
                401,
                r#"Bearer realm="belay", error="invalid_token""#.to_owned(),
                ErrorCode::Unauthenticated
            )
        );
        let bearer = format!("Bearer {SECRET}");
        assert_eq!(
            refused(Some(&bearer), Scope::Admin),
            (
                403,
                r#"Bearer realm="belay", error="insufficient_scope", scope="admin""#.to_owned(),
                ErrorCode::PermissionDenied
            )
        );
    }

    #[test]
    fn without_enforcement_everyone_is_anonymous() {
        let open = AuthConfig::default();
        let principal = authorize(&open, &request(None), Scope::Admin).unwrap();
        assert_eq!(principal, Principal::anonymous());
        let disabled = AuthConfig {
            disabled: true,
            ..auth()
        };
        assert!(authorize(&disabled, &request(Some("Bearer junk")), Scope::Admin).is_ok());
    }

    #[test]
    fn access_levels_display_as_scope_names() {
        assert_eq!(Access::Public.to_string(), "public");
        assert_eq!(Access::Token(Scope::Enqueue).to_string(), "enqueue");
    }
}
