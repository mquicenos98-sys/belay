//! SHA-256 (FIPS 180-4), hex helpers and a constant-time comparison: what
//! bearer-token checks need, and nothing more.
//!
//! Tokens are compared by digest rather than as text. Hashing first makes
//! the comparison length-independent, lets a configuration hold
//! `token_sha256` instead of the secret itself, and means the process keeps
//! no plaintext token in memory after loading its configuration.

use std::fmt;

/// Length of a digest in bytes.
pub const DIGEST_LEN: usize = 32;

/// A SHA-256 digest.
pub type Digest = [u8; DIGEST_LEN];

/// Initial hash value: the first 32 bits of the fractional parts of the
/// square roots of the first eight primes (FIPS 180-4, section 5.3.3).
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Round constants: the first 32 bits of the fractional parts of the cube
/// roots of the first sixty-four primes (section 4.2.2).
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

const BLOCK_LEN: usize = 64;

/// Incremental SHA-256.
///
/// ```
/// use belay_core::sha256::{to_hex, Sha256};
///
/// let mut h = Sha256::new();
/// h.update(b"ab");
/// h.update(b"c");
/// assert_eq!(
///     to_hex(&h.finalize()),
///     "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
/// );
/// ```
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; BLOCK_LEN],
    /// Bytes buffered in `block`.
    filled: usize,
    /// Total message length in bytes.
    length: u64,
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sha256")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

impl Default for Sha256 {
    fn default() -> Sha256 {
        Sha256::new()
    }
}

impl Sha256 {
    /// A hasher with nothing absorbed yet.
    #[must_use]
    pub fn new() -> Sha256 {
        Sha256 {
            state: H0,
            block: [0; BLOCK_LEN],
            filled: 0,
            length: 0,
        }
    }

    /// Absorbs `data`.
    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.filled > 0 {
            let take = (BLOCK_LEN - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled < BLOCK_LEN {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        let mut blocks = data.chunks_exact(BLOCK_LEN);
        for block in &mut blocks {
            let block: &[u8; BLOCK_LEN] =
                block.try_into().expect("chunks_exact yields full blocks");
            compress(&mut self.state, block);
        }
        let rest = blocks.remainder();
        self.block[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    /// Pads the message (section 5.1.1) and returns the digest.
    #[must_use]
    pub fn finalize(mut self) -> Digest {
        let bit_length = self.length.wrapping_mul(8);
        let mut tail = [0u8; 2 * BLOCK_LEN];
        tail[..self.filled].copy_from_slice(&self.block[..self.filled]);
        tail[self.filled] = 0x80;
        // The 64-bit length must fit after the 0x80 byte; if it does not,
        // the padding spills into a second block.
        let total = if self.filled + 1 + 8 <= BLOCK_LEN {
            BLOCK_LEN
        } else {
            2 * BLOCK_LEN
        };
        tail[total - 8..total].copy_from_slice(&bit_length.to_be_bytes());
        for block in tail[..total].chunks_exact(BLOCK_LEN) {
            let block: &[u8; BLOCK_LEN] =
                block.try_into().expect("chunks_exact yields full blocks");
            compress(&mut self.state, block);
        }
        let mut out = [0u8; DIGEST_LEN];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// The SHA-256 digest of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> Digest {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

/// The hash computation of section 6.2.2 for one 512-bit block.
fn compress(state: &mut [u32; 8], block: &[u8; BLOCK_LEN]) {
    let mut w = [0u32; 64];
    for (t, word) in block.chunks_exact(4).enumerate() {
        w[t] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
    }
    for t in 16..64 {
        let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
        let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16]
            .wrapping_add(s0)
            .wrapping_add(w[t - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for t in 0..64 {
        let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(big_s1)
            .wrapping_add(ch)
            .wrapping_add(K[t])
            .wrapping_add(w[t]);
        let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (slot, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(v);
    }
}

/// Lowercase hexadecimal.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Parses a digest written as 64 hexadecimal digits (either case).
///
/// ```
/// use belay_core::sha256::{parse_hex_digest, sha256};
///
/// let hex = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
/// assert_eq!(parse_hex_digest(hex).unwrap(), sha256(b"abc"));
/// assert!(parse_hex_digest("ba78").is_err());
/// ```
pub fn parse_hex_digest(text: &str) -> Result<Digest, String> {
    let text = text.trim();
    if text.len() != DIGEST_LEN * 2 {
        return Err(format!(
            "expected {} hexadecimal digits, got {}",
            DIGEST_LEN * 2,
            text.chars().count()
        ));
    }
    let mut out = [0u8; DIGEST_LEN];
    for (i, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_value(pair[0]);
        let low = hex_value(pair[1]);
        match (high, low) {
            (Some(h), Some(l)) => out[i] = h << 4 | l,
            _ => {
                return Err(format!(
                    "invalid hexadecimal digit at position {}",
                    i * 2 + usize::from(high.is_some()) + 1
                ))
            }
        }
    }
    Ok(out)
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Compares two byte strings in time that depends only on their lengths,
/// so that a mismatch position cannot be learnt by timing the comparison.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    // black_box keeps the optimiser from turning the fold into an early
    // exit on the first difference.
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(data: &[u8]) -> String {
        to_hex(&sha256(data))
    }

    // Test vectors from the NIST "SHA-256 examples" document for FIPS 180-4
    // and the long-message vector of FIPS 180-2 appendix B.3.
    #[test]
    fn matches_the_fips_180_4_example_vectors() {
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    #[test]
    fn hashes_one_million_a_characters() {
        let mut h = Sha256::new();
        let chunk = [b'a'; 1000];
        for _ in 0..1000 {
            h.update(&chunk);
        }
        assert_eq!(
            to_hex(&h.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn incremental_updates_match_one_shot_at_every_split() {
        // 55, 56 and 64 bytes are the padding boundaries: 55 is the longest
        // message whose length fits in its last block.
        for len in [0usize, 1, 55, 56, 63, 64, 65, 119, 120, 200] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let expected = sha256(&data);
            for split in 0..=len {
                let mut h = Sha256::new();
                h.update(&data[..split]);
                h.update(&data[split..]);
                assert_eq!(h.finalize(), expected, "len {len}, split {split}");
            }
        }
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_digits() {
        let digest = sha256(b"belay");
        assert_eq!(parse_hex_digest(&to_hex(&digest)).unwrap(), digest);
        assert_eq!(
            parse_hex_digest(&"z".repeat(64)).unwrap_err(),
            "invalid hexadecimal digit at position 1"
        );
        let mut bad = to_hex(&digest);
        bad.replace_range(9..10, "g");
        assert_eq!(
            parse_hex_digest(&bad).unwrap_err(),
            "invalid hexadecimal digit at position 10"
        );
        assert_eq!(
            parse_hex_digest("abc").unwrap_err(),
            "expected 64 hexadecimal digits, got 3"
        );
    }

    #[test]
    fn constant_time_eq_compares_contents_and_lengths() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
        assert!(constant_time_eq(b"", b""));
    }
}
