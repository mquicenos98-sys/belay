use super::*;
use crate::member::{MAX_ADDR_LEN, MAX_META_BYTES, MAX_NODE_ID_LEN};
use belay_common::codec::DecodeErrorKind;
use belay_common::Rng;

const TAG: u32 = 0x1234_5678;

fn id(s: &str) -> NodeId {
    NodeId::new(s).unwrap()
}

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn meta(pairs: &[(&str, &str)]) -> Meta {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}
