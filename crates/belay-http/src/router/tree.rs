//! The route tree: one node per path segment.
//!
//! At each node static children are tried first, then the parameter child,
//! then the wildcard, backtracking on failure, so `/jobs/stats` beats
//! `/jobs/:id` regardless of registration order. When the path matches a
//! route that does not allow the method, the search keeps looking for
//! another match that does and only reports 405 if there is none.

use std::fmt;
use std::sync::Arc;

use crate::handler::Handler;
use crate::method::Method;

/// Why a route could not be registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    /// The pattern is malformed.
    InvalidPattern {
        /// The offending pattern.
        pattern: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The same method and pattern were registered twice.
    Duplicate {
        /// The method.
        method: Method,
        /// The pattern.
        pattern: String,
    },
    /// Two patterns use different parameter names at the same position
    /// (`/jobs/:id` and `/jobs/:job_id/cancel`).
    ConflictingParam {
        /// The pattern being added.
        pattern: String,
        /// The name already registered at that position.
        existing: String,
    },
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteError::InvalidPattern { pattern, reason } => {
                write!(f, "invalid route pattern {pattern:?}: {reason}")
            }
            RouteError::Duplicate { method, pattern } => {
                write!(f, "route {method} {pattern} is already registered")
            }
            RouteError::ConflictingParam { pattern, existing } => write!(
                f,
                "route pattern {pattern:?} conflicts with parameter :{existing} registered at the same position"
            ),
        }
    }
}

impl std::error::Error for RouteError {}

/// All handlers registered for one pattern.
pub(super) struct Route {
    pub(super) pattern: Arc<str>,
    handlers: Vec<(Method, Arc<dyn Handler>)>,
}

impl Route {
    fn new(pattern: &str) -> Route {
        Route {
            pattern: Arc::from(pattern),
            handlers: Vec::new(),
        }
    }

    fn add(
        &mut self,
        method: Method,
        handler: Arc<dyn Handler>,
        pattern: &str,
    ) -> Result<(), RouteError> {
        if self.handler_for(&method).is_some() {
            return Err(RouteError::Duplicate {
                method,
                pattern: pattern.to_owned(),
            });
        }
        self.handlers.push((method, handler));
        Ok(())
    }

    pub(super) fn handler_for(&self, method: &Method) -> Option<&Arc<dyn Handler>> {
        self.handlers
            .iter()
            .find(|(m, _)| m == method)
            .map(|(_, h)| h)
    }

    /// Whether the router can answer `method` here: registered directly,
    /// HEAD served by GET, or the automatic OPTIONS response.
    pub(super) fn allows(&self, method: &Method) -> bool {
        self.handler_for(method).is_some()
            || *method == Method::Options
            || (*method == Method::Head && self.handler_for(&Method::Get).is_some())
    }

    /// The `Allow` header value: registered methods in registration order,
    /// then the implied HEAD and OPTIONS.
    pub(super) fn allow_header(&self) -> String {
        let mut methods: Vec<&str> = self.handlers.iter().map(|(m, _)| m.as_str()).collect();
        if self.handler_for(&Method::Get).is_some() && self.handler_for(&Method::Head).is_none() {
            methods.push("HEAD");
        }
        if self.handler_for(&Method::Options).is_none() {
            methods.push("OPTIONS");
        }
        methods.join(", ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Segment<'p> {
    Static(&'p str),
    Param(&'p str),
    Wildcard(&'p str),
}

/// Splits a pattern such as `/v1/queues/:name/pause` or `/static/*path`.
pub(super) fn parse_pattern(pattern: &str) -> Result<Vec<Segment<'_>>, RouteError> {
    let invalid = |reason| RouteError::InvalidPattern {
        pattern: pattern.to_owned(),
        reason,
    };
    let rest = pattern
        .strip_prefix('/')
        .ok_or_else(|| invalid("must start with '/'"))?;
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<&str> = rest.split('/').collect();
    let mut segments = Vec::with_capacity(parts.len());
    let mut names: Vec<&str> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let segment = if let Some(name) = part.strip_prefix(':') {
            Segment::Param(name)
        } else if let Some(name) = part.strip_prefix('*') {
            if !last {
                return Err(invalid("a wildcard must be the last segment"));
            }
            Segment::Wildcard(name)
        } else {
            if part.is_empty() && !last {
                return Err(invalid("empty path segment"));
            }
            Segment::Static(part)
        };
        if let Segment::Param(name) | Segment::Wildcard(name) = segment {
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(invalid("parameter names must be [A-Za-z0-9_]+"));
            }
            if names.contains(&name) {
                return Err(invalid("duplicate parameter name"));
            }
            names.push(name);
        }
        segments.push(segment);
    }
    Ok(segments)
}

pub(super) type Captures = Vec<(String, String)>;

/// Result of looking a path up.
pub(super) enum Lookup<'n> {
    Found(&'n Route, Captures),
    /// The path exists but no route there accepts the method.
    MethodNotAllowed(&'n Route),
    NotFound,
}

#[derive(Default)]
pub(super) struct Node {
    statics: Vec<(String, Node)>,
    param: Option<(String, Box<Node>)>,
    wildcard: Option<(String, Route)>,
    route: Option<Route>,
}

impl Node {
    pub(super) fn insert(
        &mut self,
        segments: &[Segment<'_>],
        pattern: &str,
        method: Method,
        handler: Arc<dyn Handler>,
    ) -> Result<(), RouteError> {
        let Some((first, rest)) = segments.split_first() else {
            return self
                .route
                .get_or_insert_with(|| Route::new(pattern))
                .add(method, handler, pattern);
        };
        match *first {
            Segment::Static(literal) => {
                let index = match self.statics.iter().position(|(s, _)| s == literal) {
                    Some(index) => index,
                    None => {
                        self.statics.push((literal.to_owned(), Node::default()));
                        self.statics.len() - 1
                    }
                };
                self.statics[index].1.insert(rest, pattern, method, handler)
            }
            Segment::Param(name) => {
                let (existing, child) = self
                    .param
                    .get_or_insert_with(|| (name.to_owned(), Box::default()));
                if existing != name {
                    return Err(RouteError::ConflictingParam {
                        pattern: pattern.to_owned(),
                        existing: existing.clone(),
                    });
                }
                child.insert(rest, pattern, method, handler)
            }
            Segment::Wildcard(name) => {
                let (existing, route) = self
                    .wildcard
                    .get_or_insert_with(|| (name.to_owned(), Route::new(pattern)));
                if existing != name {
                    return Err(RouteError::ConflictingParam {
                        pattern: pattern.to_owned(),
                        existing: existing.clone(),
                    });
                }
                route.add(method, handler, pattern)
            }
        }
    }

    /// Looks up decoded path segments.
    pub(super) fn lookup<'n>(&'n self, segments: &[String], method: &Method) -> Lookup<'n> {
        let mut captures = Vec::new();
        let mut fallback = None;
        match self.find(segments, method, &mut captures, &mut fallback) {
            Some(route) => Lookup::Found(route, captures),
            None => match fallback {
                Some(route) => Lookup::MethodNotAllowed(route),
                None => Lookup::NotFound,
            },
        }
    }

    fn find<'n>(
        &'n self,
        segments: &[String],
        method: &Method,
        captures: &mut Captures,
        fallback: &mut Option<&'n Route>,
    ) -> Option<&'n Route> {
        let Some((first, rest)) = segments.split_first() else {
            let route = self.route.as_ref()?;
            if route.allows(method) {
                return Some(route);
            }
            fallback.get_or_insert(route);
            return None;
        };
        if let Some((_, child)) = self.statics.iter().find(|(s, _)| s == first) {
            if let Some(route) = child.find(rest, method, captures, fallback) {
                return Some(route);
            }
        }
        if let Some((name, child)) = &self.param {
            // A parameter never matches an empty segment: `/jobs/` is not a
            // job id.
            if !first.is_empty() {
                captures.push((name.clone(), first.clone()));
                if let Some(route) = child.find(rest, method, captures, fallback) {
                    return Some(route);
                }
                captures.pop();
            }
        }
        if let Some((name, route)) = &self.wildcard {
            if route.allows(method) {
                captures.push((name.clone(), segments.join("/")));
                return Some(route);
            }
            fallback.get_or_insert(route);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Request;
    use crate::response::Response;

    fn handler() -> Arc<dyn Handler> {
        Arc::new(|_: &mut Request| Response::no_content())
    }

    fn tree(routes: &[(Method, &str)]) -> Node {
        let mut root = Node::default();
        for (method, pattern) in routes {
            let segments = parse_pattern(pattern).expect("valid pattern");
            root.insert(&segments, pattern, method.clone(), handler())
                .expect("no conflict");
        }
        root
    }

    fn segs(path: &str) -> Vec<String> {
        let trimmed = path.strip_prefix('/').unwrap_or(path);
        if trimmed.is_empty() {
            Vec::new()
        } else {
            trimmed.split('/').map(str::to_owned).collect()
        }
    }

    fn found(root: &Node, method: Method, path: &str) -> Option<(String, Captures)> {
        match root.lookup(&segs(path), &method) {
            Lookup::Found(route, captures) => Some((route.pattern.to_string(), captures)),
            _ => None,
        }
    }

    #[test]
    fn parses_patterns() {
        assert!(parse_pattern("/").expect("root").is_empty());
        assert_eq!(
            parse_pattern("/v1/jobs/:id/cancel").expect("valid"),
            [
                Segment::Static("v1"),
                Segment::Static("jobs"),
                Segment::Param("id"),
                Segment::Static("cancel")
            ]
        );
        assert_eq!(
            parse_pattern("/static/*path").expect("valid"),
            [Segment::Static("static"), Segment::Wildcard("path")]
        );
        assert_eq!(
            parse_pattern("/dir/").expect("trailing slash"),
            [Segment::Static("dir"), Segment::Static("")]
        );
    }

    #[test]
    fn rejects_bad_patterns() {
        for (pattern, reason) in [
            ("v1/jobs", "must start with '/'"),
            ("/a//b", "empty path segment"),
            ("/*rest/more", "a wildcard must be the last segment"),
            ("/:", "parameter names must be [A-Za-z0-9_]+"),
            ("/:a-b", "parameter names must be [A-Za-z0-9_]+"),
            ("/:id/x/:id", "duplicate parameter name"),
        ] {
            assert_eq!(
                parse_pattern(pattern),
                Err(RouteError::InvalidPattern {
                    pattern: pattern.into(),
                    reason
                }),
                "{pattern}"
            );
        }
    }

    #[test]
    fn static_segments_beat_parameters() {
        let root = tree(&[(Method::Get, "/jobs/:id"), (Method::Get, "/jobs/stats")]);
        assert_eq!(
            found(&root, Method::Get, "/jobs/stats").map(|f| f.0),
            Some("/jobs/stats".into())
        );
        let (pattern, captures) = found(&root, Method::Get, "/jobs/42").expect("param route");
        assert_eq!(pattern, "/jobs/:id");
        assert_eq!(captures, [("id".to_string(), "42".to_string())]);
    }

    #[test]
    fn backtracks_from_a_dead_end_static_branch() {
        let root = tree(&[(Method::Get, "/a/b/c"), (Method::Get, "/a/:x/d")]);
        let (pattern, captures) = found(&root, Method::Get, "/a/b/d").expect("param branch");
        assert_eq!(pattern, "/a/:x/d");
        assert_eq!(captures, [("x".to_string(), "b".to_string())]);
    }

    #[test]
    fn prefers_a_route_that_allows_the_method() {
        let root = tree(&[(Method::Get, "/jobs/active"), (Method::Delete, "/jobs/:id")]);
        let (pattern, _) = found(&root, Method::Delete, "/jobs/active").expect("delete by id");
        assert_eq!(pattern, "/jobs/:id");
        assert!(matches!(
            root.lookup(&segs("/jobs/active"), &Method::Post),
            Lookup::MethodNotAllowed(route) if &*route.pattern == "/jobs/active"
        ));
    }

    #[test]
    fn wildcard_captures_the_rest() {
        let root = tree(&[(Method::Get, "/static/*path")]);
        let (_, captures) = found(&root, Method::Get, "/static/css/app.css").expect("match");
        assert_eq!(captures, [("path".to_string(), "css/app.css".to_string())]);
        assert!(found(&root, Method::Get, "/static").is_none());
    }

    #[test]
    fn parameters_do_not_match_empty_segments() {
        let root = tree(&[(Method::Get, "/jobs/:id")]);
        assert!(found(&root, Method::Get, "/jobs/").is_none());
    }

    #[test]
    fn head_and_options_are_implied() {
        let root = tree(&[(Method::Get, "/x"), (Method::Post, "/x")]);
        assert!(found(&root, Method::Head, "/x").is_some());
        assert!(found(&root, Method::Options, "/x").is_some());
        match root.lookup(&segs("/x"), &Method::Get) {
            Lookup::Found(route, _) => assert_eq!(route.allow_header(), "GET, POST, HEAD, OPTIONS"),
            _ => panic!("GET /x should match"),
        }
    }

    #[test]
    fn registration_conflicts_are_reported() {
        let mut root = tree(&[(Method::Get, "/jobs/:id")]);
        let dup = parse_pattern("/jobs/:id").expect("valid");
        assert_eq!(
            root.insert(&dup, "/jobs/:id", Method::Get, handler()),
            Err(RouteError::Duplicate {
                method: Method::Get,
                pattern: "/jobs/:id".into()
            })
        );
        let conflict = parse_pattern("/jobs/:job/cancel").expect("valid");
        let err = root
            .insert(&conflict, "/jobs/:job/cancel", Method::Post, handler())
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "route pattern \"/jobs/:job/cancel\" conflicts with parameter :id registered at the same position"
        );
    }
}
