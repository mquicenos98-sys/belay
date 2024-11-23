//! Helpers for hand-written object impls: [`ObjectBuilder`] for `ToJson` and
//! [`ObjectReader`] for `FromJson`.

use std::cell::{Cell, RefCell};
use std::fmt;

use super::{FromJson, FromJsonError, ToJson};
use crate::{Map, Value};

/// Builds a JSON object field by field, in order.
///
/// ```
/// use belay_json::{ObjectBuilder, json};
///
/// let tags: Vec<String> = vec![];
/// let tenant: Option<String> = Some("acme".into());
/// let v = ObjectBuilder::new()
///     .field("queue", "emails")
///     .field("priority", 5)
///     .opt_field("tenant", &tenant)
///     .opt_field("run_at", &None::<u64>)
///     .field_if(!tags.is_empty(), "tags", &tags)
///     .build();
/// assert_eq!(v, json!({"queue": "emails", "priority": 5, "tenant": "acme"}));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[must_use = "an ObjectBuilder does nothing until `build` is called"]
pub struct ObjectBuilder {
    map: Map,
}

impl ObjectBuilder {
    /// An empty object.
    pub fn new() -> ObjectBuilder {
        ObjectBuilder::default()
    }

    /// An empty object with room for `capacity` fields.
    pub fn with_capacity(capacity: usize) -> ObjectBuilder {
        ObjectBuilder {
            map: Map::with_capacity(capacity),
        }
    }

    /// Adds a field. Setting a key twice keeps the last value.
    pub fn field(mut self, key: &str, value: impl ToJson) -> ObjectBuilder {
        self.map.insert(key, value.to_json());
        self
    }

    /// Adds a field if `value` is `Some`; omits it otherwise.
    pub fn opt_field<T: ToJson>(self, key: &str, value: &Option<T>) -> ObjectBuilder {
        match value {
            Some(v) => self.field(key, v),
            None => self,
        }
    }

    /// Like [`opt_field`](ObjectBuilder::opt_field) with an explicit encoder,
    /// for types that cannot implement [`ToJson`] where they are used (the
    /// orphan rule forbids it for another crate's type) or that need a
    /// field-specific encoding.
    ///
    /// ```
    /// use std::time::Duration;
    /// use belay_json::{json, ObjectBuilder, Value};
    ///
    /// let timeout = Some(Duration::from_millis(1500));
    /// let v = ObjectBuilder::new()
    ///     .opt_field_with("timeout_ms", &timeout, |d| Value::from(d.as_millis() as u64))
    ///     .build();
    /// assert_eq!(v, json!({"timeout_ms": 1500}));
    /// ```
    pub fn opt_field_with<T>(
        self,
        key: &str,
        value: &Option<T>,
        encode: impl FnOnce(&T) -> Value,
    ) -> ObjectBuilder {
        match value {
            Some(v) => self.raw(key, encode(v)),
            None => self,
        }
    }

    /// Adds a field only if `include` is true; `value` is not converted
    /// otherwise. Handy for omitting defaults and empty collections.
    pub fn field_if(self, include: bool, key: &str, value: impl ToJson) -> ObjectBuilder {
        if include {
            self.field(key, value)
        } else {
            self
        }
    }

    /// Adds an already-built value without cloning it.
    pub fn raw(mut self, key: &str, value: Value) -> ObjectBuilder {
        self.map.insert(key, value);
        self
    }

    /// Adds every member of `other`, e.g. the fields of an embedded struct
    /// that is flattened into this object.
    pub fn merge(mut self, other: Map) -> ObjectBuilder {
        self.map.extend(other);
        self
    }

    /// Adds a field through a mutable reference, for building in loops.
    pub fn insert(&mut self, key: &str, value: impl ToJson) -> &mut ObjectBuilder {
        self.map.insert(key, value.to_json());
        self
    }

    /// Number of fields so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True if no field has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Finishes the object.
    #[must_use]
    pub fn build(self) -> Value {
        Value::Object(self.map)
    }

    /// Finishes the object as a bare [`Map`].
    #[must_use]
    pub fn into_map(self) -> Map {
        self.map
    }
}
