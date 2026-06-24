//! Turning a model's raw completion into something safe to act on: repairing the
//! JSON arguments of a tool call clipped by an output cap, and clamping
//! Cratchit's free-text report (and tool-arg log previews) to bounded sizes.

use serde_json::Value;

/// Hard caps enforcing rule 7 (the ≤6-line report) in code: Cratchit's final
/// message is clamped before anyone expensive reads it.
pub(super) const MAX_REPORT_LINES: usize = 12;
pub(super) const MAX_REPORT_CHARS: usize = 1200;

/// Clamp a Cratchit report to the size rule 7 promises.
pub(super) fn clamp_report(s: &str) -> String {
    let mut out = s
        .lines()
        .take(MAX_REPORT_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if out.len() > MAX_REPORT_CHARS {
        out.truncate(crate::util::floor_char_boundary(&out, MAX_REPORT_CHARS));
        out.push_str("\n[report clamped]");
    } else if s.lines().count() > MAX_REPORT_LINES {
        out.push_str("\n[report clamped]");
    }
    out
}

/// Clamp a direct Scrooge `read_file` result to a peek (`max_lines`). When it
/// overflows, the surplus is dropped and Scrooge is reminded his time is too
/// valuable to page through source — `symbol_info` / `read_symbol` are cheaper.
pub(super) fn clamp_scrooge_read(out: &str, max_lines: usize) -> String {
    let total = out.lines().count();
    if total <= max_lines {
        return out.to_string();
    }
    let head: String = out.lines().take(max_lines).collect::<Vec<_>>().join("\n");
    format!(
        "{head}\n[read_file clamped to {max_lines} of {total} lines — your time is \
         valuable: use symbol_info to locate what you need, or read_symbol if you \
         really must read it, instead of paging through the whole file]"
    )
}

/// Short stderr preview of tool-call arguments — a full `write_file` body
/// would make the log unreadable.
pub(super) fn arg_preview(args: &str) -> String {
    const MAX: usize = 200;
    if args.len() <= MAX {
        return args.to_string();
    }
    format!(
        "{}… [{} chars]",
        &args[..crate::util::floor_char_boundary(args, MAX)],
        args.len()
    )
}

/// Parse tool-call arguments. A well-formed object always parses. When the
/// completion was cut short by an output cap (`finish_reason: length`, passed as
/// `truncated`), the arguments can be clipped mid-value; rather than spend
/// another turn we close whatever was left open and run with the surviving
/// (truncated) instruction. When the response was NOT truncated, a parse failure
/// is genuine malformed JSON, not a clip — we do not guess, returning `Null` so
/// the caller can reject the call instead of acting on a blind repair.
pub(super) fn parse_tool_args(raw: &str, truncated: bool) -> Value {
    if let Ok(v) = serde_json::from_str(raw) {
        return v;
    }
    if truncated && let Ok(v) = serde_json::from_str(&close_truncated_json(raw)) {
        return v;
    }
    Value::Null
}

/// Best-effort close of a JSON fragment cut off at the end: balance an open
/// string, drop a now-dangling separator, then close every still-open `{`/`[`
/// in reverse. Good enough for the dominant case — a long string value clipped
/// mid-content — which is exactly where an output cap lands.
pub(super) fn close_truncated_json(raw: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_str = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_str {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = raw.to_string();
    if escaped {
        out.pop(); // a trailing backslash would escape our closing quote
    }
    if in_str {
        out.push('"');
    } else {
        // Cut between elements: drop a dangling comma so the close is valid.
        while out.ends_with(char::is_whitespace) {
            out.pop();
        }
        if out.ends_with(',') {
            out.pop();
        }
    }
    while let Some(close) = stack.pop() {
        out.push(close);
    }
    out
}
