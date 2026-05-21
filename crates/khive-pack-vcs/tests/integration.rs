// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Integration tests for the VCS pack (ADR-042, ADR-043, ADR-015).
//!
//! Tests exercise verb handlers end-to-end through a real `VerbRegistry`
//! with both `kg` and `vcs` packs loaded, using an in-memory runtime.

use khive_pack_kg::KgPack;
use khive_pack_vcs::VcsPack;
use khive_runtime::{EntityPatch, KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::{json, Value};

// ── Test helpers ──────────────────────────────────────────────────────────────

fn make_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime")
}

async fn make_registry() -> (khive_runtime::VerbRegistry, KhiveRuntime) {
    let rt_kg = make_runtime();
    let rt_vcs = rt_kg.clone();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt_kg));
    builder.register(VcsPack::new(rt_vcs.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, rt_vcs)
}

async fn dispatch(
    registry: &khive_runtime::VerbRegistry,
    verb: &str,
    params: Value,
) -> Result<Value, khive_runtime::RuntimeError> {
    registry.dispatch(verb, params).await
}

// ── commit ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn commit_happy_path() {
    let (registry, _rt) = make_registry().await;
    let result = dispatch(
        &registry,
        "commit",
        json!({"message": "initial snapshot", "author": "test"}),
    )
    .await
    .expect("commit should succeed");

    assert!(
        result.get("id").is_some(),
        "commit result must include snapshot id"
    );
    assert_eq!(result["message"], "initial snapshot");
}

// ── branch ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn branch_create_and_list() {
    let (registry, _rt) = make_registry().await;

    // Create initial commit so "main" exists.
    dispatch(&registry, "commit", json!({"message": "init"}))
        .await
        .expect("initial commit");

    // Create a new branch from main.
    let created = dispatch(
        &registry,
        "branch",
        json!({"action": "create", "name": "feature-x", "from_branch": "main"}),
    )
    .await
    .expect("branch create should succeed");
    assert_eq!(created["name"], "feature-x");

    // List branches — main and feature-x should appear.
    let list = dispatch(&registry, "branch", json!({"action": "list"}))
        .await
        .expect("branch list should succeed");
    let names: Vec<&str> = list
        .as_array()
        .expect("branch list returns array")
        .iter()
        .filter_map(|b| b["name"].as_str())
        .collect();
    assert!(names.contains(&"main"), "main branch must be listed");
    assert!(
        names.contains(&"feature-x"),
        "feature-x branch must be listed"
    );
}

#[tokio::test]
async fn branch_get_happy_path() {
    let (registry, _rt) = make_registry().await;
    dispatch(&registry, "commit", json!({"message": "init"}))
        .await
        .expect("initial commit");

    let got = dispatch(
        &registry,
        "branch",
        json!({"action": "get", "name": "main"}),
    )
    .await
    .expect("branch get should succeed");
    assert_eq!(got["name"], "main");
}

#[tokio::test]
async fn branch_get_missing_returns_null() {
    let (registry, _rt) = make_registry().await;
    let got = dispatch(
        &registry,
        "branch",
        json!({"action": "get", "name": "no-such-branch"}),
    )
    .await
    .expect("branch get for missing branch returns null, not error");
    assert!(got.is_null(), "missing branch should return null");
}

#[tokio::test]
async fn branch_create_missing_source_returns_not_found() {
    let (registry, _rt) = make_registry().await;
    let err = dispatch(
        &registry,
        "branch",
        json!({"action": "create", "name": "child", "from_branch": "nonexistent"}),
    )
    .await
    .expect_err("creating branch from nonexistent source must fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::NotFound(_)),
        "expected NotFound, got {err:?}"
    );
}

// ── log ───────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn log_happy_path() {
    let (registry, rt) = make_registry().await;

    // First commit with entity A.
    rt.create_entity(None, "concept", "LogEntityA", None, None, vec![])
        .await
        .unwrap();
    dispatch(&registry, "commit", json!({"message": "first"}))
        .await
        .expect("commit 1");

    // Second commit with entity B added (different content hash → new snapshot).
    rt.create_entity(None, "concept", "LogEntityB", None, None, vec![])
        .await
        .unwrap();
    dispatch(&registry, "commit", json!({"message": "second"}))
        .await
        .expect("commit 2");

    let log = dispatch(&registry, "log", json!({}))
        .await
        .expect("log should succeed");
    let entries = log.as_array().expect("log returns array");
    assert!(entries.len() >= 2, "log must contain at least 2 entries");
}

// ── checkout ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn checkout_restores_snapshot() {
    let (registry, rt) = make_registry().await;

    // Create an entity, commit, then mutate the namespace.
    rt.create_entity(None, "concept", "Before", None, None, vec![])
        .await
        .unwrap();
    let snap = dispatch(&registry, "commit", json!({"message": "with Before"}))
        .await
        .expect("commit with entity");
    let _snap_id = snap["id"].as_str().expect("snapshot id").to_string();

    // Delete the entity to mutate state.
    let entities = rt.list_entities(None, None, 100).await.unwrap();
    let entity_id = entities[0].id;
    rt.delete_entity(None, entity_id, false).await.unwrap();

    // ADR-015: checkout by branch name to restore main HEAD.
    // branch_name and snapshot_id are mutually exclusive; use branch_name here.
    // force=true because we deleted an entity (live state diverges from last commit).
    let result = dispatch(
        &registry,
        "checkout",
        json!({"branch_name": "main", "force": true}),
    )
    .await
    .expect("checkout should succeed");
    assert_eq!(result["branch_name"], "main");
    assert_eq!(result["entities_restored"], 1);
}

#[tokio::test]
async fn checkout_force_false_rejects_when_live_state_diverged() {
    // ADR-015 safety: force=false checkout must fail when the live namespace state
    // has diverged from the last committed snapshot, even when kg_vcs_state.dirty
    // was not explicitly set by a VCS operation.
    let (registry, rt) = make_registry().await;

    dispatch(&registry, "commit", json!({"message": "initial"}))
        .await
        .expect("initial commit");

    // Add an entity via the runtime — this does not set dirty=1 in kg_vcs_state,
    // but the hash-comparison check in checkout() must detect the divergence.
    rt.create_entity(None, "concept", "Added", None, None, vec![])
        .await
        .unwrap();

    // force=false checkout must now fail because the live hash differs from last_committed_id.
    let err = dispatch(
        &registry,
        "checkout",
        json!({"branch_name": "main", "force": false}),
    )
    .await
    .expect_err("checkout must fail when live state has diverged and force=false");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput (UncommittedChanges), got {err:?}"
    );
}

#[tokio::test]
async fn checkout_snapshot_only_without_branch_name() {
    // ADR-015: snapshot_id is optional and does not require branch_name.
    let (registry, rt) = make_registry().await;

    rt.create_entity(None, "concept", "SnapOnly", None, None, vec![])
        .await
        .unwrap();
    let snap = dispatch(&registry, "commit", json!({"message": "snap commit"}))
        .await
        .expect("commit");
    let snap_id = snap["id"].as_str().expect("snapshot id").to_string();

    // Delete the entity.
    let entities = rt.list_entities(None, None, 100).await.unwrap();
    rt.delete_entity(None, entities[0].id, false).await.unwrap();

    // Checkout by snapshot_id alone — no branch_name required (ADR-015).
    let result = dispatch(
        &registry,
        "checkout",
        json!({"snapshot_id": snap_id, "force": true}),
    )
    .await
    .expect("snapshot-only checkout must succeed");
    // branch_name is null for a snapshot-only checkout.
    assert!(
        result["branch_name"].is_null(),
        "snapshot-only checkout must return null branch_name"
    );
    assert_eq!(result["entities_restored"], 1);
}

#[tokio::test]
async fn checkout_both_branch_and_snapshot_returns_invalid_input() {
    // ADR-015: supplying both snapshot_id and branch_name must be rejected.
    let (registry, _rt) = make_registry().await;
    let err = dispatch(
        &registry,
        "checkout",
        json!({"branch_name": "main", "snapshot_id": "sha256:aabbcc1122334455aabbcc1122334455aabbcc1122334455aabbcc1122334455", "force": true}),
    )
    .await
    .expect_err("both branch_name and snapshot_id must be rejected");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput for conflicting checkout params, got {err:?}"
    );
}

#[tokio::test]
async fn checkout_force_true_succeeds_with_dirty_state() {
    let (registry, rt) = make_registry().await;

    dispatch(&registry, "commit", json!({"message": "initial"}))
        .await
        .expect("initial commit");

    // Add entity without committing.
    rt.create_entity(None, "concept", "ToDiscard", None, None, vec![])
        .await
        .unwrap();

    // Force checkout should succeed even with uncommitted changes.
    let result = dispatch(
        &registry,
        "checkout",
        json!({"branch_name": "main", "force": true}),
    )
    .await
    .expect("force checkout should succeed");
    assert_eq!(result["branch_name"], "main");
}

// ── merge_branch ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn merge_branch_clean_applies_deletions() {
    // ADR-015: a clean merge must apply deletions — entities absent from the
    // merged archive must disappear from the live namespace after merge.
    let (registry, rt) = make_registry().await;

    // Base state on main: EntityA and EntityB both exist.
    let entity_a = rt
        .create_entity(None, "concept", "EntityA", None, None, vec![])
        .await
        .unwrap();
    rt.create_entity(None, "concept", "EntityB", None, None, vec![])
        .await
        .unwrap();
    dispatch(
        &registry,
        "commit",
        json!({"message": "base state with A and B"}),
    )
    .await
    .expect("base commit");

    // Create feature branch from main (has both A and B).
    dispatch(
        &registry,
        "branch",
        json!({"action": "create", "name": "delete-b-branch", "from_branch": "main"}),
    )
    .await
    .expect("branch create");

    // On feature branch: delete EntityB and commit.
    rt.delete_entity(None, entity_a.id, false).await.unwrap(); // We'll delete A on feature
                                                               // Actually delete EntityB so feature branch snapshot lacks it.
    let all = rt.list_entities(None, None, 100).await.unwrap();
    let entity_b = all.iter().find(|e| e.name == "EntityB").unwrap();
    rt.delete_entity(None, entity_b.id, false).await.unwrap();
    dispatch(
        &registry,
        "commit",
        json!({"message": "feature: deleted EntityA and EntityB", "branch": "delete-b-branch"}),
    )
    .await
    .expect("commit on feature branch");

    // Restore main state: checkout main so both A and B are live again.
    dispatch(
        &registry,
        "checkout",
        json!({"branch_name": "main", "force": true}),
    )
    .await
    .expect("checkout main");

    // Verify both entities are live on main.
    let before_merge = rt.list_entities(None, None, 100).await.unwrap();
    assert_eq!(
        before_merge.len(),
        2,
        "main should have 2 entities before merge"
    );

    // Merge feature branch (which lacks both EntityA and EntityB) into main.
    let merge_result = dispatch(
        &registry,
        "merge_branch",
        json!({
            "theirs": "delete-b-branch",
            "target_branch": "main",
            "strategy": "theirs"
        }),
    )
    .await
    .expect("merge_branch should succeed");

    // ADR-015: clean merge status is "clean".
    assert_eq!(
        merge_result["status"], "clean",
        "clean merge must report status=clean (ADR-015); got {merge_result}"
    );
    assert!(
        merge_result.get("snapshot_id").is_some(),
        "merge result must include snapshot_id"
    );

    // After merge with strategy=theirs: the feature branch had 0 entities, so
    // the merged result must have deleted both EntityA and EntityB from live state.
    let after_merge = rt.list_entities(None, None, 100).await.unwrap();
    assert_eq!(
        after_merge.len(),
        0,
        "deletions from feature branch must be applied: expected 0 entities, got {} — wipe+import did not apply deletes",
        after_merge.len()
    );
}

#[tokio::test]
async fn merge_branch_conflict_returns_structured_json() {
    // ADR-015 / ADR-043: when both ours and theirs modify the same entity
    // differently (true divergent edit), merge must return status="conflicts"
    // with a non-empty JSON array of conflict descriptors.
    let (registry, rt) = make_registry().await;

    // Base state: entity "Shared" exists on main.
    let shared = rt
        .create_entity(None, "concept", "Shared", None, None, vec![])
        .await
        .unwrap();
    dispatch(&registry, "commit", json!({"message": "base with Shared"}))
        .await
        .expect("base commit");

    // Create a feature branch from this base.
    dispatch(
        &registry,
        "branch",
        json!({"action": "create", "name": "conflict-branch", "from_branch": "main"}),
    )
    .await
    .expect("branch create");

    // Commit a "theirs" snapshot on conflict-branch: rename "Shared" to "SharedTheirs".
    // We snapshot the CURRENT state (which still has "Shared") as the branch head
    // — the branch was created before main diverged, so the base = this commit.
    // Then we commit a *modified* state for the feature branch by renaming and committing.
    rt.update_entity(
        None,
        shared.id,
        EntityPatch {
            name: Some("SharedTheirs".to_string()),
            ..EntityPatch::default()
        },
    )
    .await
    .unwrap();
    dispatch(
        &registry,
        "commit",
        json!({"message": "feature rename to SharedTheirs", "branch": "conflict-branch"}),
    )
    .await
    .expect("commit on conflict-branch");

    // Restore main with force (main still at base, rename undone by restore).
    dispatch(
        &registry,
        "checkout",
        json!({"branch_name": "main", "force": true}),
    )
    .await
    .expect("checkout main");

    // On main: rename "Shared" to "SharedOurs" — this diverges from conflict-branch.
    let ours_entities = rt.list_entities(None, None, 100).await.unwrap();
    let shared_live = ours_entities.iter().find(|e| e.name == "Shared").unwrap();
    rt.update_entity(
        None,
        shared_live.id,
        EntityPatch {
            name: Some("SharedOurs".to_string()),
            ..EntityPatch::default()
        },
    )
    .await
    .unwrap();
    dispatch(
        &registry,
        "commit",
        json!({"message": "main rename to SharedOurs"}),
    )
    .await
    .expect("commit rename on main");

    // Now merge conflict-branch (renamed to SharedTheirs) into main (renamed to SharedOurs).
    // Both sides modified the same entity differently from the common base → conflict.
    let conflict_result = dispatch(
        &registry,
        "merge_branch",
        json!({
            "theirs": "conflict-branch",
            "target_branch": "main",
            "strategy": "auto"
        }),
    )
    .await
    .expect("merge_branch must not error — conflicts are returned in the payload");

    // Must have a status field.
    assert!(
        conflict_result.get("status").is_some(),
        "merge response must include status field"
    );

    // The divergent rename must produce conflicts, not a clean merge.
    assert_eq!(
        conflict_result["status"], "conflicts",
        "divergent entity rename must yield status=conflicts (ADR-015); got {conflict_result}"
    );

    // Conflict payload must be a JSON array (structured, not a debug string).
    let conflicts = &conflict_result["conflicts"];
    assert!(
        conflicts.is_array(),
        "conflicts must be a JSON array, not a Debug string: {conflicts}"
    );
    assert!(
        !conflicts.as_array().unwrap().is_empty(),
        "conflicts array must be non-empty for a divergent rename"
    );
}

#[tokio::test]
async fn merge_branch_missing_source_returns_not_found() {
    // ADR-015: `theirs` is required; a branch that doesn't exist must return NotFound.
    let (registry, _rt) = make_registry().await;
    let err = dispatch(
        &registry,
        "merge_branch",
        json!({"theirs": "no-such-branch", "target_branch": "main"}),
    )
    .await
    .expect_err("merge with missing branch must fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::NotFound(_)),
        "expected NotFound for missing source branch, got {err:?}"
    );
}

#[tokio::test]
async fn merge_branch_missing_theirs_returns_invalid_input() {
    // ADR-015: `theirs` is required; omitting both `theirs` and `source_branch` must
    // return InvalidInput rather than panicking or dispatching to a nonexistent branch.
    let (registry, _rt) = make_registry().await;
    let err = dispatch(&registry, "merge_branch", json!({"target_branch": "main"}))
        .await
        .expect_err("merge without theirs must fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput when theirs is absent, got {err:?}"
    );
}

// ── export_kg / import_kg ─────────────────────────────────────────────────────

#[tokio::test]
async fn export_import_kg_roundtrip() {
    let (registry, rt) = make_registry().await;

    rt.create_entity(None, "concept", "RoundtripEntity", None, None, vec![])
        .await
        .unwrap();

    // Export.
    let archive = dispatch(&registry, "export_kg", json!({}))
        .await
        .expect("export_kg should succeed");
    assert_eq!(archive["format"], "khive-kg");
    let entities = archive["entities"].as_array().expect("entities array");
    assert_eq!(entities.len(), 1, "exported archive must have 1 entity");

    // Fresh runtime for import.
    let (registry2, rt2) = make_registry().await;
    let summary = dispatch(&registry2, "import_kg", json!({"archive": archive}))
        .await
        .expect("import_kg should succeed");
    assert_eq!(summary["entities_imported"], 1);
    assert_eq!(summary["edges_skipped"], 0);

    let imported = rt2.list_entities(None, None, 100).await.unwrap();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].name, "RoundtripEntity");
}

// ── Registry-level: bad params → InvalidInput ─────────────────────────────────

#[tokio::test]
async fn commit_missing_required_param_returns_invalid_input() {
    let (registry, _rt) = make_registry().await;
    // `commit` requires `message` — omit it.
    let err = dispatch(&registry, "commit", json!({}))
        .await
        .expect_err("commit without message must fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );
}

#[tokio::test]
async fn unknown_verb_returns_invalid_input() {
    let (registry, _rt) = make_registry().await;
    let err = dispatch(&registry, "no_such_verb_xyz", json!({}))
        .await
        .expect_err("unknown verb must fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput for unknown verb, got {err:?}"
    );
}

// ── VCS verb names in registry ────────────────────────────────────────────────

#[test]
fn vcs_verb_names_match_adr_023() {
    let rt = make_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(VcsPack::new(rt));
    let registry = builder.build().expect("registry builds");
    let verb_names: Vec<&str> = registry.all_verbs().iter().map(|v| v.name).collect();

    // ADR-023:76 names
    assert!(verb_names.contains(&"commit"), "must have 'commit'");
    assert!(verb_names.contains(&"branch"), "must have 'branch'");
    assert!(verb_names.contains(&"checkout"), "must have 'checkout'");
    assert!(
        verb_names.contains(&"merge_branch"),
        "must have 'merge_branch'"
    );
    assert!(verb_names.contains(&"log"), "must have 'log'");
    // ADR-015:37 names
    assert!(verb_names.contains(&"export_kg"), "must have 'export_kg'");
    assert!(verb_names.contains(&"import_kg"), "must have 'import_kg'");

    // Old / incorrect names must not appear
    assert!(
        !verb_names.contains(&"snapshot"),
        "'snapshot' renamed to 'commit'"
    );
    assert!(
        !verb_names.contains(&"vcs_merge"),
        "'vcs_merge' renamed to 'merge_branch'"
    );
    assert!(
        !verb_names.contains(&"export"),
        "'export' renamed to 'export_kg'"
    );
    assert!(
        !verb_names.contains(&"import"),
        "'import' renamed to 'import_kg'"
    );
    assert!(
        !verb_names.contains(&"shortest_path"),
        "'shortest_path' removed from VCS pack"
    );
}

// ── Regression: merge_branch preserves uncommitted live entities ──────────────

#[tokio::test]
async fn merge_branch_preserves_uncommitted_live_entity() {
    // Regression test for round-3 Critical finding: merge_branch must not silently
    // destroy entities that exist in the live namespace but have not been committed.
    //
    // Scenario:
    //   1. commit main (base)
    //   2. branch + commit feature (adds FeatureOnly entity)
    //   3. checkout main (restores base)
    //   4. add an uncommitted entity (LiveOnly) to the live namespace
    //   5. merge_branch theirs=feature
    //   6. assert LiveOnly is present in the merged result
    let (registry, rt) = make_registry().await;

    // Step 1: commit main with one entity.
    rt.create_entity(None, "concept", "BaseEntity", None, None, vec![])
        .await
        .unwrap();
    dispatch(&registry, "commit", json!({"message": "base commit"}))
        .await
        .expect("base commit");

    // Step 2: create feature branch and add a feature entity.
    dispatch(
        &registry,
        "branch",
        json!({"action": "create", "name": "feature-add", "from_branch": "main"}),
    )
    .await
    .expect("branch create");

    rt.create_entity(None, "concept", "FeatureOnly", None, None, vec![])
        .await
        .unwrap();
    dispatch(
        &registry,
        "commit",
        json!({"message": "feature commit", "branch": "feature-add"}),
    )
    .await
    .expect("feature commit");

    // Step 3: checkout main (restores base — only BaseEntity, no FeatureOnly).
    dispatch(
        &registry,
        "checkout",
        json!({"branch_name": "main", "force": true}),
    )
    .await
    .expect("checkout main");

    let after_checkout = rt.list_entities(None, None, 100).await.unwrap();
    assert_eq!(
        after_checkout.len(),
        1,
        "main should have 1 entity after checkout"
    );

    // Step 4: add an uncommitted entity to the live namespace.
    rt.create_entity(None, "concept", "LiveOnly", None, None, vec![])
        .await
        .unwrap();

    let before_merge = rt.list_entities(None, None, 100).await.unwrap();
    assert_eq!(
        before_merge.len(),
        2,
        "live namespace should have 2 entities before merge (BaseEntity + LiveOnly)"
    );

    // Step 5: merge feature branch into main.
    let result = dispatch(
        &registry,
        "merge_branch",
        json!({
            "theirs": "feature-add",
            "target_branch": "main",
            "strategy": "auto"
        }),
    )
    .await
    .expect("merge_branch should succeed");

    // Merge should complete cleanly (all three sides have no conflicting edits).
    assert_eq!(
        result["status"], "clean",
        "merge should be clean, got {result}"
    );

    // Step 6: LiveOnly must survive the merge.
    let after_merge = rt.list_entities(None, None, 100).await.unwrap();
    let names: Vec<&str> = after_merge.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"LiveOnly"),
        "LiveOnly (uncommitted live entity) must survive merge_branch — names after merge: {names:?}"
    );
    assert!(
        names.contains(&"FeatureOnly"),
        "FeatureOnly (from theirs branch) must be present after merge — names: {names:?}"
    );
    assert!(
        names.contains(&"BaseEntity"),
        "BaseEntity (common base) must survive merge — names: {names:?}"
    );
}

// ── Regression: checkout reports nonzero count on pure deletion ───────────────

#[tokio::test]
async fn checkout_rejects_pure_deletion_with_nonzero_count() {
    // Regression test for round-3 Medium finding: estimate_uncommitted_count must
    // return a non-zero count when the only change is a deletion (i.e. live
    // entity count is 0 but committed count was 1).
    let (registry, rt) = make_registry().await;

    // Commit one entity.
    rt.create_entity(None, "concept", "ToDelete", None, None, vec![])
        .await
        .unwrap();
    dispatch(&registry, "commit", json!({"message": "one entity"}))
        .await
        .expect("initial commit");

    // Delete the entity from the live namespace (no commit).
    let entities = rt.list_entities(None, None, 100).await.unwrap();
    assert_eq!(entities.len(), 1, "should have 1 entity before deletion");
    rt.delete_entity(None, entities[0].id, false).await.unwrap();

    // Live namespace is now empty — a naive count-based estimate returns 0.
    // Our symmetric-diff implementation must return 1 (ToDelete was in committed but not live).
    let err = dispatch(
        &registry,
        "checkout",
        json!({"branch_name": "main", "force": false}),
    )
    .await
    .expect_err("checkout must be rejected when only change is a deletion");

    // Must be InvalidInput (UncommittedChanges).
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput for pure-deletion checkout, got {err:?}"
    );

    // The error message must mention a non-zero count.
    let msg = err.to_string();
    assert!(
        !msg.contains("0 entities"),
        "error message must not claim 0 uncommitted changes for a deletion; got: {msg}"
    );
}

// ── Regression: verb catalog advertises canonical param names ─────────────────

#[test]
fn verb_catalog_advertises_canonical_param_names() {
    // Regression test for round-3 Medium finding: VerbDef descriptions must use
    // canonical ADR-015 param names (snapshot_id, required theirs) not stale aliases.
    use khive_types::Pack;

    let verbs = khive_pack_vcs::VcsPack::VERBS;

    let checkout_def = verbs
        .iter()
        .find(|v| v.name == "checkout")
        .expect("checkout verb");
    assert!(
        checkout_def.description.contains("snapshot_id"),
        "checkout VerbDef must advertise canonical 'snapshot_id' param, got: {:?}",
        checkout_def.description
    );
    assert!(
        !checkout_def.description.starts_with("snapshot?")
            && !checkout_def
                .description
                .contains("(params: branch_name, snapshot?"),
        "checkout VerbDef must not lead with stale 'snapshot?' alias, got: {:?}",
        checkout_def.description
    );

    let merge_def = verbs
        .iter()
        .find(|v| v.name == "merge_branch")
        .expect("merge_branch verb");
    assert!(
        merge_def.description.contains("theirs (required"),
        "merge_branch VerbDef must advertise 'theirs' as required, got: {:?}",
        merge_def.description
    );
    assert!(
        !merge_def.description.starts_with("source_branch"),
        "merge_branch VerbDef must not lead with stale 'source_branch' alias, got: {:?}",
        merge_def.description
    );
}
