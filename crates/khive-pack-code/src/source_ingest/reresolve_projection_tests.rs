use super::*;
use khive_storage::types::{EdgeFilter, PageRequest, SqlStatement, SqlValue};
use tempfile::TempDir;

#[tokio::test]
async fn l1_reresolve_projection_preserves_every_owner_and_resolved_edge() {
    for mode in [tests::TestJournalMode::Wal, tests::TestJournalMode::Delete] {
        let directory = TempDir::new().expect("fixture directory");
        let (rt, token) = tests::runtime_on_with_mode(&directory.path().join("map.db"), mode);
        let now = DateTime::from_timestamp(1_700_000_000, 0).expect("fixture time");
        let mut expected_edges = BTreeSet::new();
        let mut expected_owners = BTreeSet::new();
        let mut missing = HashMap::new();
        for index in 0..5 {
            let owner_name = format!("projection-owner-{index}");
            let target_name = format!("projection-target-{index}");
            let owner_id = project_uuid(&owner_name);
            let target_id = project_uuid(&target_name);
            let pending = UnresolvedSpec {
                specifier: target_name.clone(),
                target_kind: "project".into(),
                dependency_kind: "manifest".into(),
                dependency_scope: "build".into(),
                language: "rust".into(),
            };
            let unresolved = UnresolvedSpec {
                specifier: format!("projection-missing-{index}"),
                ..pending.clone()
            };
            let mut owner = Entity::new(token.namespace().as_str(), "project", &owner_name);
            owner.id = owner_id;
            owner.properties = Some(json!({
                "source_project": owner_name,
                "unresolved_specifiers": [pending, unresolved],
                "preserved": index
            }));
            let mut target = Entity::new(token.namespace().as_str(), "project", &target_name);
            target.id = target_id;
            for entity in [owner, target] {
                rt.entities(&token)
                    .expect("entities")
                    .upsert_entity(entity)
                    .await
                    .expect("seed");
            }
            expected_owners.insert(owner_id);
            expected_edges.insert((owner_id, target_id));
            missing.insert(owner_id, unresolved);
        }

        let mut reader = rt.sql().reader().await.expect("reader");
        let wide_rows = reader
            .query_all(SqlStatement {
                sql: "SELECT id, kind, properties FROM entities WHERE namespace=?1 \
                  AND deleted_at IS NULL \
                  AND json_extract(properties,'$.unresolved_specifiers') IS NOT NULL"
                    .into(),
                params: vec![SqlValue::Text(token.namespace().as_str().to_owned())],
                label: Some("code_ingest_reresolve_projection_oracle".into()),
            })
            .await
            .expect("parent projection oracle");
        drop(reader);
        let oracle: BTreeSet<Uuid> = wide_rows
            .iter()
            .map(|row| match row.get("id") {
                Some(SqlValue::Uuid(id)) => *id,
                Some(SqlValue::Text(id)) => Uuid::parse_str(id).expect("canonical id"),
                _ => panic!("missing id"),
            })
            .collect();
        assert_eq!(
            oracle, expected_owners,
            "fixture contains every unresolved owner"
        );

        let mut report = CodeSourceIngestReport::default();
        reresolve_pass(
            &rt,
            &token,
            &ManifestScopeIndex::new(),
            &ProjectRenames::new(),
            ReresolveTiers {
                l1: true,
                l1_5: false,
            },
            now,
            &mut report,
        )
        .await
        .expect("actual L1 pass");
        assert_eq!(
            report.unresolved_resolved,
            expected_owners.len() as u64,
            "every scanned owner must resolve exactly one target"
        );
        let edges = rt
            .graph(&token)
            .expect("graph")
            .query_edges(
                EdgeFilter::default(),
                Vec::new(),
                PageRequest {
                    limit: 1000,
                    offset: 0,
                },
            )
            .await
            .expect("resolved edges");
        let actual_edges: BTreeSet<_> = edges
            .items
            .into_iter()
            .map(|edge| (edge.source_id, edge.target_id))
            .collect();
        assert_eq!(
            actual_edges, expected_edges,
            "complete re-resolved set is order independent"
        );
        for owner_id in expected_owners {
            let owner = get_entity_opt(&rt, &token, owner_id)
                .await
                .expect("owner read")
                .expect("owner");
            let properties = owner.properties.expect("properties");
            assert_eq!(
                read_unresolved(&properties),
                vec![missing[&owner_id].clone()],
                "the unrelated missing reference survives every owner replay"
            );
            assert!(
                properties.get("preserved").is_some(),
                "unrelated property survives"
            );
        }
    }
}
