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
