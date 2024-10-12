//! Non-cryptographic hashes and checksums with fixed, specified output.
//!
//! Every function here is a published algorithm with known test vectors, so
//! values can be persisted (store checksums) or compared across nodes
//! (cluster placement) without depending on this crate's version or the
//! platform. None of them resist deliberate collisions; don't use them where
//! an attacker chooses the input and a collision matters.

use std::hash::{BuildHasherDefault, Hasher};
use std::io;

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;

const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// 64-bit FNV-1a. Simple and fast for short keys.
#[must_use]
pub const fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    hash
}

/// A `Hasher` computing FNV-1a, for hash maps whose iteration order must be
/// the same in every process (the standard `RandomState` is seeded per
/// process). Not DoS resistant.
#[derive(Clone, Copy, Debug)]
pub struct FnvHasher(u64);

impl Default for FnvHasher {
    fn default() -> Self {
        FnvHasher(FNV_OFFSET)
    }
}

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(FNV_PRIME);
        }
    }
}
