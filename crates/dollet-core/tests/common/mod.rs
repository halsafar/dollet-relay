//! Shared by the leak scans in `fixtures.rs`, `golden.rs` and `ingest.rs`.

/// Every `scheme://host` this text mentions, lowercased and without its port.
pub fn hosts_referenced(text: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for (index, _) in text.match_indices("://") {
        let rest = &text[index + 3..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':'))
            .unwrap_or(rest.len());
        let host = rest[..end]
            .split(':')
            .next()
            .unwrap_or_default()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if !host.is_empty() {
            hosts.push(host);
        }
    }
    hosts
}
