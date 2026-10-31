//! Tests for the persisted outbound source of the node module.
//!
//! STATUS: written without compiling or running (cargo is not available for this change).
//! Every API used here was read from the tree, but nothing has been type-checked.

use super::source::RuntimeOutboundSource;
use khive_channel_node::encoding::{
    Base64Bytes, CanonicalUuid, Epoch, HexBytes, NodeAddress, Realm,
};
use khive_channel_node::envelope::Ciphertext;
use khive_channel_node::source::{
    NodeClientBinding, OutboundSource, PersistedSubmission, SourceError,
};
use khive_db::stores::note::transport::{EnvelopeKey, SenderAssurance, SenderEnvelope};
use khive_runtime::comm_transport::{FailureClass, HoldReason};
use khive_runtime::{AllowAllGate, BackendId, KhiveRuntime, RuntimeConfig};
use khive_storage::SqlStatement;
use std::sync::Arc;
use uuid::Uuid;

const NS: &str = "local";
const OTHER_NS: &str = "other";
const SLUG: &str = "test";
const CREDENTIAL: &str = "test-credential";
const RECIPIENT_DEVICE: u128 = 9;

const MATCH: u128 = 0x100;
const FAILED: u128 = 0x200;
const FOREIGN_NAMESPACE: u128 = 0x300;
const FOREIGN_KIND: u128 = 0x400;
const FOREIGN_SLUG: u128 = 0x500;
const SENDER_EPOCH_MISMATCH: u128 = 0x600;
const CREDENTIAL_MISMATCH: u128 = 0x610;
const MISSING: u128 = 0x700;

fn runtime_for(backend: Arc<khive_db::StorageBackend>, namespace: &str) -> KhiveRuntime {
    KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            exec: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: khive_runtime::Namespace::parse(namespace).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: Some("actor:node-source-test".to_string()),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        },
    )
}

fn token_for(runtime: &KhiveRuntime, namespace: &str) -> khive_runtime::NamespaceToken {
    runtime
        .authorize(khive_types::Namespace::parse(namespace).unwrap())
        .expect("authorize the fixture namespace")
}

fn realm() -> Realm {
    Realm::parse("realm.test").expect("realm")
}

fn binding() -> NodeClientBinding {
    NodeClientBinding {
        namespace: NS.to_string(),
        slug: SLUG.to_string(),
        realm: realm(),
        agent: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        device: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
        key_epoch: Epoch::new(1).expect("epoch"),
        key_reference: CREDENTIAL.to_string(),
    }
}

fn recipient_address() -> NodeAddress {
    NodeAddress::new(realm(), CanonicalUuid::from_uuid(Uuid::from_u128(2)))
}

/// A stored envelope that matches the binding above unless a field is changed by the caller.
fn envelope(logical: u128, kind: &str, slug: &str, recipient_key_epoch: u64) -> SenderEnvelope {
    SenderEnvelope {
        namespace: NS.to_string(),
        logical_message_id: Uuid::from_u128(logical),
        outbound_note_id: Uuid::from_u128(logical + 0x1_0000),
        kind: kind.to_string(),
        slug: slug.to_string(),
        credential_ref: CREDENTIAL.to_string(),
        recipient_address: recipient_address().to_string(),
        protocol_version: 1,
        sender_agent_id: Uuid::from_u128(1).to_string(),
        sender_assurance: SenderAssurance::Claimed,
        recipient_agent_id: Uuid::from_u128(2).to_string(),
        recipient_device_id: Uuid::from_u128(RECIPIENT_DEVICE),
        recipient_key_epoch,
        contact_generation: 1,
        sender_key_epoch: 1,
        recipient_key_fingerprint: "ab".repeat(32),
        enc: vec![7u8; 32],
        ciphertext: b"first-seal".to_vec(),
    }
}

fn key(logical: u128, recipient_key_epoch: u64) -> EnvelopeKey {
    EnvelopeKey {
        logical_message_id: Uuid::from_u128(logical),
        recipient_device_id: Uuid::from_u128(RECIPIENT_DEVICE),
        recipient_key_epoch,
    }
}

fn expect_some(
    result: Result<Option<PersistedSubmission>, SourceError>,
    what: &str,
) -> PersistedSubmission {
    match result {
        Ok(Some(submission)) => submission,
        Ok(None) => panic!("{what}: expected a submission, found none"),
        Err(_) => panic!("{what}: expected a submission, got a source error"),
    }
}

fn expect_none(result: Result<Option<PersistedSubmission>, SourceError>, what: &str) {
    match result {
        Ok(None) => {}
        Ok(Some(_)) => panic!("{what}: expected no submission, found one"),
        Err(_) => panic!("{what}: expected no submission, got a source error"),
    }
}

fn expect_source_error(result: Result<Option<PersistedSubmission>, SourceError>, what: &str) {
    match result {
        Err(SourceError(_)) => {}
        Ok(_) => panic!("{what}: expected a source error, got Ok"),
    }
}

// Kills (history): in `node/source.rs`, the exact-key lookup in `for_receipt`
//   old: `                recipient_key_epoch,` (16 spaces)
//   new: `                recipient_key_epoch: recipient_key_epoch.saturating_add(1),`
// With the mutant, a receipt for epoch 1 reads the epoch-2 row after re-encryption, so the
// returned recipient epoch, enc and ciphertext are the newer ones and the history asserts fail.
//
// Kills (exclusion): in `node/source.rs`, the namespace guard in `for_receipt`
//   old: `        if row.envelope.namespace != self.binding.namespace`
//   new: `        if false && row.envelope.namespace != self.binding.namespace`
// With the mutant, a row of another namespace that matches kind and slug falls through to
// `convert`, which refuses it with a source error, so the `Ok(None)` assert for the foreign
// namespace row fails.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_source_separates_current_from_receipt_history() {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    backend.pool().run_migrations().expect("migrations");
    let backend = Arc::new(backend);

    let runtime = runtime_for(Arc::clone(&backend), NS);
    let token = token_for(&runtime, NS);
    let other_runtime = runtime_for(Arc::clone(&backend), OTHER_NS);
    let other_token = token_for(&other_runtime, OTHER_NS);
    let source = RuntimeOutboundSource::new(runtime.clone(), token.clone(), binding());
    let device = Uuid::from_u128(RECIPIENT_DEVICE);

    // No row at all: both reads report absence, not an error.
    expect_none(
        source.current(Uuid::from_u128(MISSING)).await,
        "current, no row",
    );
    expect_none(
        source
            .for_receipt(Uuid::from_u128(MISSING), device, 1)
            .await,
        "for_receipt, no row",
    );

    // Rows that belong to another namespace, kind or slug are not this source's. The
    // foreign-namespace row matches the binding in every other respect, so only the namespace
    // guard can exclude it.
    runtime
        .create_sender_transport(&token, envelope(FOREIGN_KIND, "node", SLUG, 1))
        .await
        .expect("foreign kind row");
    runtime
        .create_sender_transport(&token, envelope(FOREIGN_SLUG, "khive", "other-slug", 1))
        .await
        .expect("foreign slug row");
    let foreign = other_runtime
        .create_sender_transport(&other_token, envelope(FOREIGN_NAMESPACE, "khive", SLUG, 1))
        .await
        .expect("foreign namespace row");
    assert_eq!(foreign.envelope.namespace, OTHER_NS);
    for (logical, what) in [
        (FOREIGN_NAMESPACE, "foreign namespace"),
        (FOREIGN_KIND, "foreign kind"),
        (FOREIGN_SLUG, "foreign slug"),
    ] {
        expect_none(
            source
                .for_receipt(Uuid::from_u128(logical), device, 1)
                .await,
            &format!("for_receipt, {what}"),
        );
        expect_none(
            source.current(Uuid::from_u128(logical)).await,
            &format!("current, {what}"),
        );
    }

    // A matching row is returned in full through both reads.
    runtime
        .create_sender_transport(&token, envelope(MATCH, "khive", SLUG, 1))
        .await
        .expect("first envelope");
    let first = expect_some(
        source.for_receipt(Uuid::from_u128(MATCH), device, 1).await,
        "for_receipt, first envelope",
    );
    assert_eq!(first.namespace, NS);
    assert_eq!(first.kind, "khive");
    assert_eq!(first.slug, SLUG);
    assert_eq!(first.key_reference, CREDENTIAL);
    assert_eq!(
        first.logical_message_id,
        CanonicalUuid::from_uuid(Uuid::from_u128(MATCH))
    );
    assert_eq!(first.sender_agent, binding().agent);
    assert_eq!(first.sender_device, binding().device);
    assert_eq!(first.sender_key_epoch, Epoch::new(1).unwrap());
    assert_eq!(first.recipient, recipient_address());
    assert_eq!(first.recipient_device, CanonicalUuid::from_uuid(device));
    assert_eq!(first.recipient_key_epoch, Epoch::new(1).unwrap());
    assert_eq!(first.recipient_fingerprint, HexBytes::<32>::new([0xab; 32]));
    assert_eq!(first.contact_generation, Epoch::new(1).unwrap());
    assert_eq!(first.enc, Base64Bytes::<32>::new([7u8; 32]));
    assert_eq!(
        first.ciphertext,
        Ciphertext::new(b"first-seal".to_vec()).unwrap()
    );
    assert_eq!(
        expect_some(
            source.current(Uuid::from_u128(MATCH)).await,
            "current, first envelope"
        ),
        first
    );

    // Re-encryption after a confirmed recipient key change adds epoch 2 and keeps epoch 1.
    runtime
        .hold_sender_transport(key(MATCH, 1), Some(HoldReason::RecipientKeyChanged))
        .await
        .expect("key-change hold");
    let mut second_envelope = envelope(MATCH, "khive", SLUG, 2);
    second_envelope.enc = vec![8u8; 32];
    second_envelope.recipient_key_fingerprint = "cd".repeat(32);
    second_envelope.contact_generation = 2;
    second_envelope.ciphertext = b"second-seal".to_vec();
    let second_record = runtime
        .reencrypt_sender_transport_after_confirmed_key_change(&token, second_envelope)
        .await
        .expect("re-encrypted envelope");
    assert_eq!(second_record.envelope_seq, 2);

    let current = expect_some(
        source.current(Uuid::from_u128(MATCH)).await,
        "current, after re-encryption",
    );
    assert_eq!(current.recipient_key_epoch, Epoch::new(2).unwrap());
    assert_eq!(current.contact_generation, Epoch::new(2).unwrap());
    assert_eq!(
        current.recipient_fingerprint,
        HexBytes::<32>::new([0xcd; 32])
    );
    assert_eq!(current.enc, Base64Bytes::<32>::new([8u8; 32]));
    assert_eq!(
        current.ciphertext,
        Ciphertext::new(b"second-seal".to_vec()).unwrap()
    );

    let history = expect_some(
        source.for_receipt(Uuid::from_u128(MATCH), device, 1).await,
        "for_receipt, epoch 1 after re-encryption",
    );
    assert_eq!(
        history, first,
        "the epoch-1 history row is unchanged by re-encryption"
    );
    assert_eq!(history.recipient_key_epoch, Epoch::new(1).unwrap());
    assert_ne!(history, current);
    let newest = expect_some(
        source.for_receipt(Uuid::from_u128(MATCH), device, 2).await,
        "for_receipt, epoch 2",
    );
    assert_eq!(newest, current);

    // The exact key matters: another device or an unused epoch finds nothing.
    expect_none(
        source
            .for_receipt(
                Uuid::from_u128(MATCH),
                Uuid::from_u128(RECIPIENT_DEVICE + 1),
                1,
            )
            .await,
        "for_receipt, other device",
    );
    expect_none(
        source.for_receipt(Uuid::from_u128(MATCH), device, 3).await,
        "for_receipt, unused epoch",
    );

    // The current row is the highest sequence whatever its state: held ...
    runtime
        .hold_sender_transport(key(MATCH, 2), Some(HoldReason::InsufficientCredit))
        .await
        .expect("credit hold");
    let held = expect_some(
        source.current(Uuid::from_u128(MATCH)).await,
        "current, held row",
    );
    assert_eq!(held, current);

    // ... and failed.
    runtime
        .create_sender_transport(&token, envelope(FAILED, "khive", SLUG, 1))
        .await
        .expect("row to fail");
    runtime
        .record_sender_transport_failure(key(FAILED, 1), FailureClass::Permanent, None)
        .await
        .expect("record failure");
    let failed_record = runtime
        .sender_transport(key(FAILED, 1))
        .await
        .expect("read failed row")
        .expect("failed row exists");
    assert!(
        matches!(
            failed_record.last_failure_class,
            Some(FailureClass::Permanent)
        ),
        "fixture: the row carries the recorded permanent failure"
    );
    let failed = expect_some(
        source.current(Uuid::from_u128(FAILED)).await,
        "current, failed row",
    );
    assert_eq!(
        failed.logical_message_id,
        CanonicalUuid::from_uuid(Uuid::from_u128(FAILED))
    );

    // A stored row that does not match the enrolled sender is refused, not rewritten.
    let mut wrong_epoch = envelope(SENDER_EPOCH_MISMATCH, "khive", SLUG, 1);
    wrong_epoch.sender_key_epoch = 2;
    runtime
        .create_sender_transport(&token, wrong_epoch)
        .await
        .expect("row with another sender epoch");
    let mut wrong_credential = envelope(CREDENTIAL_MISMATCH, "khive", SLUG, 1);
    wrong_credential.credential_ref = "other-credential".to_string();
    runtime
        .create_sender_transport(&token, wrong_credential)
        .await
        .expect("row with another credential");
    for (logical, what) in [
        (SENDER_EPOCH_MISMATCH, "sender epoch mismatch"),
        (CREDENTIAL_MISMATCH, "credential mismatch"),
    ] {
        expect_source_error(
            source.current(Uuid::from_u128(logical)).await,
            &format!("current, {what}"),
        );
        expect_source_error(
            source
                .for_receipt(Uuid::from_u128(logical), device, 1)
                .await,
            &format!("for_receipt, {what}"),
        );
    }

    // A failing read is a source error, never an absent row. This runs last because it
    // removes the transport table.
    runtime
        .sql()
        .writer()
        .await
        .expect("sql writer")
        .execute(SqlStatement {
            sql: "DROP TABLE comm_sender_transport".into(),
            params: vec![],
            label: Some("node-source-read-fault".into()),
        })
        .await
        .expect("drop the transport table");
    expect_source_error(
        source.current(Uuid::from_u128(MATCH)).await,
        "current, read fault",
    );
    expect_source_error(
        source.for_receipt(Uuid::from_u128(MATCH), device, 1).await,
        "for_receipt, read fault",
    );
}
