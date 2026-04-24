//! Input-validation helpers for JSON payloads received over the network.
//!
//! [`validate_and_parse_json`] enforces two denial-of-service caps
//! *before* handing the bytes to `serde_json`:
//!
//! * a hard limit on the raw payload size;
//! * a hard limit on JSON nesting depth (counts objects and arrays
//!   against the same budget).
//!
//! The depth scan walks the bytes once, tracking whether the cursor
//! is inside a string literal so that brackets in strings are
//! ignored, and short-circuits the moment the running depth exceeds
//! the configured cap.
//!
//! All failures — oversize body, malformed structure, overflowing
//! depth, `serde_json` errors — surface as
//! [`UtilsError::Internal`](crate::error::UtilsError::Internal) reports
//! with an attached message identifying the specific failure mode.

use error_stack::{Report, ResultExt};
use serde::de::DeserializeOwned;

use crate::error::{UtilsError, UtilsResult};

/// Validate JSON structure: bracket depth stays within `max_depth`,
/// every opening bracket has a match, and string literals are closed.
///
/// Short-circuits with an `Err` report the moment the running depth
/// exceeds `max_depth`. String tracking uses a forward-only
/// `escaped` flag so the scan is O(n) regardless of how many
/// backslashes appear inside a string.
///
/// Depth is shared across `{` and `[`: `[{"k":[]}]` reaches depth 3.
fn validate_json_structure(data: &[u8], max_depth: usize) -> UtilsResult<()> {
    let mut current_depth: usize = 0;
    let mut inside_string = false;
    let mut escaped = false;

    for (i, &byte) in data.iter().enumerate() {
        // 1-based for human-readable error messages.
        let position = i + 1;

        if inside_string {
            // Toggling `escaped` on every `\` handles the `\\` case
            // correctly: the second backslash resets the flag so a
            // following `"` is treated as string-terminating.
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                inside_string = false;
            }
            continue;
        }

        match byte {
            b'"' => inside_string = true,
            b'{' | b'[' => {
                current_depth += 1;
                if current_depth > max_depth {
                    return Err(Report::new(UtilsError::Internal).attach_printable(format!(
                        "JSON depth limit exceeded at position {position}: depth {current_depth}, max {max_depth}"
                    )));
                }
            }
            b'}' | b']' => {
                if current_depth == 0 {
                    return Err(Report::new(UtilsError::Internal).attach_printable(format!(
                        "invalid JSON: unmatched closing bracket at position {position}"
                    )));
                }
                current_depth -= 1;
            }
            _ => {}
        }
    }

    // Order matters: an unterminated string swallows the rest of the
    // input, which means `current_depth` is also still non-zero.
    // Reporting the string issue first surfaces the actual root cause
    // instead of the downstream "unmatched bracket" symptom.
    if inside_string {
        return Err(Report::new(UtilsError::Internal)
            .attach_printable("invalid JSON: unterminated string literal"));
    }

    if current_depth != 0 {
        return Err(Report::new(UtilsError::Internal).attach_printable(format!(
            "invalid JSON: {current_depth} unmatched opening bracket(s)"
        )));
    }

    Ok(())
}

/// Validate size and nesting depth, then deserialize `data` as JSON
/// into `T`.
///
/// # Arguments
/// * `max_request_body_size` — hard cap on `data.len()` in bytes.
///   Payloads above this are rejected before any parsing.
/// * `max_json_depth` — maximum accepted JSON nesting depth.
///
/// # Errors
/// Returns an [`UtilsError::Internal`] report when the body is too
/// large, the structure is malformed, it nests deeper than allowed,
/// or `serde_json` cannot deserialize it into `T`. The specific
/// failure mode is encoded in the attached `attach_printable` message.
pub fn validate_and_parse_json<T>(
    data: &[u8],
    max_request_body_size: usize,
    max_json_depth: usize,
) -> UtilsResult<T>
where
    T: DeserializeOwned,
{
    if data.len() > max_request_body_size {
        return Err(Report::new(UtilsError::Internal).attach_printable(format!(
            "request body too large: {} bytes (max: {max_request_body_size})",
            data.len()
        )));
    }

    validate_json_structure(data, max_json_depth)?;

    serde_json::from_slice(data)
        .change_context(UtilsError::Internal)
        .attach_printable("JSON deserialization failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn parses_valid_payload_within_limits() {
        let data = br#"{"a": 1, "b": [2, 3]}"#;
        let v: Value = validate_and_parse_json(data, 1024, 8).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], serde_json::json!([2, 3]));
    }

    #[test]
    fn rejects_oversized_payload_before_parsing() {
        let data = br#"{}"#;
        let err = validate_and_parse_json::<Value>(data, 1, 8).unwrap_err();
        assert!(format!("{err:?}").contains("too large"));
    }

    #[test]
    fn rejects_depth_over_limit() {
        // 5 levels of nesting, cap at 3.
        let data = br#"[[[[[]]]]]"#;
        let err = validate_and_parse_json::<Value>(data, 1024, 3).unwrap_err();
        assert!(format!("{err:?}").contains("depth limit exceeded"));
    }

    #[test]
    fn allows_depth_exactly_at_the_limit() {
        // 3 levels of nesting, cap at 3.
        let data = br#"[[[1]]]"#;
        let _: Value = validate_and_parse_json(data, 1024, 3).unwrap();
    }

    #[test]
    fn rejects_unmatched_closing_bracket() {
        let data = br#"{}}"#;
        let err = validate_and_parse_json::<Value>(data, 1024, 8).unwrap_err();
        assert!(format!("{err:?}").contains("unmatched closing bracket"));
    }

    #[test]
    fn rejects_unmatched_opening_brackets() {
        let data = br#"{"k": [1, 2"#;
        let err = validate_and_parse_json::<Value>(data, 1024, 8).unwrap_err();
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("unmatched opening bracket"),
            "unexpected report: {rendered}"
        );
    }

    #[test]
    fn rejects_unterminated_string() {
        let data = br#"{"k": "oops"#;
        let err = validate_and_parse_json::<Value>(data, 1024, 8).unwrap_err();
        assert!(format!("{err:?}").contains("unterminated"));
    }

    #[test]
    fn escaped_quote_does_not_end_the_string() {
        // `"a\"b"` contains an escaped quote; depth stays at 1.
        let data = br#"{"k": "a\"b", "nested": 1}"#;
        let _: Value = validate_and_parse_json(data, 1024, 1).unwrap();
    }

    #[test]
    fn escaped_backslash_lets_next_quote_close_the_string() {
        // `"ab\\"` ends the string with a real closing quote
        // (the `\\` is an escaped backslash, not an escape for `"`).
        let data = br#"{"k": "ab\\"}"#;
        let _: Value = validate_and_parse_json(data, 1024, 1).unwrap();
    }

    #[test]
    fn brackets_inside_strings_do_not_count_toward_depth() {
        // Max structural depth is 1 even though the string contains
        // many `[` characters.
        let data = br#"{"k": "[[[[[[[[[[[[[[[[[[[[[[[[[[]"}"#;
        let _: Value = validate_and_parse_json(data, 1024, 1).unwrap();
    }

    #[test]
    fn propagates_serde_json_parse_error() {
        // Syntactically valid structure but an invalid literal.
        let data = br#"{"k": truu}"#;
        let err = validate_and_parse_json::<Value>(data, 1024, 8).unwrap_err();
        assert!(format!("{err:?}").contains("JSON deserialization failed"));
    }
}
