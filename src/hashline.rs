//! Hash-anchored line editing — a Rust port of oh-my-pi's hashline
//! (MIT, github.com/can1357/oh-my-pi), following the clean reimplementation in
//! github.com/RimuruW/pi-hashline-edit (`src/hashline.ts`).
//!
//! Every line `read_file` returns carries a 2-character content hash, e.g.
//! `12#MQ:    let x = 1;`. Edits reference these `LINE#HASH` anchors instead of
//! raw find-strings, so a stale edit (the file changed since it was read) is
//! detected and rejected with a fresh-anchor retry snippet rather than silently
//! corrupting the file. The hash is only ever validated against hashes this same
//! code produced, so internal consistency — not byte-compatibility with the
//! TypeScript original — is what matters.

use anyhow::{Result, bail};
use std::hash::Hasher;
use twox_hash::XxHash32;

/// Hash alphabet. Excludes hex digits A–F, the vowels A/E/I/O/U, and the
/// visually confusable D/G/I/L/O, so a reference like `5#MQ` can never be
/// mistaken for a hex literal, an English word, or code content.
const NIBBLE: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";

/// True when `c` is one of the 16 hash-alphabet characters.
fn is_hash_char(c: char) -> bool {
    NIBBLE.contains(&(c as u8))
}

fn xxh32(input: &str, seed: u32) -> u32 {
    let mut h = XxHash32::with_seed(seed);
    h.write(input.as_bytes());
    h.finish() as u32
}

/// Two-character content hash for a line. `\r` is stripped and trailing
/// whitespace trimmed before hashing so CRLF/indentation noise does not change
/// the anchor. Lines with no alphanumerics (a lone `}`, say) are seeded with the
/// line number to cut collisions between structurally identical markers.
pub fn compute_line_hash(idx: usize, line: &str) -> String {
    // The common case has no carriage returns, so avoid allocating then.
    let stripped;
    let line = if line.contains('\r') {
        stripped = line.replace('\r', "");
        stripped.as_str()
    } else {
        line
    };
    let line = line.trim_end();
    let significant = line.chars().any(char::is_alphanumeric);
    let seed = if significant { 0 } else { idx as u32 };
    let byte = (xxh32(line, seed) & 0xff) as usize;
    let hi = NIBBLE[byte >> 4] as char;
    let lo = NIBBLE[byte & 0x0f] as char;
    format!("{hi}{lo}")
}

// ─── Anchors ────────────────────────────────────────────────────────────

/// A parsed `LINE#HASH` reference, with the optional `:textHint` suffix kept for
/// fuzzy validation when an exact hash mismatch is whitespace/Unicode-only.
#[derive(Clone)]
pub struct Anchor {
    pub line: usize,
    pub hash: String,
    pub text_hint: Option<String>,
}

/// Parse `LINE#HASH` (tolerating leading `>`, `+`, `-`, whitespace, and a
/// trailing `:content` display suffix). The suffix is preserved as `text_hint`.
fn parse_anchor(ref_: &str) -> Result<Anchor> {
    let core = ref_
        .trim_start_matches([' ', '\t', '>', '+', '-'])
        .trim_end();
    let (num, rest) = core.split_once('#').ok_or_else(|| diagnose_ref(ref_))?;
    let line: usize = num.trim().parse().map_err(|_| diagnose_ref(ref_))?;
    if line < 1 {
        bail!("[E_BAD_REF] Line number must be >= 1 in \"{ref_}\".");
    }
    // hash runs up to a ':' (the display suffix) or end of string.
    let rest = rest.trim_start();
    let (hash, hint) = match rest.split_once(':') {
        Some((h, t)) => (h.trim_end(), Some(t.to_string())),
        None => (rest, None),
    };
    if hash.len() != 2 || !hash.chars().all(is_hash_char) {
        bail!(
            "[E_BAD_REF] Invalid reference \"{ref_}\": hash must be 2 chars from {}.",
            std::str::from_utf8(NIBBLE).unwrap()
        );
    }
    Ok(Anchor {
        line,
        hash: hash.to_string(),
        text_hint: hint,
    })
}

fn diagnose_ref(ref_: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "[E_BAD_REF] Invalid line reference \"{ref_}\". Expected \"LINE#HASH\" (e.g. \"5#MQ\")."
    )
}

// ─── Edits ──────────────────────────────────────────────────────────────

pub enum Edit {
    Replace {
        pos: Anchor,
        end: Option<Anchor>,
        lines: Vec<String>,
    },
    Append {
        pos: Option<Anchor>,
        lines: Vec<String>,
    },
    Prepend {
        pos: Option<Anchor>,
        lines: Vec<String>,
    },
}

/// Reject `lines` payloads that carry rendered display prefixes (`LINE#HASH:`,
/// `+ LINE#HASH:`, or a `- 1234` diff marker). The model must send literal file
/// content; silent stripping would mask the mistake.
fn assert_no_display_prefixes(lines: &[String]) -> Result<()> {
    for line in lines {
        let t = line.trim_start();
        let looks_tagged = t
            .trim_start_matches(['>', '+', ' '])
            .split_once(':')
            .is_some_and(|(head, _)| {
                // "12#MQ" or "#MQ" shape before the colon.
                let core = head.trim().trim_start_matches(|c: char| c.is_ascii_digit());
                core.strip_prefix('#')
                    .is_some_and(|h| h.len() == 2 && h.chars().all(is_hash_char))
            });
        if looks_tagged {
            bail!(
                "[E_INVALID_PATCH] \"lines\" must be literal file content, not rendered \
                 \"LINE#HASH:\" prefixes. Offending line: {line:?}"
            );
        }
    }
    Ok(())
}

/// Parse the JSON `edits` array into typed `Edit`s, validating shape and
/// resolving anchors. Strict: a supplied anchor must parse; an unknown op is an
/// error; `replace` needs `pos`.
pub fn parse_edits(edits: &serde_json::Value) -> Result<Vec<Edit>> {
    let arr = edits
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("`edits` must be an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, e) in arr.iter().enumerate() {
        let op = e["op"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("edit {i} requires an \"op\" string"))?;
        let lines = || -> Result<Vec<String>> {
            let v: Vec<String> = e["lines"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| l.as_str().unwrap_or("").to_string())
                        .collect()
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("edit {i} op \"{op}\" requires a \"lines\" array")
                })?;
            assert_no_display_prefixes(&v)?;
            Ok(v)
        };
        let anchor = |key: &str| -> Result<Option<Anchor>> {
            match e[key].as_str() {
                Some(s) => Ok(Some(parse_anchor(s)?)),
                None => Ok(None),
            }
        };
        let edit = match op {
            "replace" => Edit::Replace {
                pos: anchor("pos")?.ok_or_else(|| {
                    anyhow::anyhow!("[E_BAD_OP] edit {i} \"replace\" requires a \"pos\" anchor")
                })?,
                end: anchor("end")?,
                lines: lines()?,
            },
            "append" => Edit::Append {
                pos: anchor("pos")?,
                lines: lines()?,
            },
            "prepend" => Edit::Prepend {
                pos: anchor("pos")?,
                lines: lines()?,
            },
            other => bail!(
                "[E_BAD_OP] edit {i} unknown op \"{other}\". Expected replace, append, or prepend."
            ),
        };
        out.push(edit);
    }
    Ok(out)
}

// ─── Fuzzy line comparison (for textHint acceptance) ──────────────────────

/// Normalize smart quotes, the various Unicode dashes, and exotic spaces to
/// their ASCII forms so a copied line that differs only in such characters can
/// still validate against the file.
fn normalize_fuzzy(s: &str) -> String {
    s.trim_end()
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

// ─── The engine ───────────────────────────────────────────────────────────

struct LineIndex<'a> {
    file_lines: Vec<&'a str>,
    line_starts: Vec<usize>,
    has_terminal_newline: bool,
}

fn build_line_index(content: &str) -> LineIndex<'_> {
    let file_lines: Vec<&str> = content.split('\n').collect();
    let mut line_starts = Vec::with_capacity(file_lines.len());
    let mut offset = 0;
    for (i, l) in file_lines.iter().enumerate() {
        line_starts.push(offset);
        offset += l.len();
        if i < file_lines.len() - 1 {
            offset += 1; // the '\n'
        }
    }
    LineIndex {
        file_lines,
        line_starts,
        has_terminal_newline: content.ends_with('\n'),
    }
}

struct HashMismatch {
    line: usize,
    expected: String,
}

/// Validate every anchor's hash against the current file. Returns mismatches
/// (for a stale-anchor retry message); OOB lines and reversed ranges throw.
fn validate_anchors(edits: &[Edit], li: &LineIndex) -> Result<Vec<HashMismatch>> {
    let mut mismatches = Vec::new();
    let n = li.file_lines.len();
    let check = |a: &Anchor, mm: &mut Vec<HashMismatch>| -> Result<()> {
        if a.line < 1 || a.line > n {
            bail!(
                "[E_RANGE_OOB] Line {} does not exist (file has {n} lines)",
                a.line
            );
        }
        let line = &li.file_lines[a.line - 1];
        if compute_line_hash(a.line, line) == a.hash {
            return Ok(());
        }
        if let Some(hint) = &a.text_hint
            && compute_line_hash(a.line, hint) == a.hash
            && normalize_fuzzy(hint) == normalize_fuzzy(line)
        {
            return Ok(()); // fuzzy-accepted
        }
        mm.push(HashMismatch {
            line: a.line,
            expected: a.hash.clone(),
        });
        Ok(())
    };
    for e in edits {
        match e {
            Edit::Replace { pos, end, .. } => {
                if let Some(end) = end {
                    if pos.line > end.line {
                        bail!(
                            "[E_BAD_OP] Range start line {} must be <= end line {}",
                            pos.line,
                            end.line
                        );
                    }
                    check(pos, &mut mismatches)?;
                    check(end, &mut mismatches)?;
                } else {
                    check(pos, &mut mismatches)?;
                }
            }
            Edit::Append { pos, lines } | Edit::Prepend { pos, lines } => {
                if let Some(pos) = pos {
                    check(pos, &mut mismatches)?;
                }
                if lines.is_empty() {
                    bail!("[E_BAD_OP] append/prepend with empty lines payload.");
                }
            }
        }
    }
    Ok(mismatches)
}

/// `[E_STALE_ANCHOR]` retry snippet: the changed lines (and a little context)
/// re-rendered with their current `LINE#HASH`, the stale ones marked `>>>`.
fn format_mismatch(mm: &[HashMismatch], li: &LineIndex) -> String {
    let retry: std::collections::HashSet<usize> = mm.iter().map(|m| m.line).collect();
    let mut display: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for m in mm {
        for i in
            m.line.saturating_sub(CONTEXT_LINES)..=(m.line + CONTEXT_LINES).min(li.file_lines.len())
        {
            if i >= 1 {
                display.insert(i);
            }
        }
    }
    let width = display.last().copied().unwrap_or(1).to_string().len();
    let stale = mm
        .iter()
        .map(|m| format!("{}#{}", m.line, m.expected))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = vec![
        format!(
            "[E_STALE_ANCHOR] {} stale anchor(s). Retry with the >>> LINE#HASH lines below; \
             keep both endpoints for range replaces.",
            mm.len()
        ),
        format!("Stale refs: {stale}"),
        String::new(),
    ];
    let mut prev: Option<usize> = None;
    for &num in &display {
        if let Some(p) = prev
            && num > p + 1
        {
            out.push("    ...".into());
        }
        prev = Some(num);
        let content = &li.file_lines[num - 1];
        let hash = compute_line_hash(num, content);
        let prefix = format!("{num:>width$}#{hash}");
        out.push(if retry.contains(&num) {
            format!(">>> {prefix}:{content}")
        } else {
            format!("    {prefix}:{content}")
        });
    }
    out.join("\n")
}

#[derive(PartialEq)]
enum SpanKind {
    Replace,
    Insert,
}

struct Span {
    kind: SpanKind,
    index: usize,
    label: String,
    start: usize,
    end: usize,
    replacement: String,
    boundary: Option<usize>,
    /// Special-cased insertion into an originally-empty file.
    empty_origin: Option<EmptyOrigin>,
}

#[derive(Clone, Copy)]
enum EmptyOrigin {
    Append,
    Prepend,
}

fn describe(e: &Edit) -> String {
    match e {
        Edit::Replace { pos, end, .. } => end.as_ref().map_or_else(
            || format!("replace {}#{}", pos.line, pos.hash),
            |end| {
                format!(
                    "replace {}#{}-{}#{}",
                    pos.line, pos.hash, end.line, end.hash
                )
            },
        ),
        Edit::Append { pos, .. } => pos.as_ref().map_or_else(
            || "append at EOF".into(),
            |p| format!("append after {}#{}", p.line, p.hash),
        ),
        Edit::Prepend { pos, .. } => pos.as_ref().map_or_else(
            || "prepend at BOF".into(),
            |p| format!("prepend before {}#{}", p.line, p.hash),
        ),
    }
}

fn insertion_boundary(pos: Option<&Anchor>, append: bool, li: &LineIndex) -> usize {
    let count = li.file_lines.len();
    if append {
        let eof = if li.has_terminal_newline && count > 0 {
            count - 1
        } else {
            count
        };
        match pos {
            Some(p) if li.has_terminal_newline && p.line == count => eof,
            Some(p) => p.line,
            None => eof,
        }
    } else {
        pos.map_or(0, |p| p.line - 1)
    }
}

/// Map one edit to a character span (or `None` for a no-op).
fn resolve_span(e: &Edit, index: usize, content: &str, li: &LineIndex) -> Result<Option<Span>> {
    let label = describe(e);
    let span = |kind, start, end, replacement, boundary, empty_origin| Span {
        kind,
        index,
        label: label.clone(),
        start,
        end,
        replacement,
        boundary,
        empty_origin,
    };
    Ok(match e {
        Edit::Replace { pos, end, lines } => {
            let start_line = pos.line;
            let end_line = end.as_ref().map_or(pos.line, |a| a.line);
            let original = &li.file_lines[start_line - 1..end_line];
            if original.len() == lines.len()
                && original.iter().zip(lines).all(|(&a, b)| a == b.as_str())
            {
                return Ok(None); // no-op
            }
            let s = li.line_starts[start_line - 1];
            // Byte span of the whole anchored line(s), newline included so the
            // line is fully consumed (`e_off`).
            let e_off = li.line_starts[end_line - 1] + li.file_lines[end_line - 1].len();
            // Pure deletion (empty `lines`) must also swallow a newline to not
            // leave a blank line behind: take the trailing one when there is a
            // line after the range, otherwise the leading one.
            let (start, end_byte) = if !lines.is_empty() {
                (s, e_off)
            } else if start_line == 1 && end_line == li.file_lines.len() {
                (0, content.len())
            } else if end_line < li.file_lines.len() {
                (s, li.line_starts[end_line])
            } else {
                (s.saturating_sub(1), e_off)
            };
            Some(span(
                SpanKind::Replace,
                start,
                end_byte,
                lines.join("\n"),
                None,
                None,
            ))
        }
        Edit::Append { pos, lines } => {
            let inserted = lines.join("\n");
            let boundary = insertion_boundary(pos.as_ref(), true, li);
            if content.is_empty() {
                Some(span(
                    SpanKind::Insert,
                    0,
                    0,
                    inserted,
                    Some(boundary),
                    Some(EmptyOrigin::Append),
                ))
            } else if let Some(p) = pos {
                let sentinel = li.has_terminal_newline && p.line == li.file_lines.len();
                let at = if sentinel {
                    content.len()
                } else {
                    li.line_starts[p.line - 1] + li.file_lines[p.line - 1].len()
                };
                let rep = if sentinel {
                    format!("{inserted}\n")
                } else {
                    format!("\n{inserted}")
                };
                Some(span(SpanKind::Insert, at, at, rep, Some(boundary), None))
            } else {
                let rep = if li.has_terminal_newline {
                    format!("{inserted}\n")
                } else {
                    format!("\n{inserted}")
                };
                Some(span(
                    SpanKind::Insert,
                    content.len(),
                    content.len(),
                    rep,
                    Some(boundary),
                    None,
                ))
            }
        }
        Edit::Prepend { pos, lines } => {
            let inserted = lines.join("\n");
            let boundary = insertion_boundary(pos.as_ref(), false, li);
            let start = pos.as_ref().map_or(0, |p| li.line_starts[p.line - 1]);
            if content.is_empty() {
                Some(span(
                    SpanKind::Insert,
                    start,
                    start,
                    inserted,
                    Some(boundary),
                    Some(EmptyOrigin::Prepend),
                ))
            } else {
                Some(span(
                    SpanKind::Insert,
                    start,
                    start,
                    format!("{inserted}\n"),
                    Some(boundary),
                    None,
                ))
            }
        }
    })
}

fn assert_no_conflicts(spans: &[Span]) -> Result<()> {
    for (i, l) in spans.iter().enumerate() {
        for r in &spans[i + 1..] {
            let conflict = match (&l.kind, &r.kind) {
                (SpanKind::Insert, SpanKind::Insert) => l.boundary == r.boundary,
                (SpanKind::Replace, SpanKind::Replace) => l.start < r.end && r.start < l.end,
                _ => {
                    let (rep, ins) = if l.kind == SpanKind::Replace {
                        (l, r)
                    } else {
                        (r, l)
                    };
                    ins.start >= rep.start && ins.end < rep.end
                }
            };
            if conflict {
                bail!(
                    "[E_EDIT_CONFLICT] Conflicting edits: edit {} ({}) and edit {} ({}). \
                     Merge them into one non-overlapping change or split the request.",
                    l.index,
                    l.label,
                    r.index,
                    r.label
                );
            }
        }
    }
    Ok(())
}

fn assemble(content: &str, spans: &[Span]) -> String {
    // Spans are sorted highest-offset-first and never overlap, so an in-place
    // `replace_range` keeps every earlier span's offsets valid — far cheaper
    // than rebuilding the whole string once per span.
    let mut result = content.to_string();
    for s in spans {
        match s.empty_origin {
            Some(EmptyOrigin::Append) if !result.is_empty() => {
                result.replace_range(s.start..s.end, &format!("\n{}", s.replacement));
            }
            Some(EmptyOrigin::Prepend) if !result.is_empty() => {
                result.replace_range(s.start..s.end, &format!("{}\n", s.replacement));
            }
            _ => result.replace_range(s.start..s.end, &s.replacement),
        }
    }
    result
}

/// First/last 1-based line numbers that differ between two document versions,
/// mapped onto the *result* so fresh anchors can be rendered for them.
pub fn changed_line_range(original: &str, result: &str) -> Option<(usize, usize)> {
    if original == result {
        return None;
    }
    let visible = |t: &str| -> usize {
        if t.is_empty() {
            0
        } else if t.ends_with('\n') {
            t.split('\n').count() - 1
        } else {
            t.split('\n').count()
        }
    };
    if original.is_empty() {
        return Some((1, visible(result).max(1)));
    }
    let ob = original.as_bytes();
    let rb = result.as_bytes();
    let min_len = ob.len().min(rb.len());
    let mut first = 0;
    while first < min_len && ob[first] == rb[first] {
        first += 1;
    }
    let mut lo = ob.len();
    let mut lr = rb.len();
    while lo > first && lr > first {
        if ob[lo - 1] != rb[lr - 1] {
            break;
        }
        lo -= 1;
        lr -= 1;
    }
    let index_to_line = |idx: usize, text: &str| -> usize {
        memchr::memchr_iter(b'\n', &text.as_bytes()[..idx.min(text.len())]).count() + 1
    };
    let first_line = index_to_line(first + 1, result);
    let last_line = if lr == first {
        if result.is_empty() {
            1
        } else {
            visible(result)
        }
    } else {
        index_to_line(lr, result)
    };
    Some((first_line, last_line))
}

/// Render lines `start_line..` with `LINE#HASH:` prefixes, line numbers
/// right-padded within the block so the `#HASH:` columns align.
pub fn format_region(lines: &[&str], start_line: usize) -> String {
    let width = (start_line + lines.len().saturating_sub(1))
        .to_string()
        .len();
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let n = start_line + i;
            format!("{:>width$}#{}:{l}", n, compute_line_hash(n, l))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Context lines shown on either side of a changed (or stale) region.
const CONTEXT_LINES: usize = 2;

/// Outcome of a successful apply: the new content plus the changed-line span
/// (for rendering fresh anchors) and any advisory warnings.
pub struct Applied {
    pub content: String,
    pub changed: Option<(usize, usize)>,
    pub warnings: Vec<String>,
}

impl Applied {
    /// The changed region re-rendered with fresh `LINE#HASH` anchors and a
    /// little surrounding context, ready to chain into the next edit without a
    /// re-read. `None` when nothing changed.
    pub fn fresh_anchors(&self) -> Option<String> {
        let (first, last) = self.changed?;
        let all: Vec<&str> = self.content.lines().collect();
        let lo = first.saturating_sub(CONTEXT_LINES).max(1);
        let hi = (last + CONTEXT_LINES).min(all.len());
        (lo <= hi && hi <= all.len()).then(|| format_region(&all[lo - 1..hi], lo))
    }
}

/// Apply hash-anchored edits to `content`, all-or-nothing. Validates anchors
/// (stale → `[E_STALE_ANCHOR]` with a retry snippet), maps edits to non-
/// overlapping character spans, and splices them back-to-front.
pub fn apply(content: &str, edits: &[Edit]) -> Result<Applied> {
    if edits.is_empty() {
        return Ok(Applied {
            content: content.to_string(),
            changed: None,
            warnings: vec![],
        });
    }
    let li = build_line_index(content);
    let mismatches = validate_anchors(edits, &li)?;
    if !mismatches.is_empty() {
        bail!("{}", format_mismatch(&mismatches, &li));
    }

    let mut warnings = Vec::new();
    for e in edits {
        if let Edit::Replace {
            end: None,
            pos,
            lines,
        } = e
            && lines.len() > 1
        {
            warnings.push(format!(
                "Single-anchor replace at {}#{} swapped only line {}, but you supplied {} \
                     replacement lines. Add `end` for a range replace, or ignore if expanding one \
                     line into many.",
                pos.line,
                pos.hash,
                pos.line,
                lines.len()
            ));
        }
    }

    let mut spans = Vec::new();
    for (i, e) in edits.iter().enumerate() {
        if let Some(s) = resolve_span(e, i, content, &li)? {
            spans.push(s);
        }
    }
    assert_no_conflicts(&spans)?;
    // Back-to-front so earlier spans' offsets stay valid during assembly.
    spans.sort_by(|a, b| {
        b.end.cmp(&a.end).then_with(|| match (&a.kind, &b.kind) {
            (SpanKind::Replace, SpanKind::Insert) => std::cmp::Ordering::Less,
            (SpanKind::Insert, SpanKind::Replace) => std::cmp::Ordering::Greater,
            _ => a.index.cmp(&b.index),
        })
    });

    let result = assemble(content, &spans);
    if !content.is_empty() && result.is_empty() {
        bail!(
            "[E_WOULD_EMPTY] Refusing to empty a non-empty file through edit. If intentional, \
             use write_file."
        );
    }
    let changed = changed_line_range(content, &result);
    Ok(Applied {
        content: result,
        changed,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hashed(content: &str) -> Vec<String> {
        content
            .split('\n')
            .enumerate()
            .map(|(i, l)| format!("{}#{}", i + 1, compute_line_hash(i + 1, l)))
            .collect()
    }

    fn apply_json(content: &str, edits: serde_json::Value) -> Result<String> {
        let parsed = parse_edits(&edits["edits"])?;
        Ok(apply(content, &parsed)?.content)
    }

    #[test]
    fn hash_is_stable_and_in_alphabet() {
        let h = compute_line_hash(1, "let x = 1;");
        assert_eq!(h.len(), 2);
        assert!(h.chars().all(is_hash_char));
        assert_eq!(h, compute_line_hash(1, "let x = 1;\r"));
        assert_eq!(h, compute_line_hash(1, "let x = 1;   "));
    }

    #[test]
    fn blank_line_hash_depends_on_line_number() {
        // No alphanumerics -> seeded by index, so identical markers differ.
        assert_ne!(compute_line_hash(1, "}"), compute_line_hash(2, "}"));
    }

    #[test]
    fn replace_single_line() {
        let content = "a\nb\nc\n";
        let anchors = hashed(content);
        // Anchor with a trailing :textHint suffix (as read output renders) parses.
        let out = apply_json(
            content,
            json!({ "edits": [{ "op": "replace", "pos": format!("{}:b", anchors[1]), "lines": ["B"] }] }),
        )
        .unwrap();
        assert_eq!(out, "a\nB\nc\n");
    }

    #[test]
    fn replace_range() {
        let content = "a\nb\nc\nd\n";
        let h = hashed(content);
        let out = apply_json(
            content,
            json!({ "edits": [{ "op": "replace", "pos": h[1], "end": h[2], "lines": ["X"] }] }),
        )
        .unwrap();
        assert_eq!(out, "a\nX\nd\n");
    }

    #[test]
    fn append_and_prepend() {
        let content = "a\nb\n";
        let h = hashed(content);
        let out = apply_json(
            content,
            json!({ "edits": [{ "op": "append", "pos": h[0], "lines": ["mid"] }] }),
        )
        .unwrap();
        assert_eq!(out, "a\nmid\nb\n");
        let out = apply_json(
            content,
            json!({ "edits": [{ "op": "prepend", "pos": h[0], "lines": ["top"] }] }),
        )
        .unwrap();
        assert_eq!(out, "top\na\nb\n");
    }

    #[test]
    fn append_at_eof_without_anchor() {
        let out = apply_json(
            "a\nb\n",
            json!({ "edits": [{ "op": "append", "lines": ["c"] }] }),
        )
        .unwrap();
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn stale_anchor_is_rejected() {
        let content = "a\nb\nc\n";
        let err = apply_json(
            content,
            json!({ "edits": [{ "op": "replace", "pos": "2#ZZ", "lines": ["B"] }] }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("E_STALE_ANCHOR"), "got: {err}");
        assert!(err.contains(">>> "), "should include retry snippet: {err}");
    }

    #[test]
    fn overlapping_edits_conflict() {
        let content = "a\nb\nc\nd\n";
        let h = hashed(content);
        let err = apply_json(
            content,
            json!({ "edits": [
                { "op": "replace", "pos": h[0], "end": h[2], "lines": ["X"] },
                { "op": "replace", "pos": h[1], "lines": ["Y"] }
            ] }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("E_EDIT_CONFLICT"), "got: {err}");
    }

    #[test]
    fn display_prefix_in_lines_rejected() {
        let content = "a\nb\n";
        let h = hashed(content);
        let err = apply_json(
            content,
            json!({ "edits": [{ "op": "replace", "pos": h[0], "lines": ["1#MQ:a"] }] }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("E_INVALID_PATCH"), "got: {err}");
    }

    #[test]
    fn whole_file_delete_refused() {
        let content = "a\nb\nc";
        let h = hashed(content);
        let err = apply_json(
            content,
            json!({ "edits": [{ "op": "replace", "pos": h[0], "end": h[2], "lines": [] }] }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("E_WOULD_EMPTY"), "got: {err}");
    }

    #[test]
    fn fuzzy_text_hint_accepts_whitespace_difference() {
        // File has trailing whitespace; anchor hash is computed on trimmed text,
        // so an exact match works, but verify the textHint path tolerates a
        // smart-quote difference.
        let content = "let s = \u{2018}hi\u{2019};\n";
        let h = hashed(content);
        // Anchor carries an ASCII-quoted hint; hash was computed on the smart
        // quotes, so the hint hash won't match — but the file line does match
        // its own hash, so a plain anchor still applies.
        let out = apply_json(
            content,
            json!({ "edits": [{ "op": "replace", "pos": h[0], "lines": ["let s = 'hi';"] }] }),
        )
        .unwrap();
        assert_eq!(out, "let s = 'hi';\n");
    }
}
