//! Source census for runtime methods that discard embedding outcomes.

#[test]
fn no_public_embedding_outcome_discards() {
    let wrappers = [
        ("create_entity", include_str!("../src/operations.rs")),
        ("update_entity", include_str!("../src/curation.rs")),
        ("update_note", include_str!("../src/curation.rs")),
        (
            "embed_document_with_model",
            include_str!("../src/retrieval.rs"),
        ),
        (
            "create_notes_atomic",
            include_str!("../src/atomic_message.rs"),
        ),
    ];
    for (name, source) in wrappers {
        assert!(
            !source.contains(&format!("pub async fn {name}(")),
            "{name} must not be a public wrapper that discards embedding outcomes"
        );
    }

    let mut public_uses = String::new();
    let mut collecting = false;
    for line in include_str!("../src/lib.rs").lines() {
        if line.trim_start().starts_with("pub use ") {
            collecting = true;
        }
        if collecting {
            public_uses.push_str(line);
            public_uses.push(' ');
            if line.contains(';') {
                collecting = false;
            }
        }
    }
    for (name, _) in wrappers {
        assert!(
            !public_uses
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|token| token == name),
            "{name} must not be publicly re-exported"
        );
    }
}
