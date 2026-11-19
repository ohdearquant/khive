//! Canonical updates of rows that carry a runtime-reserved property (ADR-115 §4).
//!
//! A stored stamp vouches for exact bytes. A canonical update may echo it byte for byte or
//! omit it from a whole map, and it carries forward only while the bytes it vouches for are
//! unchanged. A differing or invented value is refused. Canonical writes take no manifest
//! exemption, so changed bytes drop the stamp and the row must pass the strict scans.

use super::*;
use crate::secret_gate::RESERVED_SECRET_GATE_KEY;
use crate::secret_gate_finalizer::entity_admission::fixture;
use crate::secret_gate_finalizer::manifest::fixture::TestOnlyManifestFixture;
use crate::secret_gate_finalizer::manifest::RuntimeFieldScope;
use crate::NamespaceToken;
use khive_changeset::{EntityPatch, EntityPreimage, Envelope, UpdatePreimage};
use khive_types::{Namespace, Timestamp};

const STAMP: &str = "exempted:content-sha256-manifest-v1";

struct Stamped {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    value: String,
    row: Entity,
    _manifest: fixture::ManifestFixtureGuard,
    _dir: tempfile::TempDir,
}

/// A stamped entity whose description holds a value the strict scan refuses and the
/// installed manifest exempts, as the finalizer leaves an admitted row.
async fn stamped() -> Stamped {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new_for_test(crate::RuntimeConfig {
        db_path: Some(dir.path().join("canonical-reserved.db")),
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        ..Default::default()
    })
    .unwrap();
    let namespace = Namespace::parse(&format!("canonical-reserved-{}", Uuid::new_v4())).unwrap();
    let token =
        NamespaceToken::mint_authorized(namespace, khive_gate::ActorRef::new("agent", "fixture"));
    let value = TestOnlyManifestFixture::new().exact_value().to_owned();
    assert!(crate::secret_gate::check(&value).is_err());
    let manifest = fixture::install(
        token.namespace().as_str(),
        TestOnlyManifestFixture::for_scope(RuntimeFieldScope::NameDescription).snapshot(),
    );
    let row = Entity::new(token.namespace().as_str(), "concept", "stamped")
        .with_description(value.clone())
        .with_properties(json!({"purpose": "fixture", RESERVED_SECRET_GATE_KEY: STAMP}));
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(row.clone())
        .await
        .unwrap();
    Stamped {
        runtime,
        token,
        value,
        row,
        _manifest: manifest,
        _dir: dir,
    }
}

impl Stamped {
    async fn live(&self) -> Entity {
        self.runtime
            .entities(&self.token)
            .unwrap()
            .get_entity(self.row.id)
            .await
            .unwrap()
            .unwrap()
    }

    /// The stored values of exactly the fields `patch` touches, as a canonical update must
    /// capture them.
    fn preimage(&self, patch: &EntityPatch) -> EntityPreimage {
        let projected = projection::entity(&self.row).unwrap();
        EntityPreimage {
            name: patch.name.as_ref().map(|_| projected.name),
            description: patch.description.as_ref().map(|_| projected.description),
            entity_type: None,
            properties: patch.properties.as_ref().map(|_| projected.properties),
            tags: None,
        }
    }

    async fn apply(&self, patch: EntityPatch, preimage: EntityPreimage) -> Result<(), String> {
        let op = Op::Update(
            UpdateOp::new(
                id128(self.row.id),
                UpdatePatch::Entity(patch),
                UpdatePreimage::Entity(preimage),
            )
            .unwrap(),
        );
        let changeset = ChangeSet::new(
            Envelope::new("agent:fixture", "test", Timestamp::from_secs(1)),
            vec![op],
        );
        let registry = crate::VerbRegistryBuilder::new().build().unwrap();
        let plans = prepare_canonical_changeset(&self.runtime, &registry, &self.token, &changeset)
            .await
            .map_err(|error| chain(&error))?;
        match crate::run_atomic_unit(self.runtime.sql().as_ref(), plans).await {
            Ok(crate::AtomicRunOutcome::Committed { .. }) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    }

    async fn assert_unchanged(&self) {
        assert_eq!(
            serde_json::to_value(self.live().await).unwrap(),
            serde_json::to_value(&self.row).unwrap()
        );
    }
}

/// An error and every source under it, so a refusal wrapped as one operation's admission
/// failure still shows its cause.
fn chain(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut next = error.source();
    while let Some(source) = next {
        rendered.push_str(": ");
        rendered.push_str(&source.to_string());
        next = source.source();
    }
    rendered
}

fn properties(value: Value) -> std::collections::BTreeMap<String, khive_types::PropertyValue> {
    serde_json::from_value(value).unwrap()
}

fn properties_patch(value: Value) -> EntityPatch {
    EntityPatch {
        name: None,
        description: None,
        entity_type: None,
        properties: Some(properties(value)),
        tags: None,
    }
}

/// C1: a canonical map carrying a stamp value other than the stored one is refused, and
/// the row is untouched.
#[tokio::test]
async fn canonical_update_refuses_a_differing_stamp_value() {
    let s = stamped().await;
    let patch =
        properties_patch(json!({"purpose": "fixture", RESERVED_SECRET_GATE_KEY: "exempted:other"}));
    let preimage = s.preimage(&patch);
    let error = s
        .apply(patch, preimage)
        .await
        .expect_err("a differing reserved value must be refused");
    assert!(error.contains("runtime-owned"), "{error}");
    s.assert_unchanged().await;
}

/// C2 (and C5): a canonical map that echoes the stored stamp byte for byte is admitted and
/// the stamp stays as stored.
#[tokio::test]
async fn canonical_update_admits_a_byte_exact_stamp_echo() {
    let s = stamped().await;
    let patch = properties_patch(json!({"purpose": "fixture", RESERVED_SECRET_GATE_KEY: STAMP}));
    let preimage = s.preimage(&patch);
    s.apply(patch, preimage)
        .await
        .expect("a byte-exact echo of the stored stamp must be admitted");
    s.assert_unchanged().await;
}

/// C3 (and C5): a whole map that omits the stamp, with a preimage that omits it too, leaves
/// every vouched byte unchanged, so the stored stamp carries forward.
#[tokio::test]
async fn canonical_whole_map_omitting_the_stamp_carries_it_while_bytes_are_unchanged() {
    let s = stamped().await;
    let patch = properties_patch(json!({"purpose": "fixture"}));
    let mut preimage = s.preimage(&patch);
    preimage.properties = Some(properties(json!({"purpose": "fixture"})));
    s.apply(patch, preimage)
        .await
        .expect("omitting the stamp over unchanged bytes must carry it");
    let live = s.live().await;
    assert_eq!(
        live.properties.as_ref().unwrap()[RESERVED_SECRET_GATE_KEY],
        STAMP
    );
    s.assert_unchanged().await;
}

/// C4: changing a vouched byte drops the stamp; the clean row is written without it.
#[tokio::test]
async fn canonical_update_that_changes_vouched_bytes_drops_the_stamp() {
    let s = stamped().await;
    let patch = EntityPatch {
        name: None,
        description: Some(Some("a plain description".into())),
        entity_type: None,
        properties: None,
        tags: None,
    };
    let preimage = s.preimage(&patch);
    s.apply(patch, preimage)
        .await
        .expect("a clean canonical update must be admitted");
    let live = s.live().await;
    assert_eq!(live.description.as_deref(), Some("a plain description"));
    assert_eq!(live.properties, Some(json!({"purpose": "fixture"})));
}

/// C4, must-red arm: canonical writes take no manifest exemption. Renaming the row changes
/// vouched bytes, so the stamp no longer covers the description, and the strict scan refuses
/// the manifest-exempt value even though the manifest for this namespace is installed.
#[tokio::test]
async fn canonical_update_refuses_manifest_exempt_content_once_the_stamp_drops() {
    let s = stamped().await;
    let patch = EntityPatch {
        name: Some("renamed".into()),
        description: None,
        entity_type: None,
        properties: None,
        tags: None,
    };
    let preimage = s.preimage(&patch);
    let error = s
        .apply(patch, preimage)
        .await
        .expect_err("manifest-exempt content must not pass a canonical write");
    assert!(error.contains("secret pattern"), "{error}");
    assert!(!error.contains("runtime-owned"), "{error}");
    assert!(!error.contains(&s.value), "{error}");
    s.assert_unchanged().await;
}

/// A canonical note map cannot introduce a reserved key the note does not carry.
#[tokio::test]
async fn canonical_note_update_refuses_an_invented_reserved_property() {
    let dir = tempfile::tempdir().unwrap();
    let rt = KhiveRuntime::new_for_test(crate::RuntimeConfig {
        db_path: Some(dir.path().join("canonical-note.db")),
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        ..Default::default()
    })
    .unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let note = Note::new("local", "observation", "same").with_properties(json!({"old": 1}));
    rt.notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let old = projection::note(&note).unwrap().properties;
    let op = Op::Update(
        UpdateOp::new(
            id128(note.id),
            UpdatePatch::Note(khive_changeset::NotePatch {
                content: None,
                salience: None,
                decay_factor: None,
                properties: Some(properties(
                    json!({"old": 1, RESERVED_SECRET_GATE_KEY: STAMP}),
                )),
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
    let changeset = ChangeSet::new(
        Envelope::new("agent:test", "test", Timestamp::from_secs(1)),
        vec![op],
    );
    let registry = crate::VerbRegistryBuilder::new().build().unwrap();
    let refused = match prepare_canonical_changeset(&rt, &registry, &token, &changeset).await {
        Err(error) => chain(&error),
        Ok(plans) => format!(
            "{:?}",
            crate::run_atomic_unit(rt.sql().as_ref(), plans).await
        ),
    };
    assert!(refused.contains("runtime-owned"), "{refused}");
    let live = rt
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(live).unwrap(),
        serde_json::to_value(&note).unwrap()
    );
}

/// The write-site check both canonical update routes call before building the replace: a
/// reserved value reaches the row only as an unchanged carry of the stored one.
#[test]
fn write_site_check_admits_only_an_unchanged_carry() {
    let check = crate::secret_gate::check_reserved_carry_forward;
    let stored = json!({"purpose": "fixture", RESERVED_SECRET_GATE_KEY: STAMP});
    let plain = json!({"purpose": "fixture"});
    let other = json!({"purpose": "fixture", RESERVED_SECRET_GATE_KEY: "exempted:other"});
    assert!(check(Some(&stored), Some(&stored), true).is_ok());
    assert!(check(Some(&stored), Some(&plain), false).is_ok());
    assert!(check(Some(&stored), Some(&stored), false).is_err());
    assert!(check(Some(&stored), Some(&other), true).is_err());
    assert!(check(Some(&plain), Some(&stored), true).is_err());
    assert!(check(None, Some(&stored), true).is_err());
}
