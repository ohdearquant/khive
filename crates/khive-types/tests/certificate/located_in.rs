//! ADR-076 non-redundancy certificate fixtures for the `located_in` relation (ADR-196).
//!
//! `located_in` says the source occupies, or is manifested in, the target and is not a
//! constituent of it: a disorder in an organ, an organ in a body cavity. Wire direction is
//! located -> location.
//!
//! The fixture is the one ADR-196 names: `lung`, `respiratory system` and `right lung`
//! (anatomy), `pneumonia` (disorder), with `right lung part_of lung`,
//! `lung part_of respiratory system` and `pneumonia located_in lung`; the Er fixture adds
//! `thoracic cavity` and `lung located_in thoracic cavity`. A second Er fixture tests
//! `located_in` against `extends`, the relation it shares the base `concept -> concept`
//! endpoint signature with.
//!
//! Every check derives both answers from the fixture graph. Each check is generic over the
//! relations present in the graph, so the negative controls at the bottom of the file feed
//! the same check functions a graph in which `located_in` really is redundant and require
//! them to fire: a check that could never return `Eliminated` would certify nothing.

use std::collections::BTreeSet;

use crate::harness::{
    all_pairs, assert_defeats_eliminator, assert_eliminated_by, fmt_pairs, reachable,
    run_certificate, EliminatorCheck, Fixture, GraphTriple,
};

include!("located_in/support_and_endpoints_tests.rs");

include!("located_in/attributes_and_views_tests.rs");

include!("located_in/positive_fixtures_tests.rs");

include!("located_in/negative_controls_tests.rs");
