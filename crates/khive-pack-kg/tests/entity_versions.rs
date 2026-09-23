//! Persisted entity revisions through the public KG dispatch path.

use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::ErrorKind;
use serde_json::{json, Value};
use uuid::Uuid;

const NAME: &str = "EntityRevisionContractFixture";

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("local token");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        Self {
            runtime,
            token,
            registry: builder.build().expect("KG registry"),
        }
    }

    async fn create(&self) -> Value {
        self.registry
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": NAME,
                    "description": "original description",
                    "properties": {"unrelated": "preserve"},
                    "skip_dedup_check": true
                }),
            )
            .await
            .expect("create entity through KG")
    }

    async fn stored(&self, id: &str) -> Value {
        let entity = self
            .runtime
            .entities(&self.token)
            .expect("entity store")
            .get_entity(Uuid::parse_str(id).expect("full UUID"))
            .await
            .expect("read persisted entity")
            .expect("live entity");
        serde_json::to_value(entity).expect("serialize complete persisted row")
    }

    async fn assert_read_versions(&self, id: &str, version: i64) {
        let stored = self.stored(id).await;
        assert_eq!(stored["version"], json!(version));

        let get = self
            .registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("public get");
        let list = self
            .registry
            .dispatch("list", json!({"kind": "concept", "limit": 10}))
            .await
            .expect("public list");
        let listed = list["items"]
            .as_array()
            .expect("list items")
            .iter()
            .find(|row| row["id"] == id)
            .expect("entity in list");
        let search = self
            .registry
            .dispatch(
                "search",
                json!({"kind": "concept", "query": NAME, "source": "text", "limit": 10}),
            )
            .await
            .expect("public text search");
        let hit = search
            .as_array()
            .expect("search hits")
            .iter()
            .find(|row| row["id"] == id)
            .expect("entity in search");

        for row in [&get, listed, hit] {
            assert_eq!(row["version"], json!(version), "{row}");
            assert!(
                row["version"].is_i64(),
                "revision must be an integer: {row}"
            );
            assert_eq!(row["created_at"], get["created_at"], "{row}");
            assert_eq!(row["updated_at"], get["updated_at"], "{row}");
            assert!(
                row["updated_at"].is_string(),
                "timestamp contract stays separate: {row}"
            );
        }
    }
}

fn conflict_details(error: RuntimeError) -> Value {
    let RuntimeError::Khive(error) = error else {
        panic!("typed conflict expected, got {error:?}");
    };
    assert_eq!(error.kind(), ErrorKind::Conflict);
    let details = serde_json::to_value(error.details().expect("conflict details"))
        .expect("serialize conflict details");
    assert_eq!(details["reason"], "version_conflict");
    details
}

#[tokio::test]
async fn entity_revision_round_trips_and_stale_update_preserves_the_whole_row() {
    let fixture = Fixture::new();
    let created = fixture.create().await;
    let id = created["id"].as_str().expect("created UUID");
    assert_eq!(created["version"], 1);
    fixture.assert_read_versions(id, 1).await;

    let updated = fixture
        .registry
        .dispatch(
            "update",
            json!({"id": id, "expected_version": 1, "description": "accepted description"}),
        )
        .await
        .expect("matching revision accepts update");
    assert_eq!(updated["version"], 2);
    assert_eq!(updated["properties"]["unrelated"], "preserve");
    fixture.assert_read_versions(id, 2).await;

    let before = fixture.stored(id).await;
    let stale = fixture
        .registry
        .dispatch(
            "update",
            json!({"id": id, "expected_version": 1, "description": "must not persist", "tags": ["stale"]}),
        )
        .await
        .expect_err("stale revision refuses");
    let details = conflict_details(stale);
    assert_eq!(details["expected_version"], "1");
    assert_eq!(details["current_version"], "2");
    assert_eq!(
        fixture.stored(id).await,
        before,
        "all row fields, including timestamps, stay unchanged"
    );

    // A timestamp is not a revision alias, even though both are integers.
    let timestamp = before["updated_at"].as_i64().expect("stored timestamp");
    assert_ne!(timestamp, 2);
    let timestamp_error = fixture
        .registry
        .dispatch(
            "update",
            json!({"id": id, "expected_version": timestamp, "description": "not a revision"}),
        )
        .await
        .expect_err("timestamp cannot satisfy a revision precondition");
    let details = conflict_details(timestamp_error);
    assert_eq!(details["expected_version"], timestamp.to_string());
    assert_eq!(details["current_version"], "2");
    assert_eq!(fixture.stored(id).await, before);
    fixture.assert_read_versions(id, 2).await;
}

#[tokio::test]
async fn every_accepted_entity_update_advances_once_including_identical_and_unfenced() {
    let fixture = Fixture::new();
    let created = fixture.create().await;
    let id = created["id"].as_str().expect("created UUID");
    for (args, expected) in [
        (
            json!({"id": id, "expected_version": 1, "description": "original description"}),
            2,
        ),
        (json!({"id": id, "description": "original description"}), 3),
        (
            json!({"id": id, "expected_version": null, "description": "original description"}),
            4,
        ),
    ] {
        let updated = fixture
            .registry
            .dispatch("update", args)
            .await
            .expect("accepted entity write");
        assert_eq!(updated["version"], expected);
        assert_eq!(updated["description"], "original description");
        assert_eq!(updated["properties"]["unrelated"], "preserve");
        fixture.assert_read_versions(id, expected).await;
    }
}

#[tokio::test]
async fn malformed_entity_revision_preconditions_refuse_without_mutating_the_row() {
    let fixture = Fixture::new();
    let created = fixture.create().await;
    let id = created["id"].as_str().expect("created UUID");
    let before = fixture.stored(id).await;
    for invalid in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(true),
        json!({}),
        json!([]),
        json!(u64::MAX),
    ] {
        let error = fixture
            .registry
            .dispatch(
                "update",
                json!({"id": id, "expected_version": invalid, "description": "must not persist"}),
            )
            .await
            .expect_err("malformed or nonpositive revision refuses");
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "{invalid}: {error:?}"
        );
        assert_eq!(
            fixture.stored(id).await,
            before,
            "invalid precondition: {invalid}"
        );
    }
    fixture.assert_read_versions(id, 1).await;
}

#[tokio::test]
async fn entity_and_note_stale_preconditions_share_the_typed_conflict_contract() {
    let fixture = Fixture::new();
    let entity = fixture.create().await;
    let note = fixture
        .registry
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "original"}),
        )
        .await
        .expect("create note control");
    let mut conflicts = Vec::new();
    for (record, change) in [
        (&entity, json!({"description": "changed"})),
        (&note, json!({"content": "changed"})),
    ] {
        let mut args = change;
        args["id"] = record["id"].clone();
        args["expected_version"] = json!(1);
        let accepted = fixture
            .registry
            .dispatch("update", args.clone())
            .await
            .expect("first guarded update");
        assert_eq!(accepted["version"], 2);
        let error = fixture
            .registry
            .dispatch("update", args)
            .await
            .expect_err("reused precondition is stale");
        conflicts.push(conflict_details(error));
    }
    for key in ["reason", "expected_version", "current_version"] {
        assert_eq!(conflicts[0][key], conflicts[1][key], "{key}");
    }
    assert_eq!(conflicts[0]["expected_version"], "1");
    assert_eq!(conflicts[0]["current_version"], "2");
}

#[tokio::test]
async fn rival_entity_updates_at_one_revision_have_exactly_one_winner() {
    let fixture = Fixture::new();
    let created = fixture.create().await;
    let id = created["id"].as_str().expect("created UUID");
    let (left, right) = tokio::join!(
        fixture.registry.dispatch(
            "update",
            json!({"id": id, "expected_version": 1, "description": "left"})
        ),
        fixture.registry.dispatch(
            "update",
            json!({"id": id, "expected_version": 1, "description": "right"})
        ),
    );
    let results = [left, right];
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "{results:?}"
    );
    let winner = results
        .iter()
        .find_map(|result| result.as_ref().ok())
        .expect("winner");
    assert_eq!(winner["version"], 2);
    let stored = fixture.stored(id).await;
    assert_eq!(stored["description"], winner["description"]);
    assert_eq!(stored["properties"]["unrelated"], "preserve");
    let loser = results.into_iter().find_map(Result::err).expect("loser");
    let details = conflict_details(loser);
    assert_eq!(details["expected_version"], "1");
    assert_eq!(details["current_version"], "2");
    fixture.assert_read_versions(id, 2).await;
}
