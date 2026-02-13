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
        let total = if self.filled + 8 <= BLOCK_LEN {
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
