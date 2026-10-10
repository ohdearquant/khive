//! Live inventory and the narrow opaque finalizer writer boundary.

use super::super::declaration::StampCapability;
use super::*;

const FINALIZED_SITE: &str = "khive-runtime/src/secret_gate_finalizer/entity_transaction.rs::PreparedEntityExemption::into_plan";

pub(super) fn is_finalized_candidate_site(site: &Site, row: &RouteInventoryEntry) -> bool {
    site.key == FINALIZED_SITE
        && row.id == "runtime.finalized.entity"
        && row.target == Substrate::Entity
        && row.stamp == StampCapability::AdmissionCapable
        && row.transaction == TransactionOwner::RunAtomicUnit
        && site.calls.contains("entity_insert_if_absent_statement")
        && site.calls.contains("entity_replace_if_unchanged_statement")
}

#[test]
fn source_census_matches_closed_route_inventory() {
    let sources = live_workspace_sources();
    check_store_trait_methods(&sources).expect("store method surface drifted");
    let sql_sources = live_workspace_sql();
    let mut population =
        scan_source_population_with_sql(&sources, &sql_sources).expect("parse workspace sources");
    check_sql_asset_reach(&sql_sources, &population.resolved_sql)
        .unwrap_or_else(|failure| panic!("ADR-115 route census failed:\n{failure}"));
    population
        .properties
        .extend(scan_migration_sources(&live_migration_sources()));
    population.properties.sort_by(|a, b| a.key.cmp(&b.key));
    for site in &population.properties {
        eprintln!(
            "ROUTE SITE | {} | count={} | {:?} | {:?} | {:?}",
            site.key, site.write_count, site.target, site.class, site.evidence
        );
    }
    for site in &population.runtime_tables {
        eprintln!(
            "RUNTIME TABLE SITE | {} | count={} | {:?}",
            site.key, site.write_count, site.evidence
        );
    }
    check_population(
        &population,
        ROUTE_INVENTORY,
        RUNTIME_TABLE_WRITE_INVENTORY,
        PINNED_MISSING_ACCEPTANCE,
    )
    .unwrap_or_else(|failure| panic!("ADR-115 route census failed:\n{failure}"));
}

#[test]
fn finalized_candidate_classification_cannot_authorize_an_ordinary_writer() {
    let row = *ROUTE_INVENTORY
        .iter()
        .find(|row| row.id == "runtime.finalized.entity")
        .unwrap();
    let source = "use khive_db::stores::entity::{entity_insert_if_absent_statement, entity_replace_if_unchanged_statement}; fn write(entity: Entity) { entity_insert_if_absent_statement(&entity); entity_replace_if_unchanged_statement(&entity, 0, None); }";
    let population =
        scan_source_population(&[("sample/src/lib.rs".into(), source.into())]).unwrap();
    let ordinary = RouteInventoryEntry {
        site: "sample/src/lib.rs::write",
        ..row
    };
    assert!(check_population(&population, &[ordinary], &[], 0)
        .unwrap_err()
        .contains("named check/callee"));
}

#[test]
fn finalized_candidate_is_private_scanned_and_consumed_before_row_building() {
    let admission = include_str!("entity_admission.rs");
    let parsed = syn::parse_file(admission).unwrap();
    let prepared = parsed
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Struct(item) if item.ident == "PreparedEntityExemption" => Some(item),
            _ => None,
        })
        .unwrap();
    assert!(!matches!(prepared.vis, syn::Visibility::Public(_)));
    assert!(prepared
        .fields
        .iter()
        .all(|field| !matches!(field.vis, syn::Visibility::Public(_))));
    let admit = parsed
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(item) if item.sig.ident == "admit" => Some(item),
            _ => None,
        })
        .unwrap();
    assert!(matches!(admit.vis, syn::Visibility::Inherited));
    let body = admit.block.to_token_stream().to_string();
    assert!(body.find("scan_candidate").unwrap() < body.find("EXEMPTION_STAMP").unwrap());
    assert!(body.find("EXEMPTION_STAMP").unwrap() < body.find("PreparedEntityExemption").unwrap());
    let scan = parsed
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(item) if item.sig.ident == "scan_candidate" => Some(item),
            _ => None,
        })
        .unwrap();
    assert!(scan
        .block
        .to_token_stream()
        .to_string()
        .contains("reject_reserved_secret_gate_property"));
    struct Constructors(usize);
    impl<'ast> Visit<'ast> for Constructors {
        fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
            if node
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "PreparedEntityExemption")
            {
                self.0 += 1;
            }
            syn::visit::visit_expr_struct(self, node);
        }
    }
    let mut all = Constructors(0);
    all.visit_file(&parsed);
    let mut private = Constructors(0);
    private.visit_block(&admit.block);
    assert_eq!((all.0, private.0), (1, 1));

    let transaction = syn::parse_file(include_str!("entity_transaction.rs")).unwrap();
    let owner = transaction
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Impl(item)
                if item.self_ty.to_token_stream().to_string() == "PreparedEntityExemption" =>
            {
                Some(item)
            }
            _ => None,
        })
        .unwrap();
    let into_plan = owner
        .items
        .iter()
        .find_map(|item| match item {
            syn::ImplItem::Fn(item) if item.sig.ident == "into_plan" => Some(item),
            _ => None,
        })
        .unwrap();
    let Some(syn::FnArg::Receiver(receiver)) = into_plan.sig.inputs.first() else {
        panic!("finalized writer must consume its private receiver")
    };
    assert!(receiver.reference.is_none());
    let final_plan = transaction
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Struct(item) if item.ident == "EntityFinalizationPlan" => Some(item),
            _ => None,
        })
        .unwrap();
    assert!(final_plan
        .fields
        .iter()
        .all(|field| matches!(field.vis, syn::Visibility::Inherited)));
    let sources = vec![(
        "khive-runtime/src/secret_gate_finalizer/entity_transaction.rs".into(),
        include_str!("entity_transaction.rs").into(),
    )];
    let population = scan_source_population(&sources).unwrap();
    let row = *ROUTE_INVENTORY
        .iter()
        .find(|row| row.id == "runtime.finalized.entity")
        .unwrap();
    check_population(&population, &[row], &[], 0).unwrap();
    let mut invalid = population.properties[0].clone();
    invalid
        .calls
        .remove("entity_replace_if_unchanged_statement");
    assert!(!is_finalized_candidate_site(&invalid, &row));
}
