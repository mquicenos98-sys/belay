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

#[test]
fn short_decimals_round_trip_and_keep_their_digits() {
    let mut rng = Rng(0xdec1_3a15);
    for _ in 0..20_000 {
        let mantissa = rng.below(1_000_000_000);
        let exponent = rng.below(61) as i64 - 30;
        let literal = format!("{mantissa}e{exponent}");
        let value = parse(&literal).unwrap();
        let text = to_string(&value);
        assert_eq!(parse(&text).unwrap(), value, "{literal} -> {text}");
        if mantissa != 0 {
            let expected = mantissa.to_string().trim_end_matches('0').to_owned();
            assert_eq!(significant_digits(&text), expected, "{literal} -> {text}");
        }
    }
}

#[test]
fn f32_values_round_trip_through_json() {
    let mut rng = Rng(0xf32);
    let mut checked = 0;
    while checked < 50_000 {
        let f = f32::from_bits(rng.next() as u32);
        if !f.is_finite() {
            continue;
        }
        checked += 1;
        let text = belay_json::to_string_of(&f);
        let back: f32 = belay_json::from_str(&text).unwrap();
        assert_eq!(back.to_bits(), f.to_bits(), "{f:e} was written as {text}");
        let shortest = significant_digits(&format!("{:e}", f));
        assert_eq!(significant_digits(&text), shortest, "{text} for {f:e}");
    }
}

#[test]
fn integers_round_trip_exactly() {
    let boundaries: [i128; 12] = [
        0,
        1,
        -1,
        (1 << 53) + 1,
        -(1 << 53) - 1,
        i64::MAX as i128,
        i64::MIN as i128,
        u64::MAX as i128,
        u32::MAX as i128,
        i32::MIN as i128,
        9_999_999_999_999_999,
        -9_223_372_036_854_775_807,
    ];
    for n in boundaries {
        let text = n.to_string();
        let value = parse(&text).unwrap();
        assert!(value.as_number().unwrap().is_integer(), "{text}");
        assert_eq!(to_string(&value), text);
    }
    let mut rng = Rng(0x1_2345);
    for _ in 0..20_000 {
        let u = rng.next() >> rng.below(64);
        assert_eq!(u64::from_json(&parse(&u.to_string()).unwrap()), Ok(u));
        let i = rng.next() as i64 >> rng.below(64);
        assert_eq!(i64::from_json(&parse(&i.to_string()).unwrap()), Ok(i));
        assert_eq!(to_string(&i.to_json()), i.to_string());
    }
}

const CHAR_POOL: &[char] = &[
    'a',
    'Z',
    '0',
    ' ',
    '"',
    '\\',
    '/',
    '\u{0}',
    '\u{8}',
    '\u{9}',
    '\u{a}',
    '\u{c}',
    '\u{d}',
    '\u{1f}',
    '\u{7f}',
    '\u{80}',
    '\u{e9}',
    '\u{2028}',
    '\u{2029}',
    '\u{fffd}',
    '\u{ffff}',
    '\u{10000}',
    '\u{1f600}',
    '\u{10ffff}',
];

fn random_string(rng: &mut Rng, max_len: u64) -> String {
    (0..rng.below(max_len + 1))
        .map(|_| CHAR_POOL[rng.below(CHAR_POOL.len() as u64) as usize])
        .collect()
}

#[test]
fn strings_round_trip_through_every_writer_mode() {
    let mut rng = Rng(0x57_2179);
    let ascii = WriteOptions::compact().ascii_only(true);
    for _ in 0..5_000 {
        let s = random_string(&mut rng, 24);
        let value = Value::from(s.as_str());
        let plain = to_string(&value);
        assert_eq!(parse(&plain).unwrap(), value, "{plain}");
        assert!(
            !plain.chars().any(|c| c < ' '),
            "raw control character in {plain:?}"
        );
        let escaped = to_string_with(&value, &ascii);
        assert!(escaped.is_ascii(), "{escaped}");
        assert_eq!(parse(&escaped).unwrap(), value, "{escaped}");
    }
}
