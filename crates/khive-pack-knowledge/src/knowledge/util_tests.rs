use super::*;

#[test]
fn knowledge_sql_err_message_is_context_and_error() {
    match sql_err("insert atom", "boom") {
        RuntimeError::Internal(message) => {
            assert_eq!(message, "insert atom: boom");
        }
        other => panic!("expected RuntimeError::Internal, got {other:?}"),
    }
}

#[test]
fn servable_section_predicate_excludes_every_retired_spelling() {
    let whitespace = (0..=u32::from(char::MAX))
        .filter_map(char::from_u32)
        .filter(|c| c.is_whitespace())
        .map(|c| u32::from(c).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let spellings = khive_brain_core::SectionType::RETIRED_SPELLINGS
        .iter()
        .map(|(spelling, _)| format!("'{spelling}'"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        SERVABLE_SECTION,
        format!(
            "lower(replace(replace(trim(section_type, char({whitespace})), \
                 '-', '_'), ' ', '_')) NOT IN ({spellings})"
        )
    );
}

#[test]
fn servable_section_normalization_equals_normalize_name() {
    let (expression, _) = SERVABLE_SECTION
        .split_once(" NOT IN ")
        .expect("the predicate compares a normalized value");
    let sql = format!("SELECT {}", expression.replacen("section_type", "?1", 1));
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let mut inputs = vec![
        "References".to_string(),
        "See-Also".to_string(),
        "see also".to_string(),
        "FURTHER-reading".to_string(),
        "Core Model".to_string(),
        "Boundary-Conditions".to_string(),
        "Example".to_string(),
        "pitfall".to_string(),
        "Ré-Sumé Notes".to_string(),
        "a\tb".to_string(),
        String::new(),
        " \t\n".to_string(),
    ];
    for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
        if c.is_whitespace() {
            inputs.push(format!("{c}See-Also{c}"));
            inputs.push(format!("{c}{c}Notes {c}"));
        }
    }
    for input in &inputs {
        let normalized: String = conn.query_row(&sql, [input], |row| row.get(0)).unwrap();
        assert_eq!(
            normalized,
            khive_brain_core::SectionType::normalize_name(input),
            "{input:?}"
        );
    }
}

#[test]
fn domain_marker_uses_exact_decoded_tag_and_retains_legacy_fallback() {
    for tags in [
        r#"["type:domain"]"#,
        r#"["ordinary","type:domain"]"#,
        r#"["type\u003adomain"]"#,
        "broken type:domain",
        r#""type:domain""#,
        r#"{"tag":"type:domain"}"#,
        r#"["type:domain-extra",7]"#,
    ] {
        assert!(has_domain_mirror_tag(tags), "{tags}");
    }
    for tags in [
        "[]",
        r#"["ordinary"]"#,
        r#"["prefix:type:domain"]"#,
        r#"["type:domain-extra"]"#,
        r#"["TYPE:DOMAIN"]"#,
        r#"[" type:domain "]"#,
        "broken",
        r#""ordinary""#,
        "{}",
        r#"["ordinary",7]"#,
        "null",
        "",
    ] {
        assert!(!has_domain_mirror_tag(tags), "{tags}");
    }
}
