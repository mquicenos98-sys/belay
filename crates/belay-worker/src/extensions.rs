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

impl Extensions {
    /// An empty map.
    #[must_use]
    pub fn new() -> Extensions {
        Extensions::default()
    }

    /// Stores `value`, returning the previous value of the same type.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|old| old.downcast::<T>().ok())
            .map(|old| *old)
    }

    /// The value of type `T`, if present.
    #[must_use]
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>())
    }

    /// The value of type `T`, mutably.
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|v| v.downcast_mut::<T>())
    }

    /// Removes and returns the value of type `T`.
    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|v| v.downcast::<T>().ok())
            .map(|v| *v)
    }

    /// Whether a value of type `T` is present.
    #[must_use]
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    /// Number of values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}
