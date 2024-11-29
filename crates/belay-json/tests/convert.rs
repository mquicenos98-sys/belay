//! Hand-written `ToJson`/`FromJson` impls for a small job-queue model, the
//! way downstream crates write them, checked end to end: round trips,
//! omitted defaults, validation, enum encodings and error paths.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use belay_json::{
    impl_json_via_str, json, Error, FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson,
    Value,
};

#[derive(Debug, Clone, PartialEq)]
struct QueueName(String);

impl fmt::Display for QueueName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for QueueName {
    type Err = String;

    fn from_str(s: &str) -> Result<QueueName, String> {
        let valid_first = s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
        let valid_rest = s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.:-".contains(c));
        if valid_first && valid_rest && s.len() <= 64 {
            Ok(QueueName(s.to_owned()))
        } else {
            Err("must match [a-z0-9][a-z0-9_.:-]{0,63}".to_owned())
        }
    }
}

impl_json_via_str!(QueueName, "queue name");

#[derive(Debug, Clone, Copy, PartialEq)]
enum UniqueScope {
    UntilStarted,
    UntilFinished,
    For { ms: u64 },
}

impl ToJson for UniqueScope {
    fn to_json(&self) -> Value {
        match self {
            UniqueScope::UntilStarted => json!("until_started"),
            UniqueScope::UntilFinished => json!("until_finished"),
            UniqueScope::For { ms } => json!({ "for_ms": ms }),
        }
    }
}

impl FromJson for UniqueScope {
    fn from_json(value: &Value) -> Result<UniqueScope, FromJsonError> {
        match value {
            Value::String(s) => match s.as_str() {
                "until_started" => Ok(UniqueScope::UntilStarted),
                "until_finished" => Ok(UniqueScope::UntilFinished),
                other => Err(FromJsonError::unknown_variant(
                    other,
                    "UniqueScope",
                    &["until_started", "until_finished"],
                )),
            },
            Value::Object(_) => {
                let r = ObjectReader::new(value, "UniqueScope")?;
                let scope = UniqueScope::For {
                    ms: r.req("for_ms")?,
                };
                r.deny_unknown_fields()?;
                Ok(scope)
            }
            other => Err(FromJsonError::expected(
                "UniqueScope string or object",
                other,
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Unique {
    key: String,
    scope: UniqueScope,
}

impl ToJson for Unique {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("key", &self.key)
            .field("scope", self.scope)
            .build()
    }
}

impl FromJson for Unique {
    fn from_json(value: &Value) -> Result<Unique, FromJsonError> {
        let r = ObjectReader::new(value, "Unique")?;
        let unique = Unique {
            key: r.req("key")?,
            scope: r.or("scope", UniqueScope::UntilFinished)?,
        };
        r.deny_unknown_fields()?;
        Ok(unique)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct JobSpec {
    queue: QueueName,
    kind: String,
    payload: Value,
    priority: i16,
    run_at: Option<u64>,
    max_attempts: u32,
    tags: Vec<String>,
    meta: BTreeMap<String, String>,
    unique: Option<Unique>,
}

impl JobSpec {
    fn new(queue: &str, kind: &str) -> JobSpec {
        JobSpec {
            queue: queue.parse().expect("valid queue name in test"),
            kind: kind.to_owned(),
            payload: Value::Null,
            priority: 0,
            run_at: None,
            max_attempts: 25,
            tags: Vec::new(),
            meta: BTreeMap::new(),
            unique: None,
        }
    }
}

impl ToJson for JobSpec {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("queue", &self.queue)
            .field("kind", &self.kind)
            .field_if(!self.payload.is_null(), "payload", &self.payload)
            .field_if(self.priority != 0, "priority", self.priority)
            .opt_field("run_at", &self.run_at)
            .field_if(self.max_attempts != 25, "max_attempts", self.max_attempts)
            .field_if(!self.tags.is_empty(), "tags", &self.tags)
            .field_if(!self.meta.is_empty(), "meta", &self.meta)
            .opt_field("unique", &self.unique)
            .build()
    }
}

impl FromJson for JobSpec {
    fn from_json(value: &Value) -> Result<JobSpec, FromJsonError> {
        let r = ObjectReader::new(value, "JobSpec")?;
        let spec = JobSpec {
            queue: r.req("queue")?,
            kind: r.req("kind")?,
            payload: r.or_default("payload")?,
            priority: r.or_default("priority")?,
            run_at: r.opt("run_at")?,
            max_attempts: r.or("max_attempts", 25)?,
            tags: r.or_default("tags")?,
            meta: r.or_default("meta")?,
            unique: r.opt("unique")?,
        };
        if !(-1000..=1000).contains(&spec.priority) {
            return Err(r.invalid(
                "priority",
                format_args!("must be between -1000 and 1000, got {}", spec.priority),
            ));
        }
        r.deny_unknown_fields()?;
        Ok(spec)
    }
}

/// `POST /v1/jobs` accepts one bare `JobSpec` or `{"jobs": [...]}`.
#[derive(Debug, PartialEq)]
struct EnqueueRequest {
    jobs: Vec<JobSpec>,
}

impl FromJson for EnqueueRequest {
    fn from_json(value: &Value) -> Result<EnqueueRequest, FromJsonError> {
        let r = ObjectReader::new(value, "EnqueueRequest")?;
        if !r.has("jobs") {
            return Ok(EnqueueRequest {
                jobs: vec![JobSpec::from_json(value)?],
            });
        }
        let request = EnqueueRequest {
            jobs: r.req("jobs")?,
        };
        r.deny_unknown_fields()?;
        Ok(request)
    }
}

fn full_spec() -> JobSpec {
    let mut spec = JobSpec::new("emails", "send_welcome");
    spec.payload = json!({"to": "a@example.com", "retries": [1, 2]});
    spec.priority = -5;
    spec.run_at = Some(1_741_064_767_089);
    spec.max_attempts = 3;
    spec.tags = vec!["onboarding".to_owned()];
    spec.meta
        .insert("belay.schedule".to_owned(), "daily".to_owned());
    spec.unique = Some(Unique {
        key: "welcome:42".to_owned(),
        scope: UniqueScope::For { ms: 60_000 },
    });
    spec
}

fn decode_err<T: FromJson + fmt::Debug>(value: Value) -> String {
    T::from_json(&value).unwrap_err().to_string()
}

#[test]
fn minimal_spec_omits_every_default() {
    let spec = JobSpec::new("emails", "noop");
    assert_eq!(
        spec.to_json().to_string(),
        r#"{"queue":"emails","kind":"noop"}"#
    );
    assert_eq!(JobSpec::from_json(&spec.to_json()), Ok(spec));
}

#[test]
fn full_spec_round_trips_through_text() {
    let spec = full_spec();
    let text = belay_json::to_string_of(&spec);
    assert_eq!(
        text,
        concat!(
            r#"{"queue":"emails","kind":"send_welcome","payload":{"to":"a@example.com","retries":[1,2]},"#,
            r#""priority":-5,"run_at":1741064767089,"max_attempts":3,"tags":["onboarding"],"#,
            r#""meta":{"belay.schedule":"daily"},"unique":{"key":"welcome:42","scope":{"for_ms":60000}}}"#
        )
    );
    let back: JobSpec = belay_json::from_str(&text).unwrap();
    assert_eq!(back, spec);
    let pretty = belay_json::to_string_pretty_of(&spec);
    assert_eq!(belay_json::from_str::<JobSpec>(&pretty).unwrap(), spec);
}

#[test]
fn json_macro_and_builder_agree() {
    let spec = full_spec();
    let by_macro = json!({
        "queue": spec.queue,
        "kind": "send_welcome",
        "payload": {"to": "a@example.com", "retries": [1, 2]},
        "priority": -5,
        "run_at": spec.run_at,
        "max_attempts": 3,
        "tags": ["onboarding"],
        "meta": spec.meta,
        "unique": spec.unique,
    });
    assert_eq!(by_macro, spec.to_json());
}

#[test]
fn range_errors_point_into_nested_arrays() {
    let request = json!({"jobs": [
        {"queue": "a", "kind": "k"},
        {"queue": "b", "kind": "k"},
        {"queue": "c", "kind": "k", "priority": 40000},
    ]});
    let err = EnqueueRequest::from_json(&request).unwrap_err();
    assert_eq!(
        err.to_string(),
        "$.jobs[2].priority: expected integer in range -32768..=32767, found 40000"
    );
    assert_eq!(err.pointer(), "/jobs/2/priority");
    assert_eq!(
        err.message(),
        "expected integer in range -32768..=32767, found 40000"
    );
}

#[test]
fn validation_errors_name_the_field() {
    let request = json!({"jobs": [{"queue": "a", "kind": "k", "priority": 5000}]});
    assert_eq!(
        decode_err::<EnqueueRequest>(request),
        "$.jobs[0].priority: must be between -1000 and 1000, got 5000"
    );
}

#[test]
fn enum_errors_suggest_the_intended_variant() {
    let request = json!({"jobs": [
        {"queue": "a", "kind": "k"},
        {"queue": "a", "kind": "k", "unique": {"key": "x", "scope": "until_strated"}},
    ]});
    assert_eq!(
        decode_err::<EnqueueRequest>(request),
        r#"$.jobs[1].unique.scope: unknown UniqueScope "until_strated", did you mean "until_started"?"#
    );
    let bad_object =
        json!({"queue": "a", "kind": "k", "unique": {"key": "x", "scope": {"for_ms": -1}}});
    assert_eq!(
        decode_err::<JobSpec>(bad_object),
        "$.unique.scope.for_ms: expected integer in range 0..=18446744073709551615, found -1"
    );
    let wrong_type = json!({"queue": "a", "kind": "k", "unique": {"key": "x", "scope": 7}});
    assert_eq!(
        decode_err::<JobSpec>(wrong_type),
        "$.unique.scope: expected UniqueScope string or object, found 7"
    );
}

#[test]
fn unknown_fields_are_caught_at_every_level() {
    let camel = json!({"queue": "a", "kind": "k", "maxAttempts": 3});
    assert_eq!(
        decode_err::<JobSpec>(camel),
        r#"$: unknown field "maxAttempts" in JobSpec, did you mean "max_attempts"?"#
    );
    let nested = json!({"jobs": [{"queue": "a", "kind": "k", "unique": {"key": "x", "scoop": "until_started"}}]});
    assert_eq!(
        decode_err::<EnqueueRequest>(nested),
        r#"$.jobs[0].unique: unknown field "scoop" in Unique, did you mean "scope"?"#
    );
    let stray = json!({"jobs": [], "dry_run": true});
    assert_eq!(
        decode_err::<EnqueueRequest>(stray),
        r#"$: unknown field "dry_run" in EnqueueRequest, expected one of: jobs"#
    );
}

#[test]
fn map_and_identifier_errors_carry_quoted_paths() {
    let meta = json!({"queue": "a", "kind": "k", "meta": {"owner": "ops", "belay.schedule": 5}});
    assert_eq!(
        decode_err::<JobSpec>(meta),
        r#"$.meta["belay.schedule"]: expected string, found 5"#
    );
    let queue = json!({"queue": "Emails", "kind": "k"});
    assert_eq!(
        decode_err::<JobSpec>(queue),
        r#"$.queue: invalid queue name "Emails": must match [a-z0-9][a-z0-9_.:-]{0,63}"#
    );
    let queue_type = json!({"queue": ["emails"], "kind": "k"});
    assert_eq!(
        decode_err::<JobSpec>(queue_type),
        "$.queue: expected queue name string, found array of 1 element"
    );
}

#[test]
fn missing_and_mistyped_required_fields() {
    assert_eq!(
        decode_err::<JobSpec>(json!({"queue": "a"})),
        r#"$: missing required field "kind" in JobSpec"#
    );
    assert_eq!(
        decode_err::<JobSpec>(json!({"queue": "a", "kind": null})),
        "$.kind: expected string, found null"
    );
    assert_eq!(
        decode_err::<EnqueueRequest>(json!([{"queue": "a"}])),
        "$: expected EnqueueRequest object, found array of 1 element"
    );
    assert_eq!(
        decode_err::<EnqueueRequest>(json!({"jobs": {"queue": "a"}})),
        "$.jobs: expected array, found object"
    );
}

#[test]
fn enqueue_accepts_single_spec_or_batch() {
    let single = belay_json::from_str::<EnqueueRequest>(r#"{"queue": "a", "kind": "k"}"#).unwrap();
    assert_eq!(single.jobs, vec![JobSpec::new("a", "k")]);
    let batch = belay_json::from_str::<EnqueueRequest>(
        r#"{"jobs": [{"queue": "a", "kind": "k"}, {"queue": "b", "kind": "k"}]}"#,
    )
    .unwrap();
    assert_eq!(batch.jobs.len(), 2);
    // A bare spec is still checked for unknown fields.
    assert_eq!(
        belay_json::from_str::<EnqueueRequest>(r#"{"queue": "a", "kind": "k", "jbos": []}"#)
            .unwrap_err()
            .to_string(),
        r#"$: unknown field "jbos" in JobSpec, expected one of: queue, kind, payload, priority, run_at, max_attempts, tags, meta, unique"#
    );
}

#[test]
fn text_level_errors_are_distinguished() {
    use std::error::Error as _;

    let syntax = belay_json::from_str::<JobSpec>("{\"queue\": \"a\",\n \"kind\": }").unwrap_err();
    assert!(matches!(syntax, Error::Parse(_)));
    assert_eq!(
        syntax.to_string(),
        "invalid JSON: unexpected character '}', expected a value at line 2, column 10"
    );
    let shape = belay_json::from_str::<JobSpec>(r#"{"queue": "a", "kind": 1}"#).unwrap_err();
    match &shape {
        Error::Convert(e) => assert_eq!(e.path_string(), "$.kind"),
        other => panic!("expected a conversion error, got {other:?}"),
    }
    assert!(shape.source().is_some());
}

#[test]
fn integral_floats_decode_into_integer_fields() {
    let spec: JobSpec = belay_json::from_str(
        r#"{"queue": "a", "kind": "k", "priority": 5.0, "max_attempts": 1e1}"#,
    )
    .unwrap();
    assert_eq!((spec.priority, spec.max_attempts), (5, 10));
    assert_eq!(
        belay_json::from_str::<JobSpec>(r#"{"queue": "a", "kind": "k", "max_attempts": 2.5}"#)
            .unwrap_err()
            .to_string(),
        "$.max_attempts: expected integer in range 0..=4294967295, found 2.5"
    );
}
