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
