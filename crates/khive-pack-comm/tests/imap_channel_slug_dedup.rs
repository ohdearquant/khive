use khive_pack_comm as _;
use khive_pack_kg as _;
use khive_runtime::{KhiveRuntime, Namespace, PackRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use uuid::Uuid;

const LEGACY_ID: &str = "imap:mail.example.com:17:42";
const ACCOUNT_ID: &str = "imap:mail.example.com:user@example.com:17:42";

fn delivery(slug: &str) -> Value {
    json!({
        "from": "email:sender@example.com",
        "to": "email:maintainer@example.com",
        "content": "Same IMAP mailbox and UID",
        "channel_kind": "email",
        "channel_slug": slug,
        "external_id": ACCOUNT_ID,
        "legacy_external_id": LEGACY_ID,
    })
}

#[tokio::test]
async fn case_distinct_channel_slugs_with_same_imap_uid_are_stored_separately() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["kg".to_string(), "comm".to_string()],
        rt.clone(),
        &mut builder,
    )
    .expect("register packs");
    builder.with_default_namespace("local");
    let registry = builder.build().expect("build registry");

    let first = registry
        .dispatch("comm.ingest", delivery("User@Example.com"))
        .await
        .expect("first delivery");
    assert_eq!(first["deduplicated"], false);

    let second = registry
        .dispatch("comm.ingest", delivery("user@example.com"))
        .await
        .expect("second delivery");
    assert_eq!(
        second["deduplicated"], false,
        "case-distinct channel slugs must each store their own IMAP delivery"
    );
    assert_ne!(first["full_id"], second["full_id"]);

    let token = rt.authorize(Namespace::local()).expect("local token");
    let notes = rt.notes(&token).expect("note store");
    assert_eq!(
        notes.count_notes("local", Some("message")).await.unwrap(),
        2
    );
    for (ack, slug) in [(&first, "User@Example.com"), (&second, "user@example.com")] {
        let id: Uuid = ack["full_id"].as_str().unwrap().parse().unwrap();
        let note = notes.get_note(id).await.unwrap().unwrap();
        let properties = note.properties.as_ref().unwrap();
        assert_eq!(properties["channel_slug"], slug);
        assert_eq!(properties["external_id"], ACCOUNT_ID);
    }

    for (ack, slug) in [(&first, "User@Example.com"), (&second, "user@example.com")] {
        let retry = registry
            .dispatch("comm.ingest", delivery(slug))
            .await
            .expect("same-channel retry");
        assert_eq!(retry["deduplicated"], true);
        assert_eq!(retry["thread_id"], ack["thread_id"]);
    }
    assert_eq!(
        notes.count_notes("local", Some("message")).await.unwrap(),
        2
    );
}

#[tokio::test]
async fn case_distinct_channel_slugs_without_legacy_key_dedup_within_each_slug() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["kg".to_string(), "comm".to_string()],
        rt.clone(),
        &mut builder,
    )
    .expect("register packs");
    builder.with_default_namespace("local");
    let registry = builder.build().expect("build registry");

    let without_legacy = |slug: &str| {
        let mut params = delivery(slug);
        params.as_object_mut().unwrap().remove("legacy_external_id");
        params
    };
    let upper = registry
        .dispatch("comm.ingest", without_legacy("User@Example.com"))
        .await
        .expect("first channel delivery");
    let lower = registry
        .dispatch("comm.ingest", without_legacy("user@example.com"))
        .await
        .expect("second channel delivery");
    assert_eq!(upper["deduplicated"], false);
    assert_eq!(lower["deduplicated"], false);
    assert_ne!(upper["full_id"], lower["full_id"]);

    for (slug, original) in [("User@Example.com", &upper), ("user@example.com", &lower)] {
        let retry = registry
            .dispatch("comm.ingest", without_legacy(slug))
            .await
            .expect("same-channel retry");
        assert_eq!(retry["deduplicated"], true);
        assert_eq!(retry["thread_id"], original["thread_id"]);
    }
    let token = rt.authorize(Namespace::local()).expect("local token");
    assert_eq!(
        rt.notes(&token)
            .expect("note store")
            .count_notes("local", Some("message"))
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn legacy_imap_lookup_uses_exact_channel_slug() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["kg".to_string(), "comm".to_string()],
        rt.clone(),
        &mut builder,
    )
    .expect("register packs");
    builder.with_default_namespace("local");
    let registry = builder.build().expect("build registry");

    let mut old_delivery = delivery("User@Example.com");
    old_delivery["external_id"] = json!(LEGACY_ID);
    old_delivery
        .as_object_mut()
        .unwrap()
        .remove("legacy_external_id");
    let old = registry
        .dispatch("comm.ingest", old_delivery)
        .await
        .expect("legacy row");
    let lower = registry
        .dispatch("comm.ingest", delivery("user@example.com"))
        .await
        .expect("other channel's first delivery");
    assert_eq!(lower["deduplicated"], false);

    let upper_replay = registry
        .dispatch("comm.ingest", delivery("User@Example.com"))
        .await
        .expect("same channel's legacy replay");
    assert_eq!(upper_replay["deduplicated"], true);
    assert_eq!(upper_replay["thread_id"], old["thread_id"]);
    let lower_replay = registry
        .dispatch("comm.ingest", delivery("user@example.com"))
        .await
        .expect("other channel's new-key replay");
    assert_eq!(lower_replay["deduplicated"], true);
    assert_eq!(lower_replay["thread_id"], lower["thread_id"]);
    let token = rt.authorize(Namespace::local()).expect("local token");
    assert_eq!(
        rt.notes(&token)
            .expect("note store")
            .count_notes("local", Some("message"))
            .await
            .unwrap(),
        2
    );
}
