/// Extract top-level keys from a JSON object without full parsing.
///
/// Performs an O(n) scan tracking brace/bracket depth, capturing only
/// keys at depth 1 (the root object). Skips over string values and
/// nested structures efficiently without parsing them.
///
/// Returns an empty vec for non-object inputs or malformed JSON.
pub fn extract_json_top_level_keys(content: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut depth: u32 = 0;
    let mut expecting_key = false;

    // Scan for opening '{'
    while i < len {
        match bytes[i] {
            b'{' => {
                depth = 1;
                expecting_key = true;
                i += 1;
                break;
            }
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            // Not a JSON object
            _ => return keys,
        }
    }

    while i < len && depth > 0 {
        match bytes[i] {
            b'"' => {
                i += 1;
                let start = i;
                // Scan to end of string
                while i < len {
                    match bytes[i] {
                        b'\\' => {
                            i += 2; // skip escaped char
                        }
                        b'"' => break,
                        _ => {
                            i += 1;
                        }
                    }
                }
                if depth == 1 && expecting_key {
                    if let Ok(key) = std::str::from_utf8(&bytes[start..i]) {
                        keys.push(key.to_string());
                    }
                    expecting_key = false;
                }
                if i < len {
                    i += 1; // skip closing '"'
                }
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
                i += 1;
            }
            b',' if depth == 1 => {
                expecting_key = true;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    keys
}

/// Extract keys from a JSON object at a specific depth without full parsing.
///
/// Like `extract_json_top_level_keys` but captures keys when `depth == target_depth`.
/// Depth 1 = top-level keys (same as `extract_json_top_level_keys`).
/// Depth 2 = keys inside top-level object values, etc.
///
/// Returns an empty vec for non-object inputs, malformed JSON, or if the
/// target depth is never reached.
pub fn extract_json_keys_at_depth(content: &str, target_depth: u32) -> Vec<String> {
    if target_depth == 0 {
        return Vec::new();
    }

    let mut keys = Vec::new();
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut depth: u32 = 0;
    // Track whether we're expecting a key at each depth.
    // A depth is "expecting key" after '{' or ',' at that depth.
    // We use a simple stack approach: expecting_key is true when we just
    // entered an object or saw a comma at the target depth.
    let mut expecting_key_at_depth = Vec::<bool>::new();

    while i < len {
        match bytes[i] {
            b'"' => {
                i += 1;
                let start = i;
                // Scan to end of string
                while i < len {
                    match bytes[i] {
                        b'\\' => { i += 2; }
                        b'"' => break,
                        _ => { i += 1; }
                    }
                }
                if depth == target_depth {
                    let expecting = expecting_key_at_depth.last().copied().unwrap_or(false);
                    if expecting {
                        if let Ok(key) = std::str::from_utf8(&bytes[start..i]) {
                            keys.push(key.to_string());
                        }
                        if let Some(last) = expecting_key_at_depth.last_mut() {
                            *last = false;
                        }
                    }
                }
                if i < len {
                    i += 1; // skip closing '"'
                }
            }
            b'{' => {
                depth += 1;
                expecting_key_at_depth.push(true);
                i += 1;
            }
            b'[' => {
                depth += 1;
                expecting_key_at_depth.push(false); // arrays don't have keys
                i += 1;
            }
            b'}' | b']' => {
                if depth > 0 {
                    depth -= 1;
                    expecting_key_at_depth.pop();
                }
                i += 1;
            }
            b',' => {
                if depth == target_depth {
                    if let Some(last) = expecting_key_at_depth.last_mut() {
                        *last = true;
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_object() {
        assert!(extract_json_top_level_keys("{}").is_empty());
    }

    #[test]
    fn simple_keys() {
        let json = r#"{"alpha": 1, "beta": "hello", "gamma": true}"#;
        let keys = extract_json_top_level_keys(json);
        assert_eq!(keys, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn nested_keys_ignored() {
        let json = r#"{"top": {"nested": 1, "deep": {"deeper": 2}}, "other": []}"#;
        let keys = extract_json_top_level_keys(json);
        assert_eq!(keys, vec!["top", "other"]);
    }

    #[test]
    fn keys_after_large_nested_value() {
        // Simulates a Playwright report where "config" has a large value
        // pushing "suites" and "stats" past any byte sample boundary
        let big_value = "x".repeat(5000);
        let json = format!(
            r#"{{"config": "{}", "suites": [], "stats": {{}}}}"#,
            big_value
        );
        let keys = extract_json_top_level_keys(&json);
        assert_eq!(keys, vec!["config", "suites", "stats"]);
    }

    #[test]
    fn escaped_quotes_in_strings() {
        let json = r#"{"key\"with": 1, "normal": 2}"#;
        let keys = extract_json_top_level_keys(json);
        assert_eq!(keys, vec![r#"key\"with"#, "normal"]);
    }

    #[test]
    fn not_an_object() {
        assert!(extract_json_top_level_keys("[1, 2, 3]").is_empty());
        assert!(extract_json_top_level_keys("null").is_empty());
        assert!(extract_json_top_level_keys("").is_empty());
    }

    #[test]
    fn whitespace_before_object() {
        let json = "  \n\t {\"a\": 1}";
        let keys = extract_json_top_level_keys(json);
        assert_eq!(keys, vec!["a"]);
    }

    #[test]
    fn playwright_json_shape() {
        let json = r#"{"config": {"projects": []}, "suites": [{"specs": []}], "errors": [], "stats": {"startTime": "2024-01-01", "duration": 2000, "expected": 1, "unexpected": 0, "flaky": 0, "skipped": 0}}"#;
        let keys = extract_json_top_level_keys(json);
        assert!(keys.contains(&"config".to_string()));
        assert!(keys.contains(&"suites".to_string()));
        assert!(keys.contains(&"stats".to_string()));
    }

    #[test]
    fn jest_vitest_json_shape() {
        let json = r#"{"numTotalTests": 5, "numPassedTests": 4, "numFailedTests": 1, "numPendingTests": 0, "testResults": [], "success": false, "startTime": 1700000000000}"#;
        let keys = extract_json_top_level_keys(json);
        assert!(keys.contains(&"numTotalTests".to_string()));
        assert!(keys.contains(&"testResults".to_string()));
        assert!(keys.contains(&"success".to_string()));
    }

    #[test]
    fn nested_string_with_key_like_content() {
        // A nested string value that looks like a key should not be extracted
        let json = r#"{"data": {"suites": "not a top key"}, "real": 1}"#;
        let keys = extract_json_top_level_keys(json);
        assert_eq!(keys, vec!["data", "real"]);
        assert!(!keys.contains(&"suites".to_string()));
    }

    // --- extract_json_keys_at_depth tests ---

    #[test]
    fn depth1_matches_top_level() {
        let json = r#"{"alpha": 1, "beta": "hello"}"#;
        let keys = extract_json_keys_at_depth(json, 1);
        assert_eq!(keys, vec!["alpha", "beta"]);
    }

    #[test]
    fn depth2_extracts_nested_keys() {
        let json = r#"{"results": {"tool": {}, "summary": {}, "tests": []}}"#;
        let keys = extract_json_keys_at_depth(json, 2);
        assert_eq!(keys, vec!["tool", "summary", "tests"]);
    }

    #[test]
    fn depth2_ctrf_shaped_input() {
        let json = r#"{"results": {"tool": {"name": "vitest"}, "summary": {"tests": 3}, "tests": [{"name": "test1"}]}, "reportFormat": "CTRF"}"#;
        let keys = extract_json_keys_at_depth(json, 2);
        assert!(keys.contains(&"tool".to_string()));
        assert!(keys.contains(&"summary".to_string()));
        assert!(keys.contains(&"tests".to_string()));
        // depth-3 keys should NOT appear
        assert!(!keys.contains(&"name".to_string()));
    }

    #[test]
    fn depth2_multiple_top_level_objects() {
        let json = r#"{"a": {"x": 1, "y": 2}, "b": {"z": 3}}"#;
        let keys = extract_json_keys_at_depth(json, 2);
        assert_eq!(keys, vec!["x", "y", "z"]);
    }

    #[test]
    fn depth0_returns_empty() {
        let json = r#"{"a": 1}"#;
        assert!(extract_json_keys_at_depth(json, 0).is_empty());
    }

    #[test]
    fn depth3_goes_deeper() {
        let json = r#"{"a": {"b": {"c": 1, "d": 2}}}"#;
        let keys = extract_json_keys_at_depth(json, 3);
        assert_eq!(keys, vec!["c", "d"]);
    }

    #[test]
    fn depth_with_arrays_skipped() {
        // Arrays at depth 2 should not produce keys
        let json = r#"{"items": [{"name": "x"}, {"name": "y"}]}"#;
        let keys = extract_json_keys_at_depth(json, 2);
        // "name" is at depth 3 (inside array elements which are objects)
        assert!(keys.is_empty());
    }

    #[test]
    fn depth_unreachable_returns_empty() {
        let json = r#"{"a": 1}"#;
        assert!(extract_json_keys_at_depth(json, 5).is_empty());
    }
}
