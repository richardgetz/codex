use super::parse_arguments_with_integral_float_fallback;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, PartialEq)]
struct NumericArgs {
    duration_ms: u64,
    values: Vec<i32>,
}

#[test]
fn accepts_safe_integral_float_arguments_for_integer_fields() {
    let parsed = parse_arguments_with_integral_float_fallback::<NumericArgs>(
        r#"{"duration_ms":60000.0,"values":[1,3]}"#,
    )
    .expect("integral numeric arguments should parse");

    assert_eq!(
        parsed,
        NumericArgs {
            duration_ms: 60_000,
            values: vec![1, 3],
        }
    );

    let parsed = parse_arguments_with_integral_float_fallback::<ValueWithDuration>(
        r#"{"duration_ms":60000.0}"#,
    )
    .expect("integral numeric argument should parse");
    assert_eq!(parsed.duration_ms, 60_000);
}

#[test]
fn preserves_decimal_values_when_value_is_requested() {
    let parsed =
        parse_arguments_with_integral_float_fallback::<Value>(r#"{"duration_ms":60000.0}"#)
            .expect("Value arguments should use the original JSON representation");

    assert_eq!(parsed, serde_json::json!({"duration_ms": 60000.0}));
}

#[test]
fn preserves_value_fields_during_typed_fallback() {
    let parsed = parse_arguments_with_integral_float_fallback::<ValueAndDuration>(
        r#"{"duration_ms":60000.0,"payload":{"duration_ms":60000.0}}"#,
    )
    .expect("integral typed field should parse");

    assert_eq!(
        parsed,
        ValueAndDuration {
            duration_ms: 60_000,
            payload: serde_json::json!({"duration_ms": 60000.0}),
        }
    );
}

#[test]
fn rejects_fractional_and_unsafe_integer_arguments() {
    for arguments in [
        r#"{"duration_ms":1.5}"#,
        r#"{"duration_ms":9007199254740992.5}"#,
        r#"{"duration_ms":9007199254740993.0}"#,
        r#"{"duration_ms":18446744073709551616.0}"#,
        r#"{"duration_ms":1e1000000}"#,
    ] {
        assert!(
            parse_arguments_with_integral_float_fallback::<ValueWithDuration>(arguments).is_err(),
            "unexpectedly accepted invalid duration: {arguments}"
        );
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct ValueWithDuration {
    duration_ms: u64,
}

#[derive(Debug, Deserialize, PartialEq)]
struct ValueAndDuration {
    duration_ms: u64,
    payload: Value,
}
