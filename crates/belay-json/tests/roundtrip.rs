//! Round trips through the writer and parser: randomized with a seeded PRNG
//! (so failures reproduce) plus golden output for the pretty printer.

use belay_json::{
    json, parse, to_string, to_string_pretty, to_string_with, write_to, FromJson, Map, ToJson,
    Value, WriteOptions,
};

/// SplitMix64: tiny, fast and good enough to drive property tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// The significant digits of a decimal rendering, without sign, point,
/// exponent, or leading/trailing zeros: `"-0.00150e3"` -> `"15"`.
fn significant_digits(text: &str) -> String {
    let mantissa = text.split(['e', 'E']).next().unwrap_or("");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    digits
        .trim_start_matches('0')
        .trim_end_matches('0')
        .to_owned()
}

#[test]
fn random_f64_bit_patterns_round_trip_exactly_and_shortest() {
    let mut rng = Rng(0x5eed_f64);
    let mut checked = 0;
    while checked < 100_000 {
        let f = f64::from_bits(rng.next());
        if !f.is_finite() {
            continue;
        }
        checked += 1;
        let text = to_string(&Value::from(f));
        assert!(
            text.contains(['.', 'e']),
            "{text} must read back as a float"
        );
        let back = parse(&text).unwrap().as_f64().unwrap();
        assert_eq!(back.to_bits(), f.to_bits(), "{f:e} was written as {text}");
        let shortest = significant_digits(&format!("{:e}", f));
        assert_eq!(
            significant_digits(&text),
            shortest,
            "{text} is not the shortest form of {f:e}"
        );
    }
}
