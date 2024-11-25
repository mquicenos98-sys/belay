//! The [`json!`](crate::json) literal macro and
//! [`impl_json_via_str!`](crate::impl_json_via_str).

/// Builds a [`Value`](crate::Value) from JSON-like syntax.
///
/// Literals `null`, `true`, `false`, nested `[...]` and `{...}` are written as
/// in JSON. Anything else is a Rust expression converted with
/// [`ToJson`](crate::ToJson), taken by reference so nothing is moved. Object
/// keys are string literals or any expression in parentheses that converts
/// into `String`. Trailing commas are accepted.
///
/// ```
/// use belay_json::json;
///
/// let attempt = 3;
/// let tags = vec!["billing", "eu"];
/// let key = "tenant";
/// let v = json!({
///     "queue": "emails",
///     "attempt": attempt + 1,
///     "tags": tags,
///     (key): {"id": 42, "plan": null},
///     "steps": [true, -1, 2.5, [], {}],
/// });
/// assert_eq!(v["attempt"], 4);
/// assert_eq!(v["tenant"]["id"], 42);
/// assert_eq!(
///     v.to_string(),
///     r#"{"queue":"emails","attempt":4,"tags":["billing","eu"],"tenant":{"id":42,"plan":null},"steps":[true,-1,2.5,[],{}]}"#
/// );
/// ```
///
/// Expressions that start with `true`, `false` or `null` followed by more
/// tokens (`true && x`) must be parenthesized. Like any recursive
/// `macro_rules!` macro, very large literals (roughly 40 object entries or 60
/// array elements in one level) can hit the default recursion limit; build
/// those with [`ObjectBuilder`](crate::ObjectBuilder) or raise
/// `#![recursion_limit]`.
#[macro_export]
macro_rules! json {
    (null) => {
        $crate::Value::Null
    };
    (true) => {
        $crate::Value::Bool(true)
    };
    (false) => {
        $crate::Value::Bool(false)
    };
    ([]) => {
        $crate::Value::Array(::std::vec::Vec::new())
    };
    ([ $($tt:tt)+ ]) => {
        $crate::Value::Array($crate::__json_internal!(@array [] $($tt)+))
    };
    ({}) => {
        $crate::Value::Object($crate::Map::new())
    };
    ({ $($tt:tt)+ }) => {{
        let mut object = $crate::Map::new();
        $crate::__json_internal!(@object object () ($($tt)+));
        $crate::Value::Object(object)
    }};
    ($other:expr) => {
        $crate::ToJson::to_json(&$other)
    };
}

/// Token muncher behind [`json!`]. Not public API.
///
/// Arrays accumulate finished element expressions inside `[...]`. Objects
/// accumulate the tokens of the current key inside `(...)` until a `:` shows
/// up, then parse one value and insert the entry.
#[doc(hidden)]
#[macro_export]
macro_rules! __json_internal {
    // ----- arrays -----
    (@array [$($elems:expr,)*]) => {
        ::std::vec![$($elems,)*]
    };
    (@array [$($elems:expr),*]) => {
        ::std::vec![$($elems),*]
    };
    (@array [$($elems:expr,)*] null $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::json!(null)] $($rest)*)
    };
    (@array [$($elems:expr,)*] true $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::json!(true)] $($rest)*)
    };
    (@array [$($elems:expr,)*] false $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::json!(false)] $($rest)*)
    };
    (@array [$($elems:expr,)*] [$($array:tt)*] $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::json!([$($array)*])] $($rest)*)
    };
    (@array [$($elems:expr,)*] {$($object:tt)*} $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::json!({$($object)*})] $($rest)*)
    };
    (@array [$($elems:expr,)*] $next:expr, $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::json!($next),] $($rest)*)
    };
    (@array [$($elems:expr,)*] $last:expr) => {
        $crate::__json_internal!(@array [$($elems,)* $crate::json!($last)])
    };
    (@array [$($elems:expr),*] , $($rest:tt)*) => {
        $crate::__json_internal!(@array [$($elems,)*] $($rest)*)
    };

    // ----- objects -----
    // Nothing left.
    (@object $object:ident () ()) => {};
    // A finished entry followed by a comma.
    (@object $object:ident [$($key:tt)+] ($value:expr) , $($rest:tt)*) => {
        let _ = $object.insert(($($key)+), $value);
        $crate::__json_internal!(@object $object () ($($rest)*));
    };
    // The last entry, without a trailing comma.
    (@object $object:ident [$($key:tt)+] ($value:expr)) => {
        let _ = $object.insert(($($key)+), $value);
    };
    // Values that are JSON literals or nested structures.
    (@object $object:ident ($($key:tt)+) (: null $($rest:tt)*)) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::json!(null)) $($rest)*);
    };
    (@object $object:ident ($($key:tt)+) (: true $($rest:tt)*)) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::json!(true)) $($rest)*);
    };
    (@object $object:ident ($($key:tt)+) (: false $($rest:tt)*)) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::json!(false)) $($rest)*);
    };
    (@object $object:ident ($($key:tt)+) (: [$($array:tt)*] $($rest:tt)*)) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::json!([$($array)*])) $($rest)*);
    };
    (@object $object:ident ($($key:tt)+) (: {$($map:tt)*} $($rest:tt)*)) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::json!({$($map)*})) $($rest)*);
    };
    // Values that are Rust expressions.
    (@object $object:ident ($($key:tt)+) (: $value:expr , $($rest:tt)*)) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::json!($value)) , $($rest)*);
    };
    (@object $object:ident ($($key:tt)+) (: $value:expr)) => {
        $crate::__json_internal!(@object $object [$($key)+] ($crate::json!($value)));
    };
    // Accumulate one more token of the key.
    (@object $object:ident ($($key:tt)*) ($tt:tt $($rest:tt)*)) => {
        $crate::__json_internal!(@object $object ($($key)* $tt) ($($rest)*));
    };
}

/// Implements [`ToJson`](crate::ToJson) and [`FromJson`](crate::FromJson)
/// for a type through its `Display` and `FromStr` impls, encoding it as a
/// JSON string. The second argument names the type in error messages.
///
/// ```
/// use std::fmt;
/// use std::str::FromStr;
/// use belay_json::{impl_json_via_str, json, FromJson, ToJson};
///
/// #[derive(Debug, PartialEq)]
/// struct QueueName(String);
///
/// impl fmt::Display for QueueName {
///     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
///         f.write_str(&self.0)
///     }
/// }
///
/// impl FromStr for QueueName {
///     type Err = String;
///     fn from_str(s: &str) -> Result<Self, String> {
///         if s.chars().all(|c| c.is_ascii_lowercase()) {
///             Ok(QueueName(s.to_owned()))
///         } else {
///             Err("must be lowercase letters".to_owned())
///         }
///     }
/// }
///
/// impl_json_via_str!(QueueName, "queue name");
///
/// assert_eq!(QueueName("emails".into()).to_json(), json!("emails"));
/// let err = QueueName::from_json(&json!("Emails")).unwrap_err();
/// assert_eq!(err.to_string(), r#"$: invalid queue name "Emails": must be lowercase letters"#);
/// ```
#[macro_export]
macro_rules! impl_json_via_str {
    ($ty:ty, $what:expr) => {
        impl $crate::ToJson for $ty {
            fn to_json(&self) -> $crate::Value {
                $crate::Value::String(::std::string::ToString::to_string(self))
            }
        }

        impl $crate::FromJson for $ty {
            fn from_json(
                value: &$crate::Value,
            ) -> ::std::result::Result<Self, $crate::FromJsonError> {
                $crate::parse_str_as(value, $what)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::{Map, ToJson, Value};

    struct Point {
        x: i32,
        y: i32,
    }

    impl ToJson for Point {
        fn to_json(&self) -> Value {
            json!({"x": self.x, "y": self.y})
        }
    }

    #[test]
    fn literals_and_empty_containers() {
        assert_eq!(json!(null), Value::Null);
        assert_eq!(json!(true), Value::Bool(true));
        assert_eq!(json!([]), Value::Array(vec![]));
        assert_eq!(json!({}), Value::Object(Map::new()));
        assert_eq!(json!("s"), Value::from("s"));
        assert_eq!(json!(-7), Value::from(-7));
    }

    #[test]
    fn arrays_mix_literals_and_expressions() {
        let x = 5;
        let v = json!([null, true, false, x, x * 2, -x, "s", [1, [2]], {"k": x}]);
        assert_eq!(
            v.to_string(),
            r#"[null,true,false,5,10,-5,"s",[1,[2]],{"k":5}]"#
        );
        assert_eq!(json!([1, 2,]), json!([1, 2]));
        assert_eq!(json!([null,]), json!([null]));
        assert_eq!(json!([[], {},]).to_string(), "[[],{}]");
    }

    #[test]
    fn objects_accept_expression_keys_and_trailing_commas() {
        let k = String::from("dyn");
        let v = json!({
            "a": null,
            "b": true,
            "c": false,
            "d": [1],
            "e": {"f": {}},
            (k.clone()): 1 + 1,
            (format!("n{}", 1)): "one",
        });
        assert_eq!(
            v.to_string(),
            r#"{"a":null,"b":true,"c":false,"d":[1],"e":{"f":{}},"dyn":2,"n1":"one"}"#
        );
        assert_eq!(json!({"only": 1}), json!({"only": 1,}));
    }

    #[test]
    fn expressions_use_to_json_by_reference() {
        let p = Point { x: 1, y: -2 };
        let points = vec![Point { x: 0, y: 0 }];
        let label: Option<&str> = None;
        let v = json!({"p": p, "all": points, "label": label});
        assert_eq!(
            v.to_string(),
            r#"{"p":{"x":1,"y":-2},"all":[{"x":0,"y":0}],"label":null}"#
        );
        // Still usable: nothing was moved.
        assert_eq!(p.x + points.len() as i32, 2);
    }

    #[test]
    fn method_calls_and_paths_are_expressions() {
        let items = [3, 1, 2];
        let v = json!({"len": items.len(), "max": items.iter().max(), "pi": std::f64::consts::PI});
        assert_eq!(v["len"], 3u64);
        assert_eq!(v["max"], 3);
        assert_eq!(v["pi"], std::f64::consts::PI);
    }
}
