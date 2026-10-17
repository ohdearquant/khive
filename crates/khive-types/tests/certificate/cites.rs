//! ADR-076 non-redundancy certificate fixtures for the `cites` relation.
//!
//! `cites` is proposed as a Tier-1 core relation for directed intellectual
//! reference: a document references another document (or concept) as a source.
//! Wire direction: `document → document` (primary), `document → concept`.
//!
//! Each positive test (numbered) verifies that `cites` passes one eliminator
//! from ADR-076 §D2. Each negative-control test (suffix `_rejected_by_*`)
//! verifies that a deliberately-redundant candidate IS eliminated, proving the
//! check functions are not vacuous. The final test (`full_certificate_cites`)
//! runs all seven positive fixtures together.

use std::collections::BTreeSet;

use crate::harness::{
    all_pairs, assert_defeats_eliminator, assert_eliminated_by, fmt_pairs, reachable,
    run_certificate, EliminatorCheck, Fixture, GraphTriple,
};

include!("cites/converse_and_endpoints_tests.rs");

include!("cites/attributes_and_polarity_tests.rs");

include!("cites/composition_and_reachability_tests.rs");

include!("cites/subrelation_and_certificate_tests.rs");
