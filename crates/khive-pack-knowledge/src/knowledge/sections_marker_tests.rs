use super::*;

#[test]
fn trailing_marker_declares_the_type_and_leaves_the_display_heading() {
    let (section_type, heading) = section_heading_type("Why memoization terminates {core_model}");
    assert_eq!(section_type, Some(SectionType::CoreModel));
    assert_eq!(heading, "Why memoization terminates");
}

#[test]
fn marker_overrides_a_heading_the_alias_lookup_would_type_differently() {
    let (section_type, heading) = section_heading_type("Overview {failure_modes}");
    assert_eq!(section_type, Some(SectionType::FailureModes));
    assert_eq!(heading, "Overview");
}

#[test]
fn marker_accepts_canonical_values_only() {
    // `model` is an alias of core_model for bare headings, never inside a marker.
    let (section_type, heading) = section_heading_type("How the cache is keyed {model}");
    assert_eq!(section_type, None);
    assert_eq!(heading, "How the cache is keyed");
    let (section_type, _) = section_heading_type("How the cache is keyed {not_a_type}");
    assert_eq!(section_type, None);
}

#[test]
fn heading_without_marker_keeps_the_alias_lookup_and_other_fallback() {
    assert_eq!(
        section_heading_type("Mechanism"),
        (Some(SectionType::CoreModel), "Mechanism".to_string())
    );
    assert_eq!(
        section_heading_type("A descriptive heading"),
        (
            Some(SectionType::Other),
            "A descriptive heading".to_string()
        )
    );
}

#[test]
fn brace_text_that_is_not_a_trailing_token_stays_in_the_heading() {
    for heading in [
        "Set builder {x | x > 0}",
        "The {core_model} notation explained",
        "Typed {Core_Model}",
        "Empty braces {}",
        "Digits {k2}",
    ] {
        let (section_type, display) = section_heading_type(heading);
        assert_eq!(display, heading, "{heading:?} must keep its braces");
        assert_eq!(
            section_type,
            Some(SectionType::from_str_loose(heading).unwrap_or(SectionType::Other)),
            "{heading:?} must go through the alias lookup"
        );
    }
}

#[test]
fn parser_carries_the_declared_type_per_section() {
    let markdown = "# Atom\n\npreamble\n\n\
        ## First reason {core_model}\n\nbody one\n\n\
        ## Second reason {core_model}\n\nbody two\n\n\
        ## Unknown kind {made_up}\n\nbody three\n";
    let (name, _, sections) = parse_atlas_md(markdown);
    assert_eq!(name, "Atom");
    let typed = sections
        .iter()
        .map(|(section_type, heading, body)| (*section_type, heading.as_str(), body.trim()))
        .collect::<Vec<_>>();
    assert_eq!(
        typed,
        vec![
            (Some(SectionType::CoreModel), "First reason", "body one"),
            (Some(SectionType::CoreModel), "Second reason", "body two"),
            (None, "Unknown kind", "body three"),
        ]
    );
}
