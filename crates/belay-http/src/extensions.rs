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

impl Extensions {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `value`, returning the previous value of the same type.
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|old| old.downcast::<T>().ok())
            .map(|old| *old)
    }

    /// The stored value of type `T`.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>())
    }

    /// Mutable access to the stored value of type `T`.
    pub fn get_mut<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|v| v.downcast_mut::<T>())
    }

    /// Removes and returns the stored value of type `T`.
    pub fn remove<T: Any + Send + Sync>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|v| v.downcast::<T>().ok())
            .map(|v| *v)
    }

    /// Whether a value of type `T` is stored.
    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    /// Number of stored values.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Removes everything.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}
