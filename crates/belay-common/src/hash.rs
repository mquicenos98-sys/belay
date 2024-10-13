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

/// `BuildHasher` for [`FnvHasher`], e.g. `HashMap<K, V, FnvBuildHasher>`.
pub type FnvBuildHasher = BuildHasherDefault<FnvHasher>;

/// CRC-32C (Castagnoli) polynomial, bit-reflected.
const CRC32C_POLY: u32 = 0x82F6_3B78;

/// Slicing-by-8 tables: `TABLES[0]` is the classic byte-at-a-time table and
/// `TABLES[k][b]` is the CRC of byte `b` followed by `k` zero bytes, which
/// lets the main loop fold eight input bytes per step.
static CRC32C_TABLES: [[u32; 256]; 8] = crc32c_tables();

const fn crc32c_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[k - 1][i];
            tables[k][i] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    tables
}

/// Advances a raw (pre-inverted) CRC register over `data`.
fn crc32c_update(mut crc: u32, data: &[u8]) -> u32 {
    let t = &CRC32C_TABLES;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let lo = crc ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xFF) as usize]
            ^ t[2][((hi >> 8) & 0xFF) as usize]
            ^ t[1][((hi >> 16) & 0xFF) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = t[0][((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// CRC-32C of `data`, as used by iSCSI (RFC 3720), ext4 and many storage
/// formats. `crc32c(b"123456789") == 0xE306_9283`.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    !crc32c_update(!0, data)
}

/// Extends a finished CRC-32C with more data:
/// `crc32c_append(crc32c(a), b) == crc32c(a ++ b)`.
#[must_use]
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    !crc32c_update(!crc, data)
}
