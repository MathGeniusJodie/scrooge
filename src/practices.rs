//! Best-practices document, served piecemeal: only sections whose keywords
//! match the topic are returned, so the agent never pays for irrelevant rules.

const DOC: &str = include_str!("../best_practices.md");

/// A section header is `## title [kw1, kw2, ...]`; it matches if any keyword
/// (or title word) appears in the topic string.
fn matches(header: &str, topic_lower: &str) -> bool {
    header
        .split(['[', ']', ','])
        .flat_map(|p| p.split_whitespace())
        .map(|w| w.trim().to_lowercase())
        .filter(|w| w.len() > 2)
        .any(|k| topic_lower.contains(&k))
}

/// Full text of every section relevant to the topic (for Cratchit).
pub fn relevant_sections(topic: &str) -> String {
    let topic_lower = topic.to_lowercase();
    let mut out = String::new();
    for section in DOC.split("\n## ").skip(1) {
        let header = section.lines().next().unwrap_or("");
        if matches(header, &topic_lower) {
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

/// Tiny task-tailored digest for Scrooge: one line per matching section,
/// carrying only its first (most important) rule. Cratchit gets the full
/// sections via `relevant_sections`.
pub fn summary(topic: &str) -> String {
    let topic_lower = topic.to_lowercase();
    let mut out = String::new();
    for section in DOC.split("\n## ").skip(1) {
        let header = section.lines().next().unwrap_or("");
        if !matches(header, &topic_lower) {
            continue;
        }
        let title = header.split('[').next().unwrap_or("").trim();
        if let Some(rule) = section.lines().find(|l| l.trim_start().starts_with("- ")) {
            let rule = rule.trim_start().trim_start_matches("- ");
            out.push_str(&format!("{title}: {rule}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn summary_is_one_line_per_matching_section() {
        let s = super::summary("fix the rust borrow error in the tests");
        assert!(s.lines().any(|l| l.starts_with("rust: ")));
        assert!(s.lines().any(|l| l.starts_with("testing: ")));
        assert!(!s.contains("javascript"));
        // Abridged: one rule per section, no markdown headers.
        assert!(!s.contains("##"));
        let full = super::relevant_sections("fix the rust borrow error in the tests");
        assert!(s.len() < full.len());
    }
}
