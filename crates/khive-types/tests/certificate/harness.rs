//! ADR-076 non-redundancy certificate harness.
//!
//! Implements the falsification gate described in ADR-076 §D2. Each proposed
//! new relation must supply a `Fixture` per eliminator family; the harness
//! asserts that every fixture defeats its eliminator (the cheaper encoding
//! returns a different answer than the standalone relation). This is a
//! necessary-but-not-sufficient gate: surviving all eliminators shows
//! non-redundancy, but the system-role declaration (D1) is the arbiter.

/// The seven eliminator family codes, per ADR-076 §D2.
pub const ELIMINATOR_CODES: &[&str] = &["Cv", "Er", "At", "Po", "Ch", "Mv", "Sr"];

/// A synthetic graph triple used as fixture data.
///
/// Represents an asserted edge: `(source_node, relation_str, target_node)`.
/// Node labels are arbitrary symbolic names — no database involvement.
pub type GraphTriple = (&'static str, &'static str, &'static str);

/// One fixture that attempts to defeat a single eliminator for a candidate
/// relation R.
///
/// A fixture defeats its eliminator when `cheaper_answer != r_answer` for
/// the stated `query` over the synthetic `graph`. If they are equal, the
/// cheaper encoding is indistinguishable from R on this graph, and the
/// eliminator is NOT defeated.
pub struct Fixture {
    /// Eliminator family code (one of `ELIMINATOR_CODES`).
    pub eliminator: &'static str,
    /// Prose description of the cheaper encoding being tested.
    pub cheaper_encoding: &'static str,
    /// Minimal synthetic graph that makes the distinction visible.
    pub graph: &'static [GraphTriple],
    /// The question posed to both encodings.
    pub query: &'static str,
    /// What the cheaper encoding returns for `query` over `graph`.
    pub cheaper_answer: &'static str,
    /// What the standalone relation R returns for `query` over `graph`.
    pub r_answer: &'static str,
}

/// Assert that a single fixture defeats its stated eliminator.
///
/// This is the per-eliminator assertion used in individual test functions.
/// A fixture defeats its eliminator when the cheaper encoding and the standalone
/// relation R return different answers for the same query over the same graph.
pub fn assert_defeats_eliminator(relation: &str, fixture: &Fixture) {
    assert!(
        !fixture.graph.is_empty(),
        "certificate for '{relation}': eliminator '{}' fixture has an empty graph; \
         supply at least one edge triple",
        fixture.eliminator,
    );
    assert!(
        fixture.cheaper_answer != fixture.r_answer,
        "certificate for '{relation}': eliminator '{}' NOT defeated — \
         cheaper encoding '{}' and standalone R return the same answer \
         '{}' for query '{}'; provide a graph where they diverge",
        fixture.eliminator,
        fixture.cheaper_encoding,
        fixture.r_answer,
        fixture.query,
    );
}

/// Run the full non-redundancy certificate for a relation.
///
/// Asserts:
/// 1. Every eliminator code in `ELIMINATOR_CODES` is covered by at least one
///    fixture.
/// 2. For every fixture, `cheaper_answer != r_answer` (the fixture actually
///    defeats its eliminator — the encodings diverge).
///
/// Call this once with the complete fixture list to verify certificate
/// completeness. Individual test functions may call `assert_defeats_eliminator`
/// to test each eliminator in isolation.
///
/// Panics with a descriptive message on the first failure.
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
