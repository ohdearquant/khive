use sha2::{Digest, Sha256};

/// Compute the deterministic query-class key defined by ADR-081 section 4.
pub fn compute_query_class(query_raw: &str) -> String {
    let lowered = query_raw.to_lowercase();
    let stripped: String = lowered
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let mut tokens: Vec<&str> = stripped.split_whitespace().collect();
    tokens.sort_unstable();
    tokens.dedup();
    let normalized = tokens.join(" ");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)[..16].to_string()
}
