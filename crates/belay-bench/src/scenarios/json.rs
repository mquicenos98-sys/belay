//! `json`: belay-json on job documents, the records the queue stores and
//! the API sends, in three sizes. `serialize` is `Value` to text, `parse`
//! text to `Value`, `decode` `Value` to a typed `Job`. MB/s counts the
//! document's text length for all three.

use std::hint::black_box;

use belay_common::{Rng, Timestamp, Ulid};
use belay_json::{FromJson, ObjectBuilder, ToJson, Value};
use belay_proto::{Job, JobError, JobId, JobSpec, QueueName};

use crate::harness::{measure, Params, Plan, Work};
use crate::session::{CaseResult, Session};
use crate::units::format_size;
use crate::{Error, Result};

pub const NAME: &str = "json";

/// Distinct documents per size; calls cycle through them.
const DOCS: usize = 32;

/// Document sizes by the number of line items in the payload: about
/// 0.7 KiB, 2.4 KiB and 37 KiB of compact text (the average is in each
/// case's notes).
const SIZES: &[(&str, usize)] = &[("small", 1), ("medium", 20), ("large", 400)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Serialize,
    Parse,
    Decode,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Serialize => "serialize",
            Op::Parse => "parse",
            Op::Decode => "decode",
        }
    }
}

/// A document in both forms.
struct Doc {
    value: Value,
    text: String,
}

pub fn run(s: &mut Session<'_>) -> Result<()> {
    let budget: u64 = s.pick(256 << 20, 16 << 20, 0);
    for &(size, items) in SIZES {
        let ops = [Op::Serialize, Op::Parse, Op::Decode];
        let cases: Vec<(Op, Params)> = ops
            .into_iter()
            .map(|op| (op, Params::new().with("op", op.name()).with("doc", size)))
            .filter(|(_, params)| s.wants(NAME, params))
            .collect();
        if cases.is_empty() {
            continue;
        }
        let docs = documents(&mut s.rng(&format!("{NAME}/{size}")), items)?;
        let average = docs.iter().map(|d| d.text.len() as u64).sum::<u64>() / docs.len() as u64;
        let iterations = (budget / average.max(1)).clamp(s.pick(1_000, 100, 2), 500_000);
        for (op, params) in cases {
            let sample = measure(Plan::new(iterations), |seq| {
                let doc = &docs[seq as usize % docs.len()];
                match op {
                    Op::Serialize => {
                        black_box(belay_json::to_string(black_box(&doc.value)));
                    }
                    Op::Parse => {
                        black_box(belay_json::parse(black_box(&doc.text)).map_err(invalid)?);
                    }
                    Op::Decode => {
                        black_box(Job::from_json(black_box(&doc.value)).map_err(invalid)?);
                    }
                }
                Ok(Work::ONE.with_bytes(doc.text.len() as u64))
            })?;
            s.record(CaseResult::new(NAME, params, sample).note("avg", format_size(average)));
        }
    }
    Ok(())
}

fn invalid(e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("a synthetic job document did not round-trip: {e}"))
}

/// `DOCS` job documents with `items` line items each, checked to parse
/// and decode, so the measured loops cannot fail half way.
fn documents(rng: &mut Rng, items: usize) -> Result<Vec<Doc>> {
    let now = Timestamp::from_secs(1_750_000_000);
    (0..DOCS)
        .map(|_| {
            let value = job(rng, items, now).to_json();
            let text = belay_json::to_string(&value);
            let parsed = belay_json::parse(&text).map_err(invalid)?;
            Job::from_json(&parsed).map_err(invalid)?;
            Ok(Doc { value, text })
        })
        .collect()
}

/// A plausible order-fulfilment job: nested payload, tags, metadata and,
/// for some, an error from an earlier attempt.
fn job(rng: &mut Rng, items: usize, now: Timestamp) -> Job {
    let lines: Vec<Value> = (0..items).map(|_| line_item(rng)).collect();
    let payload = ObjectBuilder::new()
        .field("order_id", format!("ord_{:012x}", rng.next_u64() >> 16))
        .raw(
            "customer",
            ObjectBuilder::new()
                .field("id", rng.gen_range(1..10_000_000u64))
                .field("name", words(rng, 2))
                .field("email", format!("{}@example.com", words(rng, 1)))
                .field("vip", rng.gen_bool(0.1))
                .build(),
        )
        .field("lines", lines)
        .field(
            "total",
            (rng.gen_range(0.0..5_000.0) * 100.0).round() / 100.0,
        )
        .field("note", text(rng))
        .build();
    let spec = JobSpec::new(QueueName::from_static("orders"), "orders.fulfil")
        .payload(payload)
        .priority(rng.gen_range(-10..=10))
        .tag("bench")
        .tag(if rng.gen_bool(0.5) { "eu" } else { "us" })
        .meta("source", "belay-bench")
        .meta("trace", format!("{:016x}", rng.next_u64()));
    let mut job = Job::new(JobId::from(Ulid::with_random(now, rng)), spec, now);
    if rng.gen_bool(0.3) {
        job.record_error(
            JobError::new(now, 1, "upstream timed out after 30s")
                .kind("timeout")
                .retryable(true),
        );
    }
    job
}

fn line_item(rng: &mut Rng) -> Value {
    ObjectBuilder::new()
        .field("sku", format!("SKU-{:06}", rng.gen_range(0..1_000_000u32)))
        .field("title", words(rng, 4))
        .field("qty", rng.gen_range(1..20u32))
        .field("price", (rng.gen_range(0.5..500.0) * 100.0).round() / 100.0)
        .field("gift", rng.gen_bool(0.05))
        .build()
}

const WORDS: &[&str] = &[
    "alpha", "belay", "canyon", "delta", "ember", "fjord", "gable", "harbor", "ivory", "juniper",
    "kestrel", "lumen", "mesa", "nectar", "orbit", "pylon", "quartz", "ridge", "summit", "tundra",
];

fn words(rng: &mut Rng, n: usize) -> String {
    let picked: Vec<&str> = (0..n)
        .map(|_| *rng.choose(WORDS).expect("the word list is not empty"))
        .collect();
    picked.join(" ")
}

/// Free text with the characters a serializer must escape (quotes,
/// backslashes, control characters) and some it must pass through
/// (accented Latin, CJK).
fn text(rng: &mut Rng) -> String {
    let mut out = words(rng, 8);
    for c in ['"', '\\', '\n', '\t'] {
        if rng.gen_bool(0.5) {
            out.push(c);
            out.push_str(&words(rng, 2));
        }
    }
    out.extend(['\u{e9}', '\u{6f22}']);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_reproducible_and_grow_with_line_items() {
        let small = documents(&mut Rng::seed_from_u64(5), 1).unwrap();
        let again = documents(&mut Rng::seed_from_u64(5), 1).unwrap();
        assert_eq!(small.len(), DOCS);
        assert_eq!(small[0].text, again[0].text);
        let large = documents(&mut Rng::seed_from_u64(5), 400).unwrap();
        assert!(large[0].text.len() > 20 * small[0].text.len());
    }

    #[test]
    fn documents_exercise_escapes_and_non_ascii_text() {
        let docs = documents(&mut Rng::seed_from_u64(5), 1).unwrap();
        assert!(
            docs.iter().any(|d| d.text.contains(r#"\""#)),
            "an escaped quote"
        );
        assert!(
            docs.iter().any(|d| d.text.contains(r"\n")),
            "an escaped newline"
        );
        assert!(
            docs.iter().all(|d| !d.text.is_ascii()),
            "accented and CJK text"
        );
    }
}
