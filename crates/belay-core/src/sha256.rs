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
