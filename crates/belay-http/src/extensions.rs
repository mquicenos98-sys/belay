//! A type-keyed map for per-request data added by middleware.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

/// Values keyed by their type, at most one per type.
///
/// Middleware uses this to hand data to handlers without widening
/// [`Request`](crate::Request): the request id, the authenticated
/// principal, the matched route pattern. Define a newtype per piece of data
/// so unrelated layers cannot collide on `String`.
///
/// ```
/// use belay_http::Extensions;
///
/// #[derive(Debug, PartialEq)]
/// struct Principal(String);
///
/// let mut ext = Extensions::new();
/// ext.insert(Principal("worker-7".into()));
/// assert_eq!(ext.get::<Principal>(), Some(&Principal("worker-7".into())));
/// assert!(ext.get::<u32>().is_none());
/// ```
#[derive(Default)]
pub struct Extensions {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}
