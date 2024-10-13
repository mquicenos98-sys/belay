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

/// Rotates and offsets a CRC before it is stored next to the data it covers.
///
/// Computing the CRC of a buffer that itself contains CRCs tends to yield
/// degenerate values (the CRC of data followed by its own CRC is constant);
/// masking, as LevelDB does, breaks that relationship.
#[must_use]
pub const fn crc32c_mask(crc: u32) -> u32 {
    crc.rotate_right(15).wrapping_add(0xA282_EAD8)
}

/// Inverse of [`crc32c_mask`].
#[must_use]
pub const fn crc32c_unmask(masked: u32) -> u32 {
    masked.wrapping_sub(0xA282_EAD8).rotate_left(15)
}

/// Incremental CRC-32C. Also an `io::Write` sink, so a file can be checked
/// with `io::copy(&mut file, &mut crc)`.
///
/// ```
/// use belay_common::hash::{crc32c, Crc32c};
///
/// let mut crc = Crc32c::new();
/// crc.update(b"1234");
/// crc.update(b"56789");
/// assert_eq!(crc.finalize(), crc32c(b"123456789"));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crc32c {
    register: u32,
}

impl Crc32c {
    /// A CRC over no data yet.
    #[must_use]
    pub const fn new() -> Crc32c {
        Crc32c { register: !0 }
    }

    /// Feeds `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.register = crc32c_update(self.register, data);
    }

    /// The CRC of everything fed so far. Further updates may follow.
    #[must_use]
    pub const fn finalize(&self) -> u32 {
        !self.register
    }

    /// Starts over.
    pub fn reset(&mut self) {
        *self = Crc32c::new();
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Crc32c::new()
    }
}

impl io::Write for Crc32c {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn read_u64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

fn xxh_round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn xxh_merge(acc: u64, lane: u64) -> u64 {
    (acc ^ xxh_round(0, lane)).wrapping_mul(P1).wrapping_add(P4)
}

/// A fast 64-bit hash of `bytes` with a `seed`: XXH64
/// (<https://github.com/Cyan4973/xxHash>), bit-for-bit compatible with the
/// reference implementation.
///
/// Different seeds give independent hash functions, which rendezvous
/// hashing and bloom-filter style structures need.
///
/// ```
/// use belay_common::hash::hash64;
///
/// assert_eq!(hash64(b"", 0), 0xEF46_DB37_51D8_E999);
/// assert_ne!(hash64(b"queue:emails", 1), hash64(b"queue:emails", 2));
/// ```
#[must_use]
pub fn hash64(bytes: &[u8], seed: u64) -> u64 {
    let len = bytes.len() as u64;
    let mut rest = bytes;
    let mut h = if bytes.len() >= 32 {
        let mut v = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        let mut stripes = bytes.chunks_exact(32);
        for stripe in &mut stripes {
            for (lane, word) in v.iter_mut().zip(stripe.chunks_exact(8)) {
                *lane = xxh_round(*lane, read_u64(word));
            }
        }
        rest = stripes.remainder();
        let mut h = v[0]
            .rotate_left(1)
            .wrapping_add(v[1].rotate_left(7))
            .wrapping_add(v[2].rotate_left(12))
            .wrapping_add(v[3].rotate_left(18));
        for lane in v {
            h = xxh_merge(h, lane);
        }
        h
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(len);

    let mut words = rest.chunks_exact(8);
    for word in &mut words {
        h ^= xxh_round(0, read_u64(word));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
    }
    let mut tail = words.remainder();
    if tail.len() >= 4 {
        let word = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
        h ^= u64::from(word).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        tail = &tail[4..];
    }
    for &b in tail {
        h ^= u64::from(b).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }

    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

/// Jump consistent hash (Lamping & Veach, 2014): maps `key` to a bucket in
/// `0..buckets` so that growing from `n` to `n + 1` buckets moves only about
/// `1/(n + 1)` of the keys, all of them into the new bucket.
///
/// Returns 0 when `buckets` is 0, since there is no valid answer.
///
/// ```
/// use belay_common::hash::{hash64, jump_hash};
///
/// let shard = jump_hash(hash64(b"tenant-42", 0), 16);
/// assert!(shard < 16);
/// ```
#[must_use]
pub fn jump_hash(mut key: u64, buckets: u32) -> u32 {
    let mut b: i64 = -1;
    let mut j: i64 = 0;
    while j < i64::from(buckets) {
        b = j;
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        j = ((b + 1) as f64 * ((1u64 << 31) as f64 / ((key >> 33) + 1) as f64)) as i64;
    }
    b.max(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand::Rng;
    use std::collections::HashMap;
    use std::io::Write;

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xCBF2_9CE4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xAF63_DC4C_8601_EC8C);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_F739_67E8);
        const AT_COMPILE_TIME: u64 = fnv1a64(b"a");
        assert_eq!(AT_COMPILE_TIME, fnv1a64(b"a"));
    }

    #[test]
    fn fnv_hasher_is_deterministic_across_maps() {
        let mut h = FnvHasher::default();
        h.write(b"foobar");
        assert_eq!(h.finish(), fnv1a64(b"foobar"));
        let mut a: HashMap<&str, u32, FnvBuildHasher> = HashMap::default();
        let mut b: HashMap<&str, u32, FnvBuildHasher> = HashMap::default();
        for (i, k) in ["x", "y", "z", "w", "v"].iter().enumerate() {
            a.insert(k, i as u32);
            b.insert(k, i as u32);
        }
        assert!(a.keys().eq(b.keys()));
    }

    #[test]
    fn crc32c_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn crc32c_matches_rfc3720_vectors() {
        // RFC 3720 appendix B.4.
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let ascending: Vec<u8> = (0..32).collect();
        assert_eq!(crc32c(&ascending), 0x46DD_794E);
        let descending: Vec<u8> = (0..32).rev().collect();
        assert_eq!(crc32c(&descending), 0x113F_DB5C);
        let iscsi_read_pdu: [u8; 48] = [
            0x01, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14,
            0x00, 0x00, 0x00, 0x18, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(crc32c(&iscsi_read_pdu), 0xD996_3A56);
    }

    fn crc32c_bytewise(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ CRC32C_POLY
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn sliced_crc_matches_bitwise_reference_for_all_lengths_and_offsets() {
        let mut rng = Rng::seed_from_u64(20);
        let mut data = vec![0u8; 80];
        rng.fill_bytes(&mut data);
        for start in 0..8 {
            for end in start..data.len() {
                let slice = &data[start..end];
                assert_eq!(crc32c(slice), crc32c_bytewise(slice), "{start}..{end}");
            }
        }
    }

    #[test]
    fn incremental_and_append_match_one_shot() {
        let mut rng = Rng::seed_from_u64(21);
        let mut data = vec![0u8; 1000];
        rng.fill_bytes(&mut data);
        let expected = crc32c(&data);
        for _ in 0..50 {
            let cut = rng.gen_range(0..=data.len());
            let mut crc = Crc32c::new();
            crc.update(&data[..cut]);
            crc.update(&data[cut..]);
            assert_eq!(crc.finalize(), expected);
            assert_eq!(crc32c_append(crc32c(&data[..cut]), &data[cut..]), expected);
        }
        let mut crc = Crc32c::default();
        crc.update(b"junk");
        crc.reset();
        crc.write_all(b"123456789")
            .expect("writing to a CRC never fails");
        crc.flush().expect("flush never fails");
        assert_eq!(crc.finalize(), 0xE306_9283);
    }

    #[test]
    fn mask_round_trips_and_changes_the_value() {
        for crc in [0, 1, 0xE306_9283, u32::MAX] {
            assert_eq!(crc32c_unmask(crc32c_mask(crc)), crc);
            assert_ne!(crc32c_mask(crc), crc);
        }
    }

    #[test]
    fn hash64_matches_xxh64_reference_vectors() {
        assert_eq!(hash64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(hash64(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(hash64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
        // 39 bytes: exercises the 32-byte stripe loop and every tail path.
        assert_eq!(
            hash64(b"Nobody inspects the spammish repetition", 0),
            0xFBCE_A83C_8A37_8BF1
        );
    }

    #[test]
    fn hash64_depends_on_seed_and_every_byte() {
        let base = hash64(b"the quick brown fox jumps over the lazy dog!!", 7);
        assert_ne!(
            base,
            hash64(b"the quick brown fox jumps over the lazy dog!!", 8)
        );
        let mut bytes = b"the quick brown fox jumps over the lazy dog!!".to_vec();
        for i in 0..bytes.len() {
            bytes[i] ^= 1;
            assert_ne!(hash64(&bytes, 7), base, "flipping byte {i}");
            bytes[i] ^= 1;
        }
    }

    #[test]
    fn jump_hash_matches_published_values() {
        assert_eq!(jump_hash(1, 1), 0);
        assert_eq!(jump_hash(42, 57), 43);
        assert_eq!(jump_hash(0xDEAD_10CC, 1), 0);
        assert_eq!(jump_hash(0xDEAD_10CC, 666), 361);
        assert_eq!(jump_hash(256, 1024), 520);
        assert_eq!(jump_hash(7, 0), 0);
    }

    #[test]
    fn jump_hash_moves_keys_only_to_the_new_bucket() {
        let mut rng = Rng::seed_from_u64(22);
        let keys: Vec<u64> = (0..2_000).map(|_| rng.next_u64()).collect();
        for n in 1..40u32 {
            let mut moved = 0;
            for &k in &keys {
                let before = jump_hash(k, n);
                let after = jump_hash(k, n + 1);
                assert!(before < n && after < n + 1);
                if before != after {
                    assert_eq!(after, n, "keys may only move into the new bucket");
                    moved += 1;
                }
            }
            // Expect about 2000 / (n + 1) moves; allow generous slack.
            let expected = 2_000 / (n + 1);
            assert!(moved <= expected * 2 + 20, "n={n} moved={moved}");
        }
    }

    #[test]
    fn jump_hash_spreads_keys_evenly() {
        let mut counts = [0u32; 10];
        for k in 0..20_000u64 {
            counts[jump_hash(hash64(&k.to_le_bytes(), 0), 10) as usize] += 1;
        }
        for c in counts {
            assert!((1_800..=2_200).contains(&c), "{counts:?}");
        }
    }
}
