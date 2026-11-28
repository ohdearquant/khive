use super::*;
use crate::portability::{ExportedEntity, ImportConflictMode, ImportOptions, KgArchive};
use crate::{EntityAliasChange, KeyedEntityCreate, Namespace};
use khive_storage::entity::EntityAliasOrigin;
use khive_types::ImportKindPolicy;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().unwrap()
}

fn tok(namespace: &str) -> NamespaceToken {
    NamespaceToken::for_namespace(Namespace::parse(namespace).unwrap())
}

fn alias(alias: &str, origin: EntityAliasOrigin) -> EntityAlias {
    EntityAlias {
        alias: alias.to_owned(),
        origin,
    }
}

fn record(
    id: Uuid,
    kind: &str,
    name: &str,
    keys: &[&str],
    aliases: Vec<EntityAlias>,
) -> ExportedEntity {
    let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    ExportedEntity {
        id,
        kind: kind.to_owned(),
        entity_type: None,
        name: name.to_owned(),
        description: None,
        properties: None,
        tags: Vec::new(),
        keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        aliases,
        created_at: at,
        updated_at: at,
    }
}

fn archive(namespace: &str, entities: Vec<ExportedEntity>) -> KgArchive {
    KgArchive {
        format: "khive-kg".to_owned(),
        version: "0.1".to_owned(),
        namespace: namespace.to_owned(),
        exported_at: chrono::Utc::now(),
        entities,
        edges: Vec::new(),
    }
}

fn options(on_conflict: ImportConflictMode) -> ImportOptions {
    ImportOptions {
        on_conflict,
        force: false,
        dry_run: false,
    }
}

async fn keyed(
    rt: &KhiveRuntime,
    tok: &NamespaceToken,
    key: &str,
    name: &str,
    aliases: &[&str],
) -> Entity {
    let aliases: Vec<String> = aliases.iter().map(|alias| (*alias).to_owned()).collect();
    match rt
        .create_keyed_entity_with_post_commit_report(
            tok,
            "concept",
            key,
            None,
            name,
            None,
            None,
            Vec::new(),
            &EntityAliasChange::replace(&aliases).unwrap(),
        )
        .await
        .unwrap()
    {
        KeyedEntityCreate::Created { entity, .. } => entity,
        other => panic!("expected a fresh keyed create, got {other:?}"),
    }
}

async fn holder(rt: &KhiveRuntime, tok: &NamespaceToken, key: &str) -> Option<Uuid> {
    rt.get_entity_by_key(tok, "concept", key)
        .await
        .unwrap()
        .map(|entity| entity.id)
}

async fn exported(rt: &KhiveRuntime, tok: &NamespaceToken, id: Uuid) -> ExportedEntity {
    rt.export_kg(tok)
        .await
        .unwrap()
        .entities
        .into_iter()
        .find(|entity| entity.id == id)
        .expect("the entity is exported")
}

fn detail<'a>(error: &'a RuntimeError, name: &str) -> Option<&'a str> {
    match error {
        RuntimeError::Khive(error) => error.details().and_then(|details| details.get(name)),
        _ => None,
    }
}

#[tokio::test]
async fn export_carries_held_keys_and_aliases_and_an_empty_store_takes_them_back() {
    let source = rt();
    let tok = tok("archive-names-round-trip");
    let entity = keyed(
        &source,
        &tok,
        "paper:smith2024",
        "Smith 2024",
        &["Smith et al."],
    )
    .await;
    let carried = exported(&source, &tok, entity.id).await;
    assert_eq!(carried.keys, vec!["paper:smith2024".to_owned()]);
    assert_eq!(
        carried.aliases,
        vec![alias("Smith et al.", EntityAliasOrigin::Caller)]
    );
    let wire = serde_json::to_value(&carried).unwrap();
    assert_eq!(wire["keys"], serde_json::json!(["paper:smith2024"]));
    assert_eq!(
        wire["aliases"],
        serde_json::json!([{"alias": "Smith et al.", "origin": "caller"}])
    );
    let archive = source.export_kg(&tok).await.unwrap();

    let legacy = rt();
    legacy.import_kg(&archive, &tok).await.unwrap();
    assert_eq!(
        holder(&legacy, &tok, "paper:smith2024").await,
        Some(entity.id)
    );
    let back = exported(&legacy, &tok, entity.id).await;
    assert_eq!(
        (back.keys, back.aliases),
        (carried.keys.clone(), carried.aliases.clone())
    );

    let policy = rt();
    policy
        .import_kg_with_options(
            &archive,
            &tok,
            ImportKindPolicy::Strict,
            options(ImportConflictMode::Error),
        )
        .await
        .unwrap();
    assert_eq!(
        holder(&policy, &tok, "paper:smith2024").await,
        Some(entity.id)
    );
    let back = exported(&policy, &tok, entity.id).await;
    assert_eq!((back.keys, back.aliases), (carried.keys, carried.aliases));
}

#[tokio::test]
async fn an_entity_holding_no_keys_or_aliases_exports_neither_field() {
    let rt = rt();
    let tok = tok("archive-names-absent");
    let id = Uuid::new_v4();
    rt.import_kg(
        &archive(
            "archive-names-absent",
            vec![record(id, "concept", "Plain", &[], Vec::new())],
        ),
        &tok,
    )
    .await
    .unwrap();
    let wire = serde_json::to_value(exported(&rt, &tok, id).await).unwrap();
    assert!(wire.get("keys").is_none(), "{wire}");
    assert!(wire.get("aliases").is_none(), "{wire}");
}

#[tokio::test]
async fn two_archive_entities_carrying_one_key_refuse_before_any_write() {
    let tok = tok("archive-names-duplicate");
    let duplicate = archive(
        "archive-names-duplicate",
        vec![
            record(Uuid::new_v4(), "concept", "First", &["k:1"], Vec::new()),
            record(Uuid::new_v4(), "concept", "Second", &["k:1"], Vec::new()),
        ],
    );
    let rt = rt();
    let error = rt.import_kg(&duplicate, &tok).await.unwrap_err();
    assert!(error.to_string().contains("both carry"), "{error}");
    for mode in [
        ImportConflictMode::Error,
        ImportConflictMode::Skip,
        ImportConflictMode::Update,
    ] {
        let error = rt
            .import_kg_with_options(&duplicate, &tok, ImportKindPolicy::Strict, options(mode))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("both carry"),
            "{mode:?}: {error}"
        );
    }
    assert!(rt.export_kg(&tok).await.unwrap().entities.is_empty());

    // A key names one entity of its kind: the same key under two kinds is
    // two keys.
    let mut scoped = duplicate.clone();
    scoped.entities[1].kind = "person".to_owned();
    rt.import_kg(&scoped, &tok).await.unwrap();
    assert_eq!(holder(&rt, &tok, "k:1").await, Some(scoped.entities[0].id));
}

#[tokio::test]
async fn a_key_another_live_entity_holds_refuses_without_naming_the_holder() {
    let rt = rt();
    let tok = tok("archive-names-held");
    let stored = keyed(&rt, &tok, "k:held", "Holder", &[]).await;
    let incoming = Uuid::new_v4();
    let carrying = archive(
        "archive-names-held",
        vec![record(
            incoming,
            "concept",
            "Incoming",
            &["k:held"],
            Vec::new(),
        )],
    );
    let mut refusals = vec![rt.import_kg(&carrying, &tok).await.unwrap_err()];
    for mode in [ImportConflictMode::Error, ImportConflictMode::Update] {
        refusals.push(
            rt.import_kg_with_options(&carrying, &tok, ImportKindPolicy::Strict, options(mode))
                .await
                .unwrap_err(),
        );
    }
    for error in &refusals {
        assert_eq!(
            detail(error, "reason"),
            Some("import_key_conflict"),
            "{error}"
        );
        assert_eq!(
            detail(error, "entity_id"),
            Some(incoming.to_string().as_str())
        );
        assert_eq!(detail(error, "key"), Some("k:held"));
        assert_eq!(detail(error, "existing_id"), None);
        assert!(
            !error.to_string().contains(&stored.id.to_string()),
            "{error}"
        );
    }
    assert!(rt.get_entity(&tok, incoming).await.is_err());

    let summary = rt
        .import_kg_with_options(
            &carrying,
            &tok,
            ImportKindPolicy::Strict,
            options(ImportConflictMode::Skip),
        )
        .await
        .unwrap();
    assert_eq!(summary.entities_skipped, 1);
    assert_eq!(summary.entities_key_conflict_skipped, 1);
    assert_eq!(summary.entities_inserted, 0);
    assert!(rt.get_entity(&tok, incoming).await.is_err());
    assert_eq!(holder(&rt, &tok, "k:held").await, Some(stored.id));
}

#[tokio::test]
async fn a_key_whose_only_holder_is_deleted_is_free() {
    let rt = rt();
    let tok = tok("archive-names-released");
    let stored = keyed(&rt, &tok, "k:released", "Former holder", &[]).await;
    assert!(rt.delete_entity(&tok, stored.id, false).await.unwrap());
    let incoming = Uuid::new_v4();
    rt.import_kg(
        &archive(
            "archive-names-released",
            vec![record(
                incoming,
                "concept",
                "New holder",
                &["k:released"],
                Vec::new(),
            )],
        ),
        &tok,
    )
    .await
    .unwrap();
    assert_eq!(holder(&rt, &tok, "k:released").await, Some(incoming));
}

#[tokio::test]
async fn importing_a_deleted_entity_with_its_own_released_key_restores_both() {
    let rt = rt();
    let tok = tok("archive-names-restore");
    let stored = keyed(&rt, &tok, "k:own", "Returning", &[]).await;
    assert!(rt.delete_entity(&tok, stored.id, false).await.unwrap());
    rt.import_kg(
        &archive(
            "archive-names-restore",
            vec![record(
                stored.id,
                "concept",
                "Returning",
                &["k:own"],
                Vec::new(),
            )],
        ),
        &tok,
    )
    .await
    .unwrap();
    assert_eq!(holder(&rt, &tok, "k:own").await, Some(stored.id));
}

#[tokio::test]
async fn importing_over_a_stored_entity_adds_keys_and_aliases_and_removes_none() {
    for mode in [None, Some(ImportConflictMode::Update)] {
        let rt = rt();
        let tok = tok("archive-names-add");
        let stored = keyed(&rt, &tok, "k:a", "Alpha", &["First"]).await;
        let carrying = archive(
            "archive-names-add",
            vec![record(
                stored.id,
                "concept",
                "Alpha",
                &["k:b"],
                vec![alias("Second", EntityAliasOrigin::Merge)],
            )],
        );
        match mode {
            None => rt.import_kg(&carrying, &tok).await.map(|_| ()),
            Some(mode) => rt
                .import_kg_with_options(&carrying, &tok, ImportKindPolicy::Strict, options(mode))
                .await
                .map(|_| ()),
        }
        .unwrap();
        let back = exported(&rt, &tok, stored.id).await;
        assert_eq!(
            back.keys,
            vec!["k:a".to_owned(), "k:b".to_owned()],
            "{mode:?}"
        );
        assert_eq!(
            back.aliases,
            vec![
                alias("First", EntityAliasOrigin::Caller),
                alias("Second", EntityAliasOrigin::Merge),
            ],
            "{mode:?}"
        );
    }
}

#[tokio::test]
async fn an_alias_equal_to_the_imported_name_is_not_stored() {
    for mode in [None, Some(ImportConflictMode::Update)] {
        let rt = rt();
        let tok = tok("archive-names-onto");
        let stored = keyed(&rt, &tok, "k:a", "Alpha", &["Beta"]).await;
        let carrying = archive(
            "archive-names-onto",
            vec![record(stored.id, "concept", "Beta", &["k:a"], Vec::new())],
        );
        match mode {
            None => rt.import_kg(&carrying, &tok).await.map(|_| ()),
            Some(mode) => rt
                .import_kg_with_options(&carrying, &tok, ImportKindPolicy::Strict, options(mode))
                .await
                .map(|_| ()),
        }
        .unwrap();
        let back = exported(&rt, &tok, stored.id).await;
        assert_eq!(back.name, "Beta");
        assert!(
            !back.aliases.iter().any(|held| held.alias == "Beta"),
            "{mode:?}: {:?}",
            back.aliases
        );
    }
}

#[tokio::test]
async fn export_refuses_a_key_held_under_a_kind_other_than_its_holders() {
    let rt = rt();
    let tok = tok("archive-names-kind");
    let stored = keyed(&rt, &tok, "k:a", "Alpha", &[]).await;
    rt.export_kg(&tok).await.unwrap();
    rt.sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "UPDATE entity_keys SET kind = 'person' WHERE entity_id = ?1",
            vec![SqlValue::Text(stored.id.to_string())],
        ))
        .await
        .unwrap();
    let error = rt.export_kg(&tok).await.unwrap_err();
    assert!(error.to_string().contains("not its own kind"), "{error}");
}

#[tokio::test]
async fn a_legacy_import_may_not_change_the_kind_of_an_entity_holding_keys() {
    let rt = rt();
    let tok = tok("archive-names-rekind");
    let stored = keyed(&rt, &tok, "k:a", "Alpha", &[]).await;
    let error = rt
        .import_kg(
            &archive(
                "archive-names-rekind",
                vec![record(stored.id, "person", "Alpha", &[], Vec::new())],
            ),
            &tok,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("changes the kind"), "{error}");
    assert_eq!(
        rt.get_entity(&tok, stored.id).await.unwrap().kind,
        "concept"
    );

    // An entity holding no keys still takes the archive's kind.
    let plain = Uuid::new_v4();
    rt.import_kg(
        &archive(
            "archive-names-rekind",
            vec![record(plain, "concept", "Plain", &[], Vec::new())],
        ),
        &tok,
    )
    .await
    .unwrap();
    rt.import_kg(
        &archive(
            "archive-names-rekind",
            vec![record(plain, "person", "Plain", &[], Vec::new())],
        ),
        &tok,
    )
    .await
    .unwrap();
    assert_eq!(rt.get_entity(&tok, plain).await.unwrap().kind, "person");
}

#[test]
fn archive_entity_names_are_validated_before_any_read() {
    let id = Uuid::new_v4();
    let caller = EntityAliasOrigin::Caller;
    validate_entity_names(id, "Alpha", &["k:a".to_owned()], &[alias("Beta", caller)]).unwrap();
    let own_name = validate_entity_names(id, "Alpha", &[], &[alias("Alpha", caller)]).unwrap_err();
    assert!(own_name.to_string().contains("own name"), "{own_name}");
    assert!(!own_name.to_string().contains("Alpha"), "{own_name}");
    let twice = ["k:a".to_owned(), "k:a".to_owned()];
    assert!(validate_entity_names(id, "Alpha", &twice, &[]).is_err());
    let both_origins = [
        alias("Beta", caller),
        alias("Beta", EntityAliasOrigin::Rename),
    ];
    assert!(validate_entity_names(id, "Alpha", &[], &both_origins).is_err());
    assert!(validate_entity_names(id, "Alpha", &[], &[alias("", caller)]).is_err());
    assert!(validate_entity_names(id, "Alpha", &["k\0".to_owned()], &[]).is_err());
}
