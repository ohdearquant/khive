use super::fn_bodies;

/// Blank out the contents of every `"..."` string literal in `text`
/// (escapes included), line by line, so a seam name mentioned in an
/// error message or `.expect(...)` string — e.g. `pack.rs`'s own
/// `"...do not call with_event_store() for this backend."` — can never
/// read as a call. This only needs to handle ordinary quoted strings:
/// nothing in this workspace's actual seam-adjacent code uses raw
/// strings or multi-line string literals for text that could collide
/// with a seam or helper name.
pub(super) fn strip_string_literals(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Persists across lines on purpose: this workspace's longer error
    // and `.expect(...)` messages routinely use backslash-newline
    // string continuations (see `pack.rs`'s own
    // `IncompatibleEventStore` message), so a literal spanning several
    // source lines must stay "in string" across all of them.
    let mut in_string = false;
    for line in text.lines() {
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if in_string {
                if c == '\\' {
                    out.push(' ');
                    if chars.next().is_some() {
                        out.push(' ');
                    }
                    continue;
                }
                if c == '"' {
                    in_string = false;
                    out.push('"');
                } else {
                    out.push(' ');
                }
            } else {
                if c == '"' {
                    in_string = true;
                }
                out.push(c);
            }
        }
        out.push('\n');
    }
    out
}

/// `true` if `text` contains a call to `name` — `name` immediately
/// followed by `(` (optional whitespace between, including newlines —
/// `rustfmt` is free to break a long call onto its own line, and a call
/// site that happens to fit on one line today is not a
/// property this scan may rely on), with a non-identifier character (or
/// start of text) before it, not immediately preceded by `fn ` (which
/// would make this the definition, not a call), and not inside a string
/// literal (which would make this prose, not a call).
///
/// The identifier-boundary check is load-bearing: a naive
/// `text.contains(format!("{name}("))` matches `fixture(` inside
/// `daemon_script_fixture(`, which is a different, unrelated function —
/// this is the difference between a real population scan and one that
/// explodes into every helper in the workspace that happens to share a
/// suffix.
pub(super) fn calls_name(text: &str, name: &str) -> bool {
    PreparedCallText::new(text).contains_call(name)
}

/// Prepared once per eligible body, then reused for every callee probe and
/// fixed-point pass. This is local to one closure calculation, never a
/// cache of filesystem contents.
struct PreparedCallText(String);

impl PreparedCallText {
    fn new(text: &str) -> Self {
        Self(strip_string_literals(text))
    }

    fn contains_call(&self, name: &str) -> bool {
        fn is_ident_byte(b: u8) -> bool {
            b.is_ascii_alphanumeric() || b == b'_'
        }
        let text = &self.0;
        let bytes = text.as_bytes();
        let mut search_from = 0usize;
        while let Some(rel) = text[search_from..].find(name) {
            let idx = search_from + rel;
            let before_ok = idx == 0 || !is_ident_byte(bytes[idx - 1]);
            let after = idx + name.len();
            let mut j = after;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let after_ok = j < bytes.len() && bytes[j] == b'(';
            let is_definition = text[..idx].ends_with("fn ");
            if before_ok && after_ok && !is_definition {
                return true;
            }
            search_from = idx + 1;
        }
        false
    }
}

/// `seed`, plus the name of every function *defined in this same text*
/// that transitively calls one of the `seed` names through a chain of
/// unambiguous same-text helpers — e.g. a local test-fixture helper
/// (`pack_with_events()`, `fixture()`, ...) that itself constructs an
/// event-backed registry, or a verb handler that calls a
/// `record_config_locked`-wrapping config reader through one or more
/// intermediate helpers (`handle_context` → `context_profile_enabled`
/// → `record_config_locked`).
///
/// Two deliberate boundaries keep this from over-matching:
///
/// - **Per input text, not per workspace.** A private helper named
///   `fixture()` in one crate's test binary has nothing to do with an
///   unrelated `fixture()` in another crate's — they're different
///   functions in different compiled binaries. Resolving against
///   exactly the text handed in (one file, or one crate's concatenated
///   `src/` tree — the caller decides which) matches a real visibility
///   boundary instead of conflating same-named helpers across the whole
///   tree.
/// - **Closure gated by per-step uniqueness, not free transitive
///   chasing.** Growing the known set one full pass at a time, and only
///   ever promoting a name that is unambiguous (defined exactly once in
///   the text) at the moment it is promoted, is what keeps a generic
///   name like `new` or `build` — reused by dozens of unrelated types —
///   from becoming a global false-positive match. Each pass reuses the
///   exact single-hop check the uniqueness gate already relied on; only
///   the number of passes changed; a wrapper that itself wraps a
///   wrapper is still only promoted once every name on its path to
///   `seed` has independently cleared that gate.
pub(super) fn file_seam_names(text: &str, seed: &[&str]) -> Vec<String> {
    let bodies = fn_bodies(text)
        .into_iter()
        .map(|(name, body)| (name, PreparedCallText::new(&body)))
        .collect::<Vec<_>>();
    let mut name_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (name, _) in &bodies {
        *name_counts.entry(name.as_str()).or_insert(0) += 1;
    }

    let mut known: Vec<String> = seed.iter().map(|s| s.to_string()).collect();
    loop {
        let mut grew = false;
        for (name, body) in &bodies {
            if known.iter().any(|k| k == name) {
                continue;
            }
            if name_counts.get(name.as_str()).copied().unwrap_or(0) != 1 {
                continue;
            }
            if known.iter().any(|seam| body.contains_call(seam)) {
                known.push(name.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    known
}

/// [`file_seam_names`]'s closure, widened to resolve an ordinary
/// function-call chain that crosses source files within one crate —
/// e.g. a coordinator method defined in one file calling a config
/// reader defined in another — while still rejecting a name this scan
/// cannot safely resolve.
///
/// `bodies_by_file` is one [`fn_bodies`] list per file; each entry
/// keeps the same per-file uniqueness gate `file_seam_names` applies
/// (a name only counts as *that file's* definition when it is the only
/// one *in that file's own text*), but growth is shared across every
/// file, so a name promoted from one file's chain is immediately
/// available to every other file's bodies on the next pass.
///
/// A name defined identically in more than one file of the crate (the
/// same function name reused by two unrelated types — this workspace
/// has a real instance: a coordinator's own search method and an
/// unrelated service wrapper by the same name, in different files) is
/// promoted only when *every* one of its per-file-unique definitions
/// independently reaches the known set. Requiring the concatenated
/// text's exact-one-definition count instead would block such a name
/// forever — even though each definition, read in its own file, is
/// unambiguous — so this loosens the count check exactly as far as
/// keeping every resolution provably seam-reaching allows, and no
/// further: a name with even one non-reaching definition among its
/// per-file-unique occurrences is never promoted, which is what keeps
/// a generic name like `new` from becoming a crate-wide false match
/// the moment any single type's constructor happens to reach a seam.
pub(super) fn crate_seam_names(
    bodies_by_file: &[Vec<(String, String)>],
    seed: &[&str],
) -> Vec<String> {
    let per_file_unique: Vec<Vec<(&str, PreparedCallText)>> = bodies_by_file
        .iter()
        .map(|bodies| {
            let mut counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for (name, _) in bodies {
                *counts.entry(name.as_str()).or_insert(0) += 1;
            }
            bodies
                .iter()
                .filter(|(name, _)| counts.get(name.as_str()).copied() == Some(1))
                .map(|(name, body)| (name.as_str(), PreparedCallText::new(body)))
                .collect()
        })
        .collect();

    let mut occurrences: std::collections::HashMap<&str, Vec<&PreparedCallText>> =
        std::collections::HashMap::new();
    for unique_in_file in &per_file_unique {
        for (name, body) in unique_in_file {
            occurrences.entry(name).or_default().push(body);
        }
    }

    let mut known: Vec<String> = seed.iter().map(|s| s.to_string()).collect();
    loop {
        let mut grew = false;
        for (name, bodies) in &occurrences {
            if known.iter().any(|k| k == name) {
                continue;
            }
            if bodies
                .iter()
                .all(|body| known.iter().any(|seam| body.contains_call(seam)))
            {
                known.push((*name).to_string());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    known
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_calls_preserve_literal_and_continuation_boundaries() {
        let body = PreparedCallText::new(
            r#"let message = "with_event_store(fake); \
            record_config_locked(fake); \" with_event_store(still_fake)";"#,
        );
        for _ in 0..3 {
            assert!(!body.contains_call("with_event_store"));
            assert!(!body.contains_call("record_config_locked"));
        }
    }

    #[test]
    fn prepared_calls_reject_identifier_suffixes_and_function_definitions() {
        let body = PreparedCallText::new(
            "fn with_event_store(store: Store) {}\n\
             daemon_with_event_store(store); with_event_store_suffix(store);",
        );
        assert!(!body.contains_call("with_event_store"));
        assert!(!body.contains_call("event_store"));
        assert!(body.contains_call("daemon_with_event_store"));
        assert!(body.contains_call("with_event_store_suffix"));
    }

    #[test]
    fn prepared_calls_accept_whitespace_and_qualified_calls() {
        let body = PreparedCallText::new(
            "with_event_store\n\t (store);\nself.record_config_locked \t\n (item);",
        );
        for _ in 0..3 {
            assert!(body.contains_call("with_event_store"));
            assert!(body.contains_call("record_config_locked"));
            assert!(!body.contains_call("unrelated"));
        }
    }

    #[test]
    fn same_file_chain_reaches_a_fixed_point_in_original_order() {
        let source = "fn top() { middle(); }\n\
                      fn middle() { bottom(); }\n\
                      fn bottom() { with_event_store(store); }\n\
                      fn unrelated() {}";
        assert_eq!(
            file_seam_names(source, &["with_event_store"]),
            ["with_event_store", "bottom", "middle", "top"]
        );
    }

    #[test]
    fn duplicate_definitions_in_one_file_do_not_promote_a_name() {
        let source = "fn wrapper() { shared(); }\n\
                      fn shared() { with_event_store(store); }\n\
                      fn shared() { with_event_store(other_store); }";
        assert_eq!(
            file_seam_names(source, &["with_event_store"]),
            ["with_event_store"]
        );
        let bodies = [super::super::fn_bodies(source)];
        assert_eq!(
            crate_seam_names(&bodies, &["with_event_store"]),
            ["with_event_store"]
        );
    }

    #[test]
    fn cross_file_chain_reaches_the_seed_without_promoting_unrelated_names() {
        let files = [
            "fn top() { middle(); }\nfn unrelated() {}",
            "fn middle() { bottom(); }",
            "fn bottom() { record_config_locked(item); }",
        ];
        let bodies = files.map(super::super::fn_bodies);
        let known = crate_seam_names(&bodies, &["record_config_locked"]);
        for name in ["record_config_locked", "top", "middle", "bottom"] {
            assert!(known.iter().any(|candidate| candidate == name), "{name}");
        }
        assert_eq!(known.len(), 4);
    }

    #[test]
    fn every_cross_file_definition_must_reach_a_seed() {
        let reaching = "fn wrapper() { shared(); }\n\
                        fn shared() { with_event_store(store); }";
        let non_reaching = "fn shared() {}";
        let bodies = [reaching, non_reaching].map(super::super::fn_bodies);
        assert_eq!(
            crate_seam_names(&bodies, &["with_event_store"]),
            ["with_event_store"]
        );

        let also_reaching = "fn shared() { with_event_store(other_store); }";
        let bodies = [reaching, also_reaching].map(super::super::fn_bodies);
        let known = crate_seam_names(&bodies, &["with_event_store"]);
        for name in ["with_event_store", "shared", "wrapper"] {
            assert!(known.iter().any(|candidate| candidate == name), "{name}");
        }
        assert_eq!(known.len(), 3);
    }
}
