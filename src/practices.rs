//! Best-practices document, served piecemeal: only sections whose keywords
//! match the topic are returned, so the agent never pays for irrelevant rules.

const DOC: &str = include_str!("../best_practices.md");

/// Sections are `## title [kw1, kw2, ...]` headers. A section is relevant if
/// any of its keywords (or title words) appear in the topic string.
pub fn relevant_sections(topic: &str) -> String {
    let topic_lower = topic.to_lowercase();
    let mut out = String::new();
    for section in DOC.split("\n## ").skip(1) {
        let header = section.lines().next().unwrap_or("");
        let keywords: Vec<String> = header
            .split(['[', ']', ','])
            .flat_map(|p| p.split_whitespace())
            .map(|w| w.trim().to_lowercase())
            .filter(|w| w.len() > 2)
            .collect();
        if keywords.iter().any(|k| topic_lower.contains(k)) {
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
