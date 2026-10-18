//! Source-side guard for ADR-115 Amendment 5's properties write population.
//! The scanner is pure over `(path, text)` pairs so mutations can exercise it
//! without building or loading another crate.

use super::declaration::{
    Acceptance, Reservation, RouteInventoryEntry, RuntimeTableWriteInventoryEntry, Substrate,
    TransactionOwner, WriteClass, PINNED_MISSING_ACCEPTANCE, ROUTE_INVENTORY,
    RUNTIME_TABLE_WRITE_INVENTORY,
};
use model::{DetectedClass, RouteClass, Site, NOTE_PROPERTY_SQL_CONSTANTS, STORE_WRITES};
use quote::ToTokens;
use source_inventory::{
    check_inventory, check_population, check_sql_asset_reach, check_store_trait_methods,
    is_finalized_candidate_site, live_workspace_sources, live_workspace_sql,
    scan_migration_sources, scan_source_population, scan_source_population_with_sql, scan_sources,
};
use sql_routes::{sql_fixed_key_paths, sql_single_key_path, sql_target, sql_unclassified_write};
use static_sql_source::StaticSqlSources;
use std::collections::BTreeSet;
use std::path::Path;
use syn::visit::Visit;

#[path = "route_census_parsed_sources.rs"]
mod parsed_sources;
#[path = "../../tests/support/static_sql_source.rs"]
mod static_sql_source;

mod acceptance;
mod acceptance_sources;
mod ast_helpers;
mod model;
mod module_resolution;
mod source_collector;
mod source_inventory;
mod sql_routes;

#[path = "route_census_migrations.rs"]
mod migrations;
use migrations::live_migration_sources;

#[cfg(test)]
#[path = "route_census_occurrence_tests.rs"]
mod route_census_occurrence_tests;

#[cfg(test)]
#[path = "route_census_declaration_tests.rs"]
mod route_census_declaration_tests;

#[cfg(test)]
#[path = "route_census_parsed_sources_tests.rs"]
mod parsed_sources_tests;

#[cfg(test)]
#[path = "route_census_synthetic_controls_tests.rs"]
mod route_census_synthetic_controls_tests;

#[cfg(test)]
#[path = "route_census_static_sql_tests.rs"]
mod route_census_static_sql_tests;

#[cfg(test)]
#[path = "route_census_finalization_tests.rs"]
mod finalization_tests;

#[cfg(test)]
#[path = "route_census_shadowing_tests.rs"]
mod shadowing_tests;

#[test]
fn migration_sql_is_inventoried() {
    let sites = scan_migration_sources(&live_migration_sources());
    assert!(sites.iter().any(|site| {
        site.key == "khive-db/sql/005-unique-comm-external-id.sql::statement_1"
            && site.target == Substrate::Note
            && site.route_class == RouteClass::Migration
    }));
    let fixture = scan_migration_sources(&[(
        "khive-db/sql/999-census-fixture.sql".into(),
        "-- UPDATE entities SET properties is only a comment\nUPDATE notes SET properties = '{}' WHERE id = 'fixture';".into(),
    )]);
    assert_eq!(fixture.len(), 1);
    assert_eq!(fixture[0].route_class, RouteClass::Migration);
    assert!(check_inventory(&fixture, &[], 0)
        .unwrap_err()
        .contains("unmapped khive-db/sql/999-census-fixture.sql::statement_1"));
}

#[cfg(test)]
#[path = "route_census_resolution_tests.rs"]
mod resolution_tests;

#[cfg(test)]
#[path = "route_census_acceptance_tests.rs"]
mod acceptance_tests;
