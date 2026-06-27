//! ADR-076 non-redundancy certificate harness.
//!
//! Implements the falsification gate described in ADR-076 §D2. Each proposed
//! new relation must supply a Fixture per eliminator family; the harness calls
//! an executable check function for each fixture and asserts that the cheaper
//! encoding gives a different answer than the standalone relation R on that graph.
//! This is a necessary-but-not-sufficient gate: surviving all eliminators shows
//! non-redundancy, but the system-role declaration (D1) is the arbiter.

use std::collections::BTreeSet;

/// The seven eliminator family codes, per ADR-076 §D2.
pub const ELIMINATOR_CODES: &[&str] = &["Cv", "Er", "At", "Po", "Ch", "Mv", "Sr"];

/// A synthetic graph triple: `(source_node, relation_name, target_node)`.
///
/// Node labels are arbitrary symbolic names with no database involvement.
/// Conventions used by check functions:
/// - `(node, "kind", "document")` — entity-kind annotation for the Er eliminator.
/// - `(a, "attr:key:value", b)` — attribute marker for the At eliminator.
pub type GraphTriple = (&'static str, &'static str, &'static str);

/// Result of executing one eliminator check against a fixture graph.
///
/// Fields are read via the `Debug` impl in assertion failure messages.
#[derive(Debug)]
#[allow(dead_code)]
pub enum EliminatorCheck {
    /// Cheaper encoding gives a DIFFERENT answer from R — relation passes this eliminator.
    Passes {
        /// Answer the cheaper encoding returns for the fixture query.
        cheaper: String,
        /// Answer the standalone relation R returns for the fixture query.
        r_answer: String,
    },
    /// Cheaper encoding gives the SAME answer as R — relation is redundant under this eliminator.
    Eliminated {
        /// The shared answer returned by both encodings.
        shared_answer: String,
        /// Human-readable reason the candidate is redundant.
        reason: String,
    },
}

impl EliminatorCheck {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Passes { .. })
    }

    pub fn is_eliminated(&self) -> bool {
        matches!(self, Self::Eliminated { .. })
    }
}

/// One fixture exercising a single eliminator for a candidate relation R.
///
/// The `check` function is the executable core: it derives both the cheaper
/// answer and the R answer from `graph` by graph traversal logic and returns an
/// `EliminatorCheck` indicating whether R is distinguishable. No pre-computed
/// answer strings are accepted — divergence is detected by the check function.
#[derive(Clone, Copy)]
pub struct Fixture {
    /// Eliminator family code (one of `ELIMINATOR_CODES`).
    pub eliminator: &'static str,
    /// Prose description of the cheaper encoding being tested.
    pub cheaper_encoding: &'static str,
    /// Minimal synthetic graph making the distinction visible.
    pub graph: &'static [GraphTriple],
    /// Natural-language description of the query posed to both encodings.
    pub query: &'static str,
    /// Executable check: derives both answers from `graph` and reports whether
    /// candidate R is distinguishable from the cheaper encoding.
    pub check: fn(&'static [GraphTriple]) -> EliminatorCheck,
}

// ── graph helpers ─────────────────────────────────────────────────────────────

/// Collect all (source, target) pairs for `rel` in the graph.
pub fn all_pairs(graph: &[GraphTriple], rel: &str) -> BTreeSet<(String, String)> {
    graph
        .iter()
        .filter(|(_, r, _)| *r == rel)
        .map(|(s, _, t)| (s.to_string(), t.to_string()))
        .collect()
}

/// BFS transitive closure: all nodes reachable from `from` via `rel` edges.
pub fn reachable(graph: &[GraphTriple], from: &str, rel: &str) -> BTreeSet<String> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = vec![from.to_string()];
    while let Some(node) = queue.pop() {
        for (_, _, t) in graph.iter().filter(|(s, r, _)| *s == node && *r == rel) {
            if visited.insert((*t).to_string()) {
                queue.push((*t).to_string());
            }
        }
    }
    visited
}

/// Format a set of (a, b) pairs for assertion messages.
pub fn fmt_pairs(s: &BTreeSet<(String, String)>) -> String {
    if s.is_empty() {
        "(empty)".to_string()
    } else {
        let items: Vec<_> = s.iter().map(|(a, b)| format!("({a},{b})")).collect();
        format!("{{{}}}", items.join(", "))
    }
}

// ── assertions ────────────────────────────────────────────────────────────────

/// Assert that a single fixture PASSES its stated eliminator.
///
/// Calls `fixture.check` to compute both answers from the fixture graph, then
/// asserts the result is `Passes` (the cheaper encoding and R diverge).
pub fn assert_defeats_eliminator(relation: &str, fixture: &Fixture) {
    assert!(
        !fixture.graph.is_empty(),
        "certificate for '{relation}': eliminator '{}' fixture has an empty graph; \
         supply at least one edge triple",
        fixture.eliminator,
    );
    let result = (fixture.check)(fixture.graph);
    assert!(
        result.is_pass(),
        "certificate for '{relation}': eliminator '{}' NOT defeated — \
         cheaper encoding '{}' is indistinguishable from standalone R on query '{}'; \
         check result: {result:?}",
        fixture.eliminator,
        fixture.cheaper_encoding,
        fixture.query,
    );
}

/// Assert that a deliberately-redundant negative-control fixture IS ELIMINATED.
///
/// Calls `fixture.check` and asserts the result is `Eliminated`. Use this for
/// negative-control tests that verify the harness actually rejects redundant
/// candidates — not just passes non-redundant ones.
pub fn assert_eliminated_by(relation: &str, fixture: &Fixture) {
    assert!(
        !fixture.graph.is_empty(),
        "negative control for '{relation}': eliminator '{}' fixture has an empty graph",
        fixture.eliminator,
    );
    let result = (fixture.check)(fixture.graph);
    assert!(
        result.is_eliminated(),
        "negative control for '{relation}': eliminator '{}' FAILED to eliminate a \
         deliberately-redundant candidate (cheaper encoding: '{}', query: '{}'); \
         the harness must reject this. check result: {result:?}",
        fixture.eliminator,
        fixture.cheaper_encoding,
        fixture.query,
    );
}

/// Run the full non-redundancy certificate for a relation.
///
/// Asserts:
/// 1. Every eliminator code in `ELIMINATOR_CODES` is covered by at least one fixture.
/// 2. For every fixture, the executable check returns `Passes`.
///
/// Call this with the complete positive fixture list. Negative-control fixtures
/// are exercised separately via `assert_eliminated_by`.
pub fn run_certificate(relation: &str, fixtures: &[Fixture]) {
    for code in ELIMINATOR_CODES {
        let found = fixtures.iter().any(|f| f.eliminator == *code);
        assert!(
            found,
            "certificate for '{relation}': no fixture covers eliminator '{code}'"
        );
    }
    for fixture in fixtures {
        assert_defeats_eliminator(relation, fixture);
    }
}
