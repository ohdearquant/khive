use super::*;
use crate::ActorRef;
use khive_types::Pack;

mod census_calls {
    include!("pack_census_calls.rs");
}
use census_calls::{calls_name, crate_seam_names, file_seam_names, strip_string_literals};

mod disposition {
    include!("pack_disposition_tests.rs");
}

mod link_audit_alias {
    include!("link_audit_alias_tests.rs");
}

#[tokio::test]
async fn pack_host_state_shares_the_registered_dispatch_instance_across_clones() {
    struct HostStatePack(Arc<AtomicUsize>);

    #[async_trait]
    impl PackRuntime for HostStatePack {
        fn name(&self) -> &str {
            "host_state"
        }
        fn host_state(&self) -> Option<Arc<dyn Any + Send + Sync>> {
            Some(self.0.clone())
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            &[HandlerDef {
                name: "host_state.touch",
                description: "shared state fixture",
                visibility: Visibility::Verb,
                category: VerbCategory::Commissive,
                params: &[],
            }]
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        }
    }

    let state = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(HostStatePack(state.clone())));
    builder.register(AlphaPack);
    let registry = builder.build().expect("registry");
    let host = registry
        .pack_host_state::<AtomicUsize>("host_state")
        .expect("registered state");
    let cloned_host = registry
        .clone()
        .pack_host_state::<AtomicUsize>("host_state")
        .expect("cloned registry state");
    assert!(Arc::ptr_eq(&state, &host));
    assert!(Arc::ptr_eq(&host, &cloned_host));
    registry
        .dispatch("host_state.touch", serde_json::json!({}))
        .await
        .expect("dispatch");
    assert_eq!(host.load(Ordering::SeqCst), 1);
    assert!(registry.pack_host_state::<String>("host_state").is_none());
    assert!(registry.pack_host_state::<AtomicUsize>("missing").is_none());
    assert!(registry.pack_host_state::<AtomicUsize>("alpha").is_none());
}

/// Verbs known, by cross-pack source review (#2147/#2217), to have
/// durable/accounting side effects or telemetry answers that must refuse
/// when admission cannot record their audit, despite being declared
/// `VerbCategory::Assertive` — see [`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`]'s
/// doc for why each is excluded. `VerbCategory::Assertive` alone cannot
/// distinguish these from a genuinely side-effect-free read (that is the
/// whole reason the allowlist exists instead of a bare category check),
/// so this denylist is the mechanizable guard against silently
/// reintroducing one of them: a category-only census would stay green if
/// any name were re-added to the allowlist.
const KNOWN_ADMISSION_UNSAFE_VERBS: &[&str] = &[
    "db_diagnostics",
    "git.checkout",
    "git.diff",
    "git.reconcile",
    "knowledge.compose",
    "knowledge.search",
    "knowledge.suggest",
    "memory.recall",
    "telemetry.channels",
    "telemetry.counts",
    "telemetry.emit",
    "telemetry.read",
];

/// Classification outcome for one `HandlerDef {` occurrence in pack
/// source, returned by [`classify_handler_def_occurrence`]. `Signature`
/// and `StructLiteral` are the two shapes the live cross-pack census
/// currently expects; `Unclassified` exists so neither the census nor a
/// direct unit test has to rely on a panic to observe a shape that is
/// neither — see `classify_handler_def_occurrence_reports_unclassifiable_shapes`
/// below.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HandlerDefOccurrence {
    /// A function/closure signature merely naming the type in
    /// return-tail position (`-> &'static HandlerDef {`, possibly
    /// qualified), not a declared handler.
    Signature,
    /// A struct-literal field block whose `name`/`visibility`/`category`
    /// fields were all found at one consistent indentation.
    StructLiteral {
        name: String,
        visibility: String,
        category: String,
    },
    /// Neither of the above: not a signature tail, and the block does
    /// not parse as a `name`/`visibility`/`category` struct literal at a
    /// single consistent indentation either.
    Unclassified { first_field_line: String },
}

/// Classify one `HandlerDef {` occurrence at `source[match_start..match_end]`
/// (`match_end` is the byte offset just past the token). Shared by the
/// live cross-pack census
/// (`admission_degrade_safe_assertive_census_matches_live_pack_sources`)
/// and `classify_handler_def_occurrence_reports_unclassifiable_shapes`'s
/// direct unit coverage of the `Unclassified` arm — extracting this as
/// its own function is what makes the negative arm testable without
/// corrupting a real pack source file to trigger it.
fn classify_handler_def_occurrence(
    source: &str,
    match_start: usize,
    match_end: usize,
) -> HandlerDefOccurrence {
    // A struct literal is never preceded on its own line by `->`; a
    // signature tail always is.
    let line_start = source[..match_start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    if source[line_start..match_start].contains("->") {
        return HandlerDefOccurrence::Signature;
    }

    let next_marker = source[match_end..].find("HandlerDef {");
    let block_end = next_marker.map(|o| match_end + o).unwrap_or(source.len());
    let block = &source[match_end..block_end];

    // The field indentation is read from the block's own first line
    // rather than hardcoded: array-element declarations (`&[HandlerDef
    // {`) indent fields one level deeper than the single-element
    // `static X: [HandlerDef; 1] = [HandlerDef {` shape, and a
    // hardcoded depth would silently stop matching whichever shape it
    // didn't anticipate — exactly how the narrower delimiter this
    // replaced went unnoticed.
    let Some(first_field_line) = block.lines().find(|line| !line.trim().is_empty()) else {
        return HandlerDefOccurrence::Unclassified {
            first_field_line: String::new(),
        };
    };
    let indent_len = first_field_line.len() - first_field_line.trim_start().len();
    let indent = &first_field_line[..indent_len];
    let name_prefix = format!("{indent}name: \"");
    let visibility_prefix = format!("{indent}visibility: ");
    let category_prefix = format!("{indent}category: ");

    let name = block.lines().find_map(|line| {
        line.strip_prefix(name_prefix.as_str())
            .and_then(|rest| rest.strip_suffix("\","))
    });
    let visibility = block
        .lines()
        .find_map(|line| line.strip_prefix(visibility_prefix.as_str()));
    let category = block
        .lines()
        .find_map(|line| line.strip_prefix(category_prefix.as_str()));

    match (name, visibility, category) {
        (Some(name), Some(visibility), Some(category)) => HandlerDefOccurrence::StructLiteral {
            name: name.to_string(),
            visibility: visibility.to_string(),
            category: category.to_string(),
        },
        _ => HandlerDefOccurrence::Unclassified {
            first_field_line: first_field_line.to_string(),
        },
    }
}

// The web macro emits its handler table from concrete declarations.
// Count macro entries independently of the census's literal scanner so
// an opaque declaration cannot silently disappear from its population.
#[test]
fn web_macro_handlers_remain_visible_to_admission_census() {
    struct Declarations(Vec<syn::Ident>);
    impl syn::parse::Parse for Declarations {
        fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
            let mut handlers = Vec::new();
            while !input.is_empty() {
                let _: syn::Ident = input.parse()?;
                let _: syn::Token![=>] = input.parse()?;
                handlers.push(input.parse()?);
                let content;
                syn::braced!(content in input);
                let _: proc_macro2::TokenStream = content.parse()?;
            }
            Ok(Self(handlers))
        }
    }

    let source = include_str!("../../khive-pack-web/src/vocab.rs");
    let file = syn::parse_file(source).unwrap();
    let declarations: Vec<_> = file
        .items
        .into_iter()
        .filter_map(|item| match item {
            syn::Item::Macro(item) if item.mac.path.is_ident("web_verbs") => {
                Some(syn::parse2::<Declarations>(item.mac.tokens).unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        declarations.len(),
        1,
        "one web handler declaration inventory"
    );
    let declared = &declarations[0].0;
    assert!(
        !declared.is_empty(),
        "web macro handler inventory must be nonempty"
    );
    for handler in declared {
        assert_eq!(
            handler.to_string(),
            "HandlerDef",
            "web handler declarations must stay census-visible"
        );
    }

    let marker = "HandlerDef {";
    let mut classified = 0;
    for (start, _) in source.match_indices(marker) {
        match classify_handler_def_occurrence(source, start, start + marker.len()) {
            HandlerDefOccurrence::StructLiteral {
                name,
                visibility,
                category,
            } => {
                assert!(
                    name.starts_with("web."),
                    "web handler name must retain its prefix"
                );
                assert_eq!(
                    visibility.trim(),
                    "Visibility::Verb,",
                    "web handlers must remain public verbs"
                );
                assert_eq!(
                    category.trim(),
                    "VerbCategory::Commissive,",
                    "web handlers must retain their admission category"
                );
                classified += 1;
            }
            other => panic!("web macro handler declaration is not census-visible: {other:?}"),
        }
    }
    assert_eq!(
        classified,
        declared.len(),
        "every web macro entry must reach the admission census"
    );
}

/// khive-oss#2311: before this fix, the live census's per-file
/// `classified_count == raw_token_count` assertion incremented
/// `classified_count` once per loop iteration — before any
/// classification ran — so it counted exactly the same occurrences
/// `raw_token_count` counts, by the same method, and could never
/// disagree regardless of what the loop body did afterward: a silently
/// dropped classification branch would have stayed green. This proves
/// the replacement — [`classify_handler_def_occurrence`], now called
/// once per occurrence and the sole source of the census's per-branch
/// counters — actually distinguishes an unclassifiable shape from the
/// two shapes the census expects, using a hand-built snippet with a
/// `HandlerDef {` block that is neither a signature tail nor a
/// well-formed struct literal (its `category:` field is missing at the
/// expected indentation).
#[test]
fn classify_handler_def_occurrence_reports_unclassifiable_shapes() {
    let signature_snippet = "fn describe() -> &'static HandlerDef {\n    HANDLER\n}\n";
    let match_start = signature_snippet.find("HandlerDef {").unwrap();
    let match_end = match_start + "HandlerDef {".len();
    assert_eq!(
        classify_handler_def_occurrence(signature_snippet, match_start, match_end),
        HandlerDefOccurrence::Signature
    );

    let struct_literal_snippet = "        HandlerDef {\n            name: \"probe\",\n            visibility: Visibility::Verb,\n            category: VerbCategory::Assertive,\n        }\n";
    let match_start = struct_literal_snippet.find("HandlerDef {").unwrap();
    let match_end = match_start + "HandlerDef {".len();
    assert_eq!(
        classify_handler_def_occurrence(struct_literal_snippet, match_start, match_end),
        HandlerDefOccurrence::StructLiteral {
            name: "probe".to_string(),
            visibility: "Visibility::Verb,".to_string(),
            category: "VerbCategory::Assertive,".to_string(),
        }
    );

    // Missing `category:` at the expected indentation: not a signature
    // tail (no `->`), and not a parseable struct literal either.
    let unclassifiable_snippet = "        HandlerDef {\n            name: \"probe\",\n            visibility: Visibility::Verb,\n        }\n";
    let match_start = unclassifiable_snippet.find("HandlerDef {").unwrap();
    let match_end = match_start + "HandlerDef {".len();
    assert!(
        matches!(
            classify_handler_def_occurrence(unclassifiable_snippet, match_start, match_end),
            HandlerDefOccurrence::Unclassified { .. }
        ),
        "a HandlerDef block missing an expected field must classify as Unclassified, not \
             silently fall through as a recognized shape"
    );
}

/// khive-runtime links no real pack crates in its own test binary (see
/// the comment on `CommProbeFactory` below), so
/// [`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`] cannot be checked
/// against a live registered `HandlerDef` here. Instead this re-derives
/// the complete public Assertive surface from each owning pack's live
/// source — the same fail-closed pattern as `adr133_writer_census.rs`'s
/// `reclassify_from_live_source`.
///
/// Every occurrence of the literal `HandlerDef {` token in scanned source
/// is classified into exactly one of: a struct-literal field block, or a
/// function/closure signature merely naming the type
/// (`-> &'static HandlerDef {`) — a per-file count assertion fails
/// closed if any occurrence goes unclassified, so a handler declared in
/// an unanticipated shape (a prior version of this census silently
/// dropped the single-element `static X: [HandlerDef; 1] = [HandlerDef {`
/// shape used by `khive-pack-code` and `khive-pack-template`) cannot
/// drop out of the count without failing the test. This also verifies
/// that each [`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`] entry's
/// claimed owning pack matches the pack whose source actually declares
/// that verb.
///
/// This test proves category membership (`VerbCategory::Assertive`),
/// pack ownership, non-membership in [`KNOWN_ADMISSION_UNSAFE_VERBS`],
/// and exhaustive classification of every currently public Assertive
/// handler. It does NOT prove general effect-purity: an Assertive
/// handler may still emit its own
/// observability/config events on an independent, best-effort background
/// path (`search`'s `SearchExecuted` telemetry, `context`'s one-time
/// `ConfigLocked` event) that this test does not inspect and that this
/// PR's admission-degrade mechanism does not touch — those events commit
/// or fail on their own path regardless of what happens to this
/// dispatch's own audit row. Proving general effect-purity would require
/// an explicit per-handler effect/accounting capability tag, which is
/// out of scope here (see ADR-103 Amendment 3's "why this is accepted"
/// section); this census instead locks down the properties that are
/// mechanizable today: declared category and an exhaustive, reviewed
/// safe-versus-incidental classification.
#[test]
fn admission_degrade_safe_assertive_census_matches_live_pack_sources() {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    fn collect_rust_sources(dir: &Path, sources: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("failed to read source directory {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry
                .unwrap_or_else(|e| panic!("failed to read entry under {}: {e}", dir.display()));
            let path = entry.path();
            let file_type = entry
                .file_type()
                .unwrap_or_else(|e| panic!("failed to stat source entry {}: {e}", path.display()));
            if file_type.is_dir() {
                collect_rust_sources(&path, sources);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates_dir = manifest_dir
        .parent()
        .expect("khive-runtime manifest must live under the workspace crates directory");
    let mut handler_sources = Vec::new();
    let crate_entries = std::fs::read_dir(crates_dir).unwrap_or_else(|e| {
        panic!(
            "failed to enumerate pack crates under {}: {e}",
            crates_dir.display()
        )
    });
    for entry in crate_entries {
        let entry = entry
            .unwrap_or_else(|e| panic!("failed to read entry under {}: {e}", crates_dir.display()));
        if !entry
            .file_type()
            .unwrap_or_else(|e| {
                panic!("failed to stat crate entry {}: {e}", entry.path().display())
            })
            .is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with("khive-pack-")
        {
            continue;
        }
        collect_rust_sources(&entry.path().join("src"), &mut handler_sources);
    }
    handler_sources.sort_unstable();
    assert!(
        !handler_sources.is_empty(),
        "cross-pack Assertive census found no pack source files"
    );

    // verb name -> (owning pack, relative source path)
    let mut live_assertive = BTreeMap::<String, (String, String)>::new();
    for path in handler_sources {
        let relative = path
            .strip_prefix(crates_dir)
            .expect("pack source must be inside the workspace crates directory");
        let relative_path = relative.display().to_string();
        let crate_dir_name = relative
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        let owning_pack = crate_dir_name
            .strip_prefix("khive-pack-")
            .unwrap_or_else(|| {
                panic!("{relative_path}: expected a khive-pack-<name> crate directory")
            })
            .to_string();
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

        // Every literal occurrence of `HandlerDef {` is classified by
        // `classify_handler_def_occurrence` into a struct-literal field
        // block, a function/closure signature merely naming the type
        // (`-> &'static HandlerDef {`), or `Unclassified`. The count
        // assertion below sums the first two — counted only where each
        // branch actually fires, not once per occurrence found — against
        // `raw_token_count`, computed by an independent method
        // (`str::matches`). Unlike comparing two counts of the same
        // occurrences by the same method, this sum can fall short: an
        // `Unclassified` occurrence increments neither counter, so a
        // future shape neither branch recognizes fails this assertion
        // instead of silently passing (`classify_handler_def_occurrence_reports_unclassifiable_shapes`
        // proves the classifier itself reports `Unclassified` rather
        // than mis-slotting such a shape into one of the two branches).
        let raw_token_count = source.matches("HandlerDef {").count();
        let mut signature_count = 0usize;
        let mut struct_literal_count = 0usize;
        let mut unclassified: Vec<String> = Vec::new();
        let mut search_from = 0usize;
        while let Some(rel_pos) = source[search_from..].find("HandlerDef {") {
            let match_start = search_from + rel_pos;
            let match_end = match_start + "HandlerDef {".len();
            search_from = match_end;

            match classify_handler_def_occurrence(&source, match_start, match_end) {
                HandlerDefOccurrence::Signature => {
                    signature_count += 1;
                }
                HandlerDefOccurrence::StructLiteral {
                    name,
                    visibility,
                    category,
                } => {
                    struct_literal_count += 1;
                    if !visibility.contains("Visibility::Verb")
                        || !category.contains("VerbCategory::Assertive")
                    {
                        continue;
                    }

                    let prior = live_assertive
                        .insert(name.clone(), (owning_pack.clone(), relative_path.clone()));
                    assert!(
                        prior.is_none(),
                        "public Assertive verb {name:?} is declared in both {prior:?} and \
                             ({owning_pack:?}, {relative_path:?}); the registry surface must \
                             remain collision-free"
                    );
                }
                HandlerDefOccurrence::Unclassified { first_field_line } => {
                    unclassified.push(format!(
                        "byte {match_start} (first field line {first_field_line:?})"
                    ));
                }
            }
        }
        assert_eq!(
            signature_count + struct_literal_count,
            raw_token_count,
            "{relative_path}: found {raw_token_count} occurrences of the `HandlerDef {{` \
                 token but classified {signature_count} as signatures and \
                 {struct_literal_count} as struct literals; unclassified: {unclassified:?} — \
                 extend this census's parser to handle the shape instead of silently excluding it"
        );
    }

    let safe: BTreeSet<(&str, &str)> = VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS
        .iter()
        .copied()
        .collect();
    assert_eq!(
        safe.len(),
        VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS.len(),
        "ADMISSION_DEGRADE_SAFE_VERBS contains duplicate (pack, verb) pairs"
    );
    let safe_verbs: BTreeSet<&str> = safe.iter().map(|&(_, v)| v).collect();
    assert_eq!(
        safe_verbs.len(),
        safe.len(),
        "ADMISSION_DEGRADE_SAFE_VERBS names the same verb under two different packs; a verb \
             belongs to exactly one pack"
    );
    for &(pack, verb) in &safe {
        let live_owner = live_assertive
            .get(verb)
            .map(|(owning_pack, _)| owning_pack.as_str());
        assert_eq!(
            live_owner,
            Some(pack),
            "ADMISSION_DEGRADE_SAFE_VERBS claims {verb:?} is owned by pack {pack:?}, but its \
                 live declaration says otherwise (found: {live_owner:?})"
        );
    }
    let incidental: BTreeSet<&str> = KNOWN_ADMISSION_UNSAFE_VERBS.iter().copied().collect();
    assert!(
        safe_verbs.is_disjoint(&incidental),
        "a public Assertive verb cannot be both admission-degrade-safe and admission-unsafe: {:?}",
        safe_verbs.intersection(&incidental).collect::<Vec<_>>()
    );

    let classified: BTreeSet<&str> = safe_verbs.union(&incidental).copied().collect();
    let live: BTreeSet<&str> = live_assertive.keys().map(String::as_str).collect();
    assert_eq!(
        classified, live,
        "every public Assertive handler must be classified exactly once after a live-source \
             effect review; live declarations: {live_assertive:#?}"
    );
}

/// A pack whose `handlers()` counts every call, so a test can prove a
/// query touches (or does not touch) it after `VerbRegistryBuilder::build`
/// has already run once over every registered pack's handler list.
struct CountingHandlersPack {
    name: &'static str,
    handlers: &'static [HandlerDef],
    calls: Arc<AtomicUsize>,
}

impl Pack for CountingHandlersPack {
    const NAME: &'static str = "counting";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for CountingHandlersPack {
    fn name(&self) -> &str {
        self.name
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.handlers
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "pack": self.name, "verb": verb }))
    }
}

/// khive-oss#2311: before this fix, `admission_degrade_safe` resolved
/// the owning pack by scanning every registered pack's `handlers()` on
/// every audited dispatch (`self.packs.iter().find_map(|pack|
/// pack.handlers().iter().find(...))`). Eligibility is now decided once
/// in `VerbRegistryBuilder::build` into `VerbRegistry::degrade_safe_verbs`,
/// so `admission_degrade_safe` is a hash-set lookup that never touches
/// `handlers()` again. Proves it directly: `handlers()` is called some
/// number of times during `build()` (unique-name validation, the
/// reserved-envelope-arg check, `available_verbs`, and this
/// eligibility precompute all read it), but that count must not move
/// across any number of `admission_degrade_safe_probe` calls afterward
/// — for an allowlisted verb (a hit) and for one that is not (a miss).
#[test]
fn admission_degrade_safe_is_a_build_time_lookup_with_no_per_call_pack_scan() {
    static KG_HANDLERS: [HandlerDef; 1] = [HandlerDef {
        name: "list",
        description: "list widgets",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];

    let calls = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.register_trusted(CountingHandlersPack {
        name: "kg",
        handlers: &KG_HANDLERS,
        calls: calls.clone(),
    });
    let registry = builder.build().expect("registry builds");

    let after_build = calls.load(Ordering::SeqCst);
    assert!(
        after_build > 0,
        "build() is expected to read handlers() at least once (unique-name validation, \
             available_verbs, and the degrade-safe precompute all do); a count of 0 means this \
             test's premise (build-time reads happen) is wrong, not that the property under \
             test holds"
    );

    assert!(
        registry.admission_degrade_safe_probe("list"),
        "\"list\" is Assertive and (\"kg\", \"list\") is allowlisted under trusted \
             registration, so this must be a hit"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        after_build,
        "a hit must not re-scan any pack's handlers() — eligibility was already decided at \
             build() time"
    );

    assert!(
        !registry.admission_degrade_safe_probe("not-a-real-verb"),
        "an unregistered verb name is never eligible"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        after_build,
        "a miss must not re-scan any pack's handlers() either"
    );
}

#[test]
fn session_stats_admission_degrade_requires_trusted_session_owner() {
    static HANDLERS: [HandlerDef; 2] = [
        HandlerDef {
            name: "session.stats",
            description: "read session store statistics",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "session.vacuum",
            description: "compact session store",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        },
    ];

    let pack = || CountingHandlersPack {
        name: "session",
        handlers: &HANDLERS,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut trusted = VerbRegistryBuilder::new();
    trusted.register_trusted(pack());
    let trusted = trusted.build().expect("trusted session registry");
    assert!(trusted.admission_degrade_safe_probe("session.stats"));
    assert!(!trusted.admission_degrade_safe_probe("session.vacuum"));

    let mut untrusted = VerbRegistryBuilder::new();
    untrusted.register(pack());
    let untrusted = untrusted.build().expect("untrusted session registry");
    assert!(!untrusted.admission_degrade_safe_probe("session.stats"));
}

#[test]
fn verb_metadata_uses_build_time_index_across_packs() {
    static FIRST_HANDLERS: [HandlerDef; 2] = [
        HandlerDef {
            name: "get",
            description: "first public handler",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "internal.shared",
            description: "first internal handler",
            visibility: Visibility::Subhandler,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
    static SECOND_HANDLERS: [HandlerDef; 2] = [
        HandlerDef {
            name: "comm.send",
            description: "second public handler",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        },
        HandlerDef {
            name: "internal.shared",
            description: "second internal handler",
            visibility: Visibility::Subhandler,
            category: VerbCategory::Directive,
            params: &[],
        },
    ];

    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(CountingHandlersPack {
        name: "kg",
        handlers: &FIRST_HANDLERS,
        calls: first_calls.clone(),
    });
    builder.register(CountingHandlersPack {
        name: "comm",
        handlers: &SECOND_HANDLERS,
        calls: second_calls.clone(),
    });
    let registry = builder.build().expect("registry builds");
    let after_build = (
        first_calls.load(Ordering::SeqCst),
        second_calls.load(Ordering::SeqCst),
    );
    assert!(after_build.0 > 0 && after_build.1 > 0);

    assert_eq!(
        registry.presentation_policy_for("get"),
        VerbPresentationPolicy::AlwaysVerbose
    );
    assert_eq!(
        registry.presentation_policy_for("comm.send"),
        VerbPresentationPolicy::Standard
    );
    assert_eq!(
        registry.presentation_policy_for("missing"),
        VerbPresentationPolicy::Standard
    );
    assert_eq!(registry.verb_category("get"), Some(VerbCategory::Assertive));
    assert_eq!(
        registry.verb_category("comm.send"),
        Some(VerbCategory::Commissive)
    );
    assert_eq!(registry.verb_category("missing"), None);
    // Duplicate internal names retain the first pack's metadata.
    assert_eq!(
        registry.verb_category("internal.shared"),
        Some(VerbCategory::Assertive)
    );
    assert!(registry.is_subhandler_verb("internal.shared"));
    assert!(!registry.is_subhandler_verb("comm.send"));
    assert!(!registry.is_subhandler_verb("missing"));
    assert!(registry.has_verb("comm.send"));
    assert!(!registry.has_verb("missing"));
    assert_eq!(
        (
            first_calls.load(Ordering::SeqCst),
            second_calls.load(Ordering::SeqCst)
        ),
        after_build,
        "metadata lookups must not re-read either pack's handler slice"
    );
}

#[test]
fn read_replay_requires_trusted_owning_pack_for_every_opted_in_verb() {
    static HANDLERS: [HandlerDef; 5] = [
        HandlerDef {
            name: "stats",
            description: "replay eligibility fixture",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "comm.thread",
            description: "replay eligibility fixture",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "comm.inbox",
            description: "replay eligibility fixture",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "comm.unread",
            description: "replay eligibility fixture",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "comm.delivered",
            description: "replay eligibility fixture",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];

    for (owner, handlers) in [("kg", &HANDLERS[..1]), ("comm", &HANDLERS[1..])] {
        for (name, trusted, expected) in [
            (owner, true, true),
            (owner, false, false),
            ("custom-impostor", true, false),
        ] {
            let mut builder = VerbRegistryBuilder::new();
            let pack = CountingHandlersPack {
                name,
                handlers,
                calls: Arc::new(AtomicUsize::new(0)),
            };
            if trusted {
                builder.register_trusted(pack);
            } else {
                builder.register(pack);
            }
            let registry = builder.build().expect("replay fixture registry");
            for handler in handlers {
                assert_eq!(
                    registry.is_read_replay_safe(handler.name),
                    expected,
                    "verb={}, owner={name}, trusted={trusted}",
                    handler.name,
                );
            }
            assert!(!registry.is_read_replay_safe("unknown.read"));
        }
    }
}

#[test]
fn read_replay_excludes_reads_with_fresh_persisted_serve_or_search_rows() {
    for (owner, verb) in [("memory", "memory.recall"), ("kg", "search")] {
        let handler = Box::leak(Box::new([HandlerDef {
            name: verb,
            description: "side-effecting replay fixture",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }]));
        let mut builder = VerbRegistryBuilder::new();
        builder.register_trusted(CountingHandlersPack {
            name: owner,
            handlers: handler,
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let registry = builder.build().expect("side-effecting read fixture");
        assert!(!registry.is_read_replay_safe(verb), "{verb}");
    }
}

#[test]
fn read_replay_uses_effect_classification_instead_of_speech_act_category() {
    static HANDLERS: [HandlerDef; 3] = [
        HandlerDef {
            name: "stats",
            description: "category cannot override the reviewed operation effects",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        },
        HandlerDef {
            name: "create",
            description: "an Assertive category cannot make a Write replayable",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "unclassified_read",
            description: "an unknown operation remains ineligible",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
    let mut builder = VerbRegistryBuilder::new();
    builder.register_trusted(CountingHandlersPack {
        name: "kg",
        handlers: &HANDLERS,
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let registry = builder.build().expect("mutating fixture registry");
    assert!(registry.is_read_replay_safe("stats"));
    assert!(!registry.is_read_replay_safe("create"));
    assert!(!registry.is_read_replay_safe("unclassified_read"));
}

/// Re-derives each [`VerbRegistry::SIDE_EFFECTING_ASSERTIVE_VERBS`] entry's
/// classification from its owning pack's live source, the same
/// fail-closed pattern as `admission_degrade_safe_verbs_are_registered_assertive`
/// above: a category-only census would stay green even if a verb here
/// were quietly dropped to a different category, leaving
/// `is_retry_safe_after_frame_omission`'s exclusion pointed at a name
/// the category check would already exclude on its own — silently
/// removing test coverage for the exclusion list without anyone
/// noticing.
#[test]
fn side_effecting_assertive_verbs_are_registered_assertive() {
    let sources: &[(&str, &str)] = &[
        ("search", "/../khive-pack-kg/src/handler_defs.rs"),
        ("memory.recall", "/../khive-pack-memory/src/pack.rs"),
        ("telemetry.emit", "/../khive-pack-telemetry/src/pack.rs"),
        ("tool.check", "/../khive-pack-tool/src/vocab.rs"),
    ];
    assert_eq!(
        sources.len(),
        VerbRegistry::SIDE_EFFECTING_ASSERTIVE_VERBS.len(),
        "every entry in SIDE_EFFECTING_ASSERTIVE_VERBS needs a source-file mapping in \
             this census, or a newly added verb would go unchecked"
    );
    for (verb, rel_path) in sources {
        assert!(
            VerbRegistry::SIDE_EFFECTING_ASSERTIVE_VERBS.contains(verb),
            "census source table lists {verb:?}, which is missing from \
                 SIDE_EFFECTING_ASSERTIVE_VERBS; keep the table and the list in sync"
        );
        let path = format!("{}{rel_path}", env!("CARGO_MANIFEST_DIR"));
        let source =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
        let needle = format!("\n        name: \"{verb}\",");
        let name_pos = source.find(&needle).unwrap_or_else(|| {
            panic!(
                "side-effecting-assertive verb {verb:?} has no top-level `HandlerDef` \
                     in {path}; update SIDE_EFFECTING_ASSERTIVE_VERBS's source table or \
                     this census"
            )
        });
        let block_end = source[name_pos..]
            .find("HandlerDef {")
            .map(|offset| name_pos + offset)
            .unwrap_or(source.len());
        let block = &source[name_pos..block_end];
        assert!(
            block.contains("VerbCategory::Assertive"),
            "side-effecting-assertive verb {verb:?} is declared in {path} but is not \
                 VerbCategory::Assertive; is_retry_safe_after_frame_omission's exclusion \
                 list only needs to cover verbs the category check would otherwise wave \
                 through"
        );
    }
}

static COMM_PROBE_GRANTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static OTHER_PROBE_GRANTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Probe factories proving the channel-ingest grant is name-bounded.
/// khive-runtime's own test binary links no real pack crates, so the
/// `comm` name is free for the probe here.
struct CommProbeFactory;
struct OtherProbeFactory;
struct AccidentalZeroVerbFactory;

fn probe_pack(
    _runtime: KhiveRuntime,
    grant_flag: &'static std::sync::atomic::AtomicBool,
) -> Box<dyn PackRuntime> {
    struct ProbePack {
        grant_flag: &'static std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl PackRuntime for ProbePack {
        fn name(&self) -> &str {
            "probe"
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            &[]
        }
        fn accept_channel_ingest_capability(&self, _capability: ChannelIngestCapability) {
            self.grant_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: serde_json::Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<serde_json::Value, crate::RuntimeError> {
            Err(crate::RuntimeError::InvalidInput("probe".into()))
        }
    }
    Box::new(ProbePack { grant_flag })
}

impl PackFactory for CommProbeFactory {
    fn name(&self) -> &'static str {
        "comm"
    }
    fn intentionally_verbless(&self) -> bool {
        true
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        probe_pack(runtime, &COMM_PROBE_GRANTED)
    }
}

impl PackFactory for OtherProbeFactory {
    fn name(&self) -> &'static str {
        "grant-probe-other"
    }
    fn intentionally_verbless(&self) -> bool {
        true
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        probe_pack(runtime, &OTHER_PROBE_GRANTED)
    }
}

impl PackFactory for AccidentalZeroVerbFactory {
    fn name(&self) -> &'static str {
        "accidental-zero-verb"
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        probe_pack(runtime, &OTHER_PROBE_GRANTED)
    }
}

inventory::submit! { PackRegistration(&CommProbeFactory) }
inventory::submit! { PackRegistration(&OtherProbeFactory) }
inventory::submit! { PackRegistration(&AccidentalZeroVerbFactory) }

#[test]
fn channel_ingest_grant_reaches_only_allowlisted_pack_names() {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["comm".to_string(), "grant-probe-other".to_string()],
        runtime,
        &mut builder,
    )
    .expect("probe registration succeeds");
    assert!(
        COMM_PROBE_GRANTED.load(std::sync::atomic::Ordering::SeqCst),
        "the comm-named factory must receive the channel-ingest grant"
    );
    assert!(
        !OTHER_PROBE_GRANTED.load(std::sync::atomic::Ordering::SeqCst),
        "a factory outside CHANNEL_INGEST_CAPABLE_PACKS must never be granted"
    );
}

#[test]
fn declared_zero_verb_pack_requires_explicit_intent_metadata() {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    let error =
        PackRegistry::register_packs(&["accidental-zero-verb".to_string()], runtime, &mut builder)
            .expect_err("an unmarked zero-verb pack must fail registration");

    assert!(matches!(
        error,
        PackLoadError::NoPublicVerbs { ref pack } if pack == "accidental-zero-verb"
    ));
    assert!(
        error
            .to_string()
            .contains("intentionally_verbless() = true"),
        "operator error must name the explicit exemption: {error}"
    );
}

#[test]
fn multi_backend_loader_enforces_zero_verb_intent_metadata() {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    let error = PackRegistry::register_packs_with_runtimes(
        &["accidental-zero-verb".to_string()],
        &HashMap::new(),
        &runtime,
        &mut builder,
    )
    .expect_err("multi-backend registration must enforce the same invariant");

    assert!(matches!(
        error,
        PackLoadError::NoPublicVerbs { ref pack } if pack == "accidental-zero-verb"
    ));
}

#[test]
fn from_token_preserves_process_ref() {
    let with_ref = NamespaceToken::mint_authorized(
        Namespace::local(),
        ActorRef::new("agent", "provenance-carrier"),
    )
    .with_process_ref(Some("proc:origin-abc123".to_string()));
    let identity = RequestIdentity::from_token(&with_ref);
    assert_eq!(identity.process_ref.as_deref(), Some("proc:origin-abc123"));

    let without_ref = NamespaceToken::mint_authorized(
        Namespace::local(),
        ActorRef::new("agent", "provenance-absent"),
    );
    let identity = RequestIdentity::from_token(&without_ref);
    assert_eq!(identity.process_ref, None);
}

struct AlphaPack;

impl Pack for AlphaPack {
    const NAME: &'static str = "alpha";
    const NOTE_KINDS: &'static [&'static str] = &["memo", "log"];
    const ENTITY_KINDS: &'static [&'static str] = &["widget"];
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &["recall", "search"];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "create",
            description: "create a widget",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        },
        HandlerDef {
            name: "list",
            description: "list widgets",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for AlphaPack {
    fn name(&self) -> &str {
        AlphaPack::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        AlphaPack::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        AlphaPack::ENTITY_KINDS
    }
    fn brain_consumer_kinds(&self) -> &'static [&'static str] {
        AlphaPack::BRAIN_CONSUMER_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        AlphaPack::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
    }
}

#[derive(Debug)]
struct GateErrorTrackingPack {
    invoked: Arc<AtomicUsize>,
}

impl Pack for GateErrorTrackingPack {
    const NAME: &'static str = "gate_error_tracking";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "guarded",
        description: "track whether gate-error dispatch reaches the handler",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for GateErrorTrackingPack {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.invoked.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({"invoked": true}))
    }
}

/// A pack whose `dispatch` sleeps for a fixed, generous duration so
/// `duration_us` regression tests (ADR-103 Stage 1) have a reliably
/// nonzero, non-flaky measured dispatch time to assert against.
struct SleepingPack;

impl Pack for SleepingPack {
    const NAME: &'static str = "sleeping";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "slow_op",
        description: "sleeps before returning",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for SleepingPack {
    fn name(&self) -> &str {
        SleepingPack::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        SleepingPack::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        SleepingPack::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        SleepingPack::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Ok(serde_json::json!({ "pack": "sleeping", "verb": verb }))
    }
}

struct BetaPack;

impl Pack for BetaPack {
    const NAME: &'static str = "beta";
    const NOTE_KINDS: &'static [&'static str] = &["alert"];
    const ENTITY_KINDS: &'static [&'static str] = &["widget", "gadget"];
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &["search", "knowledge_compose"];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "notify",
            description: "send alert",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        },
        // "create" is Subhandler so it does NOT collide with AlphaPack's
        // Verb-visibility "create" — subhandlers are pack-internal and
        // excluded from cross-pack collision detection.
        HandlerDef {
            name: "create",
            description: "beta internal create (subhandler)",
            visibility: Visibility::Subhandler,
            category: VerbCategory::Commissive,
            params: &[],
        },
    ];
}

/// Build a registry with AlphaPack + BetaPack.
///
/// BetaPack's `create` is Subhandler so there is no Verb-visibility
/// collision with AlphaPack's `create` Verb. Tests that need a collision
/// use `build_colliding_registry()` instead.
fn build_registry() -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.register(BetaPack);
    builder.build().expect("registry builds without collision")
}

/// Build a registry with two packs that declare the same Verb-visibility
/// handler — used to test that `VerbCollision` is raised at build time.
struct CollidingPack;

impl Pack for CollidingPack {
    const NAME: &'static str = "colliding";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "create",
        description: "duplicate Verb-visibility create",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for CollidingPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "pack": "colliding", "verb": verb }))
    }
}

struct ReservedEnvelopeParamPack;

impl Pack for ReservedEnvelopeParamPack {
    const NAME: &'static str = "reserved-envelope-param";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "broken.serve",
        description: "declares a transport-owned argument",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[ParamDef {
            name: "presentation",
            param_type: "object",
            required: false,
            description: "invalid collision with the request envelope",
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    }];
}

#[async_trait]
impl PackRuntime for ReservedEnvelopeParamPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        unreachable!("invalid handler metadata must fail before dispatch")
    }
}

#[async_trait]
impl PackRuntime for BetaPack {
    fn name(&self) -> &str {
        BetaPack::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        BetaPack::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        BetaPack::ENTITY_KINDS
    }
    fn brain_consumer_kinds(&self) -> &'static [&'static str] {
        BetaPack::BRAIN_CONSUMER_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        BetaPack::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "pack": "beta", "verb": verb }))
    }
}

#[tokio::test]
async fn dispatch_routes_to_correct_pack() {
    let reg = build_registry();

    let res = reg.dispatch("list", Value::Null).await.unwrap();
    assert_eq!(res["pack"], "alpha");

    let res = reg.dispatch("notify", Value::Null).await.unwrap();
    assert_eq!(res["pack"], "beta");
}

/// Two packs declaring the same `Visibility::Verb` handler must be
/// rejected at build time — the old "first registered wins" behaviour is
/// replaced by a boot error.
#[test]
fn verb_collision_is_boot_time_error() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.register(CollidingPack);
    let err = builder
        .build()
        .err()
        .expect("duplicate Verb-visibility handler must be rejected at build time");
    assert!(
        matches!(err, RuntimeError::VerbCollision { ref verb, .. } if verb == "create"),
        "expected VerbCollision for 'create', got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("create"),
        "error must name the colliding verb: {msg}"
    );
    assert!(
        msg.contains("alpha") || msg.contains("colliding"),
        "error must name one of the conflicting packs: {msg}"
    );
}

#[test]
fn reserved_request_envelope_param_is_boot_time_error() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(ReservedEnvelopeParamPack);
    let error = builder
        .build()
        .err()
        .expect("transport-owned parameter names must fail registry construction");
    assert!(
        matches!(
            error,
            RuntimeError::ReservedEnvelopeParam {
                ref pack,
                ref verb,
                ref param,
            } if pack == "reserved-envelope-param"
                && verb == "broken.serve"
                && param == "presentation"
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn reserved_request_envelope_param_is_boot_time_error_for_subhandler() {
    struct ReservedEnvelopeSubhandlerParamPack;

    impl Pack for ReservedEnvelopeSubhandlerParamPack {
        const NAME: &'static str = "reserved-envelope-subhandler-param";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "broken.internal",
            description: "declares a transport-owned argument on an internal handler",
            visibility: Visibility::Subhandler,
            category: VerbCategory::Assertive,
            params: &[ParamDef {
                name: "presentation_per_op",
                param_type: "string",
                required: false,
                description: "invalid collision with the request envelope",
                resolution_mode: IdResolutionMode::NotApplicable,
            }],
        }];
    }

    #[async_trait]
    impl PackRuntime for ReservedEnvelopeSubhandlerParamPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            unreachable!("invalid handler metadata must fail before dispatch")
        }
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(ReservedEnvelopeSubhandlerParamPack);
    let error = builder
        .build()
        .err()
        .expect("transport-owned parameter names must fail registry construction");
    assert!(
        matches!(
            error,
            RuntimeError::ReservedEnvelopeParam {
                ref pack,
                ref verb,
                ref param,
            } if pack == "reserved-envelope-subhandler-param"
                && verb == "broken.internal"
                && param == "presentation_per_op"
        ),
        "unexpected error: {error:?}"
    );
}

/// Subhandler-visibility handlers with the same name across packs are NOT
/// a collision — they are pack-internal and excluded from cross-pack
/// collision detection.
#[test]
fn subhandler_same_name_across_packs_is_not_a_collision() {
    struct SubhandlerPack;
    impl Pack for SubhandlerPack {
        const NAME: &'static str = "subhandler_pack";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "create",
            description: "internal create",
            visibility: Visibility::Subhandler,
            category: VerbCategory::Commissive,
            params: &[],
        }];
    }
    #[async_trait]
    impl PackRuntime for SubhandlerPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
            _: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({"pack": "subhandler_pack", "verb": verb}))
        }
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack); // AlphaPack has Verb "create"
    builder.register(SubhandlerPack); // SubhandlerPack has Subhandler "create" — no collision
    builder
        .build()
        .expect("subhandler same name must NOT be a collision");
}

#[tokio::test]
async fn dispatch_unknown_verb_returns_error() {
    let reg = build_registry();

    let err = reg.dispatch("explode", Value::Null).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("explode"));
    assert!(msg.contains("create"));
}

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

#[test]
fn brain_consumer_kind_wildcard_is_rejected_at_build_time() {
    struct WildcardConsumerPack;

    impl khive_types::Pack for WildcardConsumerPack {
        const NAME: &'static str = "wildcard-consumer";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &["*"];
        const HANDLERS: &'static [HandlerDef] = &[];
    }

    #[async_trait]
    impl PackRuntime for WildcardConsumerPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn brain_consumer_kinds(&self) -> &'static [&'static str] {
            Self::BRAIN_CONSUMER_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(Value::Null)
        }
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(WildcardConsumerPack);
    let Err(RuntimeError::InvalidInput(message)) = builder.build() else {
        panic!("registry must reject a pack-declared brain wildcard");
    };
    assert!(message.contains("wildcard-consumer"), "{message}");
    assert!(message.contains("registry-owned"), "{message}");
}

#[test]
fn note_kind_duplicate_rejected_at_build_time() {
    struct DupPack;

    impl khive_types::Pack for DupPack {
        const NAME: &'static str = "dup";
        // "memo" is already declared by AlphaPack — must be rejected at build.
        const NOTE_KINDS: &'static [&'static str] = &["memo"];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
    }

    #[async_trait]
    impl PackRuntime for DupPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(Value::Null)
        }
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.register(DupPack);
    let err = builder
        .build()
        .err()
        .expect("duplicate note kind must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("memo"),
        "error must name the duplicate kind: {msg}"
    );
    assert!(
        msg.contains("alpha") || msg.contains("dup"),
        "error must name one of the conflicting packs: {msg}"
    );
}

#[test]
fn entity_kinds_are_deduplicated() {
    let reg = build_registry();
    let kinds = reg.all_entity_kinds();
    assert_eq!(kinds, vec!["widget", "gadget"]);
}

// ---- ENTITY_TYPES composition (pack-declared entity-type subtypes) ----

struct GammaPack;

impl Pack for GammaPack {
    const NAME: &'static str = "gamma";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const ENTITY_TYPES: &'static [EntityTypeDef] = &[EntityTypeDef {
        kind: khive_types::EntityKind::Document,
        type_name: "gamma_report",
        aliases: &["gamma_rep"],
    }];
}

#[async_trait]
impl PackRuntime for GammaPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn entity_types(&self) -> &'static [EntityTypeDef] {
        Self::ENTITY_TYPES
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "pack": "gamma", "verb": verb }))
    }
}

/// Builtin-only behavior is unchanged when no pack declares extras:
/// `all_entity_types()` is empty, and composing it with the builtin
/// registry resolves exactly like `EntityTypeRegistry::builtin()`.
#[test]
fn all_entity_types_empty_when_no_pack_declares_extras() {
    let reg = build_registry(); // AlphaPack + BetaPack — neither declares ENTITY_TYPES.
    assert!(reg.all_entity_types().is_empty());
    let composed = khive_types::EntityTypeRegistry::with_extra(reg.all_entity_types());
    let resolved = composed
        .resolve(khive_types::EntityKind::Document, Some("paper"))
        .expect("builtin paper subtype must still resolve");
    assert_eq!(resolved.entity_type.as_deref(), Some("paper"));
}

/// A pack-declared entity type validates through the composed registry,
/// and builtin subtypes remain resolvable alongside it.
#[test]
fn pack_declared_entity_type_validates_through_composed_registry() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.register(GammaPack);
    let reg = builder.build().expect("registry builds");

    let extras = reg.all_entity_types();
    assert_eq!(extras.len(), 1);

    let composed = khive_types::EntityTypeRegistry::with_extra(extras);
    let resolved = composed
        .resolve(khive_types::EntityKind::Document, Some("gamma_rep"))
        .expect("pack-declared alias must resolve through the composed registry");
    assert_eq!(resolved.entity_type.as_deref(), Some("gamma_report"));

    let builtin_resolved = composed
        .resolve(khive_types::EntityKind::Document, Some("paper"))
        .expect("builtin subtype must remain resolvable when a pack adds extras");
    assert_eq!(builtin_resolved.entity_type.as_deref(), Some("paper"));

    composed
        .resolve(khive_types::EntityKind::Document, Some("nonexistent_type"))
        .expect_err("undeclared entity_type must still be rejected");
}

/// Two packs declaring the exact same `(kind, type_name)` subtype are
/// rejected at `build()` — ADR-001's registry-ownership collision rule
/// ("same `(base_kind, canonical_name)` from two different packs = boot
/// error") — instead of silently resolving via registration order the
/// way `EntityTypeRegistry::with_extra`'s hard-`insert` semantics would.
#[test]
fn overlapping_pack_declared_entity_types_reject_at_boot() {
    struct DeltaPack;
    impl Pack for DeltaPack {
        const NAME: &'static str = "delta";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
        const ENTITY_TYPES: &'static [EntityTypeDef] = &[EntityTypeDef {
            kind: khive_types::EntityKind::Document,
            type_name: "gamma_report",
            aliases: &["gamma_rep"],
        }];
    }
    #[async_trait]
    impl PackRuntime for DeltaPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        fn entity_types(&self) -> &'static [EntityTypeDef] {
            Self::ENTITY_TYPES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "delta", "verb": verb }))
        }
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(GammaPack);
    builder.register(DeltaPack);
    let err = builder
        .build()
        .err()
        .expect("overlapping ENTITY_TYPES declarations must fail at build, not silently compose");

    let msg = err.to_string();
    assert!(
        msg.contains("gamma") && msg.contains("delta"),
        "collision error must name both contributing packs: {msg}"
    );
    assert!(
        msg.contains("gamma_report"),
        "collision error must name the colliding entity_type key: {msg}"
    );
}

#[test]
fn entity_subtype_cannot_shadow_a_note_kind_at_composition() {
    struct CollisionPack;
    impl Pack for CollisionPack {
        const NAME: &'static str = "cross_kind_collision";
        const NOTE_KINDS: &'static [&'static str] = &["reference"];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
        const ENTITY_TYPES: &'static [EntityTypeDef] = &[EntityTypeDef {
            kind: khive_types::EntityKind::Document,
            type_name: "reference",
            aliases: &[],
        }];
    }
    #[async_trait]
    impl PackRuntime for CollisionPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        fn entity_types(&self) -> &'static [EntityTypeDef] {
            Self::ENTITY_TYPES
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(Value::Null)
        }
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(CollisionPack);
    let error = builder
        .build()
        .err()
        .expect("a note-kind/entity-subtype collision must refuse composition");
    let message = error.to_string();
    assert!(message.contains("entity subtype") && message.contains("note kind"));
    assert!(message.contains("reference") && message.contains("cross_kind_collision"));
}

#[test]
fn builtin_entity_subtype_cannot_shadow_a_note_kind_at_composition() {
    macro_rules! note_pack {
        ($name:ident, $pack_name:literal, $note_kind:literal) => {
            struct $name;
            impl Pack for $name {
                const NAME: &'static str = $pack_name;
                const NOTE_KINDS: &'static [&'static str] = &[$note_kind];
                const ENTITY_KINDS: &'static [&'static str] = &[];
                const HANDLERS: &'static [HandlerDef] = &[];
            }
            #[async_trait]
            impl PackRuntime for $name {
                fn name(&self) -> &str {
                    Self::NAME
                }
                fn note_kinds(&self) -> &'static [&'static str] {
                    Self::NOTE_KINDS
                }
                fn entity_kinds(&self) -> &'static [&'static str] {
                    Self::ENTITY_KINDS
                }
                fn handlers(&self) -> &'static [HandlerDef] {
                    Self::HANDLERS
                }
                async fn dispatch(
                    &self,
                    _verb: &str,
                    _params: Value,
                    _registry: &VerbRegistry,
                    _token: &NamespaceToken,
                ) -> Result<Value, RuntimeError> {
                    Ok(Value::Null)
                }
            }
        };
    }

    note_pack!(
        ResearchReportNotePack,
        "builtin_report_collision",
        "research_report"
    );
    note_pack!(PreprintNotePack, "builtin_preprint_collision", "preprint");
    note_pack!(
        SpacedReportNotePack,
        "builtin_spaced_collision",
        "Research Report"
    );

    fn expect_collision<P: Pack + PackRuntime + 'static>(pack: P, owner: &str, token: &str) {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(pack);
        let error = builder
            .build()
            .err()
            .expect("a built-in subtype/note-kind collision must refuse composition");
        let message = error.to_string();
        assert!(
            message.contains("builtin") && message.contains(owner) && message.contains(token),
            "collision must name the built-in subtype, note pack, and normalized token: {message}"
        );
    }
    expect_collision(
        ResearchReportNotePack,
        "builtin_report_collision",
        "research_report",
    );
    expect_collision(PreprintNotePack, "builtin_preprint_collision", "preprint");
    expect_collision(
        SpacedReportNotePack,
        "builtin_spaced_collision",
        "research_report",
    );
}

// ---- Gate wiring ----

use khive_gate::{Gate, GateError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Default, Debug)]
struct CountingGate {
    calls: AtomicUsize,
    deny_verb: Option<&'static str>,
}

impl Gate for CountingGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if Some(req.verb.as_str()) == self.deny_verb {
            Ok(GateDecision::deny(format!("test deny for {}", req.verb)))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

#[tokio::test]
async fn dispatch_consults_the_gate() {
    let gate = Arc::new(CountingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", Value::Null).await.unwrap();
    reg.dispatch("create", Value::Null).await.unwrap();
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        2,
        "gate should be consulted once per dispatch"
    );
}

#[tokio::test]
async fn dispatch_returns_permission_denied_on_deny_v03() {
    let gate = Arc::new(CountingGate {
        calls: AtomicUsize::new(0),
        deny_verb: Some("create"),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    // Gate denies — dispatch now returns PermissionDenied (hard enforcement).
    let err = reg.dispatch("create", Value::Null).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::PermissionDenied { ref verb, .. } if verb == "create"),
        "expected PermissionDenied, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("create"),
        "error message must name the verb: {msg}"
    );
    assert!(
        msg.contains("test deny for create"),
        "error message must carry the deny reason: {msg}"
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn denied_dispatch_returns_the_id_of_its_committed_gate_denied_row() {
    let gate = Arc::new(CountingGate {
        calls: AtomicUsize::new(0),
        deny_verb: Some("create"),
    });
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let err = reg.dispatch("create", Value::Null).await.unwrap_err();
    let RuntimeError::PermissionDenied { receipt, .. } = err else {
        panic!("expected PermissionDenied, got {err:?}");
    };
    assert_eq!(
        receipt.audit_outcome,
        crate::error::DenialAuditOutcome::Committed
    );
    let audit_event_id = receipt
        .audit_event_id
        .expect("a committed row carries its id");
    let events = store.events.lock().unwrap();
    let row = events
        .iter()
        .find(|e| e.id == audit_event_id)
        .expect("the receipt names a row the store holds");
    assert_eq!(row.outcome, EventOutcome::Denied);
    assert_eq!(row.kind, EventKind::Audit);
    assert_eq!(row.verb, "create");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn denied_dispatch_masks_secret_shaped_deny_reason_in_stored_event() {
    // Falsifiable arm for the audit-masking fix (khive#2944): this deny
    // reason embeds a fake credential in a shape the write-time secret
    // gate recognizes (`scheme://user:pass@host`). Deleting the masking
    // call at this call site — `dispatch_with_disposition`'s own
    // `AuditEvent` construction — turns this test red: the raw
    // credential would reach the stored row unmasked.
    #[derive(Debug)]
    struct SecretDenyGate;
    impl Gate for SecretDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            let reason = "postgres://svc:not-a-real-secret@internal-host in denied request"; // gitleaks:allow
            Ok(GateDecision::deny(reason))
        }
    }

    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(SecretDenyGate));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let _ = reg.dispatch("list", Value::Null).await.unwrap_err();

    let events = store.events.lock().unwrap();
    assert_eq!(events.len(), 1, "exactly one denial row must commit");
    let stored_reason = events[0].payload["deny_reason"]
        .as_str()
        .expect("deny_reason must be a string on the stored row");
    // Non-vacuity: the row actually captured content, so the negative
    // assertion below cannot pass merely because nothing was read.
    assert!(!stored_reason.is_empty());
    assert!(
        stored_reason.contains("in denied request"),
        "non-secret prose must survive masking: {stored_reason:?}"
    );
    assert!(
        !stored_reason.contains("not-a-real-secret"),
        "the durable row must never carry the raw credential: {stored_reason:?}"
    );
    assert!(
        stored_reason.contains("***MASKED***"),
        "the durable row must record that a credential was redacted: {stored_reason:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn denied_dispatch_without_an_event_store_reports_no_store() {
    let gate = Arc::new(CountingGate {
        calls: AtomicUsize::new(0),
        deny_verb: Some("create"),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate);
    let reg = builder.build().expect("registry builds");

    let err = reg.dispatch("create", Value::Null).await.unwrap_err();
    assert!(
        matches!(
            err,
            RuntimeError::PermissionDenied { ref receipt, .. }
                if **receipt == crate::error::DenialReceipt::no_store()
        ),
        "expected a no-store receipt, got {err:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[serial_test::serial(audit_append_failures)]
#[serial_test::serial(audit_obligation_append_failures)]
async fn denied_dispatch_whose_row_fails_to_commit_still_refuses_and_names_no_row() {
    let gate = Arc::new(CountingGate {
        calls: AtomicUsize::new(0),
        deny_verb: Some("create"),
    });
    let store = Arc::new(MemoryEventStore {
        fail_appends: true,
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let err = reg.dispatch("create", Value::Null).await.unwrap_err();
    let RuntimeError::PermissionDenied { verb, receipt, .. } = err else {
        panic!("expected PermissionDenied, got {err:?}");
    };
    assert_eq!(verb, "create");
    assert_eq!(
        receipt.audit_event_id, None,
        "a row that did not commit is not cited"
    );
    assert_eq!(
        receipt.audit_outcome,
        crate::error::DenialAuditOutcome::NotCommitted("store_failure")
    );
    assert!(store.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dispatch_allow_verb_succeeds_even_with_deny_gate_for_other_verb() {
    // Deny only "create" — "list" must still work.
    let gate = Arc::new(CountingGate {
        calls: AtomicUsize::new(0),
        deny_verb: Some("create"),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    let res = reg.dispatch("list", Value::Null).await.unwrap();
    assert_eq!(res["pack"], "alpha");
}

#[tokio::test]
async fn dispatch_uses_allow_all_gate_by_default() {
    // No `with_gate` call — builder should use `AllowAllGate` so dispatch works.
    let reg = build_registry();
    let res = reg.dispatch("list", Value::Null).await.unwrap();
    assert_eq!(res["pack"], "alpha");
}

// Captures the namespace each call sees so we can assert what the gate
// actually receives, rather than assuming a hard-wired `default_ns()`.
#[derive(Default, Debug)]
struct NamespaceCapturingGate {
    seen: std::sync::Mutex<Vec<String>>,
}

impl Gate for NamespaceCapturingGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        self.seen
            .lock()
            .unwrap()
            .push(req.namespace.as_str().to_string());
        Ok(GateDecision::allow())
    }
}

#[tokio::test]
async fn dispatch_propagates_params_namespace_to_gate() {
    let gate = Arc::new(NamespaceCapturingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    builder.with_default_namespace("tenant-x");
    let reg = builder.build().expect("registry builds");

    // Explicit namespace in params wins.
    reg.dispatch("list", serde_json::json!({"namespace": "tenant-y"}))
        .await
        .unwrap();
    // Missing namespace → registry default.
    reg.dispatch("list", Value::Null).await.unwrap();
    // Empty string is rejected: Namespace::parse("") fails → InvalidInput error.
    let err = reg
        .dispatch("list", serde_json::json!({"namespace": ""}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "empty namespace must return InvalidInput, got {err:?}"
    );

    let seen = gate.seen.lock().unwrap().clone();
    assert_eq!(seen, vec!["tenant-y", "tenant-x"]);
}

#[tokio::test]
async fn dispatch_falls_back_to_local_when_no_default_set() {
    // Builder default mirrors `Namespace::default_ns()`.
    let gate = Arc::new(NamespaceCapturingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", Value::Null).await.unwrap();
    let seen = gate.seen.lock().unwrap().clone();
    assert_eq!(seen, vec!["local"]);
}

/// A present-but-malformed `namespace` value must never reach the gate as
/// the default namespace. Table-driven over every
/// non-string JSON type; the gate-spy proves no call is ever recorded (the
/// dispatch must short-circuit with `InvalidInput` before `GateRequest` is
/// built), so the default namespace can never appear as a coerced stand-in.
#[tokio::test]
async fn namespace_null_rejected_not_coerced() {
    let cases: Vec<(&str, Value)> = vec![
        ("null", Value::Null),
        ("number", serde_json::json!(42)),
        ("boolean", serde_json::json!(true)),
        ("array", serde_json::json!(["local"])),
        ("object", serde_json::json!({"ns": "local"})),
    ];

    for (label, ns_value) in cases {
        let gate = Arc::new(NamespaceCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        builder.with_default_namespace("tenant-x");
        let reg = builder.build().expect("registry builds");

        let err = reg
            .dispatch("list", serde_json::json!({"namespace": ns_value}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "case {label}: expected InvalidInput, got {err:?}"
        );

        // The gate must never have been consulted for this malformed input —
        // proves no Allow decision (and therefore no default-namespace write)
        // can ever be reached for it.
        let seen = gate.seen.lock().unwrap().clone();
        assert!(
            seen.is_empty(),
            "case {label}: gate must not be consulted for malformed namespace, saw {seen:?}"
        );
    }
}

// ---- Audit event emission ----

use khive_gate::{AuditDecision, AuditEvent, Obligation};

/// A gate that records every audit event emitted via from_check.
#[derive(Default, Debug)]
struct AuditCapturingGate {
    events: std::sync::Mutex<Vec<AuditEvent>>,
    deny_verb: Option<&'static str>,
}

impl Gate for AuditCapturingGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        let decision = if Some(req.verb.as_str()) == self.deny_verb {
            GateDecision::deny("test deny")
        } else {
            GateDecision::allow_with(vec![Obligation::Audit {
                tag: format!("{}.check", req.verb),
            }])
        };
        // Capture what dispatch will also emit.
        let ev = AuditEvent::from_check(req, &decision, self.impl_name());
        self.events.lock().unwrap().push(ev);
        Ok(decision)
    }

    fn impl_name(&self) -> &'static str {
        "AuditCapturingGate"
    }
}

#[tokio::test]
async fn dispatch_emits_one_audit_event_per_call() {
    let gate = Arc::new(AuditCapturingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", Value::Null).await.unwrap();
    reg.dispatch("create", Value::Null).await.unwrap();

    let evs = gate.events.lock().unwrap();
    assert_eq!(evs.len(), 2, "exactly one audit event per dispatch call");
}

#[tokio::test]
async fn dispatch_audit_event_allow_carries_obligations() {
    let gate = Arc::new(AuditCapturingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", Value::Null).await.unwrap();

    let evs = gate.events.lock().unwrap();
    let ev = &evs[0];
    assert_eq!(ev.verb, "list");
    assert_eq!(ev.decision, AuditDecision::Allow);
    assert!(ev.deny_reason.is_none());
    assert_eq!(ev.obligations.len(), 1);
    assert_eq!(ev.gate_impl, "AuditCapturingGate");
}

#[tokio::test]
async fn dispatch_audit_event_deny_carries_reason() {
    let gate = Arc::new(AuditCapturingGate {
        events: Default::default(),
        deny_verb: Some("create"),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    // Gate denies — dispatch returns PermissionDenied (hard enforcement).
    // The audit event is still recorded (captured inside the gate impl).
    let err = reg.dispatch("create", Value::Null).await.unwrap_err();
    assert!(matches!(err, RuntimeError::PermissionDenied { .. }));

    let evs = gate.events.lock().unwrap();
    let ev = &evs[0];
    assert_eq!(ev.verb, "create");
    assert_eq!(ev.decision, AuditDecision::Deny);
    assert_eq!(ev.deny_reason.as_deref(), Some("test deny"));
    assert!(ev.obligations.is_empty());
}

#[tokio::test]
async fn dispatch_audit_event_fields_match_gate_request() {
    let gate = Arc::new(AuditCapturingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    builder.with_default_namespace("tenant-z");
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", serde_json::json!({"namespace": "tenant-q"}))
        .await
        .unwrap();

    let evs = gate.events.lock().unwrap();
    let ev = &evs[0];
    // Namespace from params wins.
    assert_eq!(ev.namespace, "tenant-q");
    assert_eq!(ev.verb, "list");
    assert_eq!(ev.actor.kind, "anonymous");
}

// ---- Actor attribution threading into gate request (ADR-057) ----

/// A gate spy that captures the raw `GateRequest` it receives.
#[derive(Default, Debug)]
struct ActorCapturingGate {
    requests: std::sync::Mutex<Vec<GateRequest>>,
}

impl Gate for ActorCapturingGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        self.requests.lock().unwrap().push(req.clone());
        Ok(GateDecision::allow())
    }
}

/// When `actor_id` is configured, the gate request carries that actor, not
/// anonymous. This exercises the ADR-057 attribution fix: the gate can
/// distinguish an agent caller from an unauthenticated caller.
#[tokio::test]
async fn gate_request_carries_configured_actor_when_actor_id_is_set() {
    let gate = Arc::new(ActorCapturingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    builder.with_actor_id(Some("team-abc:implementer".to_string()));
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", Value::Null).await.unwrap();

    let reqs = gate.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(
        req.actor.kind, "actor",
        "gate request must carry kind='actor' when actor_id is configured"
    );
    assert_eq!(
        req.actor.id, "team-abc:implementer",
        "gate request must carry the configured actor id"
    );
}

/// When no `actor_id` is configured, the gate request still receives the
/// anonymous actor (no regression to the party-line default).
#[tokio::test]
async fn gate_request_carries_anonymous_when_no_actor_id_configured() {
    let gate = Arc::new(ActorCapturingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate.clone());
    // actor_id left at default (None).
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", Value::Null).await.unwrap();

    let reqs = gate.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(
        req.actor.kind, "anonymous",
        "gate request must carry anonymous actor when no actor_id is configured"
    );
    assert_eq!(req.actor.id, "local");
}

/// A pack that records the `ActorRef` carried by the `NamespaceToken` it
/// is dispatched with, so tests can compare it against the gate's actor.
struct TokenCapturingPack {
    actors: Arc<std::sync::Mutex<Vec<khive_gate::ActorRef>>>,
}

impl Pack for TokenCapturingPack {
    const NAME: &'static str = "alpha";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = AlphaPack::HANDLERS;
}

#[async_trait]
impl PackRuntime for TokenCapturingPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.actors.lock().unwrap().push(token.actor().clone());
        Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
    }
}

/// The gate's actor and the storage token's actor must be the exact same
/// resolved value: both come from one `resolve_actor` call
/// (`resolved_actor`) instead of two independently hand-synchronized
/// `match` expressions, so a future edit to one copy but not the other
/// cannot silently desynchronize "who the gate thinks the caller is" from
/// "who the storage layer thinks the caller is". Reintroducing a second
/// independent actor-resolution copy for the token would regress this and
/// this test would catch it.
#[tokio::test]
async fn gate_actor_and_token_actor_are_identical_when_actor_id_is_set() {
    let gate = Arc::new(ActorCapturingGate::default());
    let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pack = TokenCapturingPack {
        actors: actors.clone(),
    };
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    builder.with_gate(gate.clone());
    builder.with_actor_id(Some("actor-alpha".to_string()));
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", Value::Null).await.unwrap();

    let reqs = gate.requests.lock().unwrap();
    let gate_actor = reqs[0].actor.clone();
    drop(reqs);

    let captured = actors.lock().unwrap();
    let token_actor = captured[0].clone();

    assert_eq!(
        gate_actor.kind, token_actor.kind,
        "gate request actor and storage token actor must carry the same kind"
    );
    assert_eq!(
        gate_actor.id, token_actor.id,
        "gate request actor and storage token actor must carry the same id"
    );
    assert_eq!(gate_actor.id, "actor-alpha");
}

struct VisibilityCapturingPack {
    visible: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

impl Pack for VisibilityCapturingPack {
    const NAME: &'static str = "alpha";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = AlphaPack::HANDLERS;
}

#[async_trait]
impl PackRuntime for VisibilityCapturingPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.visible.lock().unwrap().push(
            token
                .visible_namespace_strs()
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
    }
}

/// ADR-007 Rev 4 Rule 3b at the token seam: a per-request identity that
/// names a non-`local` actor reads that actor's namespace by default even
/// when its `visible_namespaces` list is empty, the actor appears once when
/// the list already names it, an anonymous identity keeps exactly `local`,
/// and an explicit `namespace=` stays a precise single-namespace scope.
#[tokio::test]
async fn dispatch_with_identity_folds_the_actor_namespace_into_default_reads() {
    let visible = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(VisibilityCapturingPack {
        visible: visible.clone(),
    });
    let reg = builder.build().expect("registry builds");
    let identity = |actor: Option<&str>, listed: &[&str]| RequestIdentity {
        namespace: "local".to_string(),
        actor_id: actor.map(str::to_string),
        visible_namespaces: listed.iter().map(|ns| ns.to_string()).collect(),
        ..Default::default()
    };

    reg.dispatch_with_identity(
        "list",
        Value::Null,
        Some(identity(Some("lambda:probe"), &[])),
    )
    .await
    .unwrap();
    reg.dispatch_with_identity(
        "list",
        Value::Null,
        Some(identity(Some("lambda:probe"), &["lambda:probe"])),
    )
    .await
    .unwrap();
    reg.dispatch_with_identity("list", Value::Null, Some(identity(None, &[])))
        .await
        .unwrap();
    reg.dispatch_with_identity(
        "list",
        serde_json::json!({"namespace": "lambda:probe"}),
        Some(identity(Some("lambda:probe"), &[])),
    )
    .await
    .unwrap();

    let captured = visible.lock().unwrap();
    let count = |set: &Vec<String>, ns: &str| set.iter().filter(|s| s.as_str() == ns).count();
    assert_eq!(
        count(&captured[0], "lambda:probe"),
        1,
        "empty list: {:?}",
        captured[0]
    );
    assert_eq!(
        count(&captured[0], "local"),
        1,
        "empty list: {:?}",
        captured[0]
    );
    assert_eq!(
        count(&captured[1], "lambda:probe"),
        1,
        "listed once: {:?}",
        captured[1]
    );
    assert_eq!(
        captured[2],
        vec!["local".to_string()],
        "anonymous keeps exactly local"
    );
    assert_eq!(
        captured[3],
        vec!["lambda:probe".to_string()],
        "explicit namespace is a precise scope, never widened"
    );
}

/// Same identity check with no configured `actor_id`: both the gate and
/// the storage token must independently land on `ActorRef::anonymous()`.
#[tokio::test]
async fn gate_actor_and_token_actor_are_identical_when_anonymous() {
    let gate = Arc::new(ActorCapturingGate::default());
    let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pack = TokenCapturingPack {
        actors: actors.clone(),
    };
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", Value::Null).await.unwrap();

    let reqs = gate.requests.lock().unwrap();
    let gate_actor = reqs[0].actor.clone();
    drop(reqs);

    let captured = actors.lock().unwrap();
    let token_actor = captured[0].clone();

    assert_eq!(gate_actor.kind, token_actor.kind);
    assert_eq!(gate_actor.id, token_actor.id);
    assert_eq!(gate_actor.id, "local");
}

// ---- dispatch_as: verified-actor dispatch for embedding hosts ----

/// `dispatch_as` must thread the caller-supplied verified actor through
/// to the pack handler's `NamespaceToken`, exactly as `dispatch_with_identity`
/// does with a `RequestIdentity.actor_id` — this is the observable
/// contract embedding hosts rely on.
#[tokio::test]
async fn dispatch_as_threads_verified_actor_into_token() {
    let gate = Arc::new(ActorCapturingGate::default());
    let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pack = TokenCapturingPack {
        actors: actors.clone(),
    };
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch_as(
        "list",
        Value::Null,
        VerifiedActor::new("gateway:principal-42").unwrap(),
    )
    .await
    .unwrap();

    let reqs = gate.requests.lock().unwrap();
    assert_eq!(reqs[0].actor.kind, "actor");
    assert_eq!(reqs[0].actor.id, "gateway:principal-42");
    drop(reqs);

    let captured = actors.lock().unwrap();
    assert_eq!(captured[0].kind, "actor");
    assert_eq!(
        captured[0].id, "gateway:principal-42",
        "the storage token actor must be the verified_actor supplied to dispatch_as, \
             matching exactly what pack handlers read as the acting principal"
    );
}

/// `dispatch_as` is purely additive: a registry with a baked `actor_id`
/// must still serve plain `dispatch()` calls under its own baked actor,
/// unaffected by any `dispatch_as` call made on the same (cheaply
/// cloneable) registry. No shared mutable state links the two calls.
#[tokio::test]
async fn dispatch_as_does_not_change_plain_dispatch_behavior() {
    let gate = Arc::new(ActorCapturingGate::default());
    let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pack = TokenCapturingPack {
        actors: actors.clone(),
    };
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    builder.with_gate(gate.clone());
    builder.with_actor_id(Some("baked-actor".to_string()));
    let reg = builder.build().expect("registry builds");

    reg.dispatch_as(
        "list",
        Value::Null,
        VerifiedActor::new("verified-actor").unwrap(),
    )
    .await
    .unwrap();
    reg.dispatch("list", Value::Null).await.unwrap();

    let captured = actors.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].id, "verified-actor", "dispatch_as call");
    assert_eq!(
        captured[1].id, "baked-actor",
        "a later plain dispatch() call must still use the registry's baked \
             actor_id, unaffected by the prior dispatch_as call"
    );
}

/// A request `params` payload cannot inject or override the actor:
/// `dispatch_as` resolves the acting principal solely from its Rust-side
/// `verified_actor` argument, never from `params`. An `actor` key placed
/// in `params` passes through untouched to the pack handler like any
/// other unrecognized field — the dispatch boundary itself never reads
/// `params["actor"]`.
#[tokio::test]
async fn dispatch_as_ignores_actor_key_in_params() {
    let gate = Arc::new(ActorCapturingGate::default());
    let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pack = TokenCapturingPack {
        actors: actors.clone(),
    };
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    builder.with_gate(gate.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch_as(
        "list",
        serde_json::json!({"actor": "spoofed-actor"}),
        VerifiedActor::new("verified-actor").unwrap(),
    )
    .await
    .unwrap();

    let captured = actors.lock().unwrap();
    assert_eq!(
        captured[0].id, "verified-actor",
        "an 'actor' key inside params must never override the verified_actor \
             argument threaded through dispatch_as"
    );
}

/// `VerifiedActor::new` must reject an empty identifier rather than
/// letting it reach dispatch and silently resolve to the anonymous actor.
#[test]
fn verified_actor_rejects_empty_identifier() {
    let err = VerifiedActor::new("").unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );
}

/// `VerifiedActor::new` must reject a whitespace-only identifier for the
/// same reason: it must never launder into `ActorRef::anonymous()`.
#[test]
fn verified_actor_rejects_whitespace_only_identifier() {
    let err = VerifiedActor::new("   ").unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );
}

// ---- Rego gate: fail-closed end-to-end ----

/// A `RegoGate` whose policy lacks the named entrypoint rule must cause
/// `VerbRegistry::dispatch` to return `RuntimeError::PermissionDenied` —
/// never to proceed to the pack handler.
///
/// This is the runtime-level assertion that a gate evaluation failure
/// fails closed rather than opening a security hole. `RegoGate::check`
/// converts all evaluation failures (missing rule, undefined result,
/// serialization error, poisoned engine) to `Ok(GateDecision::Deny)` with
/// a static classified reason, so dispatch is blocked as a policy
/// refusal. Infrastructure faults from other `Gate` implementations
/// remain distinguishable as `RuntimeError::GateUnavailable`.
#[tokio::test]
async fn rego_gate_missing_entrypoint_returns_permission_denied() {
    use khive_gate_rego::RegoGate;

    // Policy defines `verdict` but NOT `data.khive.gate.decision` (the
    // default entrypoint).  Construction succeeds — from_policy_str does
    // not validate the default entrypoint.  check() must convert the
    // missing-rule evaluation error to Ok(Deny) with a static classified
    // reason so the runtime reports a policy refusal rather than a gate
    // infrastructure outage.
    let policy = r#"
            package khive.gate
            import rego.v1
            verdict := "allow"
        "#;
    let gate = Arc::new(RegoGate::from_policy_str(policy).expect("policy compiles"));

    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(gate);
    let reg = builder.build().expect("registry builds");

    let err = reg.dispatch("create", Value::Null).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::PermissionDenied { ref verb, ref reason, .. }
            if verb == "create" && reason == "policy evaluation failed"),
        "expected PermissionDenied with the static classified reason for a missing rego entrypoint, got {err:?}"
    );
}

// ---- Audit tracing emission ----
//
// The AuditCapturingGate tests above prove that AuditEvent::from_check is
// called with the right inputs, but they observe the event *inside* the
// gate impl — they would still pass if dispatch's
// `tracing::info!(audit_event = ..., "gate.check")` were deleted or
// renamed. The tests below install a capture Layer and assert on the
// actual tracing event surfaced from dispatch. This locks the public
// observability contract: one `gate.check` info event per dispatch,
// carrying an `audit_event` field that round-trips back to an `AuditEvent`.

use std::sync::{Mutex as StdMutex, Once, OnceLock};

use serial_test::serial;
use tracing::field::{Field, Visit};

#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    message: Option<String>,
    audit_event: Option<String>,
    into_id: Option<String>,
    budget_rows: Option<u64>,
}

#[derive(Default)]
struct CapturedEventVisitor(CapturedEvent);

impl Visit for CapturedEventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.0.message = Some(value.to_string()),
            "audit_event" => self.0.audit_event = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `tracing::info!(audit_event = %expr, "msg")` records via the
        // Display-wrapped Debug path, so we receive the JSON string here.
        // `"msg"` literal records as a `message` field via `record_debug`
        // with a quoted Debug representation; strip the surrounding quotes
        // so the captured message matches the source.
        let formatted = format!("{value:?}");
        let cleaned = formatted
            .trim_start_matches('"')
            .trim_end_matches('"')
            .to_string();
        match field.name() {
            "message" => self.0.message = Some(cleaned),
            "audit_event" => self.0.audit_event = Some(cleaned),
            "into_id" => self.0.into_id = Some(cleaned),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "budget_rows" {
            self.0.budget_rows = Some(value);
        }
    }
}

/// Minimal `tracing::Subscriber` that captures events into a shared vec.
///
/// Implemented directly (without `tracing_subscriber::registry()` layering)
/// to avoid the layer machinery that can cause thread-local dispatch to be
/// bypassed when the registry's internal global state is initialised by
/// another subscriber in the same test binary.
///
/// Isolation across concurrent tests is handled at the dispatcher level by
/// `tracing::dispatcher::with_default`, which installs this subscriber
/// as the thread-local default for the duration of the test closure.
/// Other threads (e.g. `#[tokio::test]` pool workers) emit through their
/// own (typically NoSubscriber) dispatchers and never reach this instance.
struct CaptureSubscriber {
    events: Arc<StdMutex<Vec<CapturedEvent>>>,
}

impl CaptureSubscriber {
    fn new(events: Arc<StdMutex<Vec<CapturedEvent>>>) -> Self {
        Self { events }
    }
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = CapturedEventVisitor::default();
        event.record(&mut visitor);
        let captured = visitor.0;
        // Tee the post-commit budget logs into their own append-only sink:
        // `capture_dispatch_events` clears the main buffer, so a reader of
        // budget events sharing that buffer would race the clear.
        if let (Some(message), Some(into_id)) = (&captured.message, &captured.into_id) {
            if message.ends_with("transaction materialization budget") {
                budget_events_sink()
                    .lock()
                    .unwrap()
                    .push(CapturedBudgetLog {
                        message: message.clone(),
                        into_id: into_id.clone(),
                        budget_rows: captured.budget_rows.unwrap_or(0),
                    });
            }
        }
        self.events.lock().unwrap().push(captured);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Global capture buffer for the tracing tests.
///
/// The subscriber is installed exactly once via `set_global_default`
/// (thread-local dispatchers via `with_default` proved unreliable when
/// other tests in the binary configure their own dispatchers in parallel —
/// the global state interacted unpredictably and events were lost).
///
/// Each test that uses this buffer is `#[serial]`, so only one
/// runs at a time. The buffer is cleared at the start of each capture call.
static GLOBAL_CAPTURE: OnceLock<Arc<StdMutex<Vec<CapturedEvent>>>> = OnceLock::new();
static GLOBAL_INIT: Once = Once::new();

/// One captured post-commit budget log (curation merge tests).
#[derive(Clone)]
pub(crate) struct CapturedBudgetLog {
    pub(crate) message: String,
    pub(crate) into_id: String,
    pub(crate) budget_rows: u64,
}

/// Append-only sink the subscriber tees budget logs into. Never cleared:
/// curation tests select their own rows by `into_id`, so stale rows from
/// other tests are inert rather than a pollution hazard.
static BUDGET_EVENTS: OnceLock<Arc<StdMutex<Vec<CapturedBudgetLog>>>> = OnceLock::new();

fn budget_events_sink() -> Arc<StdMutex<Vec<CapturedBudgetLog>>> {
    Arc::clone(BUDGET_EVENTS.get_or_init(|| Arc::new(StdMutex::new(Vec::new()))))
}

/// Entry point for the curation merge tests: installs the process-global
/// capture subscriber (once for the whole test binary — a second
/// `set_global_default` elsewhere would starve one of the captures) and
/// returns the budget-log sink it tees into.
pub(crate) fn budget_log_events() -> Arc<StdMutex<Vec<CapturedBudgetLog>>> {
    let _ = global_capture();
    budget_events_sink()
}

fn global_capture() -> Arc<StdMutex<Vec<CapturedEvent>>> {
    GLOBAL_INIT.call_once(|| {
        let buffer = Arc::new(StdMutex::new(Vec::new()));
        let subscriber = CaptureSubscriber::new(Arc::clone(&buffer));
        // Ignore error: if another subscriber is already set globally, our
        // subscriber installation fails, but the buffer will simply stay
        // empty and tests will fail with a clear "got 0 events" message
        // rather than a silent corruption.
        let _ = tracing::subscriber::set_global_default(subscriber);
        let _ = GLOBAL_CAPTURE.set(buffer);
    });
    Arc::clone(GLOBAL_CAPTURE.get().expect("global capture initialized"))
}

/// Run an async block under the global capture subscriber and return
/// the events emitted during the run. Clears the buffer at the start.
///
/// Callers MUST be `#[serial]` to prevent concurrent buffer pollution.
fn capture_dispatch_events<Fut>(future: Fut) -> Vec<CapturedEvent>
where
    Fut: std::future::Future<Output = ()>,
{
    let buffer = global_capture();
    buffer.lock().unwrap().clear();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread tokio runtime");
    rt.block_on(future);

    let result = buffer.lock().unwrap().clone();
    result
}

/// Pull every captured event whose `message` matches `"gate.check"` AND
/// whose audit_event JSON declares the expected `gate_impl` name.
///
/// Filtering by `gate_impl` lets concurrent tests in the same binary
/// emit their own gate.check events into the global capture buffer
/// without polluting each others' counts.
fn gate_check_events_for(events: &[CapturedEvent], gate_impl: &str) -> Vec<CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message.as_deref() == Some("gate.check"))
        .filter(|e| {
            e.audit_event
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|v| {
                    v.get("gate_impl")
                        .and_then(|g| g.as_str().map(|s| s.to_string()))
                })
                .as_deref()
                == Some(gate_impl)
        })
        .cloned()
        .collect()
}

#[test]
#[serial]
fn dispatch_tracing_emits_one_gate_check_event_on_allow() {
    #[derive(Debug)]
    struct TracingAllowGate;
    impl Gate for TracingAllowGate {
        fn check(&self, _: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::allow())
        }
        fn impl_name(&self) -> &'static str {
            "TracingAllowGate"
        }
    }

    let events = capture_dispatch_events(async {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(TracingAllowGate));
        builder.with_default_namespace("tenant-default");
        let reg = builder.build().expect("registry builds");
        reg.dispatch("list", serde_json::json!({"namespace": "tenant-q"}))
            .await
            .unwrap();
    });

    let gate_events = gate_check_events_for(&events, "TracingAllowGate");
    assert_eq!(
        gate_events.len(),
        1,
        "exactly one gate.check tracing event per dispatch (allow); got {gate_events:?}"
    );
    let payload = gate_events[0]
        .audit_event
        .as_ref()
        .expect("gate.check event must carry an audit_event field");
    let audit: khive_gate::AuditEvent =
        serde_json::from_str(payload).expect("audit_event payload must decode to AuditEvent");
    assert_eq!(audit.decision, AuditDecision::Allow);
    assert_eq!(audit.verb, "list");
    assert_eq!(audit.namespace, "tenant-q");
    assert_eq!(audit.gate_impl, "TracingAllowGate");
    assert!(
        audit.deny_reason.is_none(),
        "deny_reason must be None on Allow"
    );
}

#[test]
#[serial]
fn dispatch_tracing_emits_one_gate_check_event_when_gate_is_unavailable() {
    #[derive(Debug)]
    struct TracingUnavailableGate;
    impl Gate for TracingUnavailableGate {
        fn check(&self, _: &GateRequest) -> Result<GateDecision, GateError> {
            Err(GateError::Internal("tracing gate broken".into()))
        }

        fn impl_name(&self) -> &'static str {
            "TracingUnavailableGate"
        }
    }

    let events = capture_dispatch_events(async {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(TracingUnavailableGate));
        let reg = builder.build().expect("registry builds");
        let error = reg
            .dispatch("list", Value::Null)
            .await
            .expect_err("gate outage must refuse dispatch");
        assert!(matches!(error, RuntimeError::GateUnavailable { .. }));
    });

    let gate_events = gate_check_events_for(&events, "TracingUnavailableGate");
    assert_eq!(
        gate_events.len(),
        1,
        "exactly one gate.check tracing event per gate outage; got {gate_events:?}"
    );
    let payload = gate_events[0]
        .audit_event
        .as_ref()
        .expect("gate outage trace must carry an audit_event field");
    let audit: AuditEvent = serde_json::from_str(payload).expect("audit_event payload must decode");
    assert_eq!(audit.decision, AuditDecision::GateUnavailable);
    assert!(audit.deny_reason.is_none());
    assert!(audit.obligations.is_empty());
    assert_eq!(audit.gate_impl, "TracingUnavailableGate");
}

// ---- Hard enforcement + EventStore persistence ----

use crate::runtime::NamespaceToken;
use async_trait::async_trait;
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, SubstrateKind,
};
use khive_types::EventOutcome;

/// Minimal stand-in for the Git pack: the receipt contract belongs to the
/// runtime dispatch seam, so these tests do not need a git repository or
/// any Git-pack dependency.
struct GitDigestResultPack {
    project_id: uuid::Uuid,
}

impl Pack for GitDigestResultPack {
    const NAME: &'static str = "git";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "git.digest",
        description: "return a deterministic digest report",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for GitDigestResultPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({
            "project_id": self.project_id,
            "project_created": false,
            "commits_ingested": 2,
            "commits_skipped_existing": 1,
            "issues_ingested": 3,
            "issues_skipped_existing": 4,
            "prs_ingested": 5,
            "prs_skipped_existing": 6,
            "done": true,
            "history_exhausted": true,
            "sources": {
                "commits": {"state": "completed"},
                "issues": {"state": "completed"},
                "pull_requests": {"state": "completed"}
            },
            "warnings": []
        }))
    }
}

/// A nominally successful handler with an invalid receipt identity. The
/// runtime must turn this into an error without consuming its generic
/// audit fallback.
struct MalformedGitDigestResultPack;

impl Pack for MalformedGitDigestResultPack {
    const NAME: &'static str = "malformed-git";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "git.digest",
        description: "return a malformed digest report",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for MalformedGitDigestResultPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({
            "project_id": "not-a-uuid",
            "done": true,
        }))
    }
}

/// One entry in the interleaved submission trace. Typed so assertions
/// match on fields instead of parsing a formatted string; both sides of
/// the ordering land on ONE vector so their relative order is observable
/// rather than assumed.
#[derive(Debug, Clone, PartialEq)]
enum TraceEntry {
    /// A handler effect that has committed (nothing downstream undoes it).
    Effect { name: String },
    /// An audit row submitted to the event store, carrying exactly the
    /// fields the obligation test discriminates on.
    Audit {
        kind: EventKind,
        outcome: EventOutcome,
        verb: String,
        /// The row's `resource.cost_unit`, `None` when the payload omits
        /// it (the error path's `base_resource_payload` does).
        cost_unit: Option<Value>,
    },
}

/// In-memory EventStore for unit tests — avoids file-backed SQLite.
#[derive(Default, Debug)]
struct MemoryEventStore {
    events: std::sync::Mutex<Vec<Event>>,
    fail_appends: bool,
    /// Fail only a generation whose batch contains an event of this
    /// kind, leaving every other generation (e.g. the deferred
    /// obligation row committed after dispatch resolves) to commit
    /// normally. Lets a test fail a pure-observability row without
    /// also failing the obligation row that shares the same store.
    fail_kind: Option<EventKind>,
    /// Append-ordered record of what was SUBMITTED to this store,
    /// written before the injected-failure check so a submission this
    /// store then rejects is still visible.
    ///
    /// `events` alone cannot show that: a rejected append returns before
    /// the store records anything, so a test reading `events` cannot
    /// tell a row that failed to commit from a row that was never built.
    /// Those are different production behaviours and only one of them is
    /// the audit contract. A test that hands the same vector to its
    /// handler also gets the ordering between the handler's effect and
    /// the audit submission, which is the only way to observe that the
    /// audit row is written after the handler rather than before it.
    trace: Option<Arc<std::sync::Mutex<Vec<TraceEntry>>>>,
    /// Hold a real batch append across the caller's audit deadline.
    append_started: Option<Arc<tokio::sync::Notify>>,
    append_release: Option<Arc<tokio::sync::Notify>>,
}

impl MemoryEventStore {
    /// Record a submission attempt. Call before any failure check.
    ///
    /// The projection carries the verb and the row's `resource.cost_unit`
    /// value, not just kind and outcome, and not merely whether the key is
    /// present.
    ///
    /// Presence alone is too weak to pin what it looks like it pins.
    /// `cost_unit::resource_payload` inserts the key unconditionally, and
    /// for most verbs `item_count` returns a constant `1` regardless of the
    /// handler's return value, so a submission built from a static or null
    /// `ok_val` still carries the key. Presence separates `resource_payload`
    /// from the error path's `base_resource_payload`, which is a real
    /// property but a different one.
    ///
    /// The value is what sources the return value, and only for a verb whose
    /// `item_count` reads it. `knowledge.index` is that verb: `item_count`
    /// takes `result["total"]`, so `cost_unit` is `total + 1` and moves with
    /// what the handler returned. The fixture below uses that verb for
    /// exactly this reason.
    fn trace_submission(&self, events: &[Event]) {
        if let Some(trace) = &self.trace {
            let mut trace = trace.lock().expect("trace lock");
            for event in events {
                let cost_unit = event
                    .payload
                    .get("resource")
                    .and_then(|resource| resource.get("cost_unit"))
                    .cloned();
                trace.push(TraceEntry::Audit {
                    kind: event.kind,
                    outcome: event.outcome,
                    verb: event.verb.clone(),
                    cost_unit,
                });
            }
        }
    }
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append_event(&self, event: Event) -> khive_storage::StorageResult<()> {
        self.trace_submission(std::slice::from_ref(&event));
        if self.fail_appends || self.fail_kind == Some(event.kind) {
            return Err(khive_storage::StorageError::Internal(
                "injected audit append failure".to_string(),
            ));
        }
        self.events.lock().unwrap().push(event);
        Ok(())
    }
    async fn append_events(
        &self,
        events: Vec<Event>,
    ) -> khive_storage::StorageResult<BatchWriteSummary> {
        self.trace_submission(&events);
        let attempted = events.len() as u64;
        let affected = attempted;
        self.events.lock().unwrap().extend(events);
        Ok(BatchWriteSummary {
            attempted,
            affected,
            ..BatchWriteSummary::default()
        })
    }
    async fn get_event(&self, id: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned())
    }
    async fn query_events(
        &self,
        _filter: EventFilter,
        _page: PageRequest,
    ) -> khive_storage::StorageResult<Page<Event>> {
        let items = self.events.lock().unwrap().clone();
        let total = items.len() as u64;
        Ok(Page {
            items,
            total: Some(total),
        })
    }
    async fn count_events(&self, _filter: EventFilter) -> khive_storage::StorageResult<u64> {
        Ok(self.events.lock().unwrap().len() as u64)
    }

    fn preflight_event(&self, _event: &Event) -> khive_storage::StorageResult<()> {
        Ok(())
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> khive_storage::StorageResult<khive_storage::event::IdempotentEventBatchResult> {
        if let Some(started) = &self.append_started {
            started.notify_one();
        }
        if let Some(release) = &self.append_release {
            release.notified().await;
        }
        self.trace_submission(&events);
        if self.fail_appends
            || self
                .fail_kind
                .is_some_and(|kind| events.iter().any(|e| e.kind == kind))
        {
            return Err(khive_storage::StorageError::Internal(
                "injected audit append failure".to_string(),
            ));
        }
        let mut store = self.events.lock().unwrap();
        let mut rows = Vec::with_capacity(events.len());
        for event in events {
            if let Some(existing) = store.iter().find(|e| e.id == event.id) {
                if *existing == event {
                    rows.push(
                        khive_storage::event::EventAppendDisposition::AlreadyPresentIdentical,
                    );
                } else {
                    rows.push(khive_storage::event::EventAppendDisposition::IdentityConflict);
                }
            } else {
                store.push(event);
                rows.push(khive_storage::event::EventAppendDisposition::Inserted);
            }
        }
        Ok(khive_storage::event::IdempotentEventBatchResult { rows })
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}

/// Recursively collect every `.rs` file under `dir` into `out`.
fn collect_rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Every `crates/<crate>/src/**/*.rs` and `crates/<crate>/tests/**/*.rs`
/// file in the workspace, read to a `String` alongside its path.
///
/// This is the compiled-test population an event-backed registry
/// constructor can actually be reached from: unit tests live under
/// `src/`, integration tests live under `tests/`. Anything outside those
/// two directories per crate (benches, examples) never runs as `cargo
/// test` and is out of scope for this census.
/// SQL text belongs in `sql/<name>.sql`, reached through each crate's `sql!`
/// macro, not in a Rust string literal. This is the burn-down instrument for that
/// move: a crate is added to `CONVERTED` by the pull request that extracts it, and
/// from then on the workspace refuses to take a statement back into Rust.
///
/// What it can and cannot see, said plainly because the answer is load-bearing.
/// It selects by SPELLING: a string literal whose first word is a SQL verb. It
/// therefore cannot see a statement assembled from fragments, one returned by a
/// helper, or one built at runtime. Its population is code that COMPILES into the
/// crate, which means `#[cfg(test)]` module bodies are stripped along with
/// `tests/` and `benches/` — a test that stands a fixture table up inline is out
/// of scope for this program. The must-match control below is what keeps those
/// limits honest: an unconverted crate has to trip the same predicate in the same
/// pass, or the detector is broken rather than the tree clean.
#[test]
fn converted_crates_keep_their_sql_out_of_rust() {
    /// Crates whose statements live in `sql/`. One pull request adds one name.
    const CONVERTED: &[&str] = &[
        "khive-pack-brain",
        "khive-pack-comm",
        "khive-pack-git",
        "khive-pack-gtd",
        "khive-pack-kg",
        "kkernel",
    ];
    /// A crate known to still hold SQL in Rust, used only to prove the detector
    /// fires. When this one is converted, move the control to another unconverted
    /// crate rather than deleting it.
    const STILL_INLINE: &str = "khive-db";

    fn strip_test_modules(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        while let Some(found) = text[cursor..].find("#[cfg(test)]") {
            let start = cursor + found;
            // Only a `mod` item is stripped; `#[cfg(test)]` on a `use` or a `fn`
            // leaves nothing to brace-match.
            let after = &text[start..];
            let Some(brace_rel) = after.find('{') else {
                out.push_str(&text[cursor..]);
                return out;
            };
            if !after[..brace_rel].contains("mod ") {
                out.push_str(&text[cursor..start + brace_rel]);
                cursor = start + brace_rel;
                continue;
            }
            out.push_str(&text[cursor..start]);
            let mut depth = 0usize;
            let mut i = start + brace_rel;
            while i < bytes.len() {
                match bytes[i] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            cursor = i;
        }
        out.push_str(&text[cursor..]);
        out
    }

    /// The body of the Rust string literal whose opening quote is at `open`, or
    /// `None` if it does not terminate. Escapes are skipped rather than decoded:
    /// this only has to find the end and hand back text to match against.
    fn literal_body(text: &str, open: usize) -> Option<&str> {
        let bytes = text.as_bytes();
        let mut i = open + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => return text.get(open + 1..i),
                _ => i += 1,
            }
        }
        None
    }

    /// One line, single-spaced. A statement in Rust wears its line breaks three
    /// ways — a real newline in a raw string, a `\n` escape, or a backslash line
    /// continuation — and this scan reads source text, so all three have to read as
    /// one space before any keyword after the first can be matched. `\n` is two
    /// characters here, and dropping only the backslash would leave `nFROM`, which
    /// is exactly how this check first failed its own must-fail control.
    fn flatten(body: &str) -> String {
        let mut out = String::with_capacity(body.len());
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.clone().next() {
                // An escape that stands for whitespace: consume both characters.
                Some('n' | 't' | 'r') => {
                    chars.next();
                    out.push(' ');
                }
                // A line continuation, or any other escape: the backslash goes,
                // what follows is kept and judged on its own.
                _ => out.push(' '),
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn sql_literals(text: &str) -> Vec<String> {
        // A leading verb alone is a heuristic, and it is wrong often enough to
        // matter: "insert serve batch" is an error label and "Create a new brain
        // profile with given name" is a verb description, and both start with a
        // SQL verb. So a literal has to carry a second structural keyword too, and
        // both are matched CASE-SENSITIVELY, because every statement in this tree
        // writes its keywords in upper case and English prose does not.
        const SHAPES: [(&str, &[&str]); 10] = [
            // A statement need not start with a verb at all. A common table
            // expression starts with WITH, and there are a dozen of them in this
            // workspace, so a census that only knows verbs reads a crate clean while
            // its largest queries sit in Rust. The second keyword here is the CTE's
            // own binding, which prose does not write.
            ("WITH ", &[" AS ("]),
            ("SELECT ", &[" FROM "]),
            ("INSERT ", &["INSERT INTO ", "INSERT OR "]),
            ("UPDATE ", &[" SET "]),
            ("DELETE ", &["DELETE FROM "]),
            (
                "CREATE ",
                &[
                    "CREATE TABLE",
                    "CREATE INDEX",
                    "CREATE UNIQUE",
                    "CREATE VIEW",
                    "CREATE VIRTUAL",
                    "CREATE TRIGGER",
                ],
            ),
            (
                "DROP ",
                &["DROP TABLE", "DROP INDEX", "DROP VIEW", "DROP TRIGGER"],
            ),
            ("ALTER ", &["ALTER TABLE"]),
            ("PRAGMA ", &["PRAGMA "]),
            ("REPLACE ", &["REPLACE INTO "]),
        ];
        let mut found = Vec::new();
        for (index, _) in text.match_indices('"') {
            let Some(body) = literal_body(text, index) else {
                continue;
            };
            let flat = flatten(body);
            let Some((verb, seconds)) = SHAPES.iter().find(|(v, _)| flat.starts_with(*v)) else {
                continue;
            };
            if !seconds.iter().any(|second| flat.contains(second)) {
                continue;
            }
            let snippet: String = flat.chars().take(70).collect();
            found.push(format!("{verb}… {snippet}"));
        }
        found
    }

    let sources = workspace_rust_sources();
    assert!(
        !sources.is_empty(),
        "the workspace source walk returned nothing, so this census read no code"
    );

    let mut scanned_files = 0usize;
    let mut offenders: Vec<String> = Vec::new();
    let mut control_hits = 0usize;
    for (path, text) in &sources {
        let display = path.display().to_string();
        if display.contains("/tests/") || display.contains("/benches/") {
            continue;
        }
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_name == "tests.rs" || file_name.ends_with("_tests.rs") {
            continue;
        }
        let production = strip_test_modules(text);
        if display.contains(&format!("/{STILL_INLINE}/")) {
            control_hits += sql_literals(&production).len();
            continue;
        }
        let Some(crate_name) = CONVERTED
            .iter()
            .find(|name| display.contains(&format!("/{name}/")))
        else {
            continue;
        };
        scanned_files += 1;
        for literal in sql_literals(&production) {
            offenders.push(format!("{crate_name} {}: {literal}", path.display()));
        }
    }

    // Must-fail control: take every statement this program has already extracted,
    // write it back into a Rust literal in each of the three shapes a statement can
    // take in Rust source, and require the predicate to catch each one. The
    // must-match control below proves the predicate fires SOMEWHERE; this proves it
    // fires on exactly the regression the census exists to stop, which is a
    // converted statement coming home. The escaped-newline shape is not decoration:
    // an earlier version of `flatten` dropped the backslash and left `nFROM`, and
    // six of eleven statements would have come back unseen.
    let mut round_tripped = 0usize;
    let crates_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("khive-runtime's Cargo.toml lives directly under crates/")
        .to_path_buf();
    for crate_name in CONVERTED {
        let sql_dir = crates_root.join(crate_name).join("sql");
        let entries = std::fs::read_dir(&sql_dir).unwrap_or_else(|e| {
            panic!("{crate_name} is on the converted list but {sql_dir:?} is unreadable: {e}")
        });
        for entry in entries.filter_map(Result::ok) {
            let file = entry.path();
            if file.extension().and_then(|e| e.to_str()) != Some("sql") {
                continue;
            }
            let statement =
                std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {file:?}: {e}"));
            // A file may open with a header comment saying what it is and where its
            // authoritative definition lives. A Rust literal carries no such header,
            // so the round trip has to drop it: otherwise the rendered literal opens
            // with `--` and the predicate correctly sees no statement, which reads as
            // a broken census rather than as a file with a preamble.
            let body = statement
                .lines()
                .skip_while(|line| {
                    let start = line.trim_start();
                    start.is_empty() || start.starts_with("--")
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !body.trim().is_empty(),
                "{file:?} holds nothing but comments, so it declares no statement for \
                     the census to protect"
            );
            // A quoted identifier would otherwise close the synthetic literal early
            // and fail this control for a reason that has nothing to do with it.
            let statement = body.trim().replace('"', "\\\"");
            let shapes = [
                (
                    "one line",
                    statement.split_whitespace().collect::<Vec<_>>().join(" "),
                ),
                ("escaped newlines", statement.replace('\n', "\\n")),
                (
                    "line continuations",
                    statement.replace('\n', " \\\n            "),
                ),
            ];
            for (shape, rendered) in shapes {
                let snippet = format!("let statement = \"{rendered}\";");
                let seen = sql_literals(&snippet);
                assert_eq!(
                    seen.len(),
                    1,
                    "must-fail control: {file:?} written back into Rust as {shape} was \
                         seen {} time(s), so the census would not notice this statement \
                         moving home",
                    seen.len()
                );
                round_tripped += 1;
            }
        }
    }
    assert!(
        round_tripped > 0,
        "must-fail control ran on nothing: {CONVERTED:?} contributed no .sql files, so \
             its passing says only that the loop body never executed"
    );

    assert!(
        control_hits > 0,
        "must-match control: {STILL_INLINE} still holds SQL in Rust, so a detector \
             finding none there is broken and its clean reading of {CONVERTED:?} means nothing"
    );
    assert!(
        scanned_files > 0,
        "no source file matched {CONVERTED:?}; the crate names in that list are how this \
             census finds its population, so an empty match reads clean for the wrong reason"
    );
    assert!(
        offenders.is_empty(),
        "SQL text belongs in sql/<name>.sql behind that crate's sql! macro; \
             {} offender(s) across {scanned_files} file(s): {offenders:#?}",
        offenders.len()
    );
}

fn workspace_rust_sources() -> Vec<(std::path::PathBuf, String)> {
    let crates_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("khive-runtime's Cargo.toml lives directly under crates/")
        .to_path_buf();
    let mut files = Vec::new();
    let Ok(crate_dirs) = std::fs::read_dir(&crates_root) else {
        return Vec::new();
    };
    for crate_dir in crate_dirs.filter_map(Result::ok) {
        let crate_dir = crate_dir.path();
        if !crate_dir.is_dir() {
            continue;
        }
        for sub in ["src", "tests"] {
            let sub_dir = crate_dir.join(sub);
            if sub_dir.is_dir() {
                collect_rust_files(&sub_dir, &mut files);
            }
        }
    }
    files
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            Some((path, text))
        })
        .collect()
}

/// The name of the function whose signature starts at `sig_line`
/// (already stripped of leading whitespace), if any.
fn fn_name_from_signature(sig_line: &str) -> Option<&str> {
    let mut rest = sig_line;
    for prefix in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            rest = stripped;
        }
    }
    let rest = rest
        .strip_prefix("async fn ")
        .or_else(|| rest.strip_prefix("fn "))?;
    Some(rest.split(['(', '<', ' ']).next().unwrap_or(rest))
}

/// `true` if `line`, once whitespace is trimmed, is a top-level `fn`
/// signature start (covering the `pub`/`pub(crate)`/`pub(super)` and
/// `async` modifiers actually used across this workspace).
fn is_fn_signature_line(trimmed: &str) -> bool {
    fn_name_from_signature(trimmed).is_some()
}

/// Regression for a scanner that only tolerated a space/tab between a
/// seam name and its `(` — `rustfmt` can and does break a call onto its
/// own line, and a scanner that only sees same-line whitespace would
/// silently stop finding calls the moment one gets formatted that way.
#[test]
fn calls_name_matches_across_a_newline_before_the_parenthesis() {
    let text = "async fn wraps_it() {\n    with_event_store\n        (store)\n}";
    assert!(calls_name(text, "with_event_store"));
}

/// `(name, body)` for every function defined in `text`, where `body`
/// spans from the function's signature line to the matching close of
/// its opening brace, found by [`brace_bounded_fn_end`]. The line
/// before the *next* function signature (or EOF) is passed to
/// `brace_bounded_fn_end` only as its own fallback bound — used when
/// brace counting never returns to depth zero, e.g. a signature shape
/// this scan doesn't recognize, or an actual brace imbalance — never as
/// this function's primary way of finding where a body ends.
fn fn_bodies(text: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_fn_signature_line(line.trim_start()))
        .map(|(index, _)| index)
        .collect();
    starts
        .iter()
        .enumerate()
        .filter_map(|(index, &start)| {
            let hard_limit = starts.get(index + 1).copied().unwrap_or(lines.len());
            let end = brace_bounded_fn_end(&lines, start, hard_limit);
            let name = fn_name_from_signature(lines[start].trim_start())?;
            Some((name.to_string(), lines[start..end].join("\n")))
        })
        .collect()
}

/// The exclusive end index (within `lines`) of the function whose
/// signature line is `lines[sig_start]`, found by counting brace depth
/// from that line until it returns to zero, bounded by `hard_limit` (a
/// caller-supplied fallback — the next known function signature, or
/// EOF) if brace counting never finds a close.
///
/// Bounding by "next signature" alone (the original design of
/// `fn_bodies`) reads past a function's real end whenever anything
/// between its `{` and the *next* recognized signature is not itself
/// matched as a signature — a nested nested `fn` with an unrecognized
/// visibility spelling, a closure, or simply a long function with a lot
/// of code after its logical end — and keeps scanning into whatever
/// comes next, which can misattribute an unrelated later call as this
/// function's own. Brace counting fixes that; `hard_limit` stays as a
/// safety net, never a primary bound, for the rare text this scan
/// cannot fully make sense of (a signature line this scan doesn't
/// recognize, or an actual brace imbalance).
fn brace_bounded_fn_end(lines: &[&str], sig_start: usize, hard_limit: usize) -> usize {
    let joined = lines[sig_start..hard_limit].join("\n");
    let stripped = strip_string_literals(&joined);
    let mut depth = 0i32;
    let mut opened = false;
    for (line_index, line) in stripped.lines().enumerate() {
        let code = match line.find("//") {
            Some(comment_at) => &line[..comment_at],
            None => line,
        };
        for ch in code.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            return (sig_start + line_index + 1).min(hard_limit);
        }
    }
    hard_limit
}

/// The `crates/<name>` crate this source `path` belongs to, or `None`
/// if `path` is not under a `crates/<name>/...` layout.
fn crate_key(path: &std::path::Path) -> Option<String> {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == "crates" {
            return components
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned());
        }
    }
    None
}

/// The first `handle_*` call found in `text`, if any — used to read off
/// the handler a dispatch match arm routes to.
fn find_handle_call(text: &str) -> Option<String> {
    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }
    let bytes = text.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find("handle_") {
        let start = search_from + rel;
        if start > 0 && is_ident_byte(bytes[start - 1]) {
            search_from = start + 1;
            continue;
        }
        let mut end = start + "handle_".len();
        while end < bytes.len() && is_ident_byte(bytes[end]) {
            end += 1;
        }
        let mut j = end;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'(' {
            return Some(text[start..end].to_string());
        }
        search_from = end.max(start + 1);
    }
    None
}

/// `(verb, handler)` for every `"verb" => ... handle_name(` dispatch
/// match arm found in `text` — the pattern every pack's `dispatch`
/// (`crates/khive-pack-*/src/{dispatch,pack}.rs`) uses to route a verb
/// string to its handler method.
///
/// This workspace's dispatch tables write one verb per arm ending in a
/// `self.handle_*(...)` call, occasionally wrapped in a short `{ }`
/// block (`"memory.recall" => { self.handle_recall_with_deadline(...)
/// .await }`), so the handler is looked up in a bounded window after
/// the arm's `=>` rather than requiring it on the same line. A combined
/// arm that dispatches on a second, nested `match` (`"create" | "list"
/// | "search" => { match verb { "create" => self.handle_create(...),
/// ... } }`) can misattribute the outer alias to the first inner
/// handler call instead of its real one; that under-attributes rather
/// than over-attributes a verb as ledger-reaching (a missed producer
/// verb is a false negative here, not a false positive), and none of
/// this workspace's combined arms currently route to a ledger
/// producer.
fn dispatch_verb_handlers(text: &str) -> Vec<(String, String)> {
    let bytes = text.as_bytes();
    let mut arms: Vec<(String, usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j] != b'"' && bytes[j] != b'\n' {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'"' {
            i += 1;
            continue;
        }
        let literal = &text[start..j];
        let verb_like = !literal.is_empty()
            && literal
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
        let mut k = j + 1;
        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if verb_like && k + 1 < bytes.len() && bytes[k] == b'=' && bytes[k + 1] == b'>' {
            arms.push((literal.to_string(), start - 1, k + 2));
        }
        i = j + 1;
    }

    let mut out = Vec::new();
    for (index, (verb, _quote_start, arrow_end)) in arms.iter().enumerate() {
        let next_arm_start = arms.get(index + 1).map(|arm| arm.1).unwrap_or(bytes.len());
        let window_end = next_arm_start.min(arrow_end + 400).min(bytes.len());
        if window_end <= *arrow_end {
            continue;
        }
        if let Some(handler) = find_handle_call(&text[*arrow_end..window_end]) {
            out.push((verb.clone(), handler));
        }
    }
    out
}

#[derive(Debug)]
struct CensusTest {
    name: String,
    calls: std::collections::BTreeSet<String>,
    dispatch_verbs: std::collections::BTreeSet<String>,
    serial_keys: Vec<String>,
}

fn census_call_sites(
    tokens: proc_macro2::TokenStream,
    calls: &mut std::collections::BTreeSet<String>,
    dispatch_verbs: &mut std::collections::BTreeSet<String>,
) {
    use proc_macro2::{Delimiter, TokenTree};

    let tokens: Vec<_> = tokens.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        if let TokenTree::Ident(name) = token {
            let is_definition = index > 0
                && matches!(&tokens[index - 1], TokenTree::Ident(previous) if previous == "fn");
            if let Some(TokenTree::Group(args)) = tokens.get(index + 1) {
                if !is_definition && args.delimiter() == Delimiter::Parenthesis {
                    calls.insert(name.to_string());
                    if name == "dispatch" {
                        if let Some(TokenTree::Literal(literal)) = args.stream().into_iter().next()
                        {
                            let literal = std::iter::once(TokenTree::Literal(literal)).collect();
                            if let Ok(verb) = syn::parse2::<syn::LitStr>(literal) {
                                dispatch_verbs.insert(verb.value());
                            }
                        }
                    }
                }
            }
        }
        // Macro arguments remain token groups even when syn cannot parse
        // their syntax as expressions. Literal contents stay opaque.
        if let TokenTree::Group(group) = token {
            census_call_sites(group.stream(), calls, dispatch_verbs);
        }
    }
}

fn attribute_path_matches(path: &syn::Path, expected: &[&str]) -> bool {
    path.segments.len() == expected.len()
        && path
            .segments
            .iter()
            .zip(expected)
            .all(|(segment, expected)| segment.ident == *expected)
}

fn serial_attribute_keys(attrs: &[syn::Attribute]) -> syn::Result<Vec<String>> {
    fn parse_keys(
        input: syn::parse::ParseStream<'_>,
    ) -> syn::Result<syn::punctuated::Punctuated<syn::Ident, syn::Token![,]>> {
        use syn::ext::IdentExt;

        syn::punctuated::Punctuated::parse_terminated_with(input, syn::Ident::parse_any)
    }

    let mut acquired = Vec::new();
    // Later serial attributes wrap the function produced by earlier ones,
    // so they acquire first. Only the keys inside one attribute are sorted.
    for attr in attrs.iter().rev() {
        if !attribute_path_matches(attr.path(), &["serial"])
            && !attribute_path_matches(attr.path(), &["serial_test", "serial"])
        {
            continue;
        }
        let mut keys = match &attr.meta {
            syn::Meta::Path(_) => Vec::new(),
            syn::Meta::List(_) => attr
                .parse_args_with(parse_keys)
                .map_err(|error| {
                    syn::Error::new_spanned(
                        attr,
                        format!("unsupported serial attribute arguments in census: {error}"),
                    )
                })?
                .into_iter()
                .map(|key| key.to_string())
                .collect(),
            syn::Meta::NameValue(_) => {
                return Err(syn::Error::new_spanned(
                    attr,
                    "unsupported serial attribute arguments in census",
                ));
            }
        };
        // serial_test 3.5 sorts only within an attribute and uses "" for
        // an unkeyed lock. Reentrant acquisitions add no new order edge.
        if keys.is_empty() {
            keys.push(String::new());
        }
        keys.sort();
        for key in keys {
            if !acquired.contains(&key) {
                acquired.push(key);
            }
        }
    }
    Ok(acquired)
}

fn census_tests(text: &str) -> syn::Result<Vec<CensusTest>> {
    use quote::ToTokens;
    use syn::visit::Visit;

    #[derive(Default)]
    struct Collector {
        scope: Vec<String>,
        tests: Vec<syn::Result<CensusTest>>,
    }
    impl<'ast> Visit<'ast> for Collector {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            self.scope.push(item.ident.to_string());
            syn::visit::visit_item_mod(self, item);
            self.scope.pop();
        }

        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            self.scope.push(item.sig.ident.to_string());
            if item.attrs.iter().any(|attr| {
                attribute_path_matches(attr.path(), &["test"])
                    || attribute_path_matches(attr.path(), &["tokio", "test"])
            }) {
                self.tests
                    .push(serial_attribute_keys(&item.attrs).map(|serial_keys| {
                        let mut calls = std::collections::BTreeSet::new();
                        let mut dispatch_verbs = std::collections::BTreeSet::new();
                        census_call_sites(
                            item.block.to_token_stream(),
                            &mut calls,
                            &mut dispatch_verbs,
                        );
                        CensusTest {
                            name: self.scope.join("::"),
                            calls,
                            dispatch_verbs,
                            serial_keys,
                        }
                    }));
            }
            syn::visit::visit_item_fn(self, item);
            self.scope.pop();
        }
    }

    let file = syn::parse_file(text)?;
    let mut collector = Collector::default();
    collector.visit_file(&file);
    collector.tests.into_iter().collect()
}

#[derive(Default)]
struct SerialLockOrders {
    pairs: std::collections::BTreeMap<(String, String), (bool, String)>,
    conflicts: Vec<String>,
}

impl SerialLockOrders {
    fn record(&mut self, name: &str, keys: &[String]) {
        for (index, first) in keys.iter().enumerate() {
            for second in &keys[index + 1..] {
                let forward = first < second;
                let pair = if forward {
                    (first.clone(), second.clone())
                } else {
                    (second.clone(), first.clone())
                };
                if let Some((prior_forward, prior_name)) = self.pairs.get(&pair) {
                    if *prior_forward != forward {
                        self.conflicts.push(format!(
                            "{name} acquires {first:?} before {second:?}, opposite to {prior_name}"
                        ));
                    }
                } else {
                    self.pairs.insert(pair, (forward, name.to_owned()));
                }
            }
        }
    }
}

fn serial_fixture_conflicts(source: &str) -> Vec<String> {
    let mut orders = SerialLockOrders::default();
    for test in census_tests(source).expect("valid fixture source") {
        orders.record(&test.name, &test.serial_keys);
    }
    orders.conflicts
}

#[test]
fn serial_census_accepts_complete_attributes_and_per_attribute_sorting() {
    let source = r#"
            #[serial]
            #[cfg(unix)]
            #[serial_test::serial(
                config_ledger,
                other,
            )]
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn first() { with_event_store(store); }

            #[test]
            #[serial_test::serial()]
            #[serial(other, config_ledger)]
            fn second() { registry.dispatch("serial_fixture_verb", params); }

            mod nested {
                #[serial_test::serial]
                #[test]
                #[serial(other)]
                #[serial(config_ledger)]
                fn third() {}
            }
        "#;
    let tests = census_tests(source).unwrap();
    assert_eq!(tests.len(), 3);
    for test in &tests {
        assert_eq!(test.serial_keys, ["config_ledger", "other", ""]);
    }
    assert_eq!(tests[2].name, "nested::third");
    assert!(tests[0].calls.contains("with_event_store"));
    assert!(tests[1].dispatch_verbs.contains("serial_fixture_verb"));
    assert!(serial_fixture_conflicts(source).is_empty());
    let keyword_keys = census_tests("#[test] #[serial(type, r#match)] fn keywords() {}").unwrap();
    assert_eq!(keyword_keys[0].serial_keys, ["r#match", "type"]);
}

#[test]
fn serial_census_rejects_each_hidden_stacked_order_reversal() {
    for (label, reverse_attrs) in [
        ("one-line", "#[serial(config_ledger)] #[serial]"),
        ("multi-key", "#[serial(config_ledger, other)] #[serial]"),
        (
            "multiline",
            "#[serial_test::serial(\nconfig_ledger,\n)]\n#[serial_test::serial]",
        ),
        (
            "before-test",
            "#[serial(config_ledger)] #[serial] #[cfg(unix)]",
        ),
    ] {
        let source = format!(
            "#[test] #[serial] #[serial(config_ledger)] fn first() {{}}\n\
                 {reverse_attrs} #[test] fn reversed() {{}}"
        );
        let conflicts = serial_fixture_conflicts(&source);
        assert_eq!(conflicts.len(), 1, "{label}: {conflicts:?}");
        assert!(conflicts[0].contains("first"), "{label}: {conflicts:?}");
        assert!(conflicts[0].contains("reversed"), "{label}: {conflicts:?}");
    }
}

#[test]
fn serial_census_compares_third_keys_and_multi_key_lock_order() {
    let source = r#"
            #[test]
            #[serial(config_ledger)]
            #[serial(audit_append_failures)]
            #[serial(audit_obligation_append_failures)]
            fn first() {}
            #[test]
            #[serial(config_ledger)]
            #[serial(audit_obligation_append_failures)]
            #[serial(audit_append_failures)]
            fn reversed() {}
        "#;
    let conflicts = serial_fixture_conflicts(source);
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(conflicts[0].contains("audit_append_failures"));
    assert!(conflicts[0].contains("audit_obligation_append_failures"));
    assert_eq!(
        serial_fixture_conflicts(
            "#[test] #[serial(beta, alpha)] fn first() {}\n\
                 #[test] #[serial(alpha)] #[serial(beta)] fn reversed() {}"
        )
        .len(),
        1
    );
}

#[test]
fn serial_census_ignores_attribute_text_and_does_not_absorb_sibling_helpers() {
    let source = r##"
            // #[test] #[serial(config_ledger)] #[serial] fn comment() {}
            const TEXT: &str = r#"#[test] #[serial(config_ledger)] #[serial] fn string() {}"#;
            #[test] #[serial] #[serial(config_ledger)] fn actual() {}
            fn helper() { with_event_store(store); }
        "##;
    let tests = census_tests(source).unwrap();
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0].name, "actual");
    assert!(!tests[0].calls.contains("with_event_store"));
    assert!(serial_fixture_conflicts(source).is_empty());
}

#[test]
fn serial_census_body_calls_ignore_literals_and_preserve_macro_tokens() {
    let source = r###"
            #[test]
            fn literals() {
                let ordinary = "with_event_store(fake); registry.dispatch(\"context\", fake)";
                let raw = r#"with_event_store(fake); registry.dispatch("context", fake)"#;
            }
            #[test]
            fn actual() {
                assert!(with_event_store(store).is_ok());
                assert_eq!(registry.dispatch("context", params).await.unwrap(), expected);
                custom! { branch => registry.dispatch("memory.recall", params); with_event_store(store) }
            }
        "###;
    let tests = census_tests(source).unwrap();
    assert_eq!(tests.len(), 2);
    assert!(!tests[0].calls.contains("with_event_store"));
    assert!(!tests[0].calls.contains("dispatch"));
    assert!(tests[0].dispatch_verbs.is_empty());
    assert!(tests[1].calls.contains("with_event_store"));
    assert!(tests[1].calls.contains("dispatch"));
    assert_eq!(
        tests[1].dispatch_verbs,
        std::collections::BTreeSet::from(["context".into(), "memory.recall".into()])
    );
}

#[test]
fn serial_census_uses_first_acquisitions_for_reentrant_keys() {
    let source = "#[test] #[serial(alpha, beta)] #[serial(alpha)] fn first() {}\n\
                      #[test] #[serial(beta)] #[serial(alpha)] fn second() {}";
    let tests = census_tests(source).unwrap();
    assert_eq!(tests[0].serial_keys, ["alpha", "beta"]);
    assert!(serial_fixture_conflicts(source).is_empty());
}

#[test]
fn serial_census_surfaces_source_and_serial_argument_parse_failures() {
    assert!(census_tests("#[test] fn broken(").is_err());
    let error =
        census_tests("#[test] #[serial(config_ledger, crate = wrapper)] fn test() {}").unwrap_err();
    assert!(error
        .to_string()
        .contains("unsupported serial attribute arguments"));
}

/// An event-backed registry can drain the process-wide config ledger at
/// dispatch, and `record_config_locked` (and every `OnceLock` reader
/// that wraps it — `context_profile_enabled`, `recall_profile_enabled`,
/// `ann_overfetch_max_rounds`, `ann_ready_timeout_ms`,
/// `recall_deadline_ms`, `request_read_timeout`,
/// `backend_search_timeout_ms`, ... — enumerated here only as the
/// evidence that motivated widening the seed, never as the source of
/// truth for who counts) writes to it, so every compiled test that
/// reaches either — directly, through a same-text wrapper, or by
/// dispatching a verb whose handler reaches one — must join the
/// ledger's serial group even when its own assertion is about another
/// audit field.
///
/// The seed is deliberately just the two true seams
/// (`with_event_store`, `record_config_locked`) rather than a
/// hand-maintained list of every wrapper: `file_seam_names` grows the
/// known set to a fixed point, so a future `OnceLock` reader that wraps
/// `record_config_locked` is picked up the moment it exists, without
/// anyone remembering to add it here.
#[test]
fn event_store_test_fixtures_are_config_ledger_serialized() {
    let sources = workspace_rust_sources();
    assert!(
        !sources.is_empty(),
        "the workspace source scan found no .rs files under crates/*/src or \
             crates/*/tests; the census's own file walk is broken, not the population \
             it walks"
    );

    let base_seed = ["with_event_store", "record_config_locked"];

    // A dispatch match arm (`"context" => self.handle_context(...)`)
    // and the handler it names are frequently split across files
    // within one crate (`khive-pack-kg`'s `dispatch.rs` vs.
    // `handlers/context.rs`), so resolving "does verb X's handler reach
    // a ledger producer" needs a wider-than-one-file view. Per crate is
    // still a real visibility boundary — a pack's dispatch table only
    // ever calls its own handlers — unlike a workspace-wide view,
    // which would risk resolving a handler name that happens to
    // collide across unrelated crates. Built from `src/` text only:
    // `tests/` helpers of the same name must never leak into what
    // counts as "the crate's own handler".
    let mut crate_src_blobs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut crate_src_bodies: std::collections::HashMap<String, Vec<Vec<(String, String)>>> =
        std::collections::HashMap::new();
    for (path, text) in &sources {
        if !path.components().any(|c| c.as_os_str() == "src") {
            continue;
        }
        let Some(key) = crate_key(path) else {
            continue;
        };
        let blob = crate_src_blobs.entry(key.clone()).or_default();
        blob.push_str(text);
        blob.push('\n');
        crate_src_bodies
            .entry(key)
            .or_default()
            .push(fn_bodies(text));
    }
    let mut crate_ledger_verbs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (crate_name, blob) in &crate_src_blobs {
        let empty = Vec::new();
        let bodies_by_file = crate_src_bodies.get(crate_name).unwrap_or(&empty);
        let producers = crate_seam_names(bodies_by_file, &base_seed);
        let verbs: Vec<String> = dispatch_verb_handlers(blob)
            .into_iter()
            .filter(|(_, handler)| producers.iter().any(|p| p == handler))
            .map(|(verb, _)| verb)
            .collect();
        crate_ledger_verbs.insert(crate_name.clone(), verbs);
    }

    // Every source file in the crate (`src/` and `tests/` alike) —
    // resolves an ordinary same-crate function-call chain that crosses
    // files (a coordinator method in `dispatch.rs` calling a config
    // reader that lands in `dispatch.rs` too, reached from a test in
    // `tests.rs`) for the direct-call match below. Kept separate from
    // `crate_src_bodies` above: that population stays `src/`-only so a
    // `tests/`-only helper can never be misread as a pack's own
    // dispatch handler.
    //
    // A pack's own verb-dispatch table (`dispatch_verb_handlers`'s own
    // target shape — one function whose body is a `"verb" => ...
    // handle_x(...)` match with many arms) is excluded from this
    // population: "body contains a call to a known name" is sound only
    // for a body that always makes that call, and a dispatch table's
    // body contains a call to nearly every handler in the pack while
    // any one invocation only ever takes one arm. Leaving it in would
    // promote the dispatch function itself the moment *any* single
    // verb's handler reaches a seam, which reads as "every verb reaches
    // the ledger" — the false-positive an ordinary wrapper closure
    // cannot produce, verb-routing is already resolved precisely by the
    // separate `crate_ledger_verbs`/`CensusTest::dispatch_verbs` path,
    // keyed by which verb string was actually invoked.
    let mut crate_all_bodies: std::collections::HashMap<String, Vec<Vec<(String, String)>>> =
        std::collections::HashMap::new();
    for (path, text) in &sources {
        let Some(key) = crate_key(path) else {
            continue;
        };
        let bodies: Vec<(String, String)> = fn_bodies(text)
            .into_iter()
            .filter(|(_, body)| dispatch_verb_handlers(body).len() <= 1)
            .collect();
        crate_all_bodies.entry(key).or_default().push(bodies);
    }
    let mut crate_direct_seams: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (crate_name, bodies_by_file) in &crate_all_bodies {
        crate_direct_seams.insert(
            crate_name.clone(),
            crate_seam_names(bodies_by_file, &base_seed),
        );
    }

    let mut candidate_count = 0usize;
    let mut offenders = Vec::new();
    let mut serial_orders = SerialLockOrders::default();

    for (path, text) in &sources {
        let seam_names = file_seam_names(text, &base_seed);
        let crate_key_for_path = crate_key(path);
        let crate_verbs = crate_key_for_path
            .as_deref()
            .and_then(|key| crate_ledger_verbs.get(key));
        let crate_seams = crate_key_for_path
            .as_deref()
            .and_then(|key| crate_direct_seams.get(key));
        let tests = census_tests(text)
            .unwrap_or_else(|error| panic!("{}: census parse failed: {error}", path.display()));
        for test in tests {
            let name = format!("{}:{}", path.display(), test.name);
            serial_orders.record(&name, &test.serial_keys);

            let matched_direct = seam_names
                .iter()
                .chain(crate_seams.into_iter().flatten())
                .find(|seam| test.calls.contains(*seam));
            let matched: Option<String> = matched_direct.cloned().or_else(|| {
                crate_verbs.and_then(|verbs| {
                    verbs
                        .iter()
                        .find(|verb| test.dispatch_verbs.contains(*verb))
                        .map(|verb| format!("dispatch(\"{verb}\")"))
                })
            });
            let Some(matched) = matched else {
                continue;
            };
            candidate_count += 1;

            let has_group = test.serial_keys.iter().any(|key| key == "config_ledger");
            if !has_group {
                offenders.push(format!(
                    "{name} (reaches config-ledger seam via `{matched}`)"
                ));
            }
        }
    }

    assert!(
        candidate_count > 0,
        "census found zero config-ledger-reaching test candidates across the whole \
             workspace scan ({} source files) — the scan is broken, not the \
             population it should have found (khive-runtime's own config-ledger \
             tests alone are known callers)",
        sources.len()
    );
    assert!(
        offenders.is_empty(),
        "config-ledger-reaching pack tests must use #[serial(config_ledger)]; \
             offenders: {offenders:?}"
    );
    assert!(
        serial_orders.conflicts.is_empty(),
        "serial_test lock pairs must have consistent acquisition order across tests; \
             keys sort within each attribute, while later attributes acquire first: {:?}",
        serial_orders.conflicts
    );
}

fn only_git_digest_event(store: &MemoryEventStore) -> Event {
    let events: Vec<Event> = store
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.verb == "git.digest")
        .cloned()
        .collect();
    assert_eq!(
        events.len(),
        1,
        "expected exactly one git.digest receipt event"
    );
    events[0].clone()
}

#[tokio::test]
#[serial(config_ledger)]
async fn git_digest_success_returns_complete_durable_receipt() {
    let project_id = uuid::Uuid::new_v4();
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(GitDigestResultPack { project_id });
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");

    let result = registry
        .dispatch_with_identity(
            "git.digest",
            serde_json::json!({
                "source": "https://user:SECRET@example.invalid/org/repo",
            }),
            Some(RequestIdentity {
                namespace: Namespace::local().as_str().to_string(),
                request_id: Some(1_510),
                ..Default::default()
            }),
        )
        .await
        .expect("durably receipted digest succeeds");
    let receipt_id = result["receipt_id"]
        .as_str()
        .and_then(|raw| raw.parse::<uuid::Uuid>().ok())
        .expect("response has UUID receipt_id");

    let event = only_git_digest_event(&store);
    assert_eq!(event.id, receipt_id);
    assert_eq!(event.target_id, Some(project_id));
    assert_eq!(event.verb, "git.digest");
    assert_eq!(event.outcome, EventOutcome::Success);
    assert_eq!(event.payload_schema_version, 2);
    assert_eq!(event.payload["result"], result);
    assert_eq!(event.payload["resource"]["request_id"], 1_510);
    assert_eq!(event.payload["result"]["commits_ingested"], 2);
    assert_eq!(event.payload["result"]["issues_ingested"], 3);
    assert_eq!(event.payload["result"]["prs_ingested"], 5);
    assert!(
        !event.payload.to_string().contains("SECRET"),
        "receipt must not persist the caller's source URL or credentials"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn malformed_git_digest_report_appends_one_generic_error_audit() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(MalformedGitDigestResultPack);
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch("git.digest", serde_json::json!({}))
        .await
        .expect_err("malformed receipt identity must fail the response");
    assert!(matches!(
        err,
        RuntimeError::AuditObligation { ref failure, .. }
            if failure.message.starts_with("git_digest_receipt_persist_failed:")
    ));

    let event = only_git_digest_event(&store);
    assert_eq!(event.outcome, EventOutcome::Error);
    assert_eq!(event.payload_schema_version, 1);
    assert!(
        event.payload.get("result").is_none(),
        "a generic Error audit must not masquerade as a success receipt"
    );
    let audit: AuditEvent =
        serde_json::from_value(event.payload).expect("generic payload remains an AuditEvent");
    assert_eq!(audit.verb, "git.digest");
    assert_eq!(audit.decision, AuditDecision::Allow);
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn git_digest_receipt_append_failure_never_returns_unqualified_success() {
    let before = audit_append_failure_count();
    let before_obligation = audit_obligation_append_failure_count();
    let store = Arc::new(MemoryEventStore {
        fail_appends: true,
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(GitDigestResultPack {
        project_id: uuid::Uuid::new_v4(),
    });
    builder.with_event_store(store);
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch("git.digest", serde_json::json!({}))
        .await
        .expect_err("receipt persistence failure must fail the response");
    assert!(
        matches!(&err, RuntimeError::AuditObligation { failure, .. }
            if failure.message.starts_with("git_digest_receipt_persist_failed:")
                && failure.message.contains("writes may have committed")),
        "error is stable, safe, and retry-aware: {err}"
    );
    // The git.digest receipt is obligation-bearing (`GitDigestReceipt`
    // classifies as `DispatchObligation`) and this failure propagated
    // into the dispatch's own error above, so it counts on the
    // obligation counter, not the swallowed-failures one.
    assert_eq!(audit_append_failure_count(), before);
    assert_eq!(
        audit_obligation_append_failure_count(),
        before_obligation + 1
    );
    // #2784: the same real sink failure must be discoverable through the
    // public diagnostics report, not only this private counter accessor.
    let runtime = crate::KhiveRuntime::memory().expect("diagnostics runtime");
    let report = runtime
        .db_diagnostics()
        .await
        .expect("diagnostics after audit failure");
    assert_eq!(
        report.writer_contention.audit_obligation_append_failures,
        Some(before_obligation + 1)
    );
    assert_eq!(report.writer_contention.audit_append_failures, Some(before));
    assert!(report
        .writer_contention
        .audit_obligation_append_failures_unavailable_reason
        .is_none());
    let json = serde_json::to_value(report).expect("serialized diagnostics");
    assert_eq!(
        json["writer_contention"]["audit_obligation_append_failures"],
        before_obligation + 1
    );
}

#[tokio::test]
async fn git_digest_without_event_store_fails_safe_after_handler_success() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(GitDigestResultPack {
        project_id: uuid::Uuid::new_v4(),
    });
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch("git.digest", serde_json::json!({}))
        .await
        .expect_err("a successful digest needs a durable store");
    assert!(matches!(
        err,
        RuntimeError::AuditObligation { ref failure, .. }
            if failure.message.starts_with("git_digest_receipt_persist_failed:")
    ));
}

#[tokio::test]
#[serial(config_ledger)]
async fn git_digest_gate_unavailable_precedes_the_receipt_contract() {
    #[derive(Debug)]
    struct FailingGate;
    impl Gate for FailingGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
            Err(khive_gate::GateError::Internal(
                "injected gate failure".into(),
            ))
        }
    }

    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(GitDigestResultPack {
        project_id: uuid::Uuid::new_v4(),
    });
    builder.with_gate(Arc::new(FailingGate));
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch("git.digest", serde_json::json!({}))
        .await
        .expect_err("gate unavailability must refuse before the handler or receipt path");
    assert!(matches!(
        err,
        RuntimeError::GateUnavailable { ref verb, ref reason }
            if verb == "git.digest"
                && reason == "gate backend unavailable"
                && !reason.contains("injected gate failure")
    ));
    let event = only_git_digest_event(&store);
    assert_eq!(event.outcome, EventOutcome::Error);
    assert_eq!(event.payload["decision"], "gate_unavailable");
    assert!(event.payload.get("result").is_none());
}

#[tokio::test]
#[serial(config_ledger)]
async fn intercepted_gate_error_returns_typed_refusal_without_invoking_operation() {
    #[derive(Debug)]
    struct FailingGate;
    impl Gate for FailingGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
            Err(khive_gate::GateError::Internal(
                "intercepted gate broken".into(),
            ))
        }
    }

    // Entry reset, not exit: a `config_ledger`-grouped test that panics
    // after queueing a row (elsewhere in this group) would otherwise
    // leave it for whichever test the serial lock hands off to next;
    // this test's exact `events.len() == 1` assertion below has no
    // tolerance for an inherited row.
    let _ = crate::config_ledger::drain_config_locked();

    let invoked = Arc::new(AtomicUsize::new(0));
    let invoked_by_operation = Arc::clone(&invoked);
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(FailingGate));
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");
    let identity = RequestIdentity {
        namespace: "identity-default".to_string(),
        request_id: Some(1_600),
        ..Default::default()
    };

    let err = registry
        .dispatch_intercepted_with_identity(
            "list",
            &serde_json::json!({"namespace": "test-ns"}),
            Some(&identity),
            move |_namespace| {
                invoked_by_operation.fetch_add(1, Ordering::SeqCst);
                async move { Ok(serde_json::json!({"invoked": true})) }
            },
        )
        .await
        .expect_err("gate unavailability must refuse intercepted dispatch");

    assert!(matches!(
        err,
        RuntimeError::GateUnavailable { ref verb, ref reason }
            if verb == "list"
                && reason == "gate backend unavailable"
                && !reason.contains("intercepted gate broken")
    ));
    assert_eq!(
        invoked.load(Ordering::SeqCst),
        0,
        "intercepted operation must not run after a gate infrastructure error"
    );

    let events = store.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].verb, "list");
    assert_eq!(events[0].namespace, "test-ns");
    assert_eq!(events[0].outcome, EventOutcome::Error);
    assert_eq!(events[0].payload["decision"], "gate_unavailable");
    assert!(events[0].payload.get("deny_reason").is_none());
    assert_eq!(events[0].payload["resource"]["work_class"], "interactive");
    assert_eq!(events[0].payload["resource"]["request_id"], 1_600);
    assert!(events[0].payload["resource"].get("cost_unit").is_none());
}

#[tokio::test]
#[serial(config_ledger)]
async fn intercepted_deny_remains_distinct_and_does_not_invoke_operation() {
    #[derive(Debug)]
    struct DenyingGate;
    impl Gate for DenyingGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("intercepted policy denied"))
        }
    }

    // Entry reset, not exit — see the sibling test above for why an
    // exact `events.len() == 1` assertion needs a clean ledger.
    let _ = crate::config_ledger::drain_config_locked();

    let invoked = Arc::new(AtomicUsize::new(0));
    let invoked_by_operation = Arc::clone(&invoked);
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(DenyingGate));
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch_intercepted_with_identity("list", &Value::Null, None, move |_namespace| {
            invoked_by_operation.fetch_add(1, Ordering::SeqCst);
            async move { Ok(serde_json::json!({"invoked": true})) }
        })
        .await
        .expect_err("explicit gate denial must refuse intercepted dispatch");

    let RuntimeError::PermissionDenied {
        verb,
        reason,
        receipt,
    } = err
    else {
        panic!("expected PermissionDenied, got {err:?}");
    };
    assert_eq!(verb, "list");
    assert_eq!(reason, "intercepted policy denied");
    assert_eq!(
        receipt.audit_outcome,
        crate::error::DenialAuditOutcome::Committed,
        "the intercepted path commits its denial row before refusing"
    );
    assert_eq!(invoked.load(Ordering::SeqCst), 0);

    let events = store.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        Some(events[0].id),
        receipt.audit_event_id,
        "the receipt names the committed row"
    );
    assert_eq!(events[0].outcome, EventOutcome::Denied);
    assert_eq!(events[0].payload["decision"], "deny");
    assert_eq!(
        events[0].payload["deny_reason"],
        "intercepted policy denied"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn intercepted_denied_dispatch_masks_secret_shaped_deny_reason_in_stored_event() {
    // Falsifiable arm for the audit-masking fix (khive#2944), exercised
    // through the intercepted dispatch path's OWN `AuditEvent`
    // construction site
    // (`dispatch_intercepted_with_metadata_and_disposition`), distinct
    // from the plain-dispatch site covered by
    // `denied_dispatch_masks_secret_shaped_deny_reason_in_stored_event`.
    // Masking only the plain-dispatch site would leave this call site's
    // stored row carrying the raw credential and this test red.
    #[derive(Debug)]
    struct InterceptedSecretDenyGate;
    impl Gate for InterceptedSecretDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            let reason = "postgres://svc:not-a-real-secret@internal-host in denied request"; // gitleaks:allow
            Ok(GateDecision::deny(reason))
        }
    }

    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(InterceptedSecretDenyGate));
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");

    let _ = registry
        .dispatch_intercepted_with_identity(
            "list",
            &Value::Null,
            None,
            move |_namespace| async move { Ok(serde_json::json!({"invoked": true})) },
        )
        .await
        .expect_err("explicit gate denial must refuse intercepted dispatch");

    let events = store.events.lock().unwrap();
    assert_eq!(events.len(), 1, "exactly one denial row must commit");
    let stored_reason = events[0].payload["deny_reason"]
        .as_str()
        .expect("deny_reason must be a string on the stored row");
    assert!(!stored_reason.is_empty());
    assert!(
        stored_reason.contains("in denied request"),
        "non-secret prose must survive masking: {stored_reason:?}"
    );
    assert!(
        !stored_reason.contains("not-a-real-secret"),
        "the durable row must never carry the raw credential: {stored_reason:?}"
    );
    assert!(
        stored_reason.contains("***MASKED***"),
        "the durable row must record that a credential was redacted: {stored_reason:?}"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn intercepted_git_digest_uses_the_same_receipt_contract() {
    let project_id = uuid::Uuid::new_v4();
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");

    let result = registry
        .dispatch_intercepted_with_identity(
            "git.digest",
            &serde_json::json!({}),
            Some(&RequestIdentity {
                namespace: Namespace::local().as_str().to_string(),
                request_id: Some(1_647),
                ..Default::default()
            }),
            |_namespace| async move {
                Ok(serde_json::json!({
                    "project_id": project_id,
                    "commits_ingested": 7,
                    "done": true,
                }))
            },
        )
        .await
        .expect("intercepted digest is durably receipted");

    let event = only_git_digest_event(&store);
    assert_eq!(result["receipt_id"], serde_json::json!(event.id));
    assert_eq!(event.payload["result"], result);
    assert_eq!(event.payload["resource"]["request_id"], 1_647);
}

#[tokio::test]
#[serial(config_ledger)]
async fn intercepted_git_digest_receipt_preserves_typed_metadata() {
    let project_id = uuid::Uuid::new_v4();
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");

    let outcome = registry
        .dispatch_intercepted_with_metadata_with_identity(
            "git.digest",
            &serde_json::json!({}),
            None,
            |_namespace| async move {
                Ok(InterceptedDispatchResult::new(
                    serde_json::json!({
                        "project_id": project_id,
                        "commits_ingested": 3,
                        "done": true,
                    }),
                    vec!["backend-a".to_string(), "backend-b".to_string()],
                ))
            },
        )
        .await
        .expect("metadata-bearing digest is durably receipted");

    let event = only_git_digest_event(&store);
    assert_eq!(outcome.result["receipt_id"], serde_json::json!(event.id));
    assert_eq!(event.payload["result"], outcome.result);
    assert_eq!(outcome.metadata, ["backend-a", "backend-b"]);
}

#[tokio::test]
#[serial(config_ledger)]
async fn intercepted_malformed_git_digest_appends_one_generic_error_audit() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch_intercepted_with_identity(
            "git.digest",
            &serde_json::json!({}),
            None,
            |_namespace| async {
                Ok(serde_json::json!({
                    "project_id": "not-a-uuid",
                    "done": true,
                }))
            },
        )
        .await
        .expect_err("malformed intercepted receipt must fail the response");
    assert!(matches!(
        err,
        RuntimeError::AuditObligation { ref failure, .. }
            if failure.message.starts_with("git_digest_receipt_persist_failed:")
    ));

    let event = only_git_digest_event(&store);
    assert_eq!(event.outcome, EventOutcome::Error);
    assert_eq!(event.payload_schema_version, 1);
    assert!(event.payload.get("result").is_none());
    let audit: AuditEvent =
        serde_json::from_value(event.payload).expect("generic payload remains an AuditEvent");
    assert_eq!(audit.verb, "git.digest");
    assert_eq!(audit.decision, AuditDecision::Allow);
}

#[tokio::test]
async fn allow_all_gate_default_remains_backward_compatible() {
    // No gate set — AllowAllGate is the default. Dispatch must succeed.
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    let reg = builder.build().expect("registry builds");

    let res = reg.dispatch("list", Value::Null).await.unwrap();
    assert_eq!(
        res["pack"], "alpha",
        "AllowAllGate must allow every verb — backward compat guarantee"
    );
    let res = reg.dispatch("create", Value::Null).await.unwrap();
    assert_eq!(res["pack"], "alpha");
}

#[tokio::test]
async fn deny_gate_returns_permission_denied_pack_never_invoked() {
    #[derive(Debug)]
    struct AlwaysDenyGate;
    impl Gate for AlwaysDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("test: always deny"))
        }
    }

    // Track whether dispatch was ever invoked on the pack.
    #[derive(Debug)]
    struct TrackedPack {
        invoked: Arc<AtomicUsize>,
    }

    impl khive_types::Pack for TrackedPack {
        const NAME: &'static str = "tracked";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "guarded",
            description: "a guarded verb",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for TrackedPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            self.invoked.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"invoked": true}))
        }
    }

    let invoked = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(TrackedPack {
        invoked: invoked.clone(),
    });
    builder.with_gate(Arc::new(AlwaysDenyGate));
    let reg = builder.build().expect("registry builds");

    let err = reg.dispatch("guarded", Value::Null).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::PermissionDenied { ref verb, ref reason, .. } if verb == "guarded" && reason.contains("always deny")),
        "expected PermissionDenied with verb=guarded and reason, got: {err:?}"
    );
    assert_eq!(
        invoked.load(Ordering::SeqCst),
        0,
        "pack dispatch MUST NOT be invoked when gate denies"
    );
}

#[tokio::test]
async fn update_denial_precedes_id_existence_resolution() {
    #[derive(Debug)]
    struct AlwaysDenyUpdateGate {
        checked: Arc<AtomicUsize>,
    }
    impl Gate for AlwaysDenyUpdateGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            self.checked.fetch_add(1, Ordering::SeqCst);
            Ok(GateDecision::deny("caller has no update capability"))
        }
    }

    #[derive(Debug)]
    struct ExistenceOracleUpdatePack {
        existing_id: String,
        invoked: Arc<AtomicUsize>,
    }

    impl khive_types::Pack for ExistenceOracleUpdatePack {
        const NAME: &'static str = "existence_oracle";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "update",
            description: "distinguish a present id from an absent id",
            visibility: Visibility::Verb,
            category: VerbCategory::Declaration,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for ExistenceOracleUpdatePack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            self.invoked.fetch_add(1, Ordering::SeqCst);
            match params.get("id").and_then(Value::as_str) {
                Some(id) if id == self.existing_id => Ok(serde_json::json!({"updated": id})),
                _ => Err(RuntimeError::NotFound("record".to_string())),
            }
        }
    }

    let existing_id = uuid::Uuid::new_v4().to_string();
    let absent_id = uuid::Uuid::new_v4().to_string();
    let invoked = Arc::new(AtomicUsize::new(0));
    let checked = Arc::new(AtomicUsize::new(0));

    let pack = || ExistenceOracleUpdatePack {
        existing_id: existing_id.clone(),
        invoked: Arc::clone(&invoked),
    };

    let mut control_builder = VerbRegistryBuilder::new();
    control_builder.register(pack());
    let control = control_builder.build().expect("control registry builds");
    control
        .dispatch("update", serde_json::json!({"id": existing_id.clone()}))
        .await
        .expect("positive control resolves the present id");
    assert!(matches!(
        control
            .dispatch("update", serde_json::json!({"id": absent_id.clone()}))
            .await,
        Err(RuntimeError::NotFound(_))
    ));
    assert_eq!(invoked.load(Ordering::SeqCst), 2);

    let mut denied_builder = VerbRegistryBuilder::new();
    denied_builder.register(pack());
    denied_builder.with_gate(Arc::new(AlwaysDenyUpdateGate {
        checked: Arc::clone(&checked),
    }));
    let denied = denied_builder.build().expect("denied registry builds");

    let present_error = denied
        .dispatch("update", serde_json::json!({"id": existing_id.clone()}))
        .await
        .expect_err("denied present-id update must not resolve the id");
    let absent_error = denied
        .dispatch("update", serde_json::json!({"id": absent_id.clone()}))
        .await
        .expect_err("denied absent-id update must not resolve the id");

    let denial = |error: RuntimeError| match error {
        RuntimeError::PermissionDenied { verb, reason, .. } => (verb, reason),
        other => panic!("expected gate refusal, got {other:?}"),
    };
    let present_denial = denial(present_error);
    let absent_denial = denial(absent_error);
    assert_eq!(present_denial.0, "update");
    assert_eq!(present_denial.1, "caller has no update capability");
    assert_eq!(present_denial, absent_denial);
    assert_eq!(
        checked.load(Ordering::SeqCst),
        2,
        "both denied requests must consult the configured gate"
    );
    assert_eq!(
        invoked.load(Ordering::SeqCst),
        2,
        "neither denied request may reach the existence oracle"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn runtime_audit_sink_uses_final_namespace_in_both_builder_orders() {
    for namespace_first in [true, false] {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        runtime
            .raw_events_for_namespace("local")
            .expect("local sink")
            .append_event(Event::new(
                "local",
                "list",
                EventKind::Audit,
                SubstrateKind::Event,
                "actor:unrelated",
            ))
            .await
            .expect("local control event");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_actor_id(Some("lambda:dispatcher".to_string()));
        if namespace_first {
            builder.with_default_namespace("audit-tenant");
        }
        builder
            .with_runtime_event_store(&runtime)
            .expect("configure runtime sink");
        if !namespace_first {
            builder.with_default_namespace("audit-tenant");
        }
        let registry = builder.build().expect("registry builds");
        registry
            .dispatch("list", serde_json::json!({}))
            .await
            .expect("dispatch persists its audit");

        // Raw writes retain their supplied namespace even when the sink's
        // read scope is stale, so a separate runtime accessor hides the bug.
        let page = registry
            .event_store()
            .expect("registry retains the sink")
            .query_events(
                EventFilter {
                    verbs: vec!["list".to_string()],
                    ..EventFilter::default()
                },
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .expect("query the registry's sink");
        assert_eq!(page.items.len(), 1, "namespace_first={namespace_first}");
        let event = &page.items[0];
        assert_eq!(event.namespace, "audit-tenant");
        assert_eq!(event.actor, "actor:lambda:dispatcher");
        assert_eq!(event.outcome, EventOutcome::Success);
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn runtime_audit_sink_configuration_preserves_last_setter() {
    #[derive(Clone, Copy, Debug)]
    enum Sink {
        Runtime,
        Custom,
        ReadOnly,
    }

    let runtime = KhiveRuntime::memory().expect("memory runtime");
    runtime
        .raw_events_for_namespace("audit-tenant")
        .expect("runtime sink")
        .append_event(Event::new(
            "audit-tenant",
            "audit.fixture",
            EventKind::Audit,
            SubstrateKind::Event,
            "actor:creator",
        ))
        .await
        .expect("runtime control event");
    let custom: Arc<dyn EventStore> = Arc::new(MemoryEventStore::default());

    for (first, last) in [
        (Sink::Runtime, Sink::Custom),
        (Sink::Runtime, Sink::ReadOnly),
        (Sink::Custom, Sink::Runtime),
        (Sink::Custom, Sink::ReadOnly),
        (Sink::ReadOnly, Sink::Runtime),
        (Sink::ReadOnly, Sink::Custom),
    ] {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("audit-tenant");
        for sink in [first, last] {
            match sink {
                Sink::Runtime => {
                    builder
                        .with_runtime_event_store(&runtime)
                        .expect("configure runtime sink");
                }
                Sink::Custom => {
                    builder.with_event_store(custom.clone());
                }
                Sink::ReadOnly => {
                    builder.with_read_only_audit_store();
                }
            }
        }
        let registry = builder.build().expect("registry builds");
        match last {
            Sink::Runtime => {
                let store = registry.event_store().expect("runtime sink wins");
                assert!(!Arc::ptr_eq(&store, &custom));
                assert_eq!(
                    store.count_events(EventFilter::default()).await.unwrap(),
                    1,
                    "runtime event remains readable after {first:?}"
                );
                assert!(registry.audit_persistence_advisory().is_none());
                assert!(registry.audit_batch_metrics().is_some());
            }
            Sink::Custom => {
                assert!(Arc::ptr_eq(
                    &registry.event_store().expect("custom sink wins"),
                    &custom
                ));
                assert!(registry.audit_persistence_advisory().is_none());
                assert!(registry.audit_batch_metrics().is_some());
            }
            Sink::ReadOnly => {
                assert!(registry.event_store().is_none());
                assert!(registry.audit_persistence_advisory().is_some());
                assert!(registry.audit_batch_metrics().is_none());
            }
        }
    }
}

#[test]
#[serial(config_ledger)]
fn runtime_audit_sink_is_not_bound_when_replaced_or_building_metadata() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let runtime = KhiveRuntime::new(crate::runtime::RuntimeConfig {
        db_path: None,
        packs: vec![],
        brain_profile: None,
        actor_id: None,
        events_split: Some(crate::events_split::EventsSplitConfig {
            db_path: directory.path().to_path_buf(),
            socket_path: None,
        }),
        ..crate::runtime::RuntimeConfig::no_embeddings()
    })
    .expect("runtime creation does not open the events sink");

    let mut metadata = VerbRegistryBuilder::new();
    metadata.register(AlphaPack);
    metadata
        .with_runtime_event_store(&runtime)
        .expect("configuration defers the invalid sink");
    let metadata = metadata.build_metadata().expect("metadata needs no sink");
    assert!(metadata.has_verb("list"));
    assert!(metadata.registry.event_store().is_none());
    assert!(metadata.registry.audit_batch_metrics().is_none());

    let mut custom = VerbRegistryBuilder::new();
    custom.with_runtime_event_store(&runtime).unwrap();
    custom.with_event_store(Arc::new(MemoryEventStore::default()));
    assert!(custom
        .build()
        .expect("custom replaces runtime")
        .event_store()
        .is_some());

    let mut read_only = VerbRegistryBuilder::new();
    read_only.with_runtime_event_store(&runtime).unwrap();
    read_only.with_read_only_audit_store();
    assert!(read_only
        .build()
        .expect("read-only replaces runtime")
        .event_store()
        .is_none());

    let mut serving = VerbRegistryBuilder::new();
    serving.with_runtime_event_store(&runtime).unwrap();
    assert!(
        serving.build().is_err(),
        "serving build must surface the error opening a directory as an events database"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn audit_event_persists_to_event_store_on_allow() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", serde_json::json!({"namespace": "test-ns"}))
        .await
        .unwrap();

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(count, 1, "one audit event persisted to EventStore on allow");

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let ev = &page.items[0];
    assert_eq!(ev.verb, "list");
    assert_eq!(ev.namespace, "test-ns");
    assert_eq!(ev.substrate, SubstrateKind::Event);
    assert_eq!(ev.outcome, EventOutcome::Success);
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn audit_append_failure_fails_an_obligation_bearing_dispatch() {
    let before = audit_append_failure_count();
    let before_obligation = audit_obligation_append_failure_count();

    let successful_store = Arc::new(MemoryEventStore::default());
    let mut successful_builder = VerbRegistryBuilder::new();
    successful_builder.register(AlphaPack);
    successful_builder.with_event_store(successful_store);
    let successful_registry = successful_builder.build().expect("registry builds");
    successful_registry
        .dispatch("list", Value::Null)
        .await
        .expect("successful audit append must not affect dispatch");
    assert_eq!(
        audit_append_failure_count(),
        before,
        "successful audit appends must not increment the swallowed-failure counter"
    );
    assert_eq!(
        audit_obligation_append_failure_count(),
        before_obligation,
        "successful audit appends must not increment the obligation-failure counter"
    );

    // ADR-133 D2/D3/D4: `list`'s deferred audit row is a
    // `DispatchSucceeded` obligation. A dispatch must not report success
    // when the row that accounts for it did not commit, so a persistent
    // commit failure here must fail the dispatch that would otherwise
    // have reported success.
    let failing_store = Arc::new(MemoryEventStore {
        fail_appends: true,
        ..MemoryEventStore::default()
    });
    let mut failing_builder = VerbRegistryBuilder::new();
    failing_builder.register(AlphaPack);
    failing_builder.with_event_store(failing_store);
    let failing_registry = failing_builder.build().expect("registry builds");
    let err = failing_registry
        .dispatch("list", Value::Null)
        .await
        .expect_err("a persistent obligation-bearing audit commit failure must fail the dispatch");
    assert!(
        matches!(&err, RuntimeError::AuditObligation { failure, .. }
            if failure.message.contains("audit obligation commit failed")),
        "error names the obligation failure so it is distinguishable from a handler error: {err}"
    );

    // `list`'s deferred audit row is `DispatchSucceeded`, an obligation
    // producer, so this propagated failure belongs on the obligation
    // counter — the swallowed-failure counter must not move for it.
    assert_eq!(
        audit_append_failure_count(),
        before,
        "an obligation failure must never inflate the swallowed-failure counter"
    );
    assert_eq!(
        audit_obligation_append_failure_count(),
        before_obligation + 1,
        "the failed obligation append must remain visible to diagnostics"
    );
}

/// An obligation-bearing audit row is written AFTER the handler returns and FROM the
/// handler's own return value. So when that row fails to commit,
/// `fold_audit_obligation` turns a would-be success into an error for a dispatch whose
/// effect has ALREADY happened and cannot be rolled back by it.
///
/// The existing obligation test above proves the flip using `list`, a read, where the
/// distinction does not matter. This one pins the part that decides caller behaviour:
/// the write landed, and the caller was told it failed. A caller that treats this error
/// as "it did not run" and retries therefore applies the effect twice, which is what the
/// last assertion covers.
///
/// The handler and the event store share one trace vector, so the ordering claim is
/// observed rather than assumed. That matters more than it looks: asserting only the
/// caller-visible error and the handler's effect would leave this test green against an
/// implementation that submits no audit row at all, or that submits one built from the
/// error path — both of which contradict the contract while producing exactly the same
/// error string.
///
/// The outcome alone does not separate those. A row can carry `Success` and still have
/// been built without the handler's return value, which is a third implementation and
/// also wrong. So the trace records whether the row's resource carries `cost_unit`:
/// `resource_payload` derives that key from `ok_val`, and `base_resource_payload`
/// documents that it omits it. Asserting the key is what pins result-sourcing; the
/// verb is asserted alongside it so a fabricated row for some other verb cannot satisfy
/// the same check.
///
/// Not pinned here: that the row commits on a SEPARATE writer acquisition from the
/// handler's. The store double has no writer to observe, so that half of the mechanism
/// needs a different fixture than this one.
// The config ledger is process-global and an event-store dispatch drains its
// queue before invoking the pack, so a concurrent config_ledger test can land
// a submission ahead of this handler's effect and break the first-entry
// assertion below. That group is held for the position assertion, not for the
// audit counters the other two groups cover.
#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn obligation_failure_reports_a_write_that_already_committed() {
    /// `total` is what `cost_unit` is computed from, so this number is the
    /// test's handle on whether the audit row was built from the return
    /// value. 41 is arbitrary but distinctive: it makes the expected
    /// `cost_unit` 42 (`base_weight` 1 + `item_count` 41 * `model_count`
    /// 1), a value no default path produces.
    const RETURNED_TOTAL: u64 = 41;

    #[derive(Debug)]
    struct RecordingWritePack {
        committed: Arc<std::sync::Mutex<Vec<TraceEntry>>>,
    }

    impl Pack for RecordingWritePack {
        const NAME: &'static str = "recording_write";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        // `knowledge.index` rather than `create`, and the choice is
        // load-bearing rather than incidental: it is the one verb whose
        // `cost_unit` reads the handler's return value (`item_count` takes
        // `result["total"]`). Under any other verb `item_count` is the
        // constant `1`, so the recorded `cost_unit` would be the same
        // whether the row was built from the result or from a static value,
        // and the assertion below could not tell those apart.
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "knowledge.index",
            description: "record one committed write",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for RecordingWritePack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            // Stands in for a committed effect: by the time this returns, the write
            // is done and nothing downstream can undo it. Pushed onto the SAME
            // vector the event store traces into, so the relative order of the
            // effect and the audit submission is observable rather than assumed.
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unnamed")
                .to_string();
            self.committed
                .lock()
                .expect("committed lock")
                .push(TraceEntry::Effect { name: name.clone() });
            Ok(serde_json::json!({ "created": name, "total": RETURNED_TOTAL }))
        }
    }

    let committed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let failing_store = Arc::new(MemoryEventStore {
        fail_appends: true,
        trace: Some(Arc::clone(&committed)),
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(RecordingWritePack {
        committed: Arc::clone(&committed),
    });
    builder.with_event_store(failing_store);
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch("knowledge.index", serde_json::json!({"name": "first"}))
        .await
        .expect_err("an obligation commit failure must fail the dispatch");
    assert!(
        matches!(&err, RuntimeError::AuditObligation { failure, .. }
            if failure.message.contains("audit obligation commit failed")),
        "the error must name the obligation failure, since that string is what tells a \
             caller the effect landed: {err}"
    );

    // The trace carries both sides, so each of the three claims above is an
    // assertion rather than a comment. Read as a sequence it says: the handler's
    // effect committed, and only THEN was an audit row submitted -- a row built
    // from the successful result, which is what makes it the obligation row and
    // not an error row.
    let first_pass = committed.lock().expect("committed lock").clone();
    let first_effect = TraceEntry::Effect {
        name: "first".to_string(),
    };
    assert_eq!(
        first_pass.first(),
        Some(&first_effect),
        "the handler's effect must land FIRST: an implementation that submitted the \
             audit row before dispatching would satisfy every other assertion here"
    );
    let audit_rows: Vec<&TraceEntry> = first_pass
        .iter()
        .filter(|entry| matches!(entry, TraceEntry::Audit { .. }))
        .collect();
    assert!(
        !audit_rows.is_empty(),
        "an audit row must actually be SUBMITTED; without this assertion the test \
             passes against an implementation that returns the same error and never \
             builds a row at all, which is a different defect wearing the same error \
             string. Trace was {first_pass:?}"
    );
    let expected_cost_unit = serde_json::json!(RETURNED_TOTAL + 1);
    assert!(
        audit_rows.iter().any(|entry| matches!(
            entry,
            TraceEntry::Audit {
                outcome: EventOutcome::Success,
                verb,
                cost_unit: Some(cost_unit),
                ..
            } if verb.as_str() == "knowledge.index" && *cost_unit == expected_cost_unit
        )),
        "the submitted row must be the SUCCESS row for THIS verb, carrying a resource \
             computed FROM the handler's return value. The outcome alone is not enough: an \
             implementation that stamps Success on a row built without `ok_val` would \
             satisfy an outcome-only assertion while breaking the contract this test \
             exists for. The exact value is the discriminator, not the key's presence: \
             `resource_payload` inserts `cost_unit` unconditionally, so presence survives a \
             static `ok_val`, while {expected_cost_unit} is reachable only from the \
             returned `total` of {RETURNED_TOTAL} (`base_weight` 1 + `item_count` \
             {RETURNED_TOTAL} * `model_count` 1). Substituting a null or static result \
             collapses it to 1, and the error path's `base_resource_payload` omits the key \
             entirely (`cost_unit: None` here), so all three implementations are \
             distinguishable. Trace was {first_pass:?}"
    );
    assert_eq!(
        first_pass
            .iter()
            .filter(|entry| **entry == first_effect)
            .count(),
        1,
        "exactly one effect on the first pass"
    );

    // What a caller does on a failure it believes means "did not run".
    let _ = registry
        .dispatch("knowledge.index", serde_json::json!({"name": "first"}))
        .await
        .expect_err("the retry fails the same way");
    assert_eq!(
        committed
            .lock()
            .expect("committed lock")
            .iter()
            .filter(|entry| **entry == first_effect)
            .count(),
        2,
        "retrying this error double-writes; a caller must re-derive state instead of \
             resubmitting"
    );
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn config_locked_row_degrades_without_failing_the_dispatch_that_observed_it() {
    // Deny-gate a dispatch so the only append this call makes is the
    // immediate `ConfigLocked` drain in the gate-check block: the
    // `GateDenied` row and the eventual `PermissionDenied` return are
    // unaffected by the store either way (see the two `let _ =` sites
    // above), so any failure this test observes is isolated to the
    // pure-observability `ConfigLocked` row. The `fail_appends: true`
    // store also fails the fire-and-forget `GateDenied` append, which
    // now counts on the obligation counter (`#[serial(...)]` above
    // keeps that from racing this file's exact-delta assertions on it).
    #[derive(Debug)]
    struct DenyGate;
    impl Gate for DenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
            Ok(GateDecision::Deny {
                reason: "denied for test".to_string(),
            })
        }
    }

    crate::config_ledger::record_config_locked("adr133_test_key", "adr133_test_value");

    let store = Arc::new(MemoryEventStore {
        fail_appends: true,
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(DenyGate));
    builder.with_event_store(store);
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch("list", Value::Null)
        .await
        .expect_err("the gate denies every request");
    assert!(
        matches!(err, RuntimeError::PermissionDenied { .. }),
        "a pure-observability row's failure must never surface as the dispatch error: {err}"
    );

    let metrics = registry
        .audit_batch_metrics()
        .expect("with_event_store configures the ADR-133 seam");
    assert!(
        metrics.degraded,
        "the config-locked row's commit failure must be visible as degradation"
    );
    assert!(metrics.degraded_rows >= 1);
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
async fn config_locked_row_failure_never_fails_a_dispatch_that_would_otherwise_succeed() {
    // ADR-133 criterion 4's success half: a pure-observability row's
    // commit failure must degrade gracefully without touching the
    // caller-visible outcome of a dispatch that has nothing to do with
    // it. Only the `ConfigLocked` generation fails here — the gate
    // allows the call, so `list`'s own `DispatchSucceeded` obligation
    // row commits in a later, unaffected generation.
    crate::config_ledger::record_config_locked(
        "adr133_success_path_key",
        "adr133_success_path_value",
    );

    let store = Arc::new(MemoryEventStore {
        fail_kind: Some(EventKind::ConfigLocked),
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store);
    let registry = builder.build().expect("registry builds");

    let result = registry
        .dispatch("list", Value::Null)
        .await
        .expect("a config-locked row's commit failure must never fail an unrelated dispatch");
    assert_eq!(
        result,
        serde_json::json!({ "pack": "alpha", "verb": "list" })
    );

    let metrics = registry
        .audit_batch_metrics()
        .expect("with_event_store configures the ADR-133 seam");
    assert!(
        metrics.degraded_rows >= 1,
        "the config-locked row's failure must remain visible as degradation"
    );
}

/// An `EventStore` that only implements the base trait — the
/// unmodified pre-ADR-133 shape. `preflight_event`/
/// `append_events_idempotent`/`supports_idempotent_audit_batch` are all
/// inherited defaults.
#[derive(Default)]
struct LegacyEventStore {
    events: std::sync::Mutex<Vec<Event>>,
}

#[async_trait]
impl EventStore for LegacyEventStore {
    async fn append_event(&self, event: Event) -> khive_storage::StorageResult<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
    async fn append_events(
        &self,
        events: Vec<Event>,
    ) -> khive_storage::StorageResult<BatchWriteSummary> {
        let attempted = events.len() as u64;
        self.events.lock().unwrap().extend(events);
        Ok(BatchWriteSummary {
            attempted,
            affected: attempted,
            ..BatchWriteSummary::default()
        })
    }
    async fn get_event(&self, id: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned())
    }
    async fn query_events(
        &self,
        _filter: EventFilter,
        _page: PageRequest,
    ) -> khive_storage::StorageResult<Page<Event>> {
        let items = self.events.lock().unwrap().clone();
        let total = items.len() as u64;
        Ok(Page {
            items,
            total: Some(total),
        })
    }
    async fn count_events(&self, _filter: EventFilter) -> khive_storage::StorageResult<u64> {
        Ok(self.events.lock().unwrap().len() as u64)
    }
}

#[test]
#[serial(config_ledger)]
fn build_rejects_a_configured_event_store_incompatible_with_the_audit_batch_seam() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(Arc::new(LegacyEventStore::default()));
    let err = match builder.build() {
        Ok(_) => {
            panic!("a store that cannot implement the seam must not build a healthy registry")
        }
        Err(err) => err,
    };
    assert!(
        matches!(&err, RuntimeError::IncompatibleEventStore(message)
            if message.contains("supports_idempotent_audit_batch")),
        "error names the missing capability so an operator can act on it: {err}"
    );
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
#[serial(audit_obligation_append_failures)]
async fn db_diagnostics_with_audit_metrics_reports_batch_failure_and_degradation() {
    // One dispatch call exercises both halves of the classifier through
    // the registry it actually owns the seam on: the queued
    // `ConfigLocked` (pure-observability) row drains during the gate
    // check regardless of allow/deny, and `list`'s deferred
    // `DispatchSucceeded` (obligation) row is appended once dispatch
    // resolves — both against the same persistently failing store.
    crate::config_ledger::record_config_locked("adr133_diag_test_key", "adr133_diag_test_value");
    let store = Arc::new(MemoryEventStore {
        fail_appends: true,
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store);
    let registry = builder.build().expect("registry builds");
    let _ = registry.dispatch("list", Value::Null).await;

    let metrics = registry
        .audit_batch_metrics()
        .expect("with_event_store configures the ADR-133 seam");
    assert!(metrics.degraded, "the config-locked row must have degraded");
    assert!(metrics.degraded_rows >= 1);
    assert!(
        metrics.flush_failures >= 1,
        "the list dispatch's obligation row must count as a flush failure"
    );

    let rt = KhiveRuntime::memory().expect("memory runtime should create");
    let report = rt
        .db_diagnostics_with_audit_metrics(Some(metrics))
        .await
        .expect("diagnostics succeed");
    assert_eq!(report.writer_contention.audit_degraded, Some(true));
    assert!(report.writer_contention.audit_degraded_rows.unwrap_or(0) >= 1);
    assert!(
        report
            .writer_contention
            .audit_batch_flush_failures
            .unwrap_or(0)
            >= 1
    );
    assert!(report
        .writer_contention
        .audit_batch_flush_failures_unavailable_reason
        .is_none());
    assert!(report
        .writer_contention
        .audit_degraded_unavailable_reason
        .is_none());

    // The no-metrics path (a bare `KhiveRuntime::db_diagnostics`, or the
    // `db_diagnostics_with_audit_metrics(None)` it delegates to) must
    // still report the batch-health fields as explicitly unavailable
    // rather than silently zero, so an operator cannot mistake "no
    // registry wired in" for "no failures occurred".
    let bare_report = rt.db_diagnostics().await.expect("diagnostics succeed");
    assert!(bare_report.writer_contention.audit_degraded.is_none());
    assert!(bare_report
        .writer_contention
        .audit_degraded_unavailable_reason
        .is_some());
    assert!(bare_report
        .writer_contention
        .audit_admission_refused_obligations
        .is_none());
    assert!(bare_report
        .writer_contention
        .audit_admission_refused_obligations_unavailable_reason
        .is_some());
    assert!(bare_report
        .writer_contention
        .audit_admission_unresolved_obligations
        .is_none());
    assert!(bare_report
        .writer_contention
        .audit_admission_unresolved_obligations_unavailable_reason
        .is_some());
}

#[tokio::test]
#[serial(config_ledger)]
async fn audit_event_duration_us_reflects_measured_dispatch_time() {
    // The persisted audit row's `duration_us` must carry the measured
    // pack-dispatch time, not the `Event::new` default of 0 (persisting
    // the row before dispatch ran always yielded 0). `SleepingPack`
    // sleeps 20ms so the assertion has a wide, non-flaky margin over
    // scheduling jitter.
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(SleepingPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("slow_op", serde_json::json!({}))
        .await
        .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    let ev = &page.items[0];
    assert!(
        ev.duration_us >= 10_000,
        "duration_us must reflect the ~20ms measured dispatch time, got {}",
        ev.duration_us
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn dispatch_unknown_verb_allowed_by_gate_still_persists_audit_row() {
    // Generalizing audit-row deferral to every Allow-outcome verb (not
    // just singleton `link`) must not silently drop the audit row for a
    // verb the gate allows but no pack owns. `duration_us` stays at the
    // `Event::new` default of 0 here since no dispatch ever ran to measure.
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let result = reg.dispatch("no_such_verb", serde_json::json!({})).await;
    assert!(result.is_err(), "unknown verb must still return an error");

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(
        count, 1,
        "an allowed-but-unknown verb must still persist one audit row"
    );
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items[0].duration_us, 0);
    // Dispatch returns UnknownVerb for an unknown verb, so the
    // persisted outcome must be Error, not the previously-hardcoded
    // Success.
    assert_eq!(page.items[0].outcome, EventOutcome::Error);
}

#[tokio::test]
#[serial(config_ledger)]
async fn audit_event_persists_to_event_store_on_deny() {
    #[derive(Debug)]
    struct AlwaysDenyGate;
    impl Gate for AlwaysDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("denied by test"))
        }
    }

    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(AlwaysDenyGate));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    // Hard enforce → PermissionDenied returned.
    let err = reg
        .dispatch("list", serde_json::json!({"namespace": "test-ns"}))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::PermissionDenied { .. }));

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(count, 1, "one audit event persisted to EventStore on deny");

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let ev = &page.items[0];
    assert_eq!(ev.verb, "list");
    assert_eq!(ev.outcome, EventOutcome::Denied);
}

#[derive(Debug)]
struct MailboxTrackingPack {
    invoked: Arc<AtomicUsize>,
}

impl Pack for MailboxTrackingPack {
    const NAME: &'static str = "mailbox_tracking";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "comm.inbox",
            description: "mailbox admission probe",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "comm.thread",
            description: "mailbox admission probe",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for MailboxTrackingPack {
    fn name(&self) -> &str {
        "mailbox_tracking"
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.invoked.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({"actor":token.actor().id}))
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn gate_mailbox_normal_and_intercepted_default_deny_audit_real_caller() {
    let store = Arc::new(MemoryEventStore::default());
    let invoked = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(MailboxTrackingPack {
        invoked: invoked.clone(),
    });
    builder.with_event_store(store.clone());
    builder.with_actor_id(Some("lambda:owner".into()));
    let registry = builder.build().unwrap();
    let identity = RequestIdentity {
        actor_id: Some("lambda:reader".into()),
        namespace: "local".into(),
        ..Default::default()
    };
    for verb in ["comm.inbox", "comm.thread"] {
        for namespace in ["local", "lambda:owner"] {
            let args = serde_json::json!({"mailbox_actor":"lambda:owner", "namespace":namespace, "actor":"lambda:owner"});
            let error = registry
                .dispatch_with_identity(verb, args.clone(), Some(identity.clone()))
                .await
                .unwrap_err();
            assert!(
                matches!(error, RuntimeError::PermissionDenied { reason, .. } if reason == "mailbox_read_not_granted")
            );
            let error = registry
                .dispatch_intercepted_with_metadata_and_disposition(
                    verb,
                    &args,
                    Some(&identity),
                    |_| async {
                        invoked.fetch_add(1, Ordering::SeqCst);
                        Ok(InterceptedDispatchResult::new(Value::Null, ()))
                    },
                )
                .await
                .unwrap_err()
                .into_source();
            assert!(
                matches!(error, RuntimeError::PermissionDenied { reason, .. } if reason == "mailbox_read_not_granted")
            );
        }
    }
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    let events = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 20,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items;
    assert_eq!(events.len(), 8);
    for event in events {
        assert_eq!(event.actor, "actor:lambda:reader");
        assert_eq!(event.outcome, EventOutcome::Denied);
        assert_eq!(event.payload["actor"]["id"], "lambda:reader");
        assert_eq!(event.payload["deny_reason"], "mailbox_read_not_granted");
    }
    // A positive own-view control proves the ordinary handler is present.
    let value = registry
        .dispatch_with_identity("comm.inbox", serde_json::json!({}), Some(identity.clone()))
        .await
        .unwrap();
    assert_eq!(value["actor"], "lambda:reader");
    assert_eq!(invoked.load(Ordering::SeqCst), 1);
    for args in [
        serde_json::json!({"mailbox_actor":null}),
        serde_json::json!({"mailbox_actor":"local"}),
    ] {
        assert!(matches!(
            registry
                .dispatch_with_identity("comm.inbox", args.clone(), Some(identity.clone()))
                .await,
            Err(RuntimeError::InvalidInput(_))
        ));
        let error = registry
            .dispatch_intercepted_with_metadata_and_disposition(
                "comm.inbox",
                &args,
                Some(&identity),
                |_| async {
                    invoked.fetch_add(1, Ordering::SeqCst);
                    Ok(InterceptedDispatchResult::new(Value::Null, ()))
                },
            )
            .await
            .unwrap_err()
            .into_source();
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
    }
    assert_eq!(invoked.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial(config_ledger)]
async fn gate_mailbox_granted_dispatch_keeps_caller_and_backend_error_fails_closed() {
    #[derive(Debug)]
    struct BrokenMailboxGate;
    impl Gate for BrokenMailboxGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::allow())
        }
        fn check_mailbox_read(
            &self,
            _req: &GateRequest,
            _owner: &khive_gate::ActorRef,
        ) -> Result<GateDecision, GateError> {
            Err(GateError::Internal("private policy outage".into()))
        }
    }
    let identity = RequestIdentity {
        actor_id: Some("lambda:reader".into()),
        namespace: "local".into(),
        ..Default::default()
    };
    let args = serde_json::json!({"mailbox_actor":"lambda:owner"});
    let gate: GateRef = Arc::new(
        khive_gate::MailboxReadGate::new(
            Arc::new(AllowAllGate),
            khive_gate::ActorRef::new("actor", "lambda:owner"),
            vec![khive_gate::ActorRef::new("actor", "lambda:reader")],
        )
        .unwrap(),
    );
    for (gate, allowed) in [
        (gate, true),
        (Arc::new(BrokenMailboxGate) as GateRef, false),
    ] {
        let store = Arc::new(MemoryEventStore::default());
        let invoked = Arc::new(AtomicUsize::new(0));
        let mut builder = VerbRegistryBuilder::new();
        builder.register(MailboxTrackingPack {
            invoked: invoked.clone(),
        });
        builder.with_actor_id(Some("lambda:owner".into()));
        builder.with_gate(gate);
        builder.with_event_store(store.clone());
        let registry = builder.build().unwrap();
        let normal = registry
            .dispatch_with_identity("comm.inbox", args.clone(), Some(identity.clone()))
            .await;
        let intercepted = registry
            .dispatch_intercepted_with_metadata_and_disposition(
                "comm.thread",
                &args,
                Some(&identity),
                |_| async {
                    invoked.fetch_add(1, Ordering::SeqCst);
                    Ok(InterceptedDispatchResult::new(Value::Null, ()))
                },
            )
            .await
            .map_err(DispatchError::into_source);
        if allowed {
            assert_eq!(normal.unwrap()["actor"], "lambda:reader");
            intercepted.unwrap();
            assert_eq!(invoked.load(Ordering::SeqCst), 2);
        } else {
            assert!(
                matches!(normal.unwrap_err(), RuntimeError::GateUnavailable { reason, .. } if reason == "gate backend unavailable")
            );
            assert!(
                matches!(intercepted.unwrap_err(), RuntimeError::GateUnavailable { reason, .. } if reason == "gate backend unavailable")
            );
            assert_eq!(invoked.load(Ordering::SeqCst), 0);
        }
        let events = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap()
            .items;
        assert_eq!(events.len(), 2);
        for event in events {
            assert_eq!(event.actor, "actor:lambda:reader");
            assert_eq!(
                event.outcome,
                if allowed {
                    EventOutcome::Success
                } else {
                    EventOutcome::Error
                }
            );
        }
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn gate_error_returns_typed_refusal_without_invoking_pack() {
    #[derive(Debug)]
    struct FailingGate;
    impl Gate for FailingGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
            Err(khive_gate::GateError::Internal("gate broken".into()))
        }
    }

    let store = Arc::new(MemoryEventStore::default());
    let invoked = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(GateErrorTrackingPack {
        invoked: Arc::clone(&invoked),
    });
    builder.with_gate(Arc::new(FailingGate));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let err = reg
        .dispatch("guarded", Value::Null)
        .await
        .expect_err("gate unavailability must refuse normal dispatch");
    assert!(matches!(
        err,
        RuntimeError::GateUnavailable { ref verb, ref reason }
            if verb == "guarded"
                && reason == "gate backend unavailable"
                && !reason.contains("gate broken")
    ));
    assert_eq!(
        invoked.load(Ordering::SeqCst),
        0,
        "pack handler must not run after a gate infrastructure error"
    );

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(count, 1, "gate infrastructure error must be audited");
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let event = &page.items[0];
    assert_eq!(event.verb, "guarded");
    assert_eq!(event.outcome, EventOutcome::Error);
    assert_eq!(event.payload["decision"], "gate_unavailable");
    assert!(event.payload.get("deny_reason").is_none());
    assert_eq!(event.payload["resource"]["work_class"], "interactive");
    assert!(event.payload["resource"].get("cost_unit").is_none());
}

#[tokio::test]
#[serial(config_ledger)]
#[serial(audit_append_failures)]
async fn gate_error_audit_failure_cannot_reopen_dispatch_or_replace_typed_error() {
    #[derive(Debug)]
    struct FailingGate;
    impl Gate for FailingGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Err(GateError::Internal("gate still broken".into()))
        }
    }

    let before = audit_append_failure_count();
    let invoked = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(MemoryEventStore {
        fail_appends: true,
        ..MemoryEventStore::default()
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(GateErrorTrackingPack {
        invoked: Arc::clone(&invoked),
    });
    builder.with_gate(Arc::new(FailingGate));
    builder.with_event_store(store);
    let registry = builder.build().expect("registry builds");

    let error = registry
        .dispatch("guarded", Value::Null)
        .await
        .expect_err("audit persistence failure must not reopen dispatch");

    assert!(matches!(
        error,
        RuntimeError::GateUnavailable { ref verb, ref reason }
            if verb == "guarded"
                && reason == "gate backend unavailable"
                && !reason.contains("gate still broken")
    ));
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    assert_eq!(
        audit_append_failure_count(),
        before + 1,
        "best-effort audit failure remains diagnostic without changing the refusal"
    );
}

/// Regression for a credential-disclosure path: a gate backend's error
/// `Display` text can embed connection details (URLs, addresses, auth
/// material). That text must never reach `RuntimeError::GateUnavailable`
/// as observed by a dispatch caller — only the stable classified
/// `wire_reason()` may cross that boundary. A bounded, masked rendering
/// of the error is logged server-side via `tracing::warn!` in
/// `gate_unavailable_error`.
#[tokio::test]
async fn gate_unavailable_reason_never_carries_backend_error_text() {
    const CANARY: &str = "postgres://svc:not-a-real-secret@internal-host";

    #[derive(Debug)]
    struct FailingGate;
    impl Gate for FailingGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Err(GateError::Internal(CANARY.to_string()))
        }
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(GateErrorTrackingPack {
        invoked: Arc::new(AtomicUsize::new(0)),
    });
    builder.with_gate(Arc::new(FailingGate));
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch("guarded", Value::Null)
        .await
        .expect_err("gate unavailability must refuse dispatch");

    let RuntimeError::GateUnavailable { reason, .. } = &err else {
        panic!("expected GateUnavailable, got {err:?}");
    };
    assert!(
        !reason.contains(CANARY),
        "caller-visible reason must not embed backend error text: {reason:?}"
    );
    assert!(
        !reason.contains("svc") && !reason.contains("internal-host"),
        "caller-visible reason must not embed backend error fragments: {reason:?}"
    );
    assert_eq!(reason, "gate backend unavailable");

    // The full error, canary included, still reaches the server-side log.
    let rendered = err.to_string();
    assert!(
        !rendered.contains(CANARY),
        "top-level Display must not embed backend error text either: {rendered:?}"
    );
}

/// Task 2 (ADR-129 gate-error classification): a `GateError::Policy`
/// failure — the gate backend is reachable but its configured policy
/// could not be evaluated — is a distinct, non-transient class from a
/// `GateError::Internal` backend-availability failure, and gets its own
/// stable reason text at the dispatch boundary.
#[tokio::test]
async fn gate_policy_error_classifies_distinctly_from_backend_unavailable() {
    #[derive(Debug)]
    struct PolicyBrokenGate;
    impl Gate for PolicyBrokenGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Err(GateError::Policy(
                "rule set has no allow clause for this namespace".to_string(),
            ))
        }
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(GateErrorTrackingPack {
        invoked: Arc::new(AtomicUsize::new(0)),
    });
    builder.with_gate(Arc::new(PolicyBrokenGate));
    let registry = builder.build().expect("registry builds");

    let err = registry
        .dispatch("guarded", Value::Null)
        .await
        .expect_err("gate unavailability must refuse dispatch");

    assert!(matches!(
        err,
        RuntimeError::GateUnavailable { ref verb, ref reason }
            if verb == "guarded"
                && reason == "gate policy evaluation failed"
                && !reason.contains("rule set has no allow clause")
    ));
}

#[tokio::test]
async fn no_event_store_configured_tracing_only() {
    // Ordinary verbs remain tracing-only without an event store. The
    // strict git.digest receipt exception is covered separately above.
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    let reg = builder.build().expect("registry builds");

    let res = reg.dispatch("list", Value::Null).await.unwrap();
    assert_eq!(res["pack"], "alpha");
}

#[test]
#[serial]
fn dispatch_tracing_emits_gate_check_event_with_deny_payload() {
    #[derive(Debug)]
    struct TracingDenyGate;
    impl Gate for TracingDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("denied by test gate"))
        }
        fn impl_name(&self) -> &'static str {
            "TracingDenyGate"
        }
    }

    let events = capture_dispatch_events(async {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(TracingDenyGate));
        let reg = builder.build().expect("registry builds");
        // Hard enforcement — dispatch returns PermissionDenied on Deny.
        // The tracing audit event is still emitted before the error is returned.
        let _ = reg.dispatch("create", serde_json::Value::Null).await;
    });

    let gate_events = gate_check_events_for(&events, "TracingDenyGate");
    assert_eq!(
        gate_events.len(),
        1,
        "exactly one gate.check tracing event per dispatch (deny); got {gate_events:?}"
    );
    let payload = gate_events[0]
        .audit_event
        .as_ref()
        .expect("gate.check event must carry an audit_event field on Deny");
    let audit: khive_gate::AuditEvent =
        serde_json::from_str(payload).expect("audit_event payload must decode to AuditEvent");
    assert_eq!(audit.decision, AuditDecision::Deny);
    assert_eq!(audit.deny_reason.as_deref(), Some("denied by test gate"));
    assert_eq!(audit.gate_impl, "TracingDenyGate");
    // Wire-shape rule: obligations is always serialized as an array, empty
    // on Deny. Round-trip back through serde_json::Value to confirm the
    // field exists on the wire and is `[]`, not missing.
    let payload_json: serde_json::Value =
        serde_json::from_str(payload).expect("payload must be valid JSON");
    assert_eq!(
        payload_json["obligations"],
        serde_json::Value::Array(Vec::new()),
        "obligations must be `[]` on Deny on the tracing payload, not omitted"
    );
}

#[test]
#[serial]
fn dispatch_tracing_emits_gate_check_event_with_masked_deny_payload() {
    // Falsifiable arm for the audit-masking fix (khive#2944): this deny
    // reason embeds a fake credential in a shape the write-time secret
    // gate recognizes (`scheme://user:pass@host`). Deleting the masking
    // call at the production call site — or masking only the durable
    // sink and not this tracing line — turns this test red.
    #[derive(Debug)]
    struct TracingSecretDenyGate;
    impl Gate for TracingSecretDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            let reason = "postgres://svc:not-a-real-secret@internal-host in denied request"; // gitleaks:allow
            Ok(GateDecision::deny(reason))
        }
        fn impl_name(&self) -> &'static str {
            "TracingSecretDenyGate"
        }
    }

    let events = capture_dispatch_events(async {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(TracingSecretDenyGate));
        let reg = builder.build().expect("registry builds");
        let _ = reg.dispatch("create", serde_json::Value::Null).await;
    });

    let gate_events = gate_check_events_for(&events, "TracingSecretDenyGate");
    assert_eq!(
        gate_events.len(),
        1,
        "exactly one gate.check tracing event per dispatch (deny); got {gate_events:?}"
    );
    let payload = gate_events[0]
        .audit_event
        .as_ref()
        .expect("gate.check event must carry an audit_event field on Deny");
    // Non-vacuity: the capture actually produced content, so the
    // negative assertion below cannot pass merely because nothing was
    // read.
    assert!(!payload.is_empty());
    let audit: khive_gate::AuditEvent =
        serde_json::from_str(payload).expect("audit_event payload must decode to AuditEvent");
    let masked_reason = audit
        .deny_reason
        .as_deref()
        .expect("deny_reason must be present on a Deny audit event");
    assert!(
        masked_reason.contains("in denied request"),
        "non-secret prose must survive masking: {masked_reason:?}"
    );
    assert!(
        !masked_reason.contains("not-a-real-secret"),
        "the process log must never carry the raw credential: {masked_reason:?}"
    );
    assert!(
        masked_reason.contains("***MASKED***"),
        "the log must record that a credential was redacted: {masked_reason:?}"
    );
}

#[test]
#[serial]
fn intercepted_dispatch_tracing_emits_gate_check_event_with_masked_deny_payload() {
    // Same falsifiable arm as
    // `dispatch_tracing_emits_gate_check_event_with_masked_deny_payload`,
    // exercised through the intercepted dispatch path
    // (`dispatch_intercepted_with_metadata_and_disposition`), the
    // audit-emission code's second production `AuditEvent` construction
    // site. Masking only the plain-dispatch call site would leave this
    // one red.
    #[derive(Debug)]
    struct InterceptedTracingSecretDenyGate;
    impl Gate for InterceptedTracingSecretDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            let reason = "postgres://svc:not-a-real-secret@internal-host in denied request"; // gitleaks:allow
            Ok(GateDecision::deny(reason))
        }
        fn impl_name(&self) -> &'static str {
            "InterceptedTracingSecretDenyGate"
        }
    }

    let events = capture_dispatch_events(async {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(Arc::new(InterceptedTracingSecretDenyGate));
        let reg = builder.build().expect("registry builds");
        let _ = reg
            .dispatch_intercepted_with_identity(
                "list",
                &Value::Null,
                None,
                move |_namespace| async move { Ok(serde_json::json!({"invoked": true})) },
            )
            .await;
    });

    let gate_events = gate_check_events_for(&events, "InterceptedTracingSecretDenyGate");
    assert_eq!(
        gate_events.len(),
        1,
        "exactly one gate.check tracing event per intercepted dispatch (deny); got {gate_events:?}"
    );
    let payload = gate_events[0]
        .audit_event
        .as_ref()
        .expect("gate.check event must carry an audit_event field on Deny");
    assert!(!payload.is_empty());
    let audit: khive_gate::AuditEvent =
        serde_json::from_str(payload).expect("audit_event payload must decode to AuditEvent");
    let masked_reason = audit
        .deny_reason
        .as_deref()
        .expect("deny_reason must be present on a Deny audit event");
    assert!(
        masked_reason.contains("in denied request"),
        "non-secret prose must survive masking: {masked_reason:?}"
    );
    assert!(
        !masked_reason.contains("not-a-real-secret"),
        "the process log must never carry the raw credential: {masked_reason:?}"
    );
    assert!(
        masked_reason.contains("***MASKED***"),
        "the log must record that a credential was redacted: {masked_reason:?}"
    );
}

// ---- EventStore audit envelope round-trip ----
//
// EventStore must not persist a summary Event without the full
// AuditEvent fields (deny_reason, gate_impl, obligations). This test
// verifies the complete envelope survives append_event → query_events.

#[tokio::test]
#[serial(config_ledger)]
async fn audit_envelope_round_trips_deny_reason_and_gate_impl_through_event_store() {
    #[derive(Debug)]
    struct DenyGateWithName;
    impl Gate for DenyGateWithName {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("policy: write forbidden for anon"))
        }
        fn impl_name(&self) -> &'static str {
            "DenyGateWithName"
        }
    }

    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(DenyGateWithName));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    // Dispatch is denied — PermissionDenied returned.
    let err = reg
        .dispatch("list", serde_json::json!({"namespace": "test-ns"}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::PermissionDenied { .. }),
        "expected PermissionDenied, got {err:?}"
    );

    // Exactly one event in the store.
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        1,
        "one audit event must be persisted on deny"
    );

    let ev = &page.items[0];
    assert_eq!(ev.outcome, EventOutcome::Denied);

    // The payload field must hold the full AuditEvent envelope.
    let data = &ev.payload;

    let audit: khive_gate::AuditEvent =
        serde_json::from_value(data.clone()).expect("Event.payload must deserialize to AuditEvent");

    assert_eq!(
        audit.deny_reason.as_deref(),
        Some("policy: write forbidden for anon"),
        "deny_reason must be preserved through EventStore"
    );
    assert_eq!(
        audit.gate_impl, "DenyGateWithName",
        "gate_impl must be preserved through EventStore"
    );
    assert_eq!(
        audit.decision,
        khive_gate::AuditDecision::Deny,
        "decision field must be preserved through EventStore"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn audit_envelope_round_trips_obligations_through_event_store() {
    use khive_gate::Obligation;

    #[derive(Debug)]
    struct ObligationGate;
    impl Gate for ObligationGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::allow_with(vec![Obligation::Audit {
                tag: "billing.meter".into(),
            }]))
        }
        fn impl_name(&self) -> &'static str {
            "ObligationGate"
        }
    }

    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(ObligationGate));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", serde_json::json!({"namespace": "test-ns"}))
        .await
        .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);

    let ev = &page.items[0];
    assert_eq!(ev.outcome, EventOutcome::Success);

    let data = &ev.payload;

    let audit: khive_gate::AuditEvent =
        serde_json::from_value(data.clone()).expect("Event.payload must deserialize to AuditEvent");

    assert_eq!(audit.gate_impl, "ObligationGate");
    assert_eq!(
        audit.obligations.len(),
        1,
        "obligations must be preserved through EventStore"
    );
    match &audit.obligations[0] {
        Obligation::Audit { tag } => assert_eq!(tag, "billing.meter"),
        other => panic!("expected Audit obligation, got {other:?}"),
    }
}

// ---- SQL-backed audit envelope round-trip ----
//
// The two tests above use MemoryEventStore (no serialization). This test
// wires the production SqlEventStore via KhiveRuntime::memory() to verify
// that the full AuditEvent envelope survives the SQL text→parse round-trip
// (Event.data is stored as TEXT and parsed back on read).

#[tokio::test]
#[serial(config_ledger)]
async fn sql_backed_audit_envelope_round_trips_deny_reason_gate_impl_and_obligations() {
    #[derive(Debug)]
    struct SqlTestDenyGate;
    impl Gate for SqlTestDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("sql-path: write denied"))
        }
        fn impl_name(&self) -> &'static str {
            "SqlTestDenyGate"
        }
    }

    // KhiveRuntime::memory() creates an in-memory SQLite pool (is_file_backed=false).
    // events_for_namespace ensures the events schema and returns a SqlEventStore
    // scoped to "test-ns". The pool is shared so reads and writes see the same data.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let test_tok = NamespaceToken::for_namespace(Namespace::parse("test-ns").unwrap());
    let sql_store = rt
        .events(&test_tok)
        .expect("events_for_namespace must succeed");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(SqlTestDenyGate));
    builder.with_event_store(sql_store.clone());
    let reg = builder.build().expect("registry builds");

    // Dispatch is denied — PermissionDenied returned.
    let err = reg
        .dispatch("list", serde_json::json!({"namespace": "test-ns"}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::PermissionDenied { .. }),
        "expected PermissionDenied, got {err:?}"
    );

    // Query via the same SqlEventStore — this is the SQL read path.
    let page = sql_store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        1,
        "one audit event must be persisted on deny through SqlEventStore"
    );

    let ev = &page.items[0];
    assert_eq!(ev.outcome, EventOutcome::Denied);

    // Event.payload must hold the full AuditEvent serialized as JSON text and
    // parsed back. If the SQL path was lossy, this deserialization would fail
    // or the field assertions below would fail.
    let data = &ev.payload;

    let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
        .expect("Event.payload must deserialize to AuditEvent after SQL round-trip");

    assert_eq!(
        audit.deny_reason.as_deref(),
        Some("sql-path: write denied"),
        "deny_reason must survive the SQL text round-trip"
    );
    assert_eq!(
        audit.gate_impl, "SqlTestDenyGate",
        "gate_impl must survive the SQL text round-trip"
    );
    assert_eq!(
        audit.decision,
        khive_gate::AuditDecision::Deny,
        "decision field must survive the SQL text round-trip"
    );
    // obligations is [] on a Deny gate (no obligations returned).
    // Verify the field is present and empty after SQL round-trip.
    assert!(
        audit.obligations.is_empty(),
        "obligations must be preserved as empty [] through SQL round-trip"
    );
}

// ---- SQL-backed audit envelope: non-empty obligations survive round-trip ----
//
// Blind spot: the deny-path SQL test above only
// asserts obligations == [], which passes even if the SQL path drops the
// field entirely (AuditEvent.obligations has #[serde(default)]).
//
// This test installs an allow-path gate that returns a non-empty obligations
// vec. After dispatch, the same SqlEventStore is queried and both layers are
// checked:
//   1. Raw Event.data["obligations"] is a non-empty JSON array.
//   2. Deserialized AuditEvent.obligations[0] matches the expected variant.
#[tokio::test]
#[serial(config_ledger)]
async fn sql_backed_audit_envelope_round_trips_non_empty_obligations() {
    use khive_gate::Obligation;

    #[derive(Debug)]
    struct SqlTestAllowWithObligationGate;
    impl Gate for SqlTestAllowWithObligationGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::allow_with(vec![Obligation::Audit {
                tag: "sql-path-billing.meter".into(),
            }]))
        }
        fn impl_name(&self) -> &'static str {
            "SqlTestAllowWithObligationGate"
        }
    }

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let test_tok = NamespaceToken::for_namespace(Namespace::parse("test-ns").unwrap());
    let sql_store = rt
        .events(&test_tok)
        .expect("events_for_namespace must succeed");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(SqlTestAllowWithObligationGate));
    builder.with_event_store(sql_store.clone());
    let reg = builder.build().expect("registry builds");

    // Dispatch succeeds — the gate allows with obligations.
    reg.dispatch("list", serde_json::json!({"namespace": "test-ns"}))
        .await
        .expect("dispatch must succeed when gate allows");

    // Query via the same SqlEventStore — this is the SQL read path.
    let page = sql_store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        1,
        "one audit event must be persisted on allow through SqlEventStore"
    );

    let ev = &page.items[0];
    assert_eq!(ev.outcome, EventOutcome::Success);

    let data = &ev.payload;

    // Layer 1: raw JSON check — obligations must be a non-empty array in
    // the persisted TEXT. If the SQL path dropped the field, the default
    // #[serde(default)] would silently deserialize it to [], so we verify
    // the raw JSON before deserializing.
    let obligations_raw = data
        .get("obligations")
        .expect("Event.data JSON must contain 'obligations' key");
    let obligations_arr = obligations_raw
        .as_array()
        .expect("'obligations' must be a JSON array");
    assert!(
        !obligations_arr.is_empty(),
        "raw Event.data['obligations'] must be non-empty after SQL round-trip"
    );

    // Layer 2: deserialized AuditEvent check — the obligation variant and
    // payload must survive the text round-trip faithfully.
    let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
        .expect("Event.data must deserialize to AuditEvent after SQL round-trip");

    assert_eq!(
        audit.gate_impl, "SqlTestAllowWithObligationGate",
        "gate_impl must survive the SQL text round-trip"
    );
    assert_eq!(
        audit.decision,
        khive_gate::AuditDecision::Allow,
        "decision field must survive the SQL text round-trip"
    );
    assert_eq!(
        audit.obligations.len(),
        1,
        "obligations must be non-empty after SQL round-trip (not silently defaulted to [])"
    );
    match &audit.obligations[0] {
        Obligation::Audit { tag } => assert_eq!(
            tag, "sql-path-billing.meter",
            "Audit obligation tag must survive the SQL text round-trip"
        ),
        other => panic!("expected Audit obligation, got {other:?}"),
    }
}

// ---- Audit payload shape for 'create' verb dispatch ----
//
// The previous audit tests verify the envelope shape for the 'list' verb.
// This test dispatches 'create' (matching the create_note + annotates path)
// and verifies that ev.verb, ev.outcome, and ev.data all round-trip correctly
// through the EventStore. Ensures the wire shape is independent of which verb
// triggers the gate check.
#[tokio::test]
#[serial(config_ledger)]
async fn audit_event_payload_shape_for_create_verb() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    // Dispatch 'create' — AlphaPack returns a stub value; what matters is
    // the EventStore entry emitted by the registry's gate-check path.
    reg.dispatch("create", serde_json::json!({"namespace": "test-ns"}))
        .await
        .unwrap();

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(count, 1, "exactly one audit event for one dispatch");

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let ev = &page.items[0];

    // Top-level Event fields.
    assert_eq!(ev.verb, "create", "ev.verb must be the dispatched verb");
    assert_eq!(
        ev.outcome,
        EventOutcome::Success,
        "ev.outcome must be Success on allow"
    );
    assert_eq!(
        ev.namespace, "test-ns",
        "ev.namespace must match the dispatch namespace"
    );

    // ev.payload must hold the full AuditEvent envelope.
    let data = &ev.payload;

    let audit: khive_gate::AuditEvent =
        serde_json::from_value(data.clone()).expect("ev.payload must deserialize to AuditEvent");

    assert_eq!(
        audit.decision,
        khive_gate::AuditDecision::Allow,
        "AuditEvent.decision must be Allow"
    );
    assert_eq!(audit.verb, "create", "AuditEvent.verb must be 'create'");
    assert_eq!(
        audit.namespace, "test-ns",
        "AuditEvent.namespace must be preserved"
    );
    assert_eq!(
        audit.gate_impl, "AllowAllGate",
        "AuditEvent.gate_impl must name the gate implementation"
    );
    assert!(
        audit.deny_reason.is_none(),
        "AuditEvent.deny_reason must be None on Allow"
    );
    // Wire-shape check: obligations serializes as [] on AllowAllGate.
    let payload_json: serde_json::Value =
        serde_json::from_value(data.clone()).expect("data must be valid JSON");
    assert_eq!(
        payload_json["obligations"],
        serde_json::Value::Array(Vec::new()),
        "obligations must be [] on AllowAllGate"
    );
}

// ---- ADR-103 Amendment 1: resource.cost_unit emission ----

/// Test pack whose `create` handler is a stub (mirrors `AlphaPack`) but
/// overrides `registered_embedding_model_names` to a configurable set,
/// exercising ADR-103 Amendment 1's `model_count` computation for
/// singleton `create` at the dispatch audit-row emission seam.
struct EmbeddingAwarePack {
    models: Vec<String>,
}

impl khive_types::Pack for EmbeddingAwarePack {
    const NAME: &'static str = "embedding_aware";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &["widget"];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "create",
        description: "create a widget (embedding-aware stub)",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for EmbeddingAwarePack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn registered_embedding_model_names(&self) -> Vec<String> {
        self.models.clone()
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "pack": "embedding_aware", "verb": verb }))
    }
}

/// Test pack whose one verb, `probe`, always fails — used to drive the
/// general (non-link) deferred-audit Err arm without a real backend.
struct FailingProbePack;

impl khive_types::Pack for FailingProbePack {
    const NAME: &'static str = "failing_probe";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "probe",
        description: "always fails",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for FailingProbePack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput("boom".into()))
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn resource_cost_unit_present_on_non_embedding_successful_dispatch() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("list", serde_json::json!({})).await.unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items[0].payload["resource"],
        serde_json::json!({"work_class": "interactive", "cost_unit": 1}),
        "non-embedding-bearing verb's resource.cost_unit must be base_weight(verb) alone"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn resource_cost_unit_scales_with_registered_model_count_for_create() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(EmbeddingAwarePack {
        models: vec!["all-minilm-l6-v2".into(), "paraphrase".into()],
    });
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("create", serde_json::json!({"kind": "widget"}))
        .await
        .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    // base_weight(1) + per_item_weight(1) * item_count(1) * model_count(2)
    assert_eq!(
        page.items[0].payload["resource"],
        serde_json::json!({"work_class": "interactive", "cost_unit": 3}),
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn resource_cost_unit_zero_registered_models_is_base_weight_only() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(EmbeddingAwarePack { models: vec![] });
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("create", serde_json::json!({"kind": "widget"}))
        .await
        .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items[0].payload["resource"]["cost_unit"], 1,
        "zero registered embedding models must vanish the term, not error or omit"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn resource_work_class_present_cost_unit_absent_when_dispatch_returns_error() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(FailingProbePack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let err = reg
        .dispatch("probe", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidInput(_)));

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].outcome, EventOutcome::Error);
    // ADR-103 Decision (a): work_class is stamped on EVERY event, denial
    // and error included -- only Amendment 1's cost_unit field is scoped
    // to a successful dispatch. An errored dispatch keeps
    // resource.work_class and omits only resource.cost_unit, never 0.
    assert_eq!(
        page.items[0].payload["resource"],
        serde_json::json!({"work_class": "interactive"}),
        "resource must carry work_class with cost_unit OMITTED (never 0) on an \
             errored dispatch: {:?}",
        page.items[0].payload
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn resource_work_class_present_cost_unit_absent_when_no_pack_owns_the_verb() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let _ = reg
        .dispatch("no_such_verb_resource_test", serde_json::json!({}))
        .await;

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items[0].payload["resource"],
        serde_json::json!({"work_class": "interactive"})
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn resource_work_class_present_cost_unit_absent_on_denied_dispatch() {
    #[derive(Debug)]
    struct AlwaysDenyGate;
    impl Gate for AlwaysDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("test: always deny"))
        }
    }
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(AlwaysDenyGate));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let _ = reg.dispatch("list", serde_json::json!({})).await;

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].outcome, EventOutcome::Denied);
    assert_eq!(
        page.items[0].payload["resource"],
        serde_json::json!({"work_class": "interactive"})
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn resource_cost_unit_present_on_link_singleton_success() {
    let store = Arc::new(MemoryEventStore::default());
    let edge_id = uuid::Uuid::new_v4();
    let source_id = uuid::Uuid::new_v4();
    let target_id = uuid::Uuid::new_v4();
    let edge_json = serde_json::json!({
        "id": edge_id,
        "namespace": "local",
        "source_id": source_id,
        "target_id": target_id,
        "relation": "depends_on",
        "weight": 1.0,
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::ok(edge_json));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch(
        "link",
        serde_json::json!({
            "source_id": source_id,
            "target_id": target_id,
            "relation": "depends_on",
        }),
    )
    .await
    .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items[0].payload["resource"],
        serde_json::json!({"work_class": "interactive", "cost_unit": 1}),
        "link has no embedding-bearing path -> base_weight(link) alone, even on the v2-enriched singleton path"
    );
}

#[tokio::test]
#[serial(config_ledger)]
async fn resource_work_class_present_cost_unit_absent_on_link_dispatch_failure() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::err("target endpoint not found"));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let _ = reg
        .dispatch(
            "link",
            serde_json::json!({
                "source_id": "note:alpha",
                "target_id": "note:missing",
                "relation": "depends_on",
            }),
        )
        .await;

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items[0].payload["resource"],
        serde_json::json!({"work_class": "interactive"})
    );
}

// Registry audit event must carry target_id when dispatch params include it.
#[tokio::test]
#[serial(config_ledger)]
async fn audit_event_threads_target_id_from_dispatch_args() {
    let store = Arc::new(MemoryEventStore::default());
    let target = uuid::Uuid::new_v4();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    reg.dispatch(
        "create",
        serde_json::json!({"namespace": "test-ns", "target_id": target}),
    )
    .await
    .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items[0].target_id,
        Some(target),
        "#282: audit event must carry target_id from dispatch params"
    );
}

// ---- Link-verb audit enrichment ----

/// Test pack exposing a single `link` verb whose one-shot result is
/// configured up front — lets tests drive both the success and failure
/// legs of the deferred link-audit path without a real KG backend.
struct LinkResultPack {
    result: std::sync::Mutex<Option<Result<Value, RuntimeError>>>,
}

impl LinkResultPack {
    fn ok(value: Value) -> Self {
        Self {
            result: std::sync::Mutex::new(Some(Ok(value))),
        }
    }
    fn err(message: &str) -> Self {
        Self {
            result: std::sync::Mutex::new(Some(Err(RuntimeError::InvalidInput(
                message.to_string(),
            )))),
        }
    }
}

impl khive_types::Pack for LinkResultPack {
    const NAME: &'static str = "kg";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "link",
        description: "test link handler",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for LinkResultPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.result
            .lock()
            .unwrap()
            .take()
            .expect("LinkResultPack dispatch called more than once in a test")
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn link_audit_enriches_successful_singleton_with_edge_v2() {
    let store = Arc::new(MemoryEventStore::default());
    let edge_id = uuid::Uuid::new_v4();
    let source_id = uuid::Uuid::new_v4();
    let target_id = uuid::Uuid::new_v4();
    let edge_json = serde_json::json!({
        "id": edge_id,
        "namespace": "local",
        "source_id": source_id,
        "target_id": target_id,
        "relation": "depends_on",
        "weight": 1.0,
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::ok(edge_json));
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    reg.dispatch(
        "link",
        serde_json::json!({
            "source_id": source_id,
            "target_id": target_id,
            "relation": "depends_on",
        }),
    )
    .await
    .unwrap();

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(
        count, 1,
        "exactly one deferred audit row must be persisted for a successful singleton link"
    );
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let ev = &page.items[0];
    assert_eq!(ev.verb, "link");
    assert_eq!(ev.outcome, EventOutcome::Success);
    assert_eq!(
        ev.payload_schema_version, 2,
        "successful singleton link uses audit schema v2"
    );
    assert_eq!(
        ev.target_id,
        Some(edge_id),
        "target_id must be the created/resolved edge id, not a raw caller arg"
    );
    assert_eq!(ev.payload["edge_id"], serde_json::json!(edge_id));
    assert_eq!(ev.payload["source_id"], serde_json::json!(source_id));
    assert_eq!(ev.payload["target_id"], serde_json::json!(target_id));
    assert_eq!(ev.payload["relation"], "depends_on");
    assert_eq!(ev.payload["weight"], 1.0);
    // v1 AuditEvent fields remain present via #[serde(flatten)].
    assert_eq!(ev.payload["verb"], "link");
    assert_eq!(ev.payload["decision"], "allow");
    assert!(ev.payload.get("gate_impl").is_some());
}

#[tokio::test]
#[serial(config_ledger)]
async fn link_audit_falls_back_to_v1_when_dispatch_fails() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::err("target endpoint not found"));
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    let err = reg
        .dispatch(
            "link",
            serde_json::json!({
                "source_id": "note:alpha",
                "target_id": "note:missing",
                "relation": "depends_on",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("not found")),
        "the original dispatch error must be returned unchanged"
    );

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        1,
        "a v1 fallback audit row must still be persisted on dispatch failure"
    );
    let ev = &page.items[0];
    assert_eq!(
        ev.payload_schema_version, 1,
        "failed link keeps the v1 audit shape"
    );
    // The persisted outcome must reflect the dispatch result (Err →
    // Error), not be hardcoded to Success from the gate's Allow decision.
    assert_eq!(
        ev.outcome,
        EventOutcome::Error,
        "outcome reflects the dispatch result (Err), not the gate decision (Allow)"
    );
    assert!(
        ev.duration_us >= 0,
        "duration_us must still be populated (measured, not the Event::new \
             default sentinel) on a failed dispatch"
    );
    assert!(
        ev.target_id.is_none(),
        "non-UUID caller-supplied ids do not spuriously populate target_id"
    );
    assert!(
        ev.payload.get("edge_id").is_none(),
        "v1 fallback must not carry edge enrichment fields"
    );
    let _: khive_gate::AuditEvent = serde_json::from_value(ev.payload.clone())
        .expect("v1 fallback payload must deserialize as AuditEvent");
}

#[tokio::test]
#[serial(config_ledger)]
async fn link_audit_bulk_links_get_no_enrichment() {
    let store = Arc::new(MemoryEventStore::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::ok(serde_json::json!({
        "attempted": 2, "created": 2, "skipped": 0, "failed": 0
    })));
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    reg.dispatch(
        "link",
        serde_json::json!({
            "links": [
                {"source_id": "a", "target_id": "b", "relation": "depends_on"},
                {"source_id": "c", "target_id": "d", "relation": "depends_on"},
            ],
        }),
    )
    .await
    .unwrap();

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(
        count, 1,
        "bulk `links` gets exactly one v1 audit row (deferred until dispatch \
             resolves like every other Allow-outcome row since ADR-103 Stage 1, \
             but never v2-enriched — enrichment is singleton-`link`-only)"
    );
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let ev = &page.items[0];
    assert_eq!(
        ev.payload_schema_version, 1,
        "bulk link mode is out of scope for #676's events.target_id enrichment"
    );
    assert!(ev.target_id.is_none());
}

#[test]
fn link_audit_success_from_result_extracts_edge_fields() {
    let gate_req = GateRequest::new(
        ActorRef::anonymous(),
        Namespace::local(),
        "link",
        serde_json::json!({}),
    );
    let decision = GateDecision::Allow {
        obligations: vec![],
    };
    let audit = AuditEvent::from_check(&gate_req, &decision, "AllowAllGate");

    let edge_id = uuid::Uuid::new_v4();
    let source_id = uuid::Uuid::new_v4();
    let target_id = uuid::Uuid::new_v4();
    let result = serde_json::json!({
        "id": edge_id,
        "source_id": source_id,
        "target_id": target_id,
        "relation": "depends_on",
        "weight": 0.5,
    });

    let (returned_id, payload) = link_audit_success_from_result(audit, &result)
        .expect("well-formed edge JSON must produce an enriched payload");
    assert_eq!(returned_id, edge_id);
    assert_eq!(payload["edge_id"], serde_json::json!(edge_id));
    assert_eq!(payload["relation"], "depends_on");
    assert_eq!(payload["weight"], 0.5);
    assert_eq!(
        payload["verb"], "link",
        "v1 AuditEvent fields must flatten into the v2 payload"
    );
}

#[test]
fn link_audit_success_from_result_rejects_incomplete_or_malformed_result() {
    let gate_req = GateRequest::new(
        ActorRef::anonymous(),
        Namespace::local(),
        "link",
        serde_json::json!({}),
    );
    let decision = GateDecision::Allow {
        obligations: vec![],
    };
    let audit = AuditEvent::from_check(&gate_req, &decision, "AllowAllGate");

    assert!(
        link_audit_success_from_result(
            audit.clone(),
            &serde_json::json!({"id": uuid::Uuid::new_v4()}),
        )
        .is_none(),
        "missing source_id/target_id/relation/weight must not enrich"
    );
    assert!(
        link_audit_success_from_result(audit, &serde_json::json!({"id": "not-a-uuid"})).is_none(),
        "a non-UUID id must not enrich"
    );
}

// ---- khive#948: request_id survives to the persisted audit event ----
//
// The pure `resource_payload`/`base_resource_payload` helpers are unit
// tested in `cost_unit.rs`; these tests prove the id actually reaches
// `resource.request_id` on a persisted `Event` through every one of
// `dispatch_with_identity`'s four audit-append sites (denied, ordinary
// success/error, singleton-link v2 success and its v1 fallback, and the
// unknown-verb error path), plus the "no id supplied" omission case.

async fn first_event(store: &Arc<MemoryEventStore>) -> Event {
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        1,
        "expected exactly one persisted audit event"
    );
    page.items[0].clone()
}

/// A fresh `MemoryEventStore` for a test that will assert `first_event`'s
/// exact one-event count, with the process-wide config ledger drained
/// first.
///
/// `first_event` itself runs *after* dispatch, so it cannot fix this: a
/// `config_ledger`-grouped test that panics after queueing a row (a
/// direct `record_config_locked` call, or a `OnceLock` reader it
/// invoked) but before its own event-backed dispatch never drains that
/// row itself, leaving it queued for whichever test the serial lock
/// hands off to next. If that next test builds its store with plain
/// `Arc::new(MemoryEventStore::default())`, its own dispatch call
/// drains the inherited row as an extra `ConfigLocked` event alongside
/// the one it expects, and `first_event`'s exact `page.items.len() ==
/// 1` assertion sees two. The fix has to run before dispatch, so it
/// lives in the store constructor every exact-one-event test calls, not
/// in the post-dispatch helper that reads the count.
fn clean_ledger_event_store() -> Arc<MemoryEventStore> {
    let _ = crate::config_ledger::drain_config_locked();
    Arc::new(MemoryEventStore::default())
}

/// Regression: a preceding
/// `config_ledger`-grouped test that queues a row (directly, or via a
/// `OnceLock` reader it invoked) and then panics before its own
/// event-backed dispatch drains it leaves that row queued for whichever
/// test the serial lock hands to next. Simulate exactly that leaked row
/// here and prove `clean_ledger_event_store` — not `first_event` itself,
/// which only runs after dispatch and so cannot fix a pre-dispatch race
/// — is what keeps `first_event`'s exact one-event assertion honest.
#[tokio::test]
#[serial(config_ledger)]
async fn first_event_is_immune_to_a_ledger_row_a_prior_test_never_drained() {
    let _ = crate::config_ledger::drain_config_locked();
    crate::config_ledger::record_config_locked("simulated_leaked_key", "leaked_value");

    let store = clean_ledger_event_store();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch_with_identity(
        "list",
        serde_json::json!({"namespace": "test-ns"}),
        Some(RequestIdentity {
            request_id: Some(9_001),
            ..Default::default()
        }),
    )
    .await
    .unwrap();

    let ev = first_event(&store).await;
    assert_eq!(ev.outcome, EventOutcome::Success);
}

#[tokio::test]
#[serial(config_ledger)]
async fn dispatch_with_identity_stamps_request_id_on_success() {
    let store = clean_ledger_event_store();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch_with_identity(
        "list",
        serde_json::json!({"namespace": "test-ns"}),
        Some(RequestIdentity {
            request_id: Some(101),
            ..Default::default()
        }),
    )
    .await
    .unwrap();

    let ev = first_event(&store).await;
    assert_eq!(ev.outcome, EventOutcome::Success);
    assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(101));
}

#[tokio::test]
#[serial(config_ledger)]
async fn dispatch_with_identity_stamps_request_id_on_dispatch_error() {
    let store = clean_ledger_event_store();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(FailingProbePack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let err = reg
        .dispatch_with_identity(
            "probe",
            serde_json::json!({"namespace": "test-ns"}),
            Some(RequestIdentity {
                request_id: Some(102),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidInput(_)));

    let ev = first_event(&store).await;
    assert_eq!(ev.outcome, EventOutcome::Error);
    assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(102));
}

#[tokio::test]
#[serial(config_ledger)]
async fn dispatch_with_identity_stamps_request_id_on_denied() {
    #[derive(Debug)]
    struct AlwaysDenyGate;
    impl Gate for AlwaysDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("denied by test"))
        }
    }

    let store = clean_ledger_event_store();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_gate(Arc::new(AlwaysDenyGate));
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let err = reg
        .dispatch_with_identity(
            "list",
            serde_json::json!({"namespace": "test-ns"}),
            Some(RequestIdentity {
                request_id: Some(103),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::PermissionDenied { .. }));

    let ev = first_event(&store).await;
    assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(103));
}

#[tokio::test]
#[serial(config_ledger)]
async fn dispatch_with_identity_stamps_request_id_on_link_v2_success() {
    let store = clean_ledger_event_store();
    let edge_id = uuid::Uuid::new_v4();
    let source_id = uuid::Uuid::new_v4();
    let target_id = uuid::Uuid::new_v4();
    let edge_json = serde_json::json!({
        "id": edge_id,
        "namespace": "local",
        "source_id": source_id,
        "target_id": target_id,
        "relation": "depends_on",
        "weight": 1.0,
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::ok(edge_json));
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    reg.dispatch_with_identity(
        "link",
        serde_json::json!({
            "source_id": source_id,
            "target_id": target_id,
            "relation": "depends_on",
        }),
        Some(RequestIdentity {
            namespace: "test-ns".to_string(),
            request_id: Some(104),
            ..Default::default()
        }),
    )
    .await
    .unwrap();

    let ev = first_event(&store).await;
    assert_eq!(
        ev.payload_schema_version, 2,
        "successful singleton link uses audit schema v2"
    );
    assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(104));
}

#[tokio::test]
#[serial(config_ledger)]
async fn dispatch_with_identity_stamps_request_id_on_link_v1_fallback() {
    let store = clean_ledger_event_store();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LinkResultPack::err("target endpoint not found"));
    builder.with_event_store(store.clone());
    builder.with_default_namespace("test-ns");
    let reg = builder.build().expect("registry builds");

    let err = reg
        .dispatch_with_identity(
            "link",
            serde_json::json!({
                "source_id": "note:alpha",
                "target_id": "note:missing",
                "relation": "depends_on",
            }),
            Some(RequestIdentity {
                namespace: "test-ns".to_string(),
                request_id: Some(105),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidInput(_)));

    let ev = first_event(&store).await;
    assert_eq!(
        ev.payload_schema_version, 1,
        "failed link keeps the v1 audit shape"
    );
    assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(105));
}

#[tokio::test]
#[serial(config_ledger)]
async fn dispatch_with_identity_stamps_request_id_on_unknown_verb() {
    let store = clean_ledger_event_store();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    let err = reg
        .dispatch_with_identity(
            "no_such_verb",
            serde_json::json!({}),
            Some(RequestIdentity {
                namespace: Namespace::local().as_str().to_string(),
                request_id: Some(106),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::UnknownVerb(_)));

    let ev = first_event(&store).await;
    assert_eq!(ev.outcome, EventOutcome::Error);
    assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(106));
}

#[tokio::test]
#[serial(config_ledger)]
async fn dispatch_with_identity_omits_request_id_key_when_absent() {
    let store = clean_ledger_event_store();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(AlphaPack);
    builder.with_event_store(store.clone());
    let reg = builder.build().expect("registry builds");

    // No identity at all — the pre-#948 call shape.
    reg.dispatch("list", serde_json::json!({"namespace": "test-ns"}))
        .await
        .unwrap();

    let ev = first_event(&store).await;
    let resource = ev.payload["resource"]
        .as_object()
        .expect("resource must be an object");
    assert!(
        !resource.contains_key("request_id"),
        "request_id key must be entirely absent when no id is supplied, \
             not present as null or 0: got {resource:?}"
    );
}
