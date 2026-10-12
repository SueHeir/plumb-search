//! The canonical JSON a signature covers: RFC 8785 (JCS).
//!
//! The same value always gives the same bytes, whoever wrote it and in
//! whatever order its keys came: no whitespace, object keys sorted by their
//! UTF-16 code units, strings escaped as JavaScript's `JSON.stringify`
//! does, and numbers written as JavaScript writes them (`1` not `1.0`,
//! `1e+21` not `1000000000000000000000`).

use anyhow::{ensure, Result};
use serde_json::{Number, Value};

/// `value` as RFC 8785 canonical JSON.
///
/// ```
/// use plumb_format::canonical::canonical_json;
/// let value = serde_json::json!({"b": [1.0, true], "a": "é\n"});
/// assert_eq!(canonical_json(&value).unwrap(), r#"{"a":"é\n","b":[1,true]}"#);
/// ```
pub fn canonical_json(value: &Value) -> Result<String> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Ok(out)
}

fn write_value(out: &mut String, value: &Value) -> Result<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)?),
        Value::String(s) => out.push_str(&serde_json::to_string(s)?),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key)?);
                out.push(':');
                write_value(out, &map[key])?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// A number as ECMAScript's `Number.prototype.toString` writes the double
/// closest to it.
fn number(n: &Number) -> Result<String> {
    let x = n.as_f64().unwrap_or(f64::NAN);
    ensure!(x.is_finite(), "{n} is not a finite number");
    if x == 0.0 {
        return Ok("0".to_string());
    }
    let magnitude = x.abs();
    if (1e-6..1e21).contains(&magnitude) {
        // Rust writes the shortest digits that round-trip, in plain
        // notation, as ECMAScript does in this range.
        return Ok(format!("{x}"));
    }
    // Outside it ECMAScript uses an exponent, with a sign: 1e+21, 1.5e-7.
    let written = format!("{x:e}");
    Ok(match written.split_once('e') {
        Some((digits, exponent)) if !exponent.starts_with('-') => format!("{digits}e+{exponent}"),
        _ => written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_as_javascript_writes_them() {
        let cases = [
            (json!(0), "0"),
            (json!(-0.0), "0"),
            (json!(1.0), "1"),
            (json!(-1.5), "-1.5"),
            (json!(0.1), "0.1"),
            (json!(123456789012345680000.0), "123456789012345680000"),
            (json!(1e21), "1e+21"),
            (json!(1.5e-7), "1.5e-7"),
            (json!(0.000001), "0.000001"),
            (json!(4.5), "4.5"),
            (json!(9007199254740993u64), "9007199254740992"),
            (json!(1e300), "1e+300"),
            (json!(5e-324), "5e-324"),
        ];
        for (value, want) in cases {
            assert_eq!(canonical_json(&value).unwrap(), want, "{value}");
        }
    }

    #[test]
    fn keys_sort_by_utf16() {
        // U+1F600 sorts after U+FF61 in UTF-8 but before it in UTF-16.
        let value = json!({"\u{FF61}": 2, "\u{1F600}": 1, "b": 3, "a": {"z": 1, "y": 2}});
        assert_eq!(
            canonical_json(&value).unwrap(),
            "{\"a\":{\"y\":2,\"z\":1},\"b\":3,\"\u{1F600}\":1,\"\u{FF61}\":2}"
        );
    }

    #[test]
    fn strings_escape_as_json_stringify() {
        let value = json!(["\u{1}\u{8}\u{c}\"\\/€\u{7f}"]);
        assert_eq!(
            canonical_json(&value).unwrap(),
            "[\"\\u0001\\b\\f\\\"\\\\/€\u{7f}\"]"
        );
    }

    #[test]
    fn same_value_same_bytes() {
        let a: Value = serde_json::from_str(r#"{ "x": [1, 2.50], "set": "plumb/a" }"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"set":"plumb/a","x":[1.0,2.5]}"#).unwrap();
        assert_eq!(canonical_json(&a).unwrap(), canonical_json(&b).unwrap());
    }
}
