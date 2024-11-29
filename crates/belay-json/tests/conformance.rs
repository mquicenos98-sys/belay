//! Parser conformance in the style of Nicolas Seriot's JSONTestSuite.
//!
//! `y_` documents must parse and `n_` documents must be rejected under the
//! default strict options. `i_` cases are those RFC 8259 leaves to the
//! implementation; the tests below pin down the choice this parser makes for
//! each. Names follow the suite's file names so failures are easy to look up.

use belay_json::{
    json, parse, parse_bytes, parse_with, to_string, to_string_pretty, ParseErrorKind, ParseOptions,
};

const MUST_ACCEPT: &[(&str, &str)] = &[
    ("y_array_arraysWithSpaces", "[[]   ]"),
    ("y_array_empty-string", r#"[""]"#),
    ("y_array_empty", "[]"),
    ("y_array_ending_with_newline", "[\"a\"]\n"),
    ("y_array_false", "[false]"),
    ("y_array_heterogeneous", r#"[null, 1, "1", {}]"#),
    ("y_array_null", "[null]"),
    ("y_array_with_1_and_newline", "[1\n]"),
    ("y_array_with_leading_space", " [1]"),
    ("y_array_with_several_null", "[1,null,null,null,2]"),
    ("y_array_with_trailing_space", "[2] "),
    ("y_number", "[123e65]"),
    ("y_number_0e+1", "[0e+1]"),
    ("y_number_0e1", "[0e1]"),
    ("y_number_after_space", "[ 4]"),
    (
        "y_number_double_close_to_zero",
        "[-0.000000000000000000000000000000000000000000000000000000000000000000000000000000001]",
    ),
    ("y_number_int_with_exp", "[20e1]"),
    ("y_number_minus_zero", "[-0]"),
    ("y_number_negative_int", "[-123]"),
    ("y_number_negative_one", "[-1]"),
    ("y_number_real_capital_e", "[1E22]"),
    ("y_number_real_capital_e_neg_exp", "[1E-2]"),
    ("y_number_real_capital_e_pos_exp", "[1E+2]"),
    ("y_number_real_exponent", "[123e45]"),
    ("y_number_real_fraction_exponent", "[123.456e78]"),
    ("y_number_real_neg_exp", "[1e-2]"),
    ("y_number_real_pos_exponent", "[1e+2]"),
    ("y_number_simple_int", "[123]"),
    ("y_number_simple_real", "[123.456789]"),
    ("y_object", r#"{"asd":"sdf", "dfg":"fgh"}"#),
    ("y_object_basic", r#"{"asd":"sdf"}"#),
    ("y_object_duplicated_key", r#"{"a":"b","a":"c"}"#),
    ("y_object_duplicated_key_and_value", r#"{"a":"b","a":"b"}"#),
    ("y_object_empty", "{}"),
    ("y_object_empty_key", r#"{"":0}"#),
    ("y_object_escaped_null_in_key", r#"{"foo\u0000bar": 42}"#),
    (
        "y_object_extreme_numbers",
        r#"{ "min": -1.0e+28, "max": 1.0e+28 }"#,
    ),
    (
        "y_object_long_strings",
        r#"{"x":[{"id": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}], "id": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#,
    ),
    ("y_object_simple", r#"{"a":[]}"#),
    (
        "y_object_string_unicode",
        r#"{"title":"\u041f\u043e\u043b\u0442\u043e\u0440\u0430 \u0417\u0435\u043c\u043b\u0435\u043a\u043e\u043f\u0430" }"#,
    ),
    ("y_object_with_newlines", "{\n\"a\": \"b\"\n}"),
    (
        "y_string_1_2_3_bytes_UTF-8_sequences",
        r#"["\u0060\u012a\u12AB"]"#,
    ),
    ("y_string_accepted_surrogate_pair", r#"["\uD801\udc37"]"#),
    (
        "y_string_accepted_surrogate_pairs",
        r#"["\ud83d\ude39\ud83d\udc8d"]"#,
    ),
    ("y_string_allowed_escapes", r#"["\"\\\/\b\f\n\r\t"]"#),
    ("y_string_backslash_and_u_escaped_zero", r#"["\\u0000"]"#),
    ("y_string_backslash_doublequotes", r#"["\""]"#),
    ("y_string_comments", r#"["a/*b*/c/*d//e"]"#),
    ("y_string_double_escape_a", r#"["\\a"]"#),
    ("y_string_double_escape_n", r#"["\\n"]"#),
    ("y_string_escaped_control_character", r#"["\u0012"]"#),
    ("y_string_escaped_noncharacter", r#"["\uFFFF"]"#),
    ("y_string_in_array", r#"["asd"]"#),
    ("y_string_in_array_with_leading_space", r#"[ "asd"]"#),
    ("y_string_last_surrogates_1_and_2", r#"["\uDBFF\uDFFF"]"#),
    ("y_string_nbsp_uescaped", r#"["new\u00A0line"]"#),
    ("y_string_nonCharacterInUTF-8_U+10FFFF", "[\"\u{10FFFF}\"]"),
    ("y_string_nonCharacterInUTF-8_U+FFFF", "[\"\u{FFFF}\"]"),
    ("y_string_null_escape", r#"["\u0000"]"#),
    ("y_string_one-byte-utf-8", r#"["\u002c"]"#),
    ("y_string_pi", "[\"\u{3c0}\"]"),
    (
        "y_string_reservedCharacterInUTF-8_U+1BFFF",
        "[\"\u{1BFFF}\"]",
    ),
    ("y_string_simple_ascii", r#"["asd "]"#),
    ("y_string_space", r#"" ""#),
    (
        "y_string_surrogates_U+1D11E_MUSICAL_SYMBOL_G_CLEF",
        r#"["\uD834\uDd1e"]"#,
    ),
    ("y_string_three-byte-utf-8", r#"["\u0821"]"#),
    ("y_string_two-byte-utf-8", r#"["\u0123"]"#),
    ("y_string_u+2028_line_sep", "[\"\u{2028}\"]"),
    ("y_string_u+2029_par_sep", "[\"\u{2029}\"]"),
    ("y_string_uEscape", r#"["\u0061\u30af\u30EA\u30b9"]"#),
    ("y_string_uescaped_newline", r#"["new\u000Aline"]"#),
    ("y_string_unescaped_char_delete", "[\"\u{7f}\"]"),
    ("y_string_unicode", r#"["\uA66D"]"#),
    ("y_string_unicodeEscapedBackslash", r#"["\u005C"]"#),
    ("y_string_unicode_2", "[\"\u{2342}\u{3234}\u{2342}\"]"),
    ("y_string_unicode_escaped_double_quote", r#"["\u0022"]"#),
    ("y_string_utf8", "[\"\u{20ac}\u{1d11e}\"]"),
    ("y_string_with_del_character", "[\"a\u{7f}a\"]"),
    ("y_structure_lonely_false", "false"),
    ("y_structure_lonely_int", "42"),
    ("y_structure_lonely_negative_real", "-0.1"),
    ("y_structure_lonely_null", "null"),
    ("y_structure_lonely_string", r#""asd""#),
    ("y_structure_lonely_true", "true"),
    ("y_structure_string_empty", r#""""#),
    ("y_structure_trailing_newline", "[\"a\"]\n"),
    ("y_structure_true_in_array", "[true]"),
    ("y_structure_whitespace_array", " [] "),
];

const MUST_REJECT: &[(&str, &str)] = &[
    ("n_array_1_true_without_comma", "[1 true]"),
    ("n_array_colon_instead_of_comma", r#"["": 1]"#),
    ("n_array_comma_after_close", r#"[""],"#),
    ("n_array_comma_and_number", "[,1]"),
    ("n_array_double_comma", "[1,,2]"),
    ("n_array_double_extra_comma", r#"["x",,]"#),
    ("n_array_extra_close", r#"["x"]]"#),
    ("n_array_extra_comma", r#"["",]"#),
    ("n_array_incomplete", r#"["x""#),
    ("n_array_incomplete_invalid_value", "[x"),
    ("n_array_inner_array_no_comma", "[3[4]]"),
    ("n_array_items_separated_by_semicolon", "[1:2]"),
    ("n_array_just_comma", "[,]"),
    ("n_array_just_minus", "[-]"),
    ("n_array_missing_value", r#"[   , ""]"#),
    ("n_array_newlines_unclosed", "[\"a\",\n4\n,1,"),
    ("n_array_number_and_comma", "[1,]"),
    ("n_array_number_and_several_commas", "[1,,]"),
    ("n_array_spaces_vertical_tab_formfeed", "[\"\u{b}a\"\u{c}]"),
    ("n_array_star_inside", "[*]"),
    ("n_array_unclosed", r#"["""#),
    ("n_array_unclosed_trailing_comma", "[1,"),
    ("n_array_unclosed_with_new_lines", "[1,\n1\n,1"),
    ("n_array_unclosed_with_object_inside", "[{}"),
    ("n_incomplete_false", "[fals]"),
    ("n_incomplete_null", "[nul]"),
    ("n_incomplete_true", "[tru]"),
    ("n_multidigit_number_then_00", "123\u{0}"),
    ("n_number_++", "[++1234]"),
    ("n_number_+1", "[+1]"),
    ("n_number_+Inf", "[+Inf]"),
    ("n_number_-01", "[-01]"),
    ("n_number_-1.0.", "[-1.0.]"),
    ("n_number_-2.", "[-2.]"),
    ("n_number_-NaN", "[-NaN]"),
    ("n_number_.-1", "[.-1]"),
    ("n_number_.2e-3", "[.2e-3]"),
    ("n_number_0.1.2", "[0.1.2]"),
    ("n_number_0.3e+", "[0.3e+]"),
    ("n_number_0.3e", "[0.3e]"),
    ("n_number_0.e1", "[0.e1]"),
    ("n_number_0_capital_E+", "[0E+]"),
    ("n_number_0_capital_E", "[0E]"),
    ("n_number_0e+", "[0e+]"),
    ("n_number_0e", "[0e]"),
    ("n_number_1.0e+", "[1.0e+]"),
    ("n_number_1.0e-", "[1.0e-]"),
    ("n_number_1.0e", "[1.0e]"),
    ("n_number_1_000", "[1 000.0]"),
    ("n_number_1eE2", "[1eE2]"),
    ("n_number_2.e+3", "[2.e+3]"),
    ("n_number_2.e-3", "[2.e-3]"),
    ("n_number_2.e3", "[2.e3]"),
    ("n_number_9.e+", "[9.e+]"),
    ("n_number_Inf", "[Inf]"),
    ("n_number_NaN", "[NaN]"),
    ("n_number_U+FF11_fullwidth_digit_one", "[\u{FF11}]"),
    ("n_number_expression", "[1+2]"),
    ("n_number_hex_1_digit", "[0x1]"),
    ("n_number_hex_2_digits", "[0x42]"),
    ("n_number_infinity", "[Infinity]"),
    ("n_number_invalid+-", "[0e+-1]"),
    ("n_number_invalid-negative-real", "[-123.123foo]"),
    ("n_number_minus_infinity", "[-Infinity]"),
    ("n_number_minus_sign_with_trailing_garbage", "[-foo]"),
    ("n_number_minus_space_1", "[- 1]"),
    ("n_number_neg_int_starting_with_zero", "[-012]"),
    ("n_number_neg_real_without_int_part", "[-.123]"),
    ("n_number_neg_with_garbage_at_end", "[-1x]"),
    ("n_number_real_garbage_after_e", "[1ea]"),
    ("n_number_real_without_fractional_part", "[1.]"),
    ("n_number_starting_with_dot", "[.123]"),
    ("n_number_with_alpha", "[1.2a-3]"),
    ("n_number_with_alpha_char", "[1.8011670033376514H-308]"),
    ("n_number_with_leading_zero", "[012]"),
    ("n_object_bad_value", r#"["x", truth]"#),
    ("n_object_bracket_key", r#"{[: "x"}"#),
    ("n_object_comma_instead_of_colon", r#"{"x", null}"#),
    ("n_object_double_colon", r#"{"x"::"b"}"#),
    ("n_object_emoji", "{\u{1F1E8}\u{1F1ED}}"),
    ("n_object_garbage_at_end", r#"{"a":"a" 123}"#),
    ("n_object_key_with_single_quotes", "{key: 'value'}"),
    ("n_object_missing_colon", r#"{"a" b}"#),
    ("n_object_missing_key", r#"{:"b"}"#),
    ("n_object_missing_semicolon", r#"{"a" "b"}"#),
    ("n_object_missing_value", r#"{"a":"#),
    ("n_object_no-colon", r#"{"a""#),
    ("n_object_non_string_key", "{1:1}"),
    (
        "n_object_non_string_key_but_huge_number_instead",
        "{9999E9999:1}",
    ),
    ("n_object_repeated_null_null", "{null:null,null:null}"),
    ("n_object_several_trailing_commas", r#"{"id":0,,,,,}"#),
    ("n_object_single_quote", "{'a':0}"),
    ("n_object_trailing_comma", r#"{"id":0,}"#),
    ("n_object_trailing_comment", r#"{"a":"b"}/**/"#),
    ("n_object_trailing_comment_open", r#"{"a":"b"}/**//"#),
    ("n_object_trailing_comment_slash_open", r#"{"a":"b"}//"#),
    (
        "n_object_trailing_comment_slash_open_incomplete",
        r#"{"a":"b"}/"#,
    ),
    ("n_object_two_commas_in_a_row", r#"{"a":"b",,"c":"d"}"#),
    ("n_object_unquoted_key", r#"{a: "b"}"#),
    ("n_object_unterminated-value", r#"{"a":"a"#),
    ("n_object_with_single_string", r#"{ "foo" : "bar", "a" }"#),
    ("n_object_with_trailing_garbage", r#"{"a":"b"}#"#),
    ("n_single_space", " "),
    ("n_string_1_surrogate_then_escape", r#"["\uD800\"]"#),
    ("n_string_1_surrogate_then_escape_u", r#"["\uD800\u"]"#),
    ("n_string_1_surrogate_then_escape_u1", r#"["\uD800\u1"]"#),
    ("n_string_1_surrogate_then_escape_u1x", r#"["\uD800\u1x"]"#),
    ("n_string_accentuated_char_no_quotes", "[\u{e9}]"),
    ("n_string_backslash_00", "[\"\\\u{0}\"]"),
    ("n_string_escape_x", r#"["\x00"]"#),
    ("n_string_escaped_backslash_bad", r#"["\\\"]"#),
    ("n_string_escaped_ctrl_char_tab", "[\"\\\t\"]"),
    ("n_string_escaped_emoji", "[\"\\\u{1F300}\"]"),
    ("n_string_incomplete_escape", r#"["\"]"#),
    ("n_string_incomplete_escaped_character", r#"["\u00A"]"#),
    ("n_string_incomplete_surrogate", r#"["\uD834\uDd"]"#),
    (
        "n_string_incomplete_surrogate_escape_invalid",
        r#"["\uD800\uD800\x"]"#,
    ),
    ("n_string_invalid_backslash_esc", r#"["\a"]"#),
    ("n_string_invalid_unicode_escape", r#"["\uqqqq"]"#),
    ("n_string_leading_uescaped_thinspace", r#"[\u0020"asd"]"#),
    ("n_string_no_quotes_with_bad_escape", r#"[\n]"#),
    ("n_string_single_doublequote", r#"""#),
    ("n_string_single_quote", "['single quote']"),
    ("n_string_single_string_no_double_quotes", "abc"),
    ("n_string_start_escape_unclosed", r#"["\"#),
    ("n_string_unescaped_ctrl_char", "[\"a\u{0}a\"]"),
    ("n_string_unescaped_newline", "[\"new\nline\"]"),
    ("n_string_unescaped_tab", "[\"\t\"]"),
    ("n_string_unicode_CapitalU", r#""\UA66D""#),
    ("n_string_with_trailing_garbage", r#"""x"#),
    ("n_structure_U+2060_word_joined", "[\u{2060}]"),
    ("n_structure_UTF8_BOM_no_data", "\u{FEFF}"),
    ("n_structure_angle_bracket_.", "<.>"),
    ("n_structure_angle_bracket_null", "[<null>]"),
    ("n_structure_array_trailing_garbage", "[1]x"),
    ("n_structure_array_with_extra_array_close", "[1]]"),
    ("n_structure_array_with_unclosed_string", r#"["asd]"#),
    ("n_structure_ascii-unicode-identifier", "a\u{e5}"),
    ("n_structure_capitalized_True", "[True]"),
    ("n_structure_close_unopened_array", "1]"),
    (
        "n_structure_comma_instead_of_closing_brace",
        r#"{"x": true,"#,
    ),
    ("n_structure_double_array", "[][]"),
    ("n_structure_end_array", "]"),
    ("n_structure_lone-open-bracket", "["),
    ("n_structure_no_data", ""),
    ("n_structure_null-byte-outside-string", "[\u{0}]"),
    ("n_structure_number_with_trailing_garbage", "2@"),
    ("n_structure_object_followed_by_closing_object", "{}}"),
    ("n_structure_object_unclosed_no_value", r#"{"":"#),
    ("n_structure_object_with_comment", r#"{"a":/*comment*/"b"}"#),
    (
        "n_structure_object_with_trailing_garbage",
        r#"{"a": true} "x""#,
    ),
    ("n_structure_open_array_apostrophe", "['"),
    ("n_structure_open_array_comma", "[,"),
    ("n_structure_open_array_open_object", "[{"),
    ("n_structure_open_array_open_string", r#"["a"#),
    ("n_structure_open_array_string", r#"["a""#),
    ("n_structure_open_object", "{"),
    ("n_structure_open_object_close_array", "{]"),
    ("n_structure_open_object_comma", "{,"),
    ("n_structure_open_object_open_array", "{["),
    ("n_structure_open_object_open_string", r#"{"a"#),
    ("n_structure_open_object_string_with_apostrophes", "{'a'"),
    ("n_structure_single_star", "*"),
    ("n_structure_trailing_#", r#"{"a":"b"}#{}"#),
    ("n_structure_uescaped_LF_before_string", r#"[\u000A""]"#),
    ("n_structure_unclosed_array", "[1"),
    ("n_structure_unclosed_array_partial_null", "[ false, nul"),
    (
        "n_structure_unclosed_array_unfinished_false",
        "[ true, fals",
    ),
    ("n_structure_unclosed_array_unfinished_true", "[ false, tru"),
    ("n_structure_unclosed_object", r#"{"asd":"asd""#),
    ("n_structure_unicode-identifier", "\u{e5}"),
    ("n_structure_whitespace_formfeed", "[\u{c}]"),
];

/// Cases that are not valid UTF-8 and so cannot be written as `&str`.
const MUST_REJECT_BYTES: &[(&str, &[u8])] = &[
    ("n_array_a_invalid_utf8", b"[a\xe5]"),
    ("n_array_invalid_utf8", b"[\xff]"),
    ("n_number_invalid-utf-8-in-bigger-int", b"[123\xe5]"),
    ("n_number_invalid-utf-8-in-exponent", b"[1e1\xe5]"),
    ("n_number_invalid-utf-8-in-int", b"[0\xe5]\n"),
    (
        "n_object_lone_continuation_byte_in_key_and_trailing_comma",
        b"{\"\xb9\":\"0\",}",
    ),
    ("n_string_invalid_utf8_after_escape", b"[\"\\\xe5\"]"),
    ("n_string_invalid_utf-8", b"[\"\xff\"]"),
    ("n_string_iso_latin_1", b"[\"\xe9\"]"),
    ("n_string_lone_utf8_continuation_byte", b"[\"\x81\"]"),
    ("n_string_overlong_sequence_2_bytes", b"[\"\xc0\xaf\"]"),
    (
        "n_string_overlong_sequence_6_bytes",
        b"[\"\xfc\x83\xbf\xbf\xbf\xbf\"]",
    ),
    ("n_string_truncated-utf-8", b"[\"\xe0\xff\"]"),
    ("n_string_UTF-16_surrogate_in_UTF-8", b"[\"\xed\xa0\x80\"]"),
    ("n_structure_incomplete_UTF8_BOM", b"\xef\xbb{}"),
    ("n_structure_lone-invalid-utf-8", b"\xe5"),
    (
        "i_string_UTF-16LE_with_BOM",
        b"\xff\xfe[\x00\"\x00\xe9\x00\"\x00]\x00",
    ),
    ("i_string_not_in_unicode_range", b"[\"\xf4\xbf\xbf\xbf\"]"),
];

/// Strict-mode `n_` cases that `ParseOptions::relaxed()` deliberately accepts.
const RELAXED_ACCEPTS: &[&str] = &[
    "n_array_extra_comma",
    "n_array_number_and_comma",
    "n_object_trailing_comma",
    "n_object_trailing_comment",
    "n_object_trailing_comment_slash_open",
    "n_structure_object_with_comment",
];

#[test]
fn accepts_every_y_case() {
    let failures: Vec<String> = MUST_ACCEPT
        .iter()
        .filter_map(|(name, src)| parse(src).err().map(|e| format!("{name}: {e}")))
        .collect();
    assert!(
        failures.is_empty(),
        "rejected valid documents: {failures:#?}"
    );
}

#[test]
fn rejects_every_n_case() {
    let accepted: Vec<String> = MUST_REJECT
        .iter()
        .filter_map(|(name, src)| parse(src).ok().map(|v| format!("{name}: {v}")))
        .collect();
    assert!(
        accepted.is_empty(),
        "accepted invalid documents: {accepted:#?}"
    );
}

#[test]
fn rejects_invalid_utf8() {
    for (name, bytes) in MUST_REJECT_BYTES {
        let err = parse_bytes(bytes).expect_err(name);
        assert_eq!(err.kind(), &ParseErrorKind::InvalidUtf8, "{name}");
    }
}

#[test]
fn accepted_documents_survive_a_write_and_reparse() {
    for (name, src) in MUST_ACCEPT {
        let value = parse(src).unwrap();
        for text in [to_string(&value), to_string_pretty(&value)] {
            let again = parse(&text).unwrap_or_else(|e| panic!("{name}: {e} in {text:?}"));
            assert_eq!(again, value, "{name}");
        }
    }
}

#[test]
fn relaxed_mode_accepts_only_comments_and_single_trailing_commas() {
    let relaxed = ParseOptions::relaxed();
    for (name, src) in MUST_REJECT {
        let expected = RELAXED_ACCEPTS.contains(name);
        assert_eq!(parse_with(src, &relaxed).is_ok(), expected, "{name}");
    }
    for (name, src) in MUST_ACCEPT {
        assert!(parse_with(src, &relaxed).is_ok(), "{name}");
    }
}

#[test]
fn decoded_strings_match_their_escapes() {
    let decoded = |src: &str| parse(src).unwrap()[0].as_str().unwrap().to_owned();
    assert_eq!(decoded(r#"["\uD834\uDd1e"]"#), "\u{1D11E}");
    assert_eq!(decoded(r#"["\uDBFF\uDFFF"]"#), "\u{10FFFF}");
    assert_eq!(decoded(r#"["\\u0000"]"#), "\\u0000");
    assert_eq!(decoded(r#"["\u005C"]"#), "\\");
    assert_eq!(decoded(r#"["new\u000Aline"]"#), "new\nline");
    assert_eq!(decoded(r#"["\"\\\/\b\f\n\r\t"]"#), "\"\\/\u{8}\u{c}\n\r\t");
    let key = parse(r#"{"foo\u0000bar": 42}"#).unwrap();
    assert_eq!(key["foo\u{0}bar"], 42);
}

#[test]
fn numbers_decode_to_the_expected_representation() {
    let first = |src: &str| parse(src).unwrap()[0].clone();
    assert_eq!(
        first("[-0]").as_f64().map(f64::is_sign_negative),
        Some(true)
    );
    assert_eq!(first("[20e1]"), json!(200.0));
    assert_eq!(first("[1E22]"), json!(1e22));
    assert_eq!(first("[123]"), json!(123));
    let extremes = parse(r#"{ "min": -1.0e+28, "max": 1.0e+28 }"#).unwrap();
    assert_eq!(extremes, json!({"min": -1e28, "max": 1e28}));
}

#[test]
fn i_number_overflow_is_rejected() {
    let huge_exponent = format!("[0.4e0066{}969999999006]", "9".repeat(400));
    let cases = [
        huge_exponent.as_str(),
        "[-1e+9999]",
        "[1.5e+9999]",
        "[-123123e100000]",
        "[123123e100000]",
    ];
    for src in cases {
        let err = parse(src).unwrap_err();
        assert_eq!(err.kind(), &ParseErrorKind::NumberOutOfRange, "{src}");
        assert_eq!(err.column(), 2, "{src}");
    }
}

#[test]
fn i_number_underflow_and_wide_integers_are_accepted_as_floats() {
    let first = |src: &str| parse(src).unwrap()[0].clone();
    assert_eq!(first("[123.456e-789]"), json!(0.0));
    assert_eq!(first("[123e-10000000]"), json!(0.0));
    assert_eq!(first("[100000000000000000000]"), json!(1e20));
    assert_eq!(
        first("[-123123123123123123123123123123]"),
        json!(-1.2312312312312312e29)
    );
    let big_negative = first("[-237462374673276894279832749832423479823246327846]");
    assert!(big_negative.is_f64());
    assert!(big_negative.as_f64().unwrap() < -2.3e47);
}

#[test]
fn i_string_unpaired_surrogates_are_rejected() {
    let cases = [
        (r#"["\uDADA"]"#, 0xDADA),
        (r#"["\uD888\u1234"]"#, 0xD888),
        (r#"["\uD800\n"]"#, 0xD800),
        (r#"["\uD800\uD800\n"]"#, 0xD800),
        (r#"["\ud800"]"#, 0xD800),
        (r#"["\ud800abc"]"#, 0xD800),
        (r#"["\uDd1e\uD834"]"#, 0xDD1E),
        (r#"["\uDFAA"]"#, 0xDFAA),
    ];
    for (src, unit) in cases {
        let err = parse(src).unwrap_err();
        assert_eq!(err.kind(), &ParseErrorKind::LoneSurrogate(unit), "{src}");
        assert_eq!(err.column(), 3, "points at the escape in {src}");
    }
}

#[test]
fn i_structure_byte_order_mark_is_rejected() {
    assert_eq!(
        parse("\u{FEFF}{}").unwrap_err().kind(),
        &ParseErrorKind::ByteOrderMark
    );
}

#[test]
fn i_structure_500_nested_arrays_needs_a_raised_limit() {
    let src = format!("{}{}", "[".repeat(500), "]".repeat(500));
    let err = parse(&src).unwrap_err();
    assert_eq!(err.kind(), &ParseErrorKind::DepthLimitExceeded(128));
    assert_eq!(err.column(), 129);
    let value = parse_with(&src, &ParseOptions::strict().max_depth(500)).unwrap();
    assert_eq!(to_string(&value), src);
}

#[test]
fn hostile_nesting_fails_fast_without_overflowing_the_stack() {
    let arrays = "[".repeat(100_000);
    assert_eq!(
        parse(&arrays).unwrap_err().kind(),
        &ParseErrorKind::DepthLimitExceeded(128)
    );
    let objects = r#"{"a":"#.repeat(50_000);
    assert_eq!(
        parse(&objects).unwrap_err().kind(),
        &ParseErrorKind::DepthLimitExceeded(128)
    );
    let mixed = r#"[{"":"#.repeat(50_000);
    assert_eq!(
        parse(&mixed).unwrap_err().kind(),
        &ParseErrorKind::DepthLimitExceeded(128)
    );
}

#[test]
fn error_messages_locate_the_problem() {
    let message = |src: &str| parse(src).unwrap_err().to_string();
    assert_eq!(
        message("[\"a\",\n4\n,1,"),
        "unexpected end of input, expected a value at line 3, column 4"
    );
    assert_eq!(
        message("[1 true]"),
        "unexpected character 't', expected ',' or ']' at line 1, column 4"
    );
    assert_eq!(
        message(r#"{"a" "b"}"#),
        "unexpected character '\"', expected ':' after object key at line 1, column 6"
    );
    assert_eq!(
        message("{1:1}"),
        "unexpected character '1', expected a string key at line 1, column 2"
    );
    assert_eq!(
        message("[tru]"),
        "invalid literal, expected `true` at line 1, column 2"
    );
    assert_eq!(
        message("[\"\t\"]"),
        "unescaped control character U+0009 in string at line 1, column 3"
    );
    assert_eq!(
        message("[1]x"),
        "unexpected characters after the top-level value at line 1, column 4"
    );
    assert_eq!(
        message(""),
        "unexpected end of input, expected a value at line 1, column 1"
    );
    assert_eq!(
        message("[\u{e9}]"),
        "unexpected character '\u{e9}', expected a value at line 1, column 2"
    );
}
