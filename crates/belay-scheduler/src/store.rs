//! Where the engines keep their records, and the JSON encoding of them.
//!
//! Keys are readable text so that `belay admin store-inspect` output makes
//! sense without a decoder:
//!
//! | key | value |
//! |---|---|
//! | `sched/<schedule id>` | [`Schedule`](belay_proto::Schedule) |
//! | `cal/<calendar name>` | [`CalendarSpec`](belay_proto::CalendarSpec) |
//! | `wf/<run id>` | [`WorkflowDetails`](crate::WorkflowDetails) |
//! | `wfr/<run id>/<step>` | the result of a step that a later step's placeholders read |
//!
//! Schedule ids, calendar names and step names cannot contain `/`, and run
//! ids are ULIDs, whose text form sorts in creation order, so a prefix scan
//! of `wf/` visits runs oldest first. None of the prefixes is a prefix of
//! another.

use belay_json::{FromJson, ToJson};
use belay_proto::{ScheduleId, WorkflowId};
use belay_store::{KeyRange, Kv};

use crate::error::{Error, Result};

const SCHEDULES: &str = "sched/";
const CALENDARS: &str = "cal/";
const WORKFLOWS: &str = "wf/";
const RESULTS: &str = "wfr/";

pub(crate) fn schedule_key(id: &ScheduleId) -> Vec<u8> {
    format!("{SCHEDULES}{id}").into_bytes()
}

pub(crate) fn schedules() -> KeyRange {
    KeyRange::prefix(SCHEDULES)
}

pub(crate) fn calendar_key(name: &str) -> Vec<u8> {
    format!("{CALENDARS}{name}").into_bytes()
}

pub(crate) fn calendars() -> KeyRange {
    KeyRange::prefix(CALENDARS)
}

pub(crate) fn workflow_key(id: WorkflowId) -> Vec<u8> {
    format!("{WORKFLOWS}{id}").into_bytes()
}

pub(crate) fn workflows() -> KeyRange {
    KeyRange::prefix(WORKFLOWS)
}

pub(crate) fn result_key(id: WorkflowId, step: &str) -> Vec<u8> {
    format!("{RESULTS}{id}/{step}").into_bytes()
}

/// Every stored result of run `id`.
pub(crate) fn results_prefix(id: WorkflowId) -> Vec<u8> {
    format!("{RESULTS}{id}/").into_bytes()
}

pub(crate) fn encode<T: ToJson + ?Sized>(value: &T) -> Vec<u8> {
    belay_json::to_string_of(value).into_bytes()
}

pub(crate) fn decode<T: FromJson>(key: &[u8], bytes: &[u8]) -> Result<T> {
    belay_json::from_bytes(bytes).map_err(|e| Error::Corrupt {
        key: printable(key),
        reason: e.to_string(),
    })
}

/// Reads and decodes one document.
pub(crate) fn load<T: FromJson>(kv: &dyn Kv, key: &[u8]) -> Result<Option<T>> {
    match kv.get(key)? {
        Some(bytes) => decode(key, &bytes).map(Some),
        None => Ok(None),
    }
}

/// Reads and decodes every document in `range`, in key order.
pub(crate) fn load_all<T: FromJson>(kv: &dyn Kv, range: KeyRange) -> Result<Vec<T>> {
    kv.scan(range)?
        .into_iter()
        .map(|(key, bytes)| decode(&key, &bytes))
        .collect()
}

pub(crate) fn printable(key: &[u8]) -> String {
    String::from_utf8_lossy(key).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_store::MemKv;

    #[test]
    fn keys_are_readable_and_prefixes_do_not_overlap() {
        let wf: WorkflowId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap();
        assert_eq!(
            printable(&schedule_key(&ScheduleId::from_static("nightly"))),
            "sched/nightly"
        );
        assert_eq!(printable(&calendar_key("holidays")), "cal/holidays");
        assert_eq!(
            printable(&workflow_key(wf)),
            "wf/01HQ3K9Z8N6V2T4R5S7W0X1Y2Z"
        );
        assert_eq!(
            printable(&result_key(wf, "fetch")),
            "wfr/01HQ3K9Z8N6V2T4R5S7W0X1Y2Z/fetch"
        );
        assert!(!workflows().contains(&result_key(wf, "fetch")));
        assert!(!schedules().contains(&calendar_key("x")));
    }

    #[test]
    fn undecodable_documents_name_their_key() {
        let kv = MemKv::new();
        kv.put(b"sched/broken", b"{not json").unwrap();
        let err = load::<belay_proto::Schedule>(&kv, b"sched/broken").unwrap_err();
        assert!(
            err.to_string()
                .starts_with("stored record sched/broken is unreadable: invalid JSON"),
            "{err}"
        );
        assert!(load::<belay_proto::Schedule>(&kv, b"sched/missing")
            .unwrap()
            .is_none());
    }
}
