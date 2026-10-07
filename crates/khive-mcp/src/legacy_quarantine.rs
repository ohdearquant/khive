//! Boot repair for quarantine originals published before channel slugs and
//! attachment ownership were installed by #3386.

use std::collections::HashSet;

use anyhow::{anyhow, bail, ensure, Context, Result};
use khive_runtime::{BackendId, KhiveRuntime, StorageBackend};
use khive_storage::attachment::{Attachment, AttachmentSubstrate, NewAttachment};
use khive_storage::note::Note;
use khive_storage::{ContentRef, SqlStatement, SqlValue};
use serde_json::Value;
use uuid::Uuid;

const PAGE_SIZE: i64 = 128;
const ORIGINAL_ROLE: &str = "quarantine-original";

// This predicate is also the read-only census required before a later GC
// epoch can admit these legacy rows. Scan all namespaces: a boot repair is
// not an actor-scoped inbox operation.
const LEGACY_PAGE: &str = "SELECT id FROM notes \
    WHERE id > ?1 AND kind = 'message' AND deleted_at IS NULL \
      AND (json_extract(properties, '$.quarantined') = 'true' \
           OR json_type(properties, '$.quarantined') = 'true') \
      AND (json_type(properties, '$.channel_slug') IS NULL \
           OR json_type(properties, '$.channel_slug') = 'null' \
           OR (json_type(properties, '$.channel_slug') = 'text' \
               AND trim(json_extract(properties, '$.channel_slug')) = '')) \
      AND json_type(properties, '$.quarantine_content_ref') IS NOT NULL \
    ORDER BY id LIMIT ?2";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Census {
    live_originals: u64,
    unowned: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepairReport {
    pub(crate) backends: usize,
    pub(crate) before_live_originals: u64,
    pub(crate) before_unowned: u64,
    pub(crate) inserted: u64,
    pub(crate) after_live_originals: u64,
    pub(crate) after_unowned: u64,
}

#[derive(Clone, Copy)]
enum Mode {
    Inspect,
    Repair,
}

/// Finish the historical ownership repair before a serving registry is
/// exposed. The configured roster may include a former comm backend, while
/// all owner rows must be written through the effective canonical main.
pub(crate) async fn repair_legacy_quarantine(
    main_runtime: &KhiveRuntime,
    sources: &[(&str, &StorageBackend)],
) -> Result<RepairReport> {
    let main = main_runtime.core();
    ensure!(
        main.backend_id().as_str() == BackendId::MAIN,
        "legacy quarantine repair requires the canonical main runtime"
    );
    let mut seen = HashSet::new();
    let unique: Vec<_> = sources
        .iter()
        .copied()
        .filter(|(_, backend)| seen.insert(*backend as *const StorageBackend as usize))
        .collect();
    ensure!(
        unique
            .iter()
            .any(|(_, backend)| std::ptr::eq(*backend, main.backend())),
        "legacy quarantine repair roster omits the effective main backend"
    );

    // Preflight the complete configured roster before the first write. A bad
    // or missing original cannot be hidden by clearing one candidate at a
    // time. A later retry is safe if a writer fails after this pass.
    let (before, _) = scan(&main, &unique, Mode::Inspect).await?;
    if before.unowned > 0 && main.is_read_only() {
        bail!(
            "legacy quarantine repair found {} unowned originals on a read-only main backend",
            before.unowned
        );
    }
    let (_, inserted) = scan(&main, &unique, Mode::Repair).await?;
    let (after, _) = scan(&main, &unique, Mode::Inspect).await?;
    ensure!(
        after.unowned == 0,
        "legacy quarantine repair left {} originals without a matching main attachment",
        after.unowned
    );
    let report = RepairReport {
        backends: unique.len(),
        before_live_originals: before.live_originals,
        before_unowned: before.unowned,
        inserted,
        after_live_originals: after.live_originals,
        after_unowned: after.unowned,
    };
    tracing::info!(
        target: "khive.boot",
        backends = report.backends,
        before_live_originals = report.before_live_originals,
        before_unowned = report.before_unowned,
        inserted = report.inserted,
        after_live_originals = report.after_live_originals,
        after_unowned = report.after_unowned,
        "legacy slugless quarantine original repair"
    );
    if report.inserted > 0 {
        tracing::warn!(
            target: "khive.boot",
            before_live_originals = report.before_live_originals,
            before_unowned = report.before_unowned,
            inserted = report.inserted,
            after_live_originals = report.after_live_originals,
            after_unowned = report.after_unowned,
            "repaired legacy slugless quarantine originals on main"
        );
    }
    Ok(report)
}

async fn scan(
    main: &KhiveRuntime,
    sources: &[(&str, &StorageBackend)],
    mode: Mode,
) -> Result<(Census, u64)> {
    let attachments = main.attachments()?;
    let blob = main.blob_store();
    let mut census = Census::default();
    let mut inserted = 0;
    for &(name, source) in sources {
        let notes = source
            .notes()
            .with_context(|| format!("open notes on {name}"))?;
        let sql = source.sql();
        let mut after = String::new();
        loop {
            let mut reader = sql.reader().await.with_context(|| format!("read {name}"))?;
            let rows = reader
                .query_all(SqlStatement {
                    sql: LEGACY_PAGE.into(),
                    params: vec![SqlValue::Text(after.clone()), SqlValue::Integer(PAGE_SIZE)],
                    label: Some("legacy_slugless_quarantine_page".into()),
                })
                .await
                .with_context(|| format!("scan legacy quarantines on {name}"))?;
            drop(reader);
            if rows.is_empty() {
                break;
            }
            let count = rows.len();
            for row in rows {
                let raw_id = match row.get("id") {
                    Some(SqlValue::Text(id)) => id.clone(),
                    other => {
                        bail!("legacy quarantine scan on {name} returned invalid id: {other:?}")
                    }
                };
                after = raw_id.clone();
                let id = Uuid::parse_str(&raw_id)
                    .with_context(|| format!("legacy quarantine note id on {name}"))?;
                let note = notes.get_note(id).await?.ok_or_else(|| {
                    anyhow!("legacy quarantine {id} on {name} vanished during census")
                })?;
                ensure!(
                    is_legacy_original(&note),
                    "legacy quarantine {id} on {name} changed during census"
                );
                census.live_originals += 1;
                let raw_ref = note
                    .properties
                    .as_ref()
                    .and_then(|properties| properties.get("quarantine_content_ref"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!("legacy quarantine {id} on {name} has a non-text original ref")
                    })?;
                let content_ref = ContentRef::from_hex(raw_ref.to_string()).map_err(|error| {
                    anyhow!("legacy quarantine {id} on {name} has an invalid original ref: {error}")
                })?;
                let published = blob.as_deref().ok_or_else(|| {
                    anyhow!("legacy quarantine {id} on {name} requires a configured BlobStore")
                })?;
                ensure!(
                    published.exists(&content_ref).await?,
                    "legacy quarantine {id} on {name} original {content_ref} is not published"
                );
                match attachments.get_attachment(id, ORIGINAL_ROLE).await? {
                    Some(owner) => verify_owner(&owner, &content_ref, id, name)?,
                    None => {
                        census.unowned += 1;
                        if matches!(mode, Mode::Repair) {
                            let owner = Attachment::from_new(
                                id,
                                AttachmentSubstrate::Note,
                                NewAttachment {
                                    role: ORIGINAL_ROLE.into(),
                                    content_ref: content_ref.clone(),
                                    media_type: None,
                                    size_bytes: None,
                                },
                                note.created_at,
                            );
                            if attachments.try_insert_attachment(owner).await? {
                                inserted += 1;
                            } else {
                                let current = attachments
                                    .get_attachment(id, ORIGINAL_ROLE)
                                    .await?
                                    .ok_or_else(|| anyhow!("legacy quarantine {id} on {name} owner disappeared during repair"))?;
                                verify_owner(&current, &content_ref, id, name)?;
                            }
                        }
                    }
                }
            }
            if count < PAGE_SIZE as usize {
                break;
            }
        }
    }
    Ok((census, inserted))
}

fn is_legacy_original(note: &Note) -> bool {
    if note.kind != "message" || note.deleted_at.is_some() {
        return false;
    }
    let Some(properties) = note.properties.as_ref() else {
        return false;
    };
    let quarantined = properties.get("quarantined") == Some(&Value::Bool(true))
        || properties.get("quarantined").and_then(Value::as_str) == Some("true");
    let slugless = match properties.get("channel_slug") {
        None | Some(Value::Null) => true,
        Some(Value::String(slug)) => slug.trim().is_empty(),
        _ => false,
    };
    quarantined && slugless && properties.get("quarantine_content_ref").is_some()
}

fn verify_owner(owner: &Attachment, expected: &ContentRef, id: Uuid, name: &str) -> Result<()> {
    ensure!(
        owner.substrate == AttachmentSubstrate::Note && &owner.content_ref == expected,
        "legacy quarantine {id} on {name} has a mismatched main quarantine-original attachment"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use khive_db::stores::blob::FsBlobStore;
    use khive_runtime::{
        BackendConfig, BackendKind, BlobConfig, KhiveConfig, PackConfig, RuntimeConfig,
        StorageSectionConfig,
    };
    use khive_storage::note::Note;
    use khive_storage::BlobStore;
    use serde_json::json;

    async fn fixture() -> (tempfile::TempDir, KhiveRuntime, Arc<dyn BlobStore>) {
        let root = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStore> =
            Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).unwrap());
        let runtime = KhiveRuntime::memory().unwrap();
        runtime.install_blob_store(Arc::clone(&blob)).unwrap();
        (root, runtime, blob)
    }

    async fn seed(source: &StorageBackend, namespace: &str, properties: Value, due: bool) -> Uuid {
        let mut note =
            Note::new(namespace, "message", "quarantined notification").with_properties(properties);
        if due {
            note.expires_at = Some(note.created_at - 1);
        }
        let id = note.id;
        assert!(source.notes().unwrap().try_insert_note(note).await.unwrap());
        id
    }

    #[tokio::test]
    async fn repairs_published_slugless_originals_on_main_and_is_idempotent() {
        let (_root, runtime, blob) = fixture().await;
        let original = blob.put(b"legacy original".to_vec()).await.unwrap();
        let id = seed(
            runtime.backend(),
            "mail-archive",
            json!({"quarantined": "true", "channel_kind": "email", "quarantine_content_ref": original.to_string()}),
            true,
        )
        .await;
        let named = seed(
            runtime.backend(),
            "mail-archive",
            json!({"quarantined": true, "channel_slug": "inbox", "quarantine_content_ref": original.to_string()}),
            false,
        )
        .await;
        let first = repair_legacy_quarantine(&runtime, &[("main", runtime.backend())])
            .await
            .unwrap();
        assert_eq!(first.before_live_originals, 1);
        assert_eq!(first.before_unowned, 1);
        assert_eq!(first.inserted, 1);
        assert_eq!(first.after_unowned, 0);
        let owner = runtime
            .attachments()
            .unwrap()
            .get_attachment(id, ORIGINAL_ROLE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.content_ref, original);
        assert_eq!(owner.substrate, AttachmentSubstrate::Note);
        assert!(
            runtime
                .backend()
                .notes()
                .unwrap()
                .get_note(id)
                .await
                .unwrap()
                .is_some(),
            "even a due note is preserved"
        );
        assert!(runtime
            .attachments()
            .unwrap()
            .get_attachment(named, ORIGINAL_ROLE)
            .await
            .unwrap()
            .is_none());
        let second = repair_legacy_quarantine(&runtime, &[("main", runtime.backend())])
            .await
            .unwrap();
        assert_eq!(second.before_unowned, 0);
        assert_eq!(second.inserted, 0);
        assert_eq!(second.after_unowned, 0);
    }

    #[tokio::test]
    async fn repairs_secondary_note_only_in_main_attachment_store() {
        let (_root, runtime, blob) = fixture().await;
        let secondary = StorageBackend::memory().unwrap();
        secondary.prepare_core_schema().unwrap();
        let original = blob
            .put(b"secondary legacy original".to_vec())
            .await
            .unwrap();
        let id = seed(
            &secondary,
            "other-namespace",
            json!({"quarantined": true, "quarantine_content_ref": original.to_string()}),
            false,
        )
        .await;
        let report = repair_legacy_quarantine(
            &runtime,
            &[("main", runtime.backend()), ("old-comm", &secondary)],
        )
        .await
        .unwrap();
        assert_eq!(report.backends, 2);
        assert_eq!(report.inserted, 1);
        assert_eq!(report.after_unowned, 0);
        assert_eq!(
            runtime
                .attachments()
                .unwrap()
                .get_attachment(id, ORIGINAL_ROLE)
                .await
                .unwrap()
                .unwrap()
                .content_ref,
            original
        );
        assert!(secondary
            .attachments()
            .unwrap()
            .get_attachment(id, ORIGINAL_ROLE)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn invalid_or_missing_original_ref_fails_before_any_repair() {
        let (_root, runtime, blob) = fixture().await;
        let published = blob.put(b"good original".to_vec()).await.unwrap();
        let good = seed(
            runtime.backend(),
            "local",
            json!({"quarantined": true, "quarantine_content_ref": published.to_string()}),
            false,
        )
        .await;
        let malformed = seed(
            runtime.backend(),
            "local",
            json!({"quarantined": true, "quarantine_content_ref": "wrong"}),
            false,
        )
        .await;
        let error = repair_legacy_quarantine(&runtime, &[("main", runtime.backend())])
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("invalid original ref"),
            "{error}"
        );
        assert!(runtime
            .attachments()
            .unwrap()
            .get_attachment(good, ORIGINAL_ROLE)
            .await
            .unwrap()
            .is_none());
        assert!(runtime
            .backend()
            .notes()
            .unwrap()
            .get_note(malformed)
            .await
            .unwrap()
            .is_some());

        runtime
            .backend()
            .notes()
            .unwrap()
            .delete_note(malformed, khive_storage::DeleteMode::Hard)
            .await
            .unwrap();
        let missing = seed(
            runtime.backend(),
            "local",
            json!({"quarantined": true, "quarantine_content_ref": "0".repeat(64)}),
            false,
        )
        .await;
        let error = repair_legacy_quarantine(&runtime, &[("main", runtime.backend())])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not published"), "{error}");
        assert!(runtime
            .attachments()
            .unwrap()
            .get_attachment(good, ORIGINAL_ROLE)
            .await
            .unwrap()
            .is_none());
        assert!(runtime
            .backend()
            .notes()
            .unwrap()
            .get_note(missing)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn conflicting_main_role_refuses_repair() {
        let (_root, runtime, blob) = fixture().await;
        let original = blob.put(b"expected".to_vec()).await.unwrap();
        let other = blob.put(b"different".to_vec()).await.unwrap();
        let id = seed(
            runtime.backend(),
            "local",
            json!({"quarantined": true, "quarantine_content_ref": original.to_string()}),
            false,
        )
        .await;
        runtime
            .attachments()
            .unwrap()
            .upsert_attachment(Attachment::from_new(
                id,
                AttachmentSubstrate::Note,
                NewAttachment {
                    role: ORIGINAL_ROLE.into(),
                    content_ref: other,
                    media_type: None,
                    size_bytes: None,
                },
                1,
            ))
            .await
            .unwrap();
        let error = repair_legacy_quarantine(&runtime, &[("main", runtime.backend())])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mismatched main"), "{error}");
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn single_backend_boot_repairs_before_returning_the_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let config = RuntimeConfig {
            db_path: Some(dir.path().join("main.db")),
            packs: vec!["kg".into(), "comm".into(), "blob".into()],
            ..RuntimeConfig::no_embeddings()
        };
        let khive_cfg = KhiveConfig {
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::Fs {
                    root: Some(dir.path().join("blobs").display().to_string()),
                    floor_bytes: Some(0),
                }),
            },
            ..KhiveConfig::default()
        };
        let runtime = crate::serve::build_single_backend_runtime(config.clone(), &khive_cfg)
            .await
            .unwrap();
        let blob = runtime.blob_store().unwrap();
        let original = blob.put(b"before boot upgrade".to_vec()).await.unwrap();
        let id = seed(
            runtime.backend(),
            "archived-mail",
            json!({"quarantined": true, "channel_kind": "email", "quarantine_content_ref": original.to_string()}),
            false,
        )
        .await;
        assert!(runtime
            .attachments()
            .unwrap()
            .get_attachment(id, ORIGINAL_ROLE)
            .await
            .unwrap()
            .is_none());
        drop(blob);
        drop(runtime);

        let reopened = crate::serve::build_single_backend_runtime(config, &khive_cfg)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .attachments()
                .unwrap()
                .get_attachment(id, ORIGINAL_ROLE)
                .await
                .unwrap()
                .unwrap()
                .content_ref,
            original
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn multi_backend_boot_repairs_old_comm_backend_on_main() {
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main.db");
        let comm_path = dir.path().join("comm.db");
        let khive_cfg = KhiveConfig {
            backends: [(&main_path, "main"), (&comm_path, "old-comm")]
                .into_iter()
                .map(|(path, name)| BackendConfig {
                    name: name.into(),
                    kind: BackendKind::Sqlite,
                    path: Some(path.to_path_buf()),
                    cache_mb: None,
                    journal_mode: None,
                    wal_ceiling_bytes: None,
                    disk_reserve_bytes: None,
                    disk_guard_deadline_ms: None,
                    served_kinds: None,
                    read_only: false,
                })
                .collect(),
            packs: [(
                "comm".into(),
                PackConfig {
                    backend: "old-comm".into(),
                    no_embed: false,
                },
            )]
            .into_iter()
            .collect(),
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::Fs {
                    root: Some(dir.path().join("blobs").display().to_string()),
                    floor_bytes: Some(0),
                }),
            },
            ..KhiveConfig::default()
        };
        let config = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(None),
            packs: vec!["kg".into(), "comm".into(), "blob".into()],
            ..RuntimeConfig::no_embeddings()
        };
        let first =
            crate::serve::build_registry_for_multi_backend(config.clone(), &khive_cfg, None)
                .await
                .unwrap();
        let original = first
            .default_runtime
            .blob_store()
            .unwrap()
            .put(b"old routed mail".to_vec())
            .await
            .unwrap();
        let id = seed(
            first.per_pack_runtimes["comm"].backend(),
            "routed-namespace",
            json!({"quarantined": true, "channel_kind": "email", "quarantine_content_ref": original.to_string()}),
            false,
        )
        .await;
        let retry_id = seed(
            first.per_pack_runtimes["comm"].backend(),
            "routed-namespace",
            json!({"quarantined": true, "channel_kind": "email", "quarantine_content_ref": original.to_string()}),
            false,
        )
        .await;
        drop(first);

        let second = crate::serve::build_registry_for_multi_backend(config, &khive_cfg, None)
            .await
            .unwrap();
        assert_eq!(
            second
                .default_runtime
                .attachments()
                .unwrap()
                .get_attachment(id, ORIGINAL_ROLE)
                .await
                .unwrap()
                .unwrap()
                .content_ref,
            original
        );
        assert!(second.per_pack_runtimes["comm"]
            .backend()
            .attachments()
            .unwrap()
            .get_attachment(id, ORIGINAL_ROLE)
            .await
            .unwrap()
            .is_none());
        assert!(second
            .default_runtime
            .attachments()
            .unwrap()
            .get_attachment(retry_id, ORIGINAL_ROLE)
            .await
            .unwrap()
            .is_some());

        let comm = &second.per_pack_runtimes["comm"];
        let token = comm
            .authorize(khive_runtime::Namespace::parse("routed-namespace").unwrap())
            .unwrap();
        let notes = comm.notes(&token).unwrap();
        let as_of = notes
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
            .created_at
            .max(notes.get_note(retry_id).await.unwrap().unwrap().created_at)
            + 14 * 24 * 60 * 60 * 1_000_000
            + 1;
        // A historical operator-soft-deleted note remains eligible for the
        // hard-delete pass and its targeted canonical-main owner detach.
        assert!(notes
            .delete_note(retry_id, khive_storage::DeleteMode::Soft)
            .await
            .unwrap());
        assert!(notes.get_note(retry_id).await.unwrap().is_none());
        assert!(notes
            .get_note_including_deleted(retry_id)
            .await
            .unwrap()
            .is_some());
        let cleaned = second
            .registry
            .dispatch(
                "comm.cleanup_expired_quarantine",
                json!({
                    "namespace": "routed-namespace",
                    "channel_kind": "email",
                    "channel_slug": "",
                    "mode": "legacy_slugless",
                    "as_of_micros": as_of,
                }),
            )
            .await
            .unwrap();
        assert_eq!(cleaned["deleted"], 2);
        assert_eq!(cleaned["routed_owners_detached"], 2);
        for note_id in [id, retry_id] {
            assert!(notes
                .get_note_including_deleted(note_id)
                .await
                .unwrap()
                .is_none());
            assert!(
                second
                    .default_runtime
                    .attachments()
                    .unwrap()
                    .get_attachment(note_id, ORIGINAL_ROLE)
                    .await
                    .unwrap()
                    .is_none(),
                "main owner must be detached for a routed note"
            );
        }

        // A wrong main owner makes the targeted detach refuse after the
        // note's hard delete. The error reports one possible owner residue
        // for the operator's ownerless-row inspection path (#3178).
        let mismatch_id = seed(
            comm.backend(),
            "routed-namespace",
            json!({"quarantined": true, "channel_kind": "email", "quarantine_content_ref": original.to_string()}),
            true,
        )
        .await;
        let wrong_ref = second
            .default_runtime
            .blob_store()
            .unwrap()
            .put(b"different original".to_vec())
            .await
            .unwrap();
        let wrong_owner = Attachment::from_new(
            mismatch_id,
            AttachmentSubstrate::Note,
            NewAttachment {
                role: ORIGINAL_ROLE.into(),
                content_ref: wrong_ref.clone(),
                media_type: None,
                size_bytes: None,
            },
            as_of,
        );
        assert!(second
            .default_runtime
            .attachments()
            .unwrap()
            .try_insert_attachment(wrong_owner)
            .await
            .unwrap());
        let error = second
            .registry
            .dispatch(
                "comm.cleanup_expired_quarantine",
                json!({
                    "namespace": "routed-namespace",
                    "channel_kind": "email",
                    "channel_slug": "",
                    "mode": "legacy_slugless",
                    "as_of_micros": as_of,
                }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("possible_owner_residue=1"), "{error}");
        assert!(
            notes
                .get_note_including_deleted(mismatch_id)
                .await
                .unwrap()
                .is_none(),
            "note hard-delete must precede owner detachment"
        );
        assert_eq!(
            second
                .default_runtime
                .attachments()
                .unwrap()
                .get_attachment(mismatch_id, ORIGINAL_ROLE)
                .await
                .unwrap()
                .unwrap()
                .content_ref,
            wrong_ref,
            "a mismatched original remains for operator investigation"
        );
    }
}
