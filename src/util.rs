//! Small shared text/argument helpers used across modules, so the
//! char-boundary backoff and tail-truncation logic live in one place instead
//! of being re-derived in tools.rs, checks.rs and agents.rs.

use serde_json::Value;

/// Largest char boundary <= `i`, so byte-budget truncation never splits a
/// multibyte UTF-8 sequence (tool output and reports are full of `—`, `’`, …).
pub const fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary >= `i`, clamped to `s.len()`.
pub const fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    if i < s.len() { i } else { s.len() }
}

/// Keep the last `max_chars` of `s` (trimmed), prefixed with a marker and
/// snapped forward to a line boundary — failure summaries and error dumps put
/// what matters at the end, so a head-only cut would discard it.
pub fn tail(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.len() <= max_chars {
        return s.to_string();
    }
    let cut = floor_char_boundary(s, s.len() - max_chars);
    let cut = s[cut..].find('\n').map_or(cut, |i| cut + i + 1);
    format!("[...]\n{}", &s[cut..])
}

/// `args[key]` as an owned string, empty when absent or non-string — the
/// one-liner every tool dispatcher needs to pull a string parameter.
pub fn str_arg(args: &Value, key: &str) -> String {
    args[key].as_str().unwrap_or("").to_string()
}
