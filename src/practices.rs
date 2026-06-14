//! Best-practices document, served piecemeal: only sections whose keywords
//! match the topic are returned, so the agent never pays for irrelevant rules.

use std::fmt::Write;

const DOC: &str = include_str!("../best_practices.md");

/// At least this many rules per section in the digest Scrooge sees (or all of
/// them, if the section has fewer).
const MIN_SUMMARY_RULES: usize = 3;

/// The keywords of a section header `## title [kw1, kw2, ...]`: the title words
/// and the bracket keywords, lowercased, short noise words dropped.
fn header_keywords(header: &str) -> impl Iterator<Item = String> + '_ {
    header
        .split(['[', ']', ','])
        .flat_map(|p| p.split_whitespace())
        .map(|w| w.trim().to_lowercase())
        .filter(|w| w.len() > 2)
}

/// A section matches if any of its keywords appears in the haystack string.
fn matches(header: &str, haystack_lower: &str) -> bool {
    header_keywords(header).any(|k| crate::codemap::contains_word(haystack_lower, &k))
}

/// A section is relevant when its keywords match the task (activity sections
/// like `testing`/`editing`) OR the project's languages (language sections
/// like `rust`/`python`). Language guidance is keyed off what the project *is*,
/// not whether the task text names the language — it almost never does.
fn relevant(header: &str, topic_lower: &str, langs_lower: &str) -> bool {
    matches(header, topic_lower) || matches(header, langs_lower)
}

/// Full text of every relevant section (for Cratchit). `langs` are the
/// project's language tags (see `codemap::CodeMap::languages`).
pub fn relevant_sections(topic: &str, langs: &[&str]) -> String {
    let topic_lower = topic.to_lowercase();
    let langs_lower = langs.join(" ");
    let mut out = String::new();
    for section in DOC.split("\n## ").skip(1) {
        let header = section.lines().next().unwrap_or("");
        if relevant(header, &topic_lower, &langs_lower) {
            out.push_str("## ");
            out.push_str(section.trim());
            out.push_str("\n\n");
        }
    }
    if out.is_empty() {
        "no specific guidance for this topic; use general good judgment".into()
    } else {
        out
    }
}

/// Task-tailored digest for Scrooge: a few rules per relevant section, with the
/// rules that mention a matched keyword pulled to the front (each group in
/// source order), so the bullet most relevant to this task leads. Shows at
/// least `MIN_SUMMARY_RULES` rules (or all, if fewer). Cratchit gets the full
/// sections via `relevant_sections`. `langs` are the project's language tags.
pub fn summary(topic: &str, langs: &[&str]) -> String {
    let topic_lower = topic.to_lowercase();
    let langs_lower = langs.join(" ");
    let haystack = format!("{topic_lower} {langs_lower}");
    let mut out = String::new();
    for section in DOC.split("\n## ").skip(1) {
        let header = section.lines().next().unwrap_or("");
        if !relevant(header, &topic_lower, &langs_lower) {
            continue;
        }
        let title = header.split('[').next().unwrap_or("").trim();
        // The header keywords that actually fired this section.
        let matched: Vec<String> = header_keywords(header)
            .filter(|k| crate::codemap::contains_word(&haystack, k))
            .collect();
        let rules: Vec<&str> = section
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("- "))
            .collect();
        // A rule leads when it mentions one of the matched keywords.
        let (hits, rest): (Vec<&str>, Vec<&str>) = rules.iter().partition(|r| {
            let r = r.to_lowercase();
            matched.iter().any(|k| crate::codemap::contains_word(&r, k))
        });
        let take = hits.len().max(MIN_SUMMARY_RULES);
        for rule in hits.iter().chain(rest.iter()).take(take) {
            writeln!(out, "{title}: {rule}").unwrap();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn summary_abridges_matching_sections() {
        let s = super::summary("fix the borrow error in the tests", &["rust"]);
        assert!(s.lines().any(|l| l.starts_with("rust: ")));
        assert!(s.lines().any(|l| l.starts_with("testing: ")));
        assert!(!s.contains("javascript"));
        // Abridged: rules only, no markdown headers.
        assert!(!s.contains("##"));
        let full = super::relevant_sections("fix the borrow error in the tests", &["rust"]);
        assert!(s.len() < full.len());
    }

    #[test]
    fn summary_leads_with_keyword_matching_rules() {
        // "borrow" matches the rust section's second rule, which must lead.
        let s = super::summary("fix the borrow checker complaint", &["rust"]);
        let rust: Vec<&str> = s.lines().filter_map(|l| l.strip_prefix("rust: ")).collect();
        assert!(rust[0].contains("borrow"), "keyword rule leads: {rust:?}");
        // At least three rules shown (rust has exactly three).
        assert_eq!(rust.len(), 3);
        // Non-matching rules keep their source order after the hits.
        assert!(rust[1].contains('?') && rust[2].contains("allocation"));
    }

    #[test]
    fn language_guidance_keys_off_project_not_task_text() {
        // A task that never says "rust" still gets the rust section because
        // the project is a rust project.
        let s = super::summary("add a flag to the readme", &["rust"]);
        assert!(s.lines().any(|l| l.starts_with("rust: ")));
        // ...and a python project would not get rust guidance for it.
        let s = super::summary("add a flag to the readme", &["python"]);
        assert!(!s.lines().any(|l| l.starts_with("rust: ")));
        assert!(s.lines().any(|l| l.starts_with("python: ")));
    }
}
