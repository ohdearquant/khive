/// `all_verbs` returns only `Visibility::Verb` entries.
///
/// BetaPack's `create` is `Visibility::Subhandler` — it must NOT appear
/// in `all_verbs()` even though it has the same name as a Verb in AlphaPack.
#[test]
fn all_verbs_aggregates_across_packs_excludes_subhandlers() {
    let reg = build_registry();
    let verbs: Vec<&str> = reg.all_verbs().iter().map(|v| v.name).collect();
    // BetaPack's "create" (Subhandler) is absent; only Verb-visibility entries appear.
    assert_eq!(verbs, vec!["create", "list", "notify"]);
}

#[test]
fn all_verbs_with_names_pairs_pack_name_excludes_subhandlers() {
    let reg = build_registry();
    let pairs: Vec<(&str, &str)> = reg
        .all_verbs_with_names()
        .iter()
        .map(|(pack, v)| (*pack, v.name))
        .collect();
    // BetaPack's "create" is Subhandler and must NOT appear here.
    assert_eq!(
        pairs,
        vec![("alpha", "create"), ("alpha", "list"), ("beta", "notify"),]
    );
}

#[test]
fn all_handlers_with_names_includes_subhandlers() {
    let reg = build_registry();
    let pairs: Vec<(&str, &str)> = reg
        .all_handlers_with_names()
        .iter()
        .map(|(pack, v)| (*pack, v.name))
        .collect();
    // BetaPack's Subhandler "create" IS present in the full handler list.
    assert_eq!(
        pairs,
        vec![
            ("alpha", "create"),
            ("alpha", "list"),
            ("beta", "notify"),
            ("beta", "create"),
        ]
    );
}

#[test]
fn disabled_verbs_are_hidden_only_from_public_catalogs() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.register(BetaPack);
    builder.with_disabled_verbs("alpha", &["create".to_string()]);
    let reg = builder
        .build()
        .expect("registry with a disabled public verb");

    let verbs: Vec<_> = reg.all_verbs().iter().map(|handler| handler.name).collect();
    assert_eq!(verbs, vec!["list", "notify"]);
    let public: Vec<_> = reg
        .all_verbs_with_names()
        .iter()
        .map(|(pack, handler)| (*pack, handler.name))
        .collect();
    assert_eq!(public, vec![("alpha", "list"), ("beta", "notify")]);
    let complete: Vec<_> = reg
        .all_handlers_with_names()
        .iter()
        .map(|(pack, handler)| (*pack, handler.name, handler.visibility))
        .collect();
    assert_eq!(
        complete,
        vec![
            ("alpha", "create", Visibility::Verb),
            ("alpha", "list", Visibility::Verb),
            ("beta", "notify", Visibility::Verb),
            ("beta", "create", Visibility::Subhandler),
        ]
    );
}

#[test]
fn pack_metadata_lookup_is_exact_and_preserves_declared_slices() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.register(BetaPack);
    builder.with_disabled_verbs("alpha", &["create".to_string()]);
    let reg = builder.build().expect("registry with declared metadata");

    // A registered pack's empty dependency list is distinct from a missing pack.
    assert_eq!(reg.pack_requires("alpha"), Some(&[][..]));
    assert_eq!(reg.pack_requires("beta"), Some(&[][..]));
    assert_eq!(reg.pack_note_kinds("alpha"), Some(&["memo", "log"][..]));
    assert_eq!(reg.pack_note_kinds("beta"), Some(&["alert"][..]));
    assert_eq!(reg.pack_entity_kinds("alpha"), Some(&["widget"][..]));
    assert_eq!(
        reg.pack_entity_kinds("beta"),
        Some(&["widget", "gadget"][..])
    );
    // Metadata retains disabled public handlers and internal handlers, including
    // their description, visibility, category and parameter declarations.
    assert_eq!(reg.pack_verbs("alpha"), Some(AlphaPack::HANDLERS));
    assert_eq!(reg.pack_verbs("beta"), Some(BetaPack::HANDLERS));

    for name in ["missing", "Alpha", "Beta"] {
        assert!(reg.pack_requires(name).is_none(), "requires for {name}");
        assert!(reg.pack_note_kinds(name).is_none(), "note kinds for {name}");
        assert!(
            reg.pack_entity_kinds(name).is_none(),
            "entity kinds for {name}"
        );
        assert!(reg.pack_verbs(name).is_none(), "handlers for {name}");
    }
}

#[test]
fn note_kinds_are_ordered() {
    let reg = build_registry();
    let kinds = reg.all_note_kinds();
    assert_eq!(kinds, vec!["memo", "log", "alert"]);
}

#[test]
fn brain_consumer_kinds_are_ordered_and_deduplicated() {
    let reg = build_registry();
    assert_eq!(
        reg.all_brain_consumer_kinds(),
        vec!["recall", "search", "knowledge_compose"]
    );
}
