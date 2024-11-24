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

impl From<ObjectBuilder> for Value {
    fn from(builder: ObjectBuilder) -> Value {
        builder.build()
    }
}

/// Reads the fields of a JSON object for a `FromJson` impl.
///
/// Every accessor records the field name, so that after all fields are read
/// [`deny_unknown_fields`](ObjectReader::deny_unknown_fields) can reject
/// anything unexpected and suggest what was probably meant. Field names are
/// `&'static str` because they are part of the type's schema. Errors from a
/// field's decoder get the field name added to their path.
///
/// Accessors take `&self`, so a reader does not need to be `mut` and can be
/// passed to helper functions by shared reference.
///
/// ```
/// use belay_json::{json, FromJson, FromJsonError, ObjectReader, Value};
///
/// #[derive(Debug)]
/// struct Retry { max: u32, backoff: String, jitter: bool }
///
/// impl FromJson for Retry {
///     fn from_json(value: &Value) -> Result<Self, FromJsonError> {
///         let r = ObjectReader::new(value, "Retry")?;
///         let retry = Retry {
///             max: r.req("max")?,
///             backoff: r.or_else("backoff", || "exp:1s*2..10m".to_owned())?,
///             jitter: r.or_default("jitter")?,
///         };
///         if retry.max == 0 {
///             return Err(r.invalid("max", "must be at least 1"));
///         }
///         r.deny_unknown_fields()?;
///         Ok(retry)
///     }
/// }
///
/// let err = Retry::from_json(&json!({"max": 3, "jiter": true})).unwrap_err();
/// assert_eq!(err.to_string(), r#"$: unknown field "jiter" in Retry, did you mean "jitter"?"#);
///
/// let err = Retry::from_json(&json!({"max": 0})).unwrap_err();
/// assert_eq!(err.to_string(), "$.max: must be at least 1");
///
/// let err = Retry::from_json(&json!({"backoff": "const:5s"})).unwrap_err();
/// assert_eq!(err.to_string(), r#"$: missing required field "max" in Retry"#);
/// ```
#[derive(Debug)]
pub struct ObjectReader<'a> {
    map: &'a Map,
    type_name: &'static str,
    /// Per entry position: has a field accessor looked at it?
    consumed: Vec<Cell<bool>>,
    /// Every field name asked for, present or not.
    known: RefCell<Vec<&'static str>>,
}

impl<'a> ObjectReader<'a> {
    /// Starts reading `value`, which must be an object. `type_name` appears in
    /// messages: `expected JobSpec object, found array of 2 elements`.
    pub fn new(
        value: &'a Value,
        type_name: &'static str,
    ) -> Result<ObjectReader<'a>, FromJsonError> {
        match value {
            Value::Object(map) => Ok(ObjectReader::from_map(map, type_name)),
            other => Err(FromJsonError::expected(
                format_args!("{type_name} object"),
                other,
            )),
        }
    }

    /// Starts reading an already-unwrapped map.
    #[must_use]
    pub fn from_map(map: &'a Map, type_name: &'static str) -> ObjectReader<'a> {
        ObjectReader {
            map,
            type_name,
            consumed: vec![Cell::new(false); map.len()],
            known: RefCell::new(Vec::with_capacity(map.len())),
        }
    }

    /// The type name given at construction.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }

    /// The underlying map.
    #[must_use]
    pub fn map(&self) -> &'a Map {
        self.map
    }

    /// Looks up a field and marks it as read. An explicit `null` is returned
    /// as `Some(&Value::Null)`.
    #[must_use]
    pub fn value(&self, key: &'static str) -> Option<&'a Value> {
        self.known.borrow_mut().push(key);
        let position = self.map.get_index_of(key)?;
        self.consumed[position].set(true);
        self.map.get_index(position).map(|(_, v)| v)
    }

    /// Like [`value`](ObjectReader::value), but treats `null` as absent.
    fn present(&self, key: &'static str) -> Option<&'a Value> {
        self.value(key).filter(|v| !v.is_null())
    }

    /// Decodes a required field. A missing field is an error; an explicit
    /// `null` is handed to `T`, so `req::<Option<T>>` accepts it.
    pub fn req<T: FromJson>(&self, key: &'static str) -> Result<T, FromJsonError> {
        self.req_with(key, T::from_json)
    }

    /// Decodes an optional field: absent or `null` gives `None`.
    pub fn opt<T: FromJson>(&self, key: &'static str) -> Result<Option<T>, FromJsonError> {
        self.opt_with(key, T::from_json)
    }

    /// Decodes a field, using `default` when it is absent or `null`.
    pub fn or<T: FromJson>(&self, key: &'static str, default: T) -> Result<T, FromJsonError> {
        Ok(self.opt(key)?.unwrap_or(default))
    }

    /// Decodes a field, calling `default` when it is absent or `null`.
    pub fn or_else<T: FromJson>(
        &self,
        key: &'static str,
        default: impl FnOnce() -> T,
    ) -> Result<T, FromJsonError> {
        Ok(self.opt(key)?.unwrap_or_else(default))
    }

    /// Decodes a field, using `T::default()` when it is absent or `null`.
    pub fn or_default<T: FromJson + Default>(&self, key: &'static str) -> Result<T, FromJsonError> {
        Ok(self.opt(key)?.unwrap_or_default())
    }

    /// Decodes a required field with a custom function; its errors get the
    /// field added to their path.
    ///
    /// ```
    /// use belay_json::{json, parse_str_as, ObjectReader};
    /// use std::net::SocketAddr;
    ///
    /// let v = json!({"listen": "127.0.0.1:7070"});
    /// let r = ObjectReader::new(&v, "ServerConfig").unwrap();
    /// let addr: SocketAddr = r.req_with("listen", |v| parse_str_as(v, "socket address")).unwrap();
    /// assert_eq!(addr.port(), 7070);
    /// ```
    pub fn req_with<T>(
        &self,
        key: &'static str,
        decode: impl FnOnce(&'a Value) -> Result<T, FromJsonError>,
    ) -> Result<T, FromJsonError> {
        match self.value(key) {
            Some(v) => decode(v).map_err(|e| e.at_key(key)),
            None => Err(FromJsonError::missing_field(key, self.type_name)),
        }
    }

    /// Decodes an optional field with a custom function; absent or `null`
    /// gives `None`.
    pub fn opt_with<T>(
        &self,
        key: &'static str,
        decode: impl FnOnce(&'a Value) -> Result<T, FromJsonError>,
    ) -> Result<Option<T>, FromJsonError> {
        self.present(key)
            .map(|v| decode(v).map_err(|e| e.at_key(key)))
            .transpose()
    }

    /// Borrows a required string field, e.g. an enum tag, without allocating.
    pub fn req_str(&self, key: &'static str) -> Result<&'a str, FromJsonError> {
        self.req_with(key, |v| {
            v.as_str()
                .ok_or_else(|| FromJsonError::expected("string", v))
        })
    }

    /// Borrows an optional string field; absent or `null` gives `None`.
    pub fn opt_str(&self, key: &'static str) -> Result<Option<&'a str>, FromJsonError> {
        self.opt_with(key, |v| {
            v.as_str()
                .ok_or_else(|| FromJsonError::expected("string", v))
        })
    }

    /// True if the field is present with a non-null value. Does not count as
    /// reading it.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.map.get(key).is_some_and(|v| !v.is_null())
    }

    /// Accepts a field without decoding it, e.g. a deprecated field that is
    /// still tolerated.
    pub fn ignore(&self, key: &'static str) {
        let _ = self.value(key);
    }

    /// Builds an error for a field whose value decoded but is not acceptable.
    #[must_use]
    pub fn invalid(&self, key: &str, message: impl fmt::Display) -> FromJsonError {
        FromJsonError::new(message.to_string()).at_key(key)
    }

    /// Fields present in the object that no accessor has read, in document
    /// order. Useful to log instead of reject.
    #[must_use]
    pub fn unknown_fields(&self) -> Vec<&'a str> {
        self.map
            .keys()
            .zip(&self.consumed)
            .filter(|(_, consumed)| !consumed.get())
            .map(|(key, _)| key)
            .collect()
    }

    /// Fails on the first field that no accessor has read. Call it after all
    /// fields are read, since only then are all known names recorded.
    pub fn deny_unknown_fields(&self) -> Result<(), FromJsonError> {
        match self.unknown_fields().first() {
            None => Ok(()),
            Some(field) => Err(FromJsonError::unknown_field(
                field,
                self.type_name,
                &self.known.borrow(),
            )),
        }
    }
}
