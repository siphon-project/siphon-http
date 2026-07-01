//! Pure request-parsing helpers: path-param extraction, query parsing, and a
//! small percent-decoder. Kept free of pyo3/axum types so they're unit-testable
//! and benchable on their own (see `tests/parse.rs`, `benches/parse.rs`).

use std::collections::HashMap;

/// Re-pair a route template against an actual path, extracting path params.
///
/// Handles axum 0.8 syntax: `{name}` (single segment) and `{*name}`
/// (catch-all, eats the remaining segments). Values are URL-decoded.
///
/// ```
/// # use siphon_http::parse::extract_path_params;
/// let p = extract_path_params("/users/{id}/profile", "/users/42/profile");
/// assert_eq!(p.get("id").map(String::as_str), Some("42"));
/// ```
pub fn extract_path_params(template: &str, actual: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let t_segs: Vec<&str> = template.trim_start_matches('/').split('/').collect();
    let a_segs: Vec<&str> = actual.trim_start_matches('/').split('/').collect();
    for (i, t) in t_segs.iter().enumerate() {
        if t.starts_with("{*") && t.ends_with('}') {
            let name = &t[2..t.len() - 1];
            let rest = a_segs.get(i..).map(|s| s.join("/")).unwrap_or_default();
            out.insert(name.to_string(), urldecode(&rest));
            break;
        } else if t.starts_with('{') && t.ends_with('}') {
            let name = &t[1..t.len() - 1];
            let name = name.split_once(':').map(|(n, _)| n).unwrap_or(name);
            if let Some(a) = a_segs.get(i) {
                out.insert(name.to_string(), urldecode(a));
            }
        }
    }
    out
}

/// Parse an `a=1&b=2` query string into a map. Bare keys map to `""`.
pub fn parse_query(q: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if q.is_empty() {
        return out;
    }
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(urldecode(k), urldecode(v));
        } else {
            out.insert(urldecode(pair), String::new());
        }
    }
    out
}

/// Tiny percent-decoder (`%XX` → byte, `+` → space). The URL crate would be
/// overkill given the input is already a partial (path segment or query value).
pub fn urldecode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte as char);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
