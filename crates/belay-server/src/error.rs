//! Errors from starting a server. Once it runs, every failure is answered
//! to the client as an [`ApiError`](belay_proto::api::ApiError) instead.

use std::fmt;
use std::io;

/// Why a server could not be started.
#[derive(Debug)]
pub enum Error {
    /// The listen address could not be bound.
    Bind {
        /// The address that was asked for, as written in `server.listen`.
        addr: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The HTTP layer refused the listener or its settings, or a server
    /// thread could not be started.
    Http(belay_http::Error),
}

/// `Result` defaulting to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bind { addr, source } => write!(f, "cannot listen on {addr}: {source}"),
            Error::Http(e) => write!(f, "starting the HTTP server failed: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Bind { source, .. } => Some(source),
            Error::Http(e) => Some(e),
        }
    }
}

impl From<belay_http::Error> for Error {
    fn from(e: belay_http::Error) -> Error {
        Error::Http(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn bind_errors_name_the_address_and_keep_the_cause() {
        let err = Error::Bind {
            addr: "10.0.0.1:7700".to_owned(),
            source: io::Error::new(io::ErrorKind::AddrNotAvailable, "address not available"),
        };
        assert_eq!(
            err.to_string(),
            "cannot listen on 10.0.0.1:7700: address not available"
        );
        let source = err.source().expect("bind errors wrap the io error");
        assert_eq!(source.to_string(), "address not available");
    }

    #[test]
    fn http_errors_convert_and_display_their_cause() {
        let err: Error =
            belay_http::Error::Config("max_connections must be at least 1".into()).into();
        assert!(matches!(err, Error::Http(_)));
        assert_eq!(
            err.to_string(),
            "starting the HTTP server failed: invalid configuration: max_connections must be at least 1"
        );
    }
}
