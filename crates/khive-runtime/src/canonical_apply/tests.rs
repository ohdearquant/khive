use super::*;
use khive_changeset::{EntityPatch, EntityPreimage, Envelope, UpdatePreimage};
use khive_storage::entity::{EntityAlias, EntityAliasOrigin};
use khive_storage::{SqlStatement, SqlValue};
use khive_types::{Namespace, Timestamp};

struct Fixture {
    runtime: KhiveRuntime,
    _dir: tempfile::TempDir,
}
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new_for_test(crate::RuntimeConfig {
        db_path: Some(dir.path().join("canonical.db")),
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        ..Default::default()
    })
    .unwrap();
    Fixture { runtime, _dir: dir }
}
fn changeset(ops: Vec<Op>) -> ChangeSet {
    ChangeSet::new(
        Envelope::new("agent:test", "test", Timestamp::from_secs(1)),
        ops,
    )
}
fn entity_create(id: Uuid, name: &str) -> Op {
    Op::Create(khive_changeset::CreateOp {
        id: id128(id),
        namespace: Namespace::local(),
        target: CreateTarget::Entity(khive_changeset::EntityCreateFields {
            entity_kind: khive_types::EntityKind::Concept,
            entity_type: None,
            name: name.into(),
            description: None,
            properties: Default::default(),
            tags: Vec::new(),
            key: None,
            aliases: vec![],
        }),
    })
}
fn rename(id: Uuid, old: &str, new: &str) -> Op {
    Op::Update(
        UpdateOp::new(
            id128(id),
            UpdatePatch::Entity(EntityPatch {
                name: Some(new.into()),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
                aliases: None,
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: Some(old.into()),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
                aliases: None,
            }),
        )
        .unwrap(),
    )
}
fn registry() -> VerbRegistry {
    crate::VerbRegistryBuilder::new().build().unwrap()
}

#[tokio::test]
async fn ordered_creates_link_and_updates_keep_staged_ids() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let cs = changeset(vec![
        entity_create(a, "a"),
        entity_create(b, "b"),
        rename(a, "a", "a2"),
        rename(a, "a2", "a3"),
        Op::Link(khive_changeset::LinkOp {
            id: id128(e),
            namespace: Namespace::local(),
            source: id128(a),
            target: id128(b),
            relation: khive_types::EdgeRelation::CompetesWith,
            weight: 0.75,
            properties: Default::default(),
        }),
    ]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let row = rt
        .entities(&token)
        .unwrap()
        .get_entity(a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.name, "a3");
    let edge = rt.get_edge(&token, e).await.unwrap().unwrap();
    assert_eq!(Uuid::from(edge.id), e);
    assert_eq!(edge.weight, 0.75);
}

#[tokio::test]
async fn unchanged_entity_update_is_a_no_op_and_replays_without_a_new_revision() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let a = Uuid::new_v4();
    let updated = || khive_storage::EventFilter {
        target_id: Some(a),
        kinds: vec![khive_types::EventKind::EntityUpdated],
        ..Default::default()
    };
    let version = |rt: &KhiveRuntime, token: &NamespaceToken| {
        let entities = rt.entities(token).unwrap();
        async move { entities.get_entity(a).await.unwrap().unwrap().version }
    };
    let create = changeset(vec![entity_create(a, "same")]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &create)
        .await
        .unwrap();
    crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap();
    let created = version(rt, &token).await;
    for cs in [
        changeset(vec![rename(a, "same", "same")]),
        changeset(vec![rename(a, "same", "same")]),
    ] {
        let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
            .await
            .unwrap();
        assert!(matches!(
            crate::run_atomic_unit(rt.sql().as_ref(), plans)
                .await
                .unwrap(),
            crate::AtomicRunOutcome::Committed { .. }
        ));
    }
    assert_eq!(version(rt, &token).await, created);
    let events = rt.events(&token).unwrap();
    assert_eq!(events.count_events(updated()).await.unwrap(), 0);

    // A real change still writes one revision and one event.
    let cs = changeset(vec![rename(a, "same", "changed")]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .unwrap();
    crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap();
    assert_eq!(version(rt, &token).await, created + 1);
    assert_eq!(events.count_events(updated()).await.unwrap(), 1);
}

fn keyed_create(id: Uuid, name: &str, key: Option<&str>, aliases: &[&str]) -> Op {
    let Op::Create(mut create) = entity_create(id, name) else {
        unreachable!("entity_create builds a create")
    };
    let CreateTarget::Entity(fields) = &mut create.target else {
        unreachable!("entity_create builds an entity create")
    };
    fields.key = key.map(str::to_owned);
    fields.aliases = aliases.iter().map(|alias| (*alias).to_owned()).collect();
    Op::Create(create)
}

fn alias(name: &str, origin: EntityAliasOrigin) -> EntityAlias {
    EntityAlias {
        alias: name.into(),
        origin,
    }
}

/// An update of the alias set, renaming from `old` to `new` when they differ.
fn alias_update(
    id: Uuid,
    old: &str,
    new: &str,
    before: Vec<EntityAlias>,
    after: Vec<EntityAlias>,
) -> Op {
    let renamed = old != new;
    Op::Update(
        UpdateOp::new(
            id128(id),
            UpdatePatch::Entity(EntityPatch {
                name: renamed.then(|| new.into()),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
                aliases: Some(after),
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: renamed.then(|| old.into()),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
                aliases: Some(before),
            }),
        )
        .unwrap(),
    )
}

fn entity_delete(row: &Entity, keys: &[&str], aliases: Vec<EntityAlias>) -> Op {
    Op::Delete(DeleteOp {
        target_id: id128(row.id),
        hard: false,
        preimage: khive_changeset::DeletePreimage::Entity(Box::new(
            khive_changeset::EntityRecord {
                entity: projection::entity(row).unwrap(),
                keys: keys.iter().map(|key| (*key).to_owned()).collect(),
                aliases,
            },
        )),
    })
}

async fn apply(rt: &KhiveRuntime, token: &NamespaceToken, ops: Vec<Op>) -> crate::AtomicRunOutcome {
    let plans = prepare_canonical_changeset(rt, &registry(), token, &changeset(ops))
        .await
        .unwrap();
    crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap()
}

async fn refusal(rt: &KhiveRuntime, token: &NamespaceToken, ops: Vec<Op>) -> (usize, String) {
    match prepare_canonical_changeset(rt, &registry(), token, &changeset(ops)).await {
        Err(RuntimeError::CanonicalAdmission { op_index, source }) => {
            (op_index, source.to_string())
        }
        Err(other) => panic!("expected an admission refusal, got {other:?}"),
        Ok(_) => panic!("expected an admission refusal, the change set was prepared"),
    }
}

async fn entity_row(rt: &KhiveRuntime, token: &NamespaceToken, id: Uuid) -> Option<Entity> {
    rt.entities(token).unwrap().get_entity(id).await.unwrap()
}

async fn insert_alias(rt: &KhiveRuntime, token: &NamespaceToken, id: Uuid, name: &str) {
    rt.sql()
        .writer()
        .await
        .unwrap()
        .execute(khive_db::stores::entity::entity_alias_insert_statement(
            token.namespace().as_str(),
            id,
            name,
            EntityAliasOrigin::Caller,
            1,
        ))
        .await
        .unwrap();
}

fn sorted_aliases(row: &Entity) -> Vec<EntityAlias> {
    let mut aliases = row.aliases.clone();
    aliases.sort();
    aliases
}

/// ADR-221 sections 1 and 2: a canonical create claims its key and writes its
/// caller aliases in the unit that writes the row; an alias equal to the name
/// is not stored.
#[tokio::test]
async fn a_keyed_create_claims_its_key_and_writes_its_aliases() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let a = Uuid::new_v4();
    let create = keyed_create(a, "name", Some("k1"), &["nick", "name"]);
    assert!(matches!(
        apply(rt, &token, vec![create]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let holder = rt
        .get_entity_by_key(&token, "concept", "k1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(holder.id, a);
    assert_eq!(
        sorted_aliases(&holder),
        vec![alias("nick", EntityAliasOrigin::Caller)]
    );
}

/// Two creates in one change set taking one key refuse the whole set as
/// `key_conflict` before anything is written.
#[tokio::test]
async fn two_creates_taking_one_key_refuse_the_whole_change_set() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let ops = vec![
        keyed_create(a, "a", Some("k1"), &[]),
        keyed_create(b, "b", Some("k1"), &[]),
    ];
    let (op_index, message) = refusal(rt, &token, ops).await;
    assert_eq!(op_index, 1);
    assert!(message.contains("key_conflict"), "{message}");
    assert!(message.contains("\"k1\""), "{message}");
    assert!(entity_row(rt, &token, a).await.is_none());
    assert!(entity_row(rt, &token, b).await.is_none());
}

/// A key a live entity holds refuses the create, naming the key and not the
/// holder; a delete of the holder earlier in the same change set releases it.
#[tokio::test]
async fn a_held_key_refuses_a_create_unless_an_earlier_delete_releases_it() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let (h, b) = (Uuid::new_v4(), Uuid::new_v4());
    assert!(matches!(
        apply(rt, &token, vec![keyed_create(h, "holder", Some("k1"), &[])]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let (op_index, message) =
        refusal(rt, &token, vec![keyed_create(b, "b", Some("k1"), &[])]).await;
    assert_eq!(op_index, 0);
    assert!(message.contains("key_conflict"), "{message}");
    assert!(!message.contains(&h.to_string()), "{message}");
    let holder = entity_row(rt, &token, h).await.unwrap();
    let ops = vec![
        entity_delete(&holder, &["k1"], Vec::new()),
        keyed_create(b, "b", Some("k1"), &[]),
    ];
    assert!(matches!(
        apply(rt, &token, ops).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let holder = rt
        .get_entity_by_key(&token, "concept", "k1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(holder.id, b);
}

/// The delete record's keys and aliases are compared with the live row's.
#[tokio::test]
async fn a_delete_whose_record_differs_in_keys_or_aliases_is_refused() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let h = Uuid::new_v4();
    let create = keyed_create(h, "holder", Some("k1"), &["nick"]);
    assert!(matches!(
        apply(rt, &token, vec![create]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let row = entity_row(rt, &token, h).await.unwrap();
    let held = sorted_aliases(&row);
    for (field, op) in [
        ("keys", entity_delete(&row, &[], held.clone())),
        ("aliases", entity_delete(&row, &["k1"], Vec::new())),
    ] {
        assert!(matches!(apply(rt, &token, vec![op]).await,
            crate::AtomicRunOutcome::RolledBack{failure:AtomicOpFailure::PreimageMismatch{field_path,..},..} if field_path==field));
        assert!(entity_row(rt, &token, h).await.is_some());
    }
    assert!(matches!(
        apply(rt, &token, vec![entity_delete(&row, &["k1"], held)]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    assert!(entity_row(rt, &token, h).await.is_none());
}

/// A change that carries the alias set writes exactly that set, with the
/// origins it names, and an alias set naming the entity's name is refused.
#[tokio::test]
async fn an_alias_set_update_writes_exactly_that_set_with_its_origins() {
    use EntityAliasOrigin::{Caller, Rename};
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let a = Uuid::new_v4();
    assert!(matches!(
        apply(rt, &token, vec![keyed_create(a, "old", None, &["nick"])]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let after = vec![alias("handle", Caller), alias("old", Rename)];
    let op = alias_update(a, "old", "new", vec![alias("nick", Caller)], after.clone());
    assert!(matches!(
        apply(rt, &token, vec![op]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let row = entity_row(rt, &token, a).await.unwrap();
    assert_eq!(row.name, "new");
    assert_eq!(sorted_aliases(&row), after);
    let op = alias_update(
        a,
        "new",
        "handle",
        after.clone(),
        vec![alias("handle", Caller)],
    );
    let (op_index, message) = refusal(rt, &token, vec![op]).await;
    assert_eq!(op_index, 0);
    assert!(message.contains("equals the entity's name"), "{message}");
    assert_eq!(
        sorted_aliases(&entity_row(rt, &token, a).await.unwrap()),
        after
    );
}

async fn alias_rows(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Vec<(String, String, i64)> {
    let mut reader = rt.sql().reader().await.unwrap();
    let rows = reader
        .query_all(SqlStatement::new(
            "SELECT alias, origin, created_at FROM entity_aliases \
             WHERE namespace = ?1 AND entity_id = ?2 ORDER BY alias",
            vec![
                SqlValue::Text(token.namespace().as_str().to_owned()),
                SqlValue::Text(id.to_string()),
            ],
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|row| {
            (
                row.text("alias").unwrap().to_owned(),
                row.text("origin").unwrap().to_owned(),
                row.i64("created_at").unwrap(),
            )
        })
        .collect()
}

/// An alias-set update writes only the difference: an alias it keeps with the
/// same origin keeps its row and its time, an alias whose origin changes is
/// written again, and a dropped alias is gone.
#[tokio::test]
async fn an_alias_set_update_keeps_the_time_of_an_unchanged_alias() {
    use EntityAliasOrigin::{Caller, Rename};
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let a = Uuid::new_v4();
    assert!(matches!(
        apply(rt, &token, vec![entity_create(a, "name")]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    insert_alias(rt, &token, a, "kept").await;
    insert_alias(rt, &token, a, "moved").await;
    insert_alias(rt, &token, a, "dropped").await;

    let before = vec![
        alias("dropped", Caller),
        alias("kept", Caller),
        alias("moved", Caller),
    ];
    let after = vec![
        alias("added", Caller),
        alias("kept", Caller),
        alias("moved", Rename),
    ];
    let op = alias_update(a, "name", "name", before, after.clone());
    assert!(matches!(
        apply(rt, &token, vec![op]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));

    assert_eq!(
        sorted_aliases(&entity_row(rt, &token, a).await.unwrap()),
        after
    );
    let rows = alias_rows(rt, &token, a).await;
    let names: Vec<&str> = rows.iter().map(|(alias, _, _)| alias.as_str()).collect();
    assert_eq!(names, ["added", "kept", "moved"]);
    assert_eq!(rows[1], ("kept".to_owned(), "caller".to_owned(), 1));
    assert_eq!(rows[2].1, "rename");
    assert_ne!(rows[2].2, 1, "an origin change writes the alias again");
    assert_ne!(rows[0].2, 1);
}

/// ADR-221 section 2: an alias never equals its entity's name. A rename onto
/// a held alias is refused, naming the alias and the entity, unless the change
/// carries the alias set; with it the set is written and the alias is gone.
#[tokio::test]
async fn a_rename_onto_a_held_alias_needs_the_alias_set_in_the_change() {
    use EntityAliasOrigin::{Caller, Rename};
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let a = Uuid::new_v4();
    assert!(matches!(
        apply(rt, &token, vec![entity_create(a, "old")]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    insert_alias(rt, &token, a, "new").await;

    let (op_index, message) = refusal(rt, &token, vec![rename(a, "old", "new")]).await;
    assert_eq!(op_index, 0);
    assert!(message.contains("\"new\""), "{message}");
    assert!(message.contains(&a.to_string()), "{message}");
    let row = entity_row(rt, &token, a).await.unwrap();
    assert_eq!(row.name, "old");
    assert_eq!(sorted_aliases(&row), vec![alias("new", Caller)]);

    let op = alias_update(
        a,
        "old",
        "new",
        vec![alias("new", Caller)],
        vec![alias("old", Rename)],
    );
    assert!(matches!(
        apply(rt, &token, vec![op]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let row = entity_row(rt, &token, a).await.unwrap();
    assert_eq!(row.name, "new");
    assert_eq!(sorted_aliases(&row), vec![alias("old", Rename)]);
}

fn secret_shaped_alias() -> String {
    const ALPHANUMERIC: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let candidate: String = (0..48)
        .map(|index| char::from(ALPHANUMERIC[(index * 17 + 11) % ALPHANUMERIC.len()]))
        .collect();
    format!("secret value: {candidate}")
}

async fn secret_refusal(rt: &KhiveRuntime, token: &NamespaceToken, ops: Vec<Op>) -> usize {
    match prepare_canonical_changeset(rt, &registry(), token, &changeset(ops)).await {
        Err(RuntimeError::CanonicalAdmission { op_index, source }) => {
            assert!(
                matches!(
                    source.as_ref(),
                    RuntimeError::SecretDetected(found)
                        if found.location.as_deref() == Some("entity.aliases")
                ),
                "{source:?}"
            );
            op_index
        }
        Err(other) => panic!("expected an admission refusal, got {other:?}"),
        Ok(_) => panic!("expected an admission refusal, the change set was prepared"),
    }
}

/// ADR-221 section 2: every alias in a change is scanned by the secret gate
/// before any write. A secret-shaped alias in an update's alias set, or in a
/// create's aliases, refuses the change set at the alias field and leaves the
/// stored aliases as they were.
#[tokio::test]
async fn a_secret_shaped_alias_is_refused_before_any_write() {
    use EntityAliasOrigin::Caller;
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let secret = secret_shaped_alias();
    assert!(
        matches!(
            crate::secret_gate::check(&secret),
            Err(RuntimeError::SecretDetected(_))
        ),
        "fixture alias must trip the detector on its own"
    );
    let a = Uuid::new_v4();
    assert!(matches!(
        apply(rt, &token, vec![keyed_create(a, "name", None, &["nick"])]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));

    let op = alias_update(
        a,
        "name",
        "name",
        vec![alias("nick", Caller)],
        vec![alias("nick", Caller), alias(&secret, Caller)],
    );
    assert_eq!(secret_refusal(rt, &token, vec![op]).await, 0);
    assert_eq!(
        sorted_aliases(&entity_row(rt, &token, a).await.unwrap()),
        vec![alias("nick", Caller)]
    );

    let b = Uuid::new_v4();
    let create = keyed_create(b, "other", None, &["fine", &secret]);
    assert_eq!(secret_refusal(rt, &token, vec![create]).await, 0);
    assert!(entity_row(rt, &token, b).await.is_none());
}

/// The writer checks the rename again: an alias equal to the new name that
/// appeared after preparation rolls the change set back.
#[tokio::test]
async fn a_held_alias_taken_after_preparation_rolls_the_rename_back() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let a = Uuid::new_v4();
    assert!(matches!(
        apply(rt, &token, vec![entity_create(a, "old")]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(a, "old", "new")]),
    )
    .await
    .unwrap();
    insert_alias(rt, &token, a, "new").await;
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::RolledBack { .. }
    ));
    let row = entity_row(rt, &token, a).await.unwrap();
    assert_eq!(row.name, "old");
    assert_eq!(
        sorted_aliases(&row),
        vec![alias("new", EntityAliasOrigin::Caller)]
    );
}

#[tokio::test]
async fn unhooked_writer_drift_survives_but_touched_drift_rolls_back_prior_ops() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut row = Entity::new("local", "concept", "old");
    let id = row.id;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row.clone())
        .await
        .unwrap();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(id, "old", "new")]),
    )
    .await
    .unwrap();
    row.description = Some("writer-visible".into());
    row.updated_at += 1;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let mut live = rt
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.description.as_deref(), Some("writer-visible"));
    assert_eq!(live.name, "new");
    let prior = Uuid::new_v4();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![
            entity_create(prior, "rollback"),
            rename(id, "new", "after"),
        ]),
    )
    .await
    .unwrap();
    live.name = "concurrent".into();
    live.updated_at += 1;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(live)
        .await
        .unwrap();
    assert!(
        matches!(crate::run_atomic_unit(rt.sql().as_ref(),plans).await.unwrap(),
        crate::AtomicRunOutcome::RolledBack{failed_op_index:1,failure:AtomicOpFailure::PreimageMismatch{field_path,..}} if field_path=="name")
    );
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(prior)
        .await
        .unwrap()
        .is_none());
}

#[derive(Debug)]
struct Owner;
#[async_trait::async_trait]
impl crate::pack::KindHook for Owner {
    async fn prepare_create(&self, _: &KhiveRuntime, _: &mut Value) -> RuntimeResult<()> {
        Ok(())
    }
}
#[tokio::test]
async fn owner_snapshot_conflict_is_distinct_from_field_preimage() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    rt.install_entity_kind_hooks(vec![("concept".into(), std::sync::Arc::new(Owner))]);
    let mut row = Entity::new("local", "concept", "old");
    let id = row.id;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row.clone())
        .await
        .unwrap();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(id, "old", "new")]),
    )
    .await
    .unwrap();
    row.description = Some("concurrent".into());
    row.updated_at += 1;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row)
        .await
        .unwrap();
    assert!(
        matches!(crate::run_atomic_unit(rt.sql().as_ref(),plans).await.unwrap(),
        crate::AtomicRunOutcome::RolledBack{failure:AtomicOpFailure::OwnerSnapshotConflict{target_id,..},..} if target_id==id)
    );
}

#[tokio::test]
async fn delete_ignores_only_updated_at_and_keeps_ordinary_delete_effects() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut row = Entity::new("local", "concept", "delete");
    let id = row.id;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row.clone())
        .await
        .unwrap();
    let op = Op::Delete(DeleteOp {
        target_id: id128(id),
        hard: false,
        preimage: khive_changeset::DeletePreimage::Entity(Box::new(
            projection::entity(&row).unwrap().into(),
        )),
    });
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![op]))
        .await
        .unwrap();
    row.updated_at += 1;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn live_and_tombstone_ids_and_edge_keys_never_absorb_staged_ids() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    for deleted in [false, true] {
        let mut row = Entity::new("local", "concept", "holder");
        let id = row.id;
        if deleted {
            row.deleted_at = Some(row.updated_at);
        }
        rt.entities(&token)
            .unwrap()
            .upsert_entity(row)
            .await
            .unwrap();
        let plans = prepare_canonical_changeset(
            rt,
            &registry(),
            &token,
            &changeset(vec![entity_create(id, "staged")]),
        )
        .await
        .unwrap();
        // The staged-id check refuses it, not a later key constraint on the insert.
        match crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap()
        {
            crate::AtomicRunOutcome::RolledBack {
                failed_op_index: 0,
                failure,
            } => assert!(
                format!("{failure:?}").contains("canonical staged id already exists"),
                "{failure:?}"
            ),
            other => panic!("deleted={deleted}: {other:?}"),
        }
    }
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let link = |id: Uuid| {
        Op::Link(khive_changeset::LinkOp {
            id: id128(id),
            namespace: Namespace::local(),
            source: id128(a),
            target: id128(b),
            relation: khive_types::EdgeRelation::CompetesWith,
            weight: 0.5,
            properties: Default::default(),
        })
    };
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![
            entity_create(a, "a"),
            entity_create(b, "b"),
            link(e),
            link(Uuid::new_v4()),
        ]),
    )
    .await
    .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::RolledBack {
            failed_op_index: 3,
            ..
        }
    ));
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(a)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn missing_and_wrong_substrate_preimages_are_indexed_refusals() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(id, "old", "new")]),
    )
    .await
    .unwrap();
    assert!(
        matches!(crate::run_atomic_unit(rt.sql().as_ref(),plans).await.unwrap(),crate::AtomicRunOutcome::RolledBack{
        failure:AtomicOpFailure::PreimageMismatch{field_path,..},..} if field_path=="id")
    );
    let row = Note::new("local", "observation", "note");
    let id = row.id;
    rt.notes(&token).unwrap().upsert_note(row).await.unwrap();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(id, "old", "new")]),
    )
    .await
    .unwrap();
    assert!(
        matches!(crate::run_atomic_unit(rt.sql().as_ref(),plans).await.unwrap(),crate::AtomicRunOutcome::RolledBack{
        failure:AtomicOpFailure::PreimageMismatch{field_path,..},..} if field_path=="substrate")
    );
}

#[test]
fn schema_count_and_merge_are_checked_before_preparation() {
    let mut cs = changeset(Vec::new());
    cs.envelope.schema_version += 1;
    assert!(validate(&cs).is_err());
    let cs = changeset(vec![entity_create(Uuid::new_v4(), "many"); 2001]);
    assert!(validate(&cs).is_err());
    let into = projection::entity(&Entity::new("local", "concept", "into")).unwrap();
    let from = projection::entity(&Entity::new("local", "concept", "from")).unwrap();
    let merge = Op::Merge(khive_changeset::MergeOp {
        into_id: into.header.id,
        from_id: from.header.id,
        preimage: khive_changeset::MergePreimage {
            into: Box::new(into.into()),
            from: Box::new(from.into()),
            incident_edges: Vec::new(),
        },
    });
    assert!(
        matches!(validate(&changeset(vec![merge])).unwrap_err(), RuntimeError::CanonicalAdmission { op_index: 0, source } if source.to_string().contains("KnownUnimplemented"))
    );
}

#[tokio::test]
async fn note_map_replacement_preserves_same_writer_tag_drift_and_tag_order() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut note =
        Note::new("local", "observation", "same").with_properties(json!({"old":1,"tags":["old"]}));
    let id = note.id;
    rt.notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let old = projection::note(&note).unwrap().properties;
    let new = serde_json::from_value(json!({"replacement":2})).unwrap();
    let op = Op::Update(
        UpdateOp::new(
            id128(id),
            UpdatePatch::Note(khive_changeset::NotePatch {
                content: None,
                salience: None,
                decay_factor: None,
                properties: Some(new),
                tags: None,
            }),
            UpdatePreimage::Note(khive_changeset::NotePreimage {
                content: None,
                salience: None,
                decay_factor: None,
                properties: Some(old),
                tags: None,
            }),
        )
        .unwrap(),
    );
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![op]))
        .await
        .unwrap();
    note.properties = Some(json!({"old":1,"tags":["writer-b","writer-a"]}));
    note.updated_at += 1;
    rt.notes(&token).unwrap().upsert_note(note).await.unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let live = rt
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        live.properties,
        Some(json!({"replacement":2,"tags":["writer-b","writer-a"]}))
    );
}

#[tokio::test]
async fn canonical_writer_materialization_resolves_on_first_poll_and_suspend_misuse_refuses() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![
            entity_create(id, "ready"),
            rename(id, "ready", "committed"),
        ]),
    )
    .await
    .unwrap();
    let pool = std::sync::Arc::new(
        khive_db::ConnectionPool::new(khive_db::PoolConfig {
            path: Some(f._dir.path().join("canonical.db")),
            write_queue_enabled: Some(true),
            ..khive_db::PoolConfig::for_test()
        })
        .unwrap(),
    );
    let bridge = khive_db::SqlBridge::new(pool, true);
    assert!(matches!(
        crate::run_atomic_unit(&bridge, plans).await.unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let row = rt
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.name, "committed");
    let op: khive_storage::AtomicUnitOp = Box::new(|_writer| {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });
    assert!(khive_storage::SqlAccess::atomic_unit(&bridge, op)
        .await
        .is_err());
}

#[tokio::test]
async fn edge_update_then_delete_observes_earlier_writer_state() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let initial = changeset(vec![
        entity_create(a, "a"),
        entity_create(b, "b"),
        Op::Link(khive_changeset::LinkOp {
            id: id128(e),
            namespace: Namespace::local(),
            source: id128(a),
            target: id128(b),
            relation: khive_types::EdgeRelation::CompetesWith,
            weight: 0.5,
            properties: Default::default(),
        }),
    ]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &initial)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let live = rt.get_edge(&token, e).await.unwrap().unwrap();
    let mut deleted = projection::edge(&live).unwrap();
    deleted.weight = 0.75;
    let update = Op::Update(
        UpdateOp::new(
            id128(e),
            UpdatePatch::Edge(khive_changeset::EdgePatch {
                relation: None,
                weight: Some(0.75),
            }),
            UpdatePreimage::Edge(khive_changeset::EdgePreimage {
                relation: None,
                weight: Some(0.5),
            }),
        )
        .unwrap(),
    );
    let delete = Op::Delete(DeleteOp {
        target_id: id128(e),
        hard: false,
        preimage: khive_changeset::DeletePreimage::Edge(Box::new(deleted)),
    });
    let plans =
        prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![update, delete]))
            .await
            .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    assert!(rt.get_edge(&token, e).await.unwrap().is_none());
}

#[test]
fn shared_entity_fields_keep_legacy_merge_and_canonical_replace_distinct() {
    let original =
        Entity::new("local", "concept", "same").with_properties(json!({"old":1,"keep":2}));
    let patch = crate::curation::EntityPatch {
        properties: Some(json!({"new":3})),
        ..Default::default()
    };
    let mut legacy = original.clone();
    let mut canonical = original;
    KhiveRuntime::apply_entity_patch_fields(&mut legacy, patch.clone(), &[], false);
    KhiveRuntime::apply_entity_patch_fields(&mut canonical, patch, &[], true);
    assert_eq!(legacy.properties, Some(json!({"old":1,"keep":2,"new":3})));
    assert_eq!(canonical.properties, Some(json!({"new":3})));
}

async fn delete_entity_and_invert(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
) -> (ChangeSet, Entity) {
    use EntityAliasOrigin::{Caller, Rename};

    let id = Uuid::new_v4();
    let aliases = vec![alias("handle", Caller), alias("old", Rename)];
    assert!(matches!(
        apply(
            rt,
            token,
            vec![keyed_create(id, "old", Some("k1"), &["nick"])]
        )
        .await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    assert!(matches!(
        apply(
            rt,
            token,
            vec![alias_update(
                id,
                "old",
                "new",
                vec![alias("nick", Caller)],
                aliases.clone()
            )],
        )
        .await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    assert_eq!(
        rt.sql()
            .writer()
            .await
            .unwrap()
            .execute(khive_db::stores::entity::entity_key_claim_statement(
                "local", "concept", "k2", id, 1,
            ))
            .await
            .unwrap(),
        1
    );
    let live = entity_row(rt, token, id).await.unwrap();
    let delete = entity_delete(&live, &["k1", "k2"], aliases);
    assert!(matches!(
        apply(rt, token, vec![delete.clone()]).await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let tombstone = rt
        .entities(token)
        .unwrap()
        .get_entity_including_deleted(id)
        .await
        .unwrap()
        .unwrap();
    assert!(tombstone.deleted_at.is_some());
    for key in ["k1", "k2"] {
        assert!(rt
            .get_entity_by_key(token, "concept", key)
            .await
            .unwrap()
            .is_none());
    }
    let mut current = khive_changeset::CurrentState::default();
    current.entities.insert(
        id128(id),
        entity_record(&tombstone, vec!["k1".into(), "k2".into()]).unwrap(),
    );
    let inverse = changeset(vec![delete])
        .invert(
            Envelope::new("agent:test", "inverse", Timestamp::from_secs(2)),
            &current,
        )
        .unwrap();
    (inverse, tombstone)
}

#[tokio::test]
async fn canonical_inverted_delete_restores_keys_alias_origins_and_indexes() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let (inverse, tombstone) = delete_entity_and_invert(rt, &token).await;
    let id = tombstone.id;
    let fts = rt.text(&token).unwrap();
    let filter = || khive_storage::TextFilter {
        ids: vec![id],
        namespaces: vec!["local".into()],
        ..Default::default()
    };
    assert_eq!(fts.count(filter()).await.unwrap(), 0);
    let Op::Restore(restore) = &inverse.ops[0] else {
        panic!("the inverse must contain a restore");
    };
    for invalid_keys in [
        vec!["k1".into()],
        vec!["k1".into(), "k2".into(), "k3".into()],
    ] {
        let mut wrong_record = restore.record().clone();
        let DeletePreimage::Entity(wrong) = &mut wrong_record else {
            unreachable!();
        };
        wrong.keys = invalid_keys;
        let wrong = Op::Restore(khive_changeset::RestoreOp::new(id128(id), wrong_record).unwrap());
        let refused = prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![wrong]))
            .await
            .unwrap_err();
        let wire = crate::runtime_error_value(refused, crate::DomainDisposition::Unknown);
        assert_eq!(wire["details"]["field"], "keys");
    }
    assert!(entity_row(rt, &token, id).await.is_none());

    let plans = prepare_canonical_changeset(rt, &registry(), &token, &inverse)
        .await
        .unwrap();
    let crate::AtomicRunOutcome::Committed { post_commit } =
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap()
    else {
        panic!("the inverse must commit");
    };
    assert_eq!(
        post_commit.as_slice(),
        &[crate::atomic_plan::PostCommitEffect::ReindexEntity { entity_id: id }]
    );
    let restored = entity_row(rt, &token, id).await.unwrap();
    assert_eq!(restored.created_at, tombstone.created_at);
    assert!(restored.updated_at > tombstone.updated_at);
    assert_eq!(restored.version, tombstone.version + 1);
    assert_eq!(restored.deleted_at, None);
    assert_eq!(restored.name, "new");
    assert_eq!(fts.count(filter()).await.unwrap(), 1);
    assert_eq!(sorted_aliases(&restored), sorted_aliases(&tombstone));
    assert_eq!(
        alias_rows(rt, &token, id).await,
        vec![
            ("handle".into(), "caller".into(), restored.updated_at),
            ("old".into(), "rename".into(), restored.updated_at),
        ]
    );
    for key in ["k1", "k2"] {
        assert_eq!(
            rt.get_entity_by_key(&token, "concept", key)
                .await
                .unwrap()
                .unwrap()
                .id,
            id
        );
    }
    assert!(
        prepare_canonical_changeset(rt, &registry(), &token, &inverse)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn canonical_restore_observes_an_earlier_soft_delete_in_the_same_unit() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    assert!(matches!(
        apply(
            rt,
            &token,
            vec![keyed_create(id, "old", Some("k1"), &["nick"])]
        )
        .await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let live = entity_row(rt, &token, id).await.unwrap();
    let aliases = live.aliases.clone();
    let Op::Delete(delete) = entity_delete(&live, &["k1"], aliases.clone()) else {
        unreachable!();
    };
    let restore = khive_changeset::RestoreOp::new(id128(id), delete.preimage.clone()).unwrap();
    let rename = alias_update(
        id,
        "old",
        "new",
        aliases.clone(),
        [aliases, vec![alias("old", EntityAliasOrigin::Rename)]].concat(),
    );
    assert!(matches!(
        apply(
            rt,
            &token,
            vec![Op::Delete(delete), Op::Restore(restore), rename]
        )
        .await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let restored = rt
        .get_entity_by_key(&token, "concept", "k1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.id, id);
    assert_eq!(restored.name, "new");
    assert_eq!(restored.version, live.version + 3);
    assert_eq!(restored.aliases.len(), 2);
}

#[tokio::test]
async fn canonical_restore_reports_every_key_taken_after_preparation() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let (inverse, tombstone) = delete_entity_and_invert(rt, &token).await;
    let (first, last, holder_a, holder_b) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let draft = changeset(vec![
        entity_create(first, "before"),
        inverse.ops[0].clone(),
        entity_create(last, "after"),
    ]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &draft)
        .await
        .unwrap();
    assert!(matches!(
        apply(
            rt,
            &token,
            vec![
                keyed_create(holder_a, "one", Some("k1"), &[]),
                keyed_create(holder_b, "two", Some("k2"), &[]),
            ]
        )
        .await,
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let events = rt.events(&token).unwrap();
    let before = events.count_events(Default::default()).await.unwrap();
    let crate::AtomicRunOutcome::RolledBack {
        failed_op_index: 1,
        failure: AtomicOpFailure::RestoreKeyConflict(conflict),
    } = crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap()
    else {
        panic!("the writer must refuse the taken keys");
    };
    assert_eq!(conflict.target_id, tombstone.id);
    assert_eq!(
        conflict
            .taken_keys
            .iter()
            .map(|key| key.key.as_str())
            .collect::<Vec<_>>(),
        vec!["k1", "k2"]
    );
    for id in [first, tombstone.id, last] {
        assert!(entity_row(rt, &token, id).await.is_none());
    }
    let unchanged = rt
        .entities(&token)
        .unwrap()
        .get_entity_including_deleted(tombstone.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.deleted_at, tombstone.deleted_at);
    assert_eq!(unchanged.version, tombstone.version);
    assert_eq!(sorted_aliases(&unchanged), sorted_aliases(&tombstone));
    for (key, holder) in [("k1", holder_a), ("k2", holder_b)] {
        assert_eq!(
            rt.get_entity_by_key(&token, "concept", key)
                .await
                .unwrap()
                .unwrap()
                .id,
            holder
        );
    }
    assert_eq!(
        events.count_events(Default::default()).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn note_restore_refuses_the_whole_changeset_before_preparation() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let record = projection::note(&Note::new("local", "observation", "restored")).unwrap();
    let restored_id = uuid(record.header.id);
    let restore = Op::Restore(
        khive_changeset::RestoreOp::new(record.header.id, DeletePreimage::Note(Box::new(record)))
            .unwrap(),
    );
    let first = Uuid::new_v4();
    let last = Uuid::new_v4();
    let events = rt.events(&token).unwrap();
    let before = events.count_events(Default::default()).await.unwrap();
    let error = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![
            entity_create(first, "before"),
            restore,
            entity_create(last, "after"),
        ]),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        &error,
        RuntimeError::CanonicalUnsupportedOperation {
            op_index: 1,
            operation: "restore",
        }
    ));
    let wire = crate::runtime_error_value(error, crate::DomainDisposition::Unknown);
    assert_eq!(wire["op_index"], 1);
    assert_eq!(wire["operation"], "restore");
    assert_eq!(wire["reason"], "KnownUnimplemented");
    assert_eq!(wire["domain_disposition"], "not_committed");
    assert_eq!(wire["retryable"], false);
    for id in [first, restored_id, last] {
        assert!(rt
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .is_none());
    }
    assert_eq!(
        events.count_events(Default::default()).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn canonical_memory_link_equal_namespace_keeps_source_namespace() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let source = Note::new("local", "memory", "source");
    let target = Note::new("local", "memory", "target");
    let (source_id, target_id, edge_id) = (source.id, target.id, Uuid::new_v4());
    rt.notes(&token).unwrap().upsert_note(source).await.unwrap();
    rt.notes(&token).unwrap().upsert_note(target).await.unwrap();
    for id in [source_id, target_id] {
        let stored = rt
            .resolve_by_id_including_deleted(&token, id)
            .await
            .unwrap();
        let Some(Resolved::Note(stored)) = stored else {
            panic!("setup requires a stored note");
        };
        assert_eq!(stored.kind, "memory");
        assert_eq!(stored.namespace.as_str(), "local");
        assert!(stored.deleted_at.is_none());
    }
    let cs = changeset(vec![Op::Link(khive_changeset::LinkOp {
        id: id128(edge_id),
        namespace: Namespace::local(),
        source: id128(source_id),
        target: id128(target_id),
        relation: khive_types::EdgeRelation::Annotates,
        weight: 1.0,
        properties: Default::default(),
    })]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let edge = rt.get_edge(&token, edge_id).await.unwrap().unwrap();
    assert_eq!(Uuid::from(edge.id), edge_id);
    assert_eq!(edge.source_id, source_id);
    assert_eq!(edge.target_id, target_id);
    assert_eq!(edge.namespace.as_str(), "local");
}

#[tokio::test]
async fn canonical_memory_namespace_mismatch_refuses_before_any_write() {
    let f = fixture();
    let rt = &f.runtime;
    let source_token = rt.authorize(Namespace::local()).unwrap();
    // Keep the source namespace readable for this diagnostic assertion.
    let token = rt
        .authorize_with_visibility(Namespace::parse("other").unwrap(), vec![Namespace::local()])
        .unwrap();
    assert_eq!(token.namespace().as_str(), "other");
    let source = Note::new("local", "memory", "source");
    let target = Note::new("local", "memory", "target");
    let (source_id, target_id, edge_id) = (source.id, target.id, Uuid::new_v4());
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(source)
        .await
        .unwrap();
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(target)
        .await
        .unwrap();
    for id in [source_id, target_id] {
        let stored = rt
            .resolve_by_id_including_deleted(&token, id)
            .await
            .unwrap();
        let Some(Resolved::Note(stored)) = stored else {
            panic!("setup requires a readable stored note");
        };
        assert_eq!(stored.kind, "memory");
        assert_eq!(stored.namespace.as_str(), "local");
        assert!(stored.deleted_at.is_none());
    }
    let side_id = Uuid::new_v4();
    let mut side = entity_create(side_id, "earlier side op");
    let Op::Create(create) = &mut side else {
        panic!("setup requires an entity create");
    };
    create.namespace = Namespace::parse("other").unwrap();
    let cs = changeset(vec![
        side,
        Op::Link(khive_changeset::LinkOp {
            id: id128(edge_id),
            namespace: Namespace::parse("other").unwrap(),
            source: id128(source_id),
            target: id128(target_id),
            relation: khive_types::EdgeRelation::Annotates,
            weight: 1.0,
            properties: Default::default(),
        }),
    ]);
    let error = match prepare_canonical_changeset(rt, &registry(), &token, &cs).await {
        Err(error) => error,
        Ok(_) => panic!("canonical_memory_namespace_mismatch_admitted"),
    };
    let RuntimeError::CanonicalAdmission { op_index, source } = error else {
        panic!("expected canonical namespace admission refusal");
    };
    assert_eq!(op_index, 1);
    let RuntimeError::InvalidInput(message) = *source else {
        panic!("expected the memory namespace rule");
    };
    assert_eq!(
        message,
        "apply_changeset: memory edge source namespace local does not match link operation namespace other; set this link operation's namespace to local and prepare the change-set again"
    );
    assert!(rt.get_edge(&token, edge_id).await.unwrap().is_none());
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(side_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn canonical_staged_memory_source_namespace_mismatch_refuses_before_any_write() {
    let f = fixture();
    let rt = &f.runtime;
    let source_token = rt.authorize(Namespace::local()).unwrap();
    // Keep the source namespace readable for this diagnostic assertion.
    let token = rt
        .authorize_with_visibility(Namespace::parse("other").unwrap(), vec![Namespace::local()])
        .unwrap();
    assert_eq!(token.namespace().as_str(), "other");
    let target = Note::new("local", "memory", "target");
    let (source_id, target_id, edge_id, side_id) =
        (Uuid::new_v4(), target.id, Uuid::new_v4(), Uuid::new_v4());
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(target)
        .await
        .unwrap();
    let stored = rt
        .resolve_by_id_including_deleted(&token, target_id)
        .await
        .unwrap();
    let Some(Resolved::Note(stored)) = stored else {
        panic!("setup requires a readable stored target note");
    };
    assert_eq!(stored.kind, "memory");
    assert_eq!(stored.namespace.as_str(), "local");
    assert!(stored.deleted_at.is_none());
    let staged = Op::Create(khive_changeset::CreateOp {
        id: id128(source_id),
        namespace: Namespace::local(),
        target: CreateTarget::Note(khive_changeset::NoteCreateFields {
            note_kind: "memory".into(),
            content: "staged source".into(),
            properties: Default::default(),
            tags: Vec::new(),
            salience: None,
            decay_factor: None,
        }),
    });
    let setup_plans =
        prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![staged.clone()]))
            .await
            .expect("setup must admit the staged memory kind");
    assert_eq!(setup_plans.len(), 1);
    drop(setup_plans);
    assert!(rt
        .resolve_by_id_including_deleted(&token, source_id)
        .await
        .unwrap()
        .is_none());
    let mut side = entity_create(side_id, "earlier side op");
    let Op::Create(create) = &mut side else {
        panic!("setup requires an entity create");
    };
    create.namespace = Namespace::parse("other").unwrap();
    let cs = changeset(vec![
        side,
        staged,
        Op::Link(khive_changeset::LinkOp {
            id: id128(edge_id),
            namespace: Namespace::parse("other").unwrap(),
            source: id128(source_id),
            target: id128(target_id),
            relation: khive_types::EdgeRelation::Annotates,
            weight: 1.0,
            properties: Default::default(),
        }),
    ]);
    let error = match prepare_canonical_changeset(rt, &registry(), &token, &cs).await {
        Err(error) => error,
        Ok(_) => panic!("canonical_staged_memory_namespace_mismatch_admitted"),
    };
    let RuntimeError::CanonicalAdmission { op_index, source } = error else {
        panic!("expected canonical namespace admission refusal");
    };
    assert_eq!(op_index, 2);
    let RuntimeError::InvalidInput(message) = *source else {
        panic!("expected the memory namespace rule");
    };
    assert_eq!(
        message,
        "apply_changeset: memory edge source namespace local does not match link operation namespace other; set this link operation's namespace to local and prepare the change-set again"
    );
    assert!(rt.get_edge(&token, edge_id).await.unwrap().is_none());
    assert!(rt
        .resolve_by_id_including_deleted(&token, source_id)
        .await
        .unwrap()
        .is_none());
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(side_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn canonical_memory_to_nonmemory_keeps_op_namespace() {
    let f = fixture();
    let rt = &f.runtime;
    let source_token = rt.authorize(Namespace::local()).unwrap();
    let token = rt.authorize(Namespace::parse("other").unwrap()).unwrap();
    assert_eq!(token.namespace().as_str(), "other");
    let source = Note::new("local", "memory", "source");
    let target = Note::new("other", "observation", "target");
    let (source_id, target_id, edge_id) = (source.id, target.id, Uuid::new_v4());
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(source)
        .await
        .unwrap();
    rt.notes(&token).unwrap().upsert_note(target).await.unwrap();
    for (id, kind, namespace) in [
        (source_id, "memory", "local"),
        (target_id, "observation", "other"),
    ] {
        let stored = rt
            .resolve_by_id_including_deleted(&token, id)
            .await
            .unwrap();
        let Some(Resolved::Note(stored)) = stored else {
            panic!("setup requires a readable stored note");
        };
        assert_eq!(stored.kind, kind);
        assert_eq!(stored.namespace.as_str(), namespace);
        assert!(stored.deleted_at.is_none());
    }
    let cs = changeset(vec![Op::Link(khive_changeset::LinkOp {
        id: id128(edge_id),
        namespace: Namespace::parse("other").unwrap(),
        source: id128(source_id),
        target: id128(target_id),
        relation: khive_types::EdgeRelation::Annotates,
        weight: 1.0,
        properties: Default::default(),
    })]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let edge = rt.get_edge(&token, edge_id).await.unwrap().unwrap();
    assert_eq!(Uuid::from(edge.id), edge_id);
    assert_eq!(edge.source_id, source_id);
    assert_eq!(edge.target_id, target_id);
    assert_eq!(edge.namespace.as_str(), "other");
}

#[tokio::test]
async fn canonical_memory_namespace_change_before_writer_rolls_back() {
    let f = fixture();
    let rt = &f.runtime;
    // The drifted source remains readable, so its namespace may be named.
    let token = rt
        .authorize_with_visibility(Namespace::local(), vec![Namespace::parse("other").unwrap()])
        .unwrap();
    let moved_token = rt.authorize(Namespace::parse("other").unwrap()).unwrap();
    let source = Note::new("local", "memory", "source");
    let target = Note::new("local", "memory", "target");
    let (source_id, target_id, edge_id, side_id) =
        (source.id, target.id, Uuid::new_v4(), Uuid::new_v4());
    rt.notes(&token).unwrap().upsert_note(source).await.unwrap();
    rt.notes(&token).unwrap().upsert_note(target).await.unwrap();
    for id in [source_id, target_id] {
        let stored = rt
            .resolve_by_id_including_deleted(&token, id)
            .await
            .unwrap();
        let Some(Resolved::Note(stored)) = stored else {
            panic!("setup requires a stored note");
        };
        assert_eq!(stored.kind, "memory");
        assert_eq!(stored.namespace.as_str(), "local");
        assert!(stored.deleted_at.is_none());
    }
    let cs = changeset(vec![
        entity_create(side_id, "earlier side op"),
        Op::Link(khive_changeset::LinkOp {
            id: id128(edge_id),
            namespace: Namespace::local(),
            source: id128(source_id),
            target: id128(target_id),
            relation: khive_types::EdgeRelation::Annotates,
            weight: 1.0,
            properties: Default::default(),
        }),
    ]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .expect("equal-namespace prepare must succeed before drift");
    assert_eq!(plans.len(), 2);
    let statement = SqlStatement::new(
        "UPDATE notes SET namespace = ?1 WHERE id = ?2",
        vec![
            SqlValue::Text("other".into()),
            SqlValue::Text(source_id.to_string()),
        ],
    );
    let drift: khive_storage::AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let affected = writer.execute(statement).await?;
            Ok(Box::new(affected) as Box<dyn std::any::Any + Send>)
        })
    });
    let changed = khive_storage::SqlAccess::atomic_unit(rt.sql().as_ref(), drift)
        .await
        .expect("private fixture namespace drift");
    let affected = match changed.downcast::<u64>() {
        Ok(affected) => *affected,
        Err(_) => panic!("namespace drift result type"),
    };
    assert_eq!(affected, 1, "fixture must move exactly one source row");
    let stored = rt
        .resolve_by_id_including_deleted(&moved_token, source_id)
        .await
        .unwrap();
    let Some(Resolved::Note(stored)) = stored else {
        panic!("setup requires the drifted stored source");
    };
    assert_eq!(stored.kind, "memory");
    assert_eq!(stored.namespace.as_str(), "other");
    assert!(stored.deleted_at.is_none());
    let outcome = crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap();
    if matches!(&outcome, crate::AtomicRunOutcome::Committed { .. }) {
        panic!("canonical_memory_writer_namespace_mismatch_admitted");
    }
    let crate::AtomicRunOutcome::RolledBack { failure, .. } = outcome else {
        panic!("expected a canonical writer rollback");
    };
    let AtomicOpFailure::SqlError {
        statement_label,
        message,
    } = failure
    else {
        panic!("expected the canonical namespace SQL failure");
    };
    assert_eq!(statement_label.as_deref(), Some("canonical-materialize"));
    assert_eq!(
        message,
        RuntimeError::InvalidInput(
            "apply_changeset: memory edge source namespace other does not match link operation namespace local; set this link operation's namespace to other and prepare the change-set again".into()
        )
        .to_string()
    );
    assert!(rt.get_edge(&token, edge_id).await.unwrap().is_none());
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(side_id)
        .await
        .unwrap()
        .is_none());
    let stored = rt
        .resolve_by_id_including_deleted(&moved_token, source_id)
        .await
        .unwrap();
    let Some(Resolved::Note(stored)) = stored else {
        panic!("external namespace drift must remain stored");
    };
    assert_eq!(stored.kind, "memory");
    assert_eq!(stored.namespace.as_str(), "other");
}

#[tokio::test]
async fn memory_namespace_refusals_name_operation_and_supported_remedy() {
    let f = fixture();
    let token = f
        .runtime
        .authorize_with_visibility(Namespace::local(), vec![Namespace::parse("other").unwrap()])
        .unwrap();
    let source = Note::new("other", "memory", "source");
    let target = Note::new("target", "memory", "target");
    let nonmemory = Note::new("target", "observation", "nonmemory target");
    assert_eq!(source.kind, "memory");
    assert_eq!(source.namespace.as_str(), "other");
    assert_eq!(target.kind, "memory");
    assert!(source.deleted_at.is_none());
    assert!(target.deleted_at.is_none());
    for (operation, expected) in [
        (
            "restore",
            "restore: memory edge source namespace other does not match stored edge namespace local; leave this tombstone deleted and create a replacement link in namespace=other",
        ),
        (
            "apply_changeset",
            "apply_changeset: memory edge source namespace other does not match link operation namespace local; set this link operation's namespace to other and prepare the change-set again",
        ),
        (
            "link",
            "link: memory edge source namespace other does not match write namespace local; retry with namespace=other",
        ),
    ] {
        assert!(KhiveRuntime::ensure_memory_note_edge_namespace(
            &token,
            operation,
            "other",
            (source.id, target.id),
            khive_types::EdgeRelation::Annotates,
            &source,
            &target,
        )
        .is_ok());
        assert!(KhiveRuntime::ensure_memory_note_edge_namespace(
            &token,
            operation,
            "local",
            (source.id, nonmemory.id),
            khive_types::EdgeRelation::Annotates,
            &source,
            &nonmemory,
        )
        .is_ok());
        let error = KhiveRuntime::ensure_memory_note_edge_namespace(
            &token,
            operation,
            "local",
            (source.id, target.id),
            khive_types::EdgeRelation::Annotates,
            &source,
            &target,
        )
        .unwrap_err();
        let RuntimeError::InvalidInput(message) = error else {
            panic!("namespace refusal must preserve InvalidInput");
        };
        assert_eq!(
            message, expected,
            "memory_namespace_refusal_identifies_operation_and_action"
        );
    }
}

#[tokio::test]
async fn link_refusal_hides_unreadable_memory_source_namespace() {
    let f = fixture();
    let rt = &f.runtime;
    let source_namespace = "guard-private-source";
    let source_token = rt
        .authorize(Namespace::parse(source_namespace).unwrap())
        .unwrap();
    let readable = rt
        .authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse(source_namespace).unwrap()],
        )
        .unwrap();
    let unreadable = rt
        .authorize_with_visibility(Namespace::local(), Vec::new())
        .unwrap();
    assert!(readable
        .visible_namespaces()
        .iter()
        .any(|ns| ns.as_str() == source_namespace));
    assert!(!unreadable
        .visible_namespaces()
        .iter()
        .any(|ns| ns.as_str() == source_namespace));
    let source = Note::new(source_namespace, "memory", "source");
    let target = Note::new(source_namespace, "memory", "target");
    let (source_id, target_id) = (source.id, target.id);
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(source)
        .await
        .unwrap();
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(target)
        .await
        .unwrap();
    for id in [source_id, target_id] {
        let Some(Resolved::Note(stored)) = rt
            .resolve_by_id_including_deleted(&source_token, id)
            .await
            .unwrap()
        else {
            panic!("setup requires a stored memory note");
        };
        assert_eq!(stored.kind, "memory");
        assert_eq!(stored.namespace.as_str(), source_namespace);
        assert!(stored.deleted_at.is_none());
    }
    let edge = rt
        .link(
            &source_token,
            source_id,
            target_id,
            khive_types::EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .expect("matching source namespace must admit the link");
    assert_eq!(edge.namespace.as_str(), source_namespace);
    let RuntimeError::InvalidInput(message) = rt
        .link(
            &readable,
            source_id,
            target_id,
            khive_types::EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap_err()
    else {
        panic!("readable mismatch must preserve InvalidInput");
    };
    assert_eq!(message, format!("link: memory edge source namespace {source_namespace} does not match write namespace local; retry with namespace={source_namespace}"));
    let RuntimeError::InvalidInput(message) = rt
        .link(
            &unreadable,
            source_id,
            target_id,
            khive_types::EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap_err()
    else {
        panic!("unreadable mismatch must preserve InvalidInput");
    };
    assert!(
        !message.contains(source_namespace),
        "link_unreadable_source_namespace_not_disclosed"
    );
    assert!(!message.contains(&source_id.to_string()));
    assert!(!message.contains(&target_id.to_string()));
    assert_eq!(message, "link: memory edge source row is in a namespace the caller cannot read; the edge cannot be written under request namespace local");
    let stored = rt
        .get_edge(&source_token, Uuid::from(edge.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.namespace.as_str(), source_namespace);
    assert_eq!(stored.source_id, source_id);
    assert_eq!(stored.target_id, target_id);
}

#[tokio::test]
async fn canonical_prepare_refusal_hides_unreadable_memory_source_namespace() {
    let f = fixture();
    let rt = &f.runtime;
    let source_namespace = "guard-private-source";
    let source_token = rt
        .authorize(Namespace::parse(source_namespace).unwrap())
        .unwrap();
    let readable = rt
        .authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse(source_namespace).unwrap()],
        )
        .unwrap();
    let unreadable = rt
        .authorize_with_visibility(Namespace::local(), Vec::new())
        .unwrap();
    assert!(readable
        .visible_namespaces()
        .iter()
        .any(|ns| ns.as_str() == source_namespace));
    assert!(!unreadable
        .visible_namespaces()
        .iter()
        .any(|ns| ns.as_str() == source_namespace));
    let source = Note::new(source_namespace, "memory", "source");
    let target = Note::new(source_namespace, "memory", "target");
    let (source_id, target_id, edge_id) = (source.id, target.id, Uuid::new_v4());
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(source)
        .await
        .unwrap();
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(target)
        .await
        .unwrap();
    for id in [source_id, target_id] {
        let Some(Resolved::Note(stored)) = rt
            .resolve_by_id_including_deleted(&source_token, id)
            .await
            .unwrap()
        else {
            panic!("setup requires a stored memory note");
        };
        assert_eq!(stored.kind, "memory");
        assert_eq!(stored.namespace.as_str(), source_namespace);
        assert!(stored.deleted_at.is_none());
    }
    let edge = rt
        .link(
            &source_token,
            source_id,
            target_id,
            khive_types::EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .expect("matching source namespace must admit the fixture relation");
    assert_eq!(edge.namespace.as_str(), source_namespace);
    let cs = changeset(vec![Op::Link(khive_changeset::LinkOp {
        id: id128(edge_id),
        namespace: Namespace::local(),
        source: id128(source_id),
        target: id128(target_id),
        relation: khive_types::EdgeRelation::Annotates,
        weight: 1.0,
        properties: Default::default(),
    })]);
    let visible_error = match prepare_canonical_changeset(rt, &registry(), &readable, &cs).await {
        Err(error) => error,
        Ok(_) => panic!("readable mismatch must refuse during prepare"),
    };
    let RuntimeError::CanonicalAdmission { op_index, source } = visible_error else {
        panic!("readable mismatch must preserve CanonicalAdmission");
    };
    assert_eq!(op_index, 0);
    let RuntimeError::InvalidInput(message) = *source else {
        panic!("readable mismatch must preserve InvalidInput");
    };
    assert_eq!(message, format!("apply_changeset: memory edge source namespace {source_namespace} does not match link operation namespace local; set this link operation's namespace to {source_namespace} and prepare the change-set again"));
    let hidden_error = match prepare_canonical_changeset(rt, &registry(), &unreadable, &cs).await {
        Err(error) => error,
        Ok(_) => panic!("unreadable mismatch must refuse during prepare"),
    };
    let RuntimeError::CanonicalAdmission { op_index, source } = hidden_error else {
        panic!("unreadable mismatch must preserve CanonicalAdmission");
    };
    assert_eq!(op_index, 0);
    let RuntimeError::InvalidInput(message) = *source else {
        panic!("unreadable mismatch must preserve InvalidInput");
    };
    assert!(
        !message.contains(source_namespace),
        "canonical_prepare_unreadable_source_namespace_not_disclosed"
    );
    assert!(!message.contains(&source_id.to_string()));
    assert!(!message.contains(&target_id.to_string()));
    assert_eq!(message, "apply_changeset: memory edge source row is in a namespace the caller cannot read; this link operation cannot be written under link operation namespace local");
    assert!(rt.get_edge(&unreadable, edge_id).await.unwrap().is_none());

    // Prepare with a local source, then move it outside the original caller's
    // readable set. Writer-local diagnostics must use that original scope too.
    let writer_source = Note::new("local", "memory", "writer source");
    let writer_target = Note::new("local", "memory", "writer target");
    let (writer_source_id, writer_target_id, writer_edge_id) =
        (writer_source.id, writer_target.id, Uuid::new_v4());
    rt.notes(&unreadable)
        .unwrap()
        .upsert_note(writer_source)
        .await
        .unwrap();
    rt.notes(&unreadable)
        .unwrap()
        .upsert_note(writer_target)
        .await
        .unwrap();
    let writer_cs = changeset(vec![Op::Link(khive_changeset::LinkOp {
        id: id128(writer_edge_id),
        namespace: Namespace::local(),
        source: id128(writer_source_id),
        target: id128(writer_target_id),
        relation: khive_types::EdgeRelation::Annotates,
        weight: 1.0,
        properties: Default::default(),
    })]);
    let plans = prepare_canonical_changeset(rt, &registry(), &unreadable, &writer_cs)
        .await
        .expect("matching namespace must prepare before writer drift");
    assert_eq!(plans.len(), 1);
    let statement = SqlStatement::new(
        "UPDATE notes SET namespace = ?1 WHERE id = ?2",
        vec![
            SqlValue::Text(source_namespace.into()),
            SqlValue::Text(writer_source_id.to_string()),
        ],
    );
    let drift: khive_storage::AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let affected = writer.execute(statement).await?;
            Ok(Box::new(affected) as Box<dyn std::any::Any + Send>)
        })
    });
    let changed = khive_storage::SqlAccess::atomic_unit(rt.sql().as_ref(), drift)
        .await
        .expect("private fixture namespace drift");
    let affected = match changed.downcast::<u64>() {
        Ok(affected) => *affected,
        Err(_) => panic!("namespace drift result type"),
    };
    assert_eq!(
        affected, 1,
        "fixture must move exactly one writer source row"
    );
    let Some(Resolved::Note(stored)) = rt
        .resolve_by_id_including_deleted(&source_token, writer_source_id)
        .await
        .unwrap()
    else {
        panic!("setup requires the drifted stored memory source");
    };
    assert_eq!(stored.kind, "memory");
    assert_eq!(stored.namespace.as_str(), source_namespace);
    assert!(stored.deleted_at.is_none());
    let outcome = crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap();
    let crate::AtomicRunOutcome::RolledBack { failure, .. } = outcome else {
        panic!("canonical writer namespace mismatch must roll back");
    };
    let AtomicOpFailure::SqlError {
        statement_label,
        message,
    } = failure
    else {
        panic!("writer mismatch must preserve the canonical SQL failure");
    };
    assert_eq!(statement_label.as_deref(), Some("canonical-materialize"));
    assert!(
        !message.contains(source_namespace),
        "canonical_writer_unreadable_source_namespace_not_disclosed"
    );
    assert!(!message.contains(&writer_source_id.to_string()));
    assert!(!message.contains(&writer_target_id.to_string()));
    assert_eq!(message, RuntimeError::InvalidInput("apply_changeset: memory edge source row is in a namespace the caller cannot read; this link operation cannot be written under link operation namespace local".into()).to_string());
    assert!(rt
        .get_edge(&unreadable, writer_edge_id)
        .await
        .unwrap()
        .is_none());
}
