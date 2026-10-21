mod fixture;
pub mod thread_rendering;

use std::collections::BTreeSet;

use serde_json::{json, Value};
use uuid::Uuid;

use fixture::{check_inbound, check_ingest_receipt, ingest_params, Fixture, ACTOR, CHANNEL, SLUG};

pub fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .expect("bounded executor")
}

#[derive(Clone, Copy)]
pub enum Scene {
    Ingest,
    Cleanup,
    Cursor,
    CursorGet,
    CursorCommit,
    MarkRead,
    Suppression,
}

impl Scene {
    pub const ALL: [Self; 7] = [
        Self::Ingest,
        Self::Cleanup,
        Self::Cursor,
        Self::CursorGet,
        Self::CursorCommit,
        Self::MarkRead,
        Self::Suppression,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Cleanup => "cleanup_expired_quarantine/100",
            Self::Cursor => "cursor_get_commit",
            Self::CursorGet => "cursor_get",
            Self::CursorCommit => "cursor_commit",
            Self::MarkRead => "mark_read/200",
            Self::Suppression => "suppression_list/100",
        }
    }

    pub fn operation(self) -> &'static str {
        match self {
            Self::Ingest => "one fresh inbound ingest dispatch",
            Self::Cleanup => "one cleanup dispatch deleting 100 expired quarantines",
            Self::Cursor => "one get plus one commit dispatch",
            Self::CursorGet => "one get dispatch",
            Self::CursorCommit => "one commit dispatch",
            Self::MarkRead => "one default non-atomic dispatch marking 200 messages",
            Self::Suppression => "one list dispatch returning 100 entries",
        }
    }
}

type Call = (&'static str, Value);

pub struct Prepared {
    pub fixture: Fixture,
    scene: Scene,
    first: Call,
    second: Option<Call>,
    ids: Vec<Uuid>,
    suppressions: BTreeSet<(String, String)>,
}

impl Prepared {
    pub async fn new(scene: Scene) -> Self {
        let fixture = Fixture::new();
        let mut ids = Vec::new();
        let mut suppressions = BTreeSet::new();
        let mut second = None;
        let first = match scene {
            Scene::Ingest => ("comm.ingest", ingest_params(0, false)),
            Scene::Cleanup => {
                let (seeded, expiry) = fixture.seed(100, true).await;
                ids = seeded;
                let as_of = expiry
                    .expect("all 100 retention deadlines")
                    .checked_add(1)
                    .expect("cutoff");
                (
                    "comm.cleanup_expired_quarantine",
                    json!({
                        "channel_kind":CHANNEL, "channel_slug":SLUG, "as_of_micros":as_of,
                    }),
                )
            }
            Scene::Cursor => {
                let initial = fixture
                    .registry
                    .dispatch("comm.cursor_commit", cursor_commit(100))
                    .await
                    .expect("seed one cursor row");
                check_cursor(&initial, 100);
                second = Some(("comm.cursor_commit", cursor_commit(101)));
                ("comm.cursor_get", cursor_key())
            }
            Scene::CursorGet | Scene::CursorCommit => {
                let initial = fixture
                    .registry
                    .dispatch("comm.cursor_commit", cursor_commit(100))
                    .await
                    .expect("seed one cursor row");
                check_cursor(&initial, 100);
                if matches!(scene, Scene::CursorGet) {
                    ("comm.cursor_get", cursor_key())
                } else {
                    ("comm.cursor_commit", cursor_commit(101))
                }
            }
            Scene::MarkRead => {
                ids = fixture.seed(200, false).await.0;
                (
                    "comm.mark_read",
                    json!({"ids":ids.iter().map(Uuid::to_string).collect::<Vec<_>>(), "atomic":false}),
                )
            }
            Scene::Suppression => {
                for index in 0..100 {
                    let address = format!("generated{index:03}@example.com");
                    let added = fixture
                        .registry
                        .dispatch(
                            "comm.suppression_add",
                            json!({
                                "scope":"outbound", "kind":"address", "value":address,
                                "source":"operator", "reason":"generated benchmark fixture",
                            }),
                        )
                        .await
                        .expect("seed suppression through public dispatch");
                    check_suppression(&added);
                    assert!(suppressions.insert((added["id"].as_str().unwrap().into(), address)));
                }
                (
                    "comm.suppression_list",
                    json!({"scope":"outbound","kind":"address","limit":100,"offset":0}),
                )
            }
        };
        Self {
            fixture,
            scene,
            first,
            second,
            ids,
            suppressions,
        }
    }

    pub fn take_calls(&mut self) -> (Call, Option<Call>) {
        (
            (self.first.0, std::mem::take(&mut self.first.1)),
            self.second.take(),
        )
    }

    pub async fn check(&self, result: &(Value, Option<Value>)) {
        let (first, second) = result;
        if !matches!(self.scene, Scene::Cursor) {
            assert!(second.is_none());
        }
        match self.scene {
            Scene::Ingest => {
                let id = check_ingest_receipt(first);
                let note = self
                    .fixture
                    .stored(id)
                    .await
                    .expect("ingest actually persisted");
                check_inbound(&note, 0, false);
            }
            Scene::Cleanup => {
                assert_eq!(first["ok"], true, "{first}");
                assert_eq!(first["deleted"], 100, "{first}");
                assert_eq!(first["routed_owners_detached"], 0);
                for id in &self.ids {
                    assert!(
                        self.fixture.stored(*id).await.is_none(),
                        "quarantine {id} still exists"
                    );
                }
            }
            Scene::Cursor => {
                check_cursor(first, 100);
                check_cursor(second.as_ref().expect("commit receipt"), 101);
                // A third read proves persistence; it is outside the timed pair.
                let stored = self
                    .fixture
                    .registry
                    .dispatch("comm.cursor_get", cursor_key())
                    .await
                    .expect("read committed cursor");
                assert_eq!(&stored, second.as_ref().unwrap());
            }
            Scene::CursorGet | Scene::CursorCommit => {
                let high_water = if matches!(self.scene, Scene::CursorGet) {
                    100
                } else {
                    101
                };
                check_cursor(first, high_water);
                let stored = self
                    .fixture
                    .registry
                    .dispatch("comm.cursor_get", cursor_key())
                    .await
                    .expect("read persisted cursor");
                assert_eq!(&stored, first);
            }
            Scene::MarkRead => {
                assert_eq!(first["status"], "success", "{first}");
                for field in ["requested_count", "unique_count", "marked_count"] {
                    assert_eq!(first[field], 200, "{first}");
                }
                assert_eq!(first["failed_count"], 0);
                assert_eq!(first["unknown_count"], 0);
                let rows = first["results"].as_array().expect("per-message results");
                assert_eq!(rows.len(), 200);
                let returned: BTreeSet<_> = rows
                    .iter()
                    .map(|row| {
                        assert_eq!(row["status"], "success");
                        assert_eq!(row["read"], true);
                        Uuid::parse_str(row["full_id"].as_str().expect("canonical result id"))
                            .unwrap()
                    })
                    .collect();
                assert_eq!(returned, self.ids.iter().copied().collect());
                for (index, id) in self.ids.iter().enumerate() {
                    check_inbound(
                        &self.fixture.stored(*id).await.expect("marked note"),
                        index,
                        true,
                    );
                }
            }
            Scene::Suppression => {
                assert_eq!(first["limit"], 100);
                assert_eq!(first["offset"], 0);
                let items = first["items"].as_array().expect("suppression list");
                assert_eq!(items.len(), 100);
                let returned: BTreeSet<_> = items
                    .iter()
                    .map(|item| {
                        check_suppression(item);
                        (
                            item["id"].as_str().unwrap().to_owned(),
                            item["value"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect();
                assert_eq!(returned, self.suppressions);
            }
        }
    }

    pub async fn close(self) {
        self.fixture.close().await;
    }
}

fn cursor_key() -> Value {
    json!({"channel_kind":CHANNEL,"channel_slug":SLUG})
}

fn cursor_commit(high_water: u64) -> Value {
    json!({"channel_kind":CHANNEL,"channel_slug":SLUG,"source":"generated-source-5658",
        "generation":1,"high_water":high_water})
}

fn check_cursor(row: &Value, high_water: u64) {
    assert_eq!(row["source"], "generated-source-5658");
    assert_eq!(row["generation"], 1);
    assert_eq!(row["high_water"], high_water);
    chrono::DateTime::parse_from_rfc3339(row["committed_at"].as_str().expect("commit timestamp"))
        .expect("valid commit timestamp");
}

fn check_suppression(row: &Value) {
    let id = row["id"].as_str().expect("suppression id");
    assert_eq!(
        Uuid::parse_str(id).expect("suppression UUID").to_string(),
        id
    );
    assert_eq!(row["scope"], "outbound");
    assert_eq!(row["kind"], "address");
    assert_eq!(row["source"], "operator");
    assert_eq!(row["created_by"], ACTOR);
    assert_eq!(row["reason"], "generated benchmark fixture");
}
