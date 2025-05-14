//! A type-keyed map carried by each [`JobContext`](crate::JobContext), so
//! middleware can hand values (a database handle, a tenant's settings, a
//! trace id) to the handlers it wraps without global state.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

/// At most one value per type.
///
/// ```
/// use belay_worker::Extensions;
///
/// #[derive(Debug, PartialEq)]
/// struct TraceId(u64);
///
/// let mut ext = Extensions::new();
/// assert!(ext.insert(TraceId(7)).is_none());
/// assert_eq!(ext.get::<TraceId>(), Some(&TraceId(7)));
/// assert_eq!(ext.insert(TraceId(8)), Some(TraceId(7)));
/// assert_eq!(ext.remove::<TraceId>(), Some(TraceId(8)));
/// assert!(ext.is_empty());
/// ```
#[derive(Default)]
pub struct Extensions {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}
