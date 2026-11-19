mod fixture;

use std::collections::BTreeSet;

use criterion::black_box;
use khive_runtime::RuntimeError;
use khive_storage::Note;
use serde_json::{json, Value};
use uuid::Uuid;

use fixture::{text, Fixture, ACTOR, MICROS, MILLIS, SECONDS};

#[derive(Clone, Copy)]
pub enum Scene {
    Complete,
    RoundTrip,
    Census,
    Repair,
}

impl Scene {
    pub const ALL: [Self; 4] = [Self::Complete, Self::RoundTrip, Self::Census, Self::Repair];
    pub fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::RoundTrip => "assign_complete",
            Self::Census => "census/1000",
            Self::Repair => "repair_apply/100",
        }
    }
}

pub struct Prepared {
    fixture: Fixture,
    scene: Scene,
    params: Value,
    ids: Vec<Uuid>,
    originals: Vec<Note>,
}

fn assign_params() -> Value {
    json!({"title":"Generated completion task","status":"inbox","assignee":ACTOR})
}

impl Prepared {
    pub async fn new(scene: Scene) -> Self {
        let fixture = Fixture::new();
        let mut originals = Vec::new();
        let mut ids = Vec::with_capacity(1);
        let params = match scene {
            Scene::Complete => {
                let created = fixture
                    .registry
                    .dispatch("gtd.assign", assign_params())
                    .await
                    .expect("seed task");
                let id = Uuid::parse_str(created["full_id"].as_str().expect("created id")).unwrap();
                assert_eq!(
                    fixture.stored(id).await.properties.as_ref().unwrap()["status"],
                    "inbox"
                );
                ids.push(id);
                json!({"id":id.to_string()})
            }
            Scene::RoundTrip => assign_params(),
            Scene::Census => {
                originals = fixture.seed(1000, false).await;
                json!({})
            }
            Scene::Repair => {
                originals = fixture.seed(100, true).await;
                json!({"apply":true,"items":originals.iter().map(|note| json!({
                    "id":note.id.to_string(), "changes":{
                        "created_at":{"observed":SECONDS.to_string(),"value":MICROS},
                        "updated_at":{"observed":MILLIS.to_string(),"value":MICROS},
                        "status":{"observed":"\"archived\"","value":"done"},
                    }
                })).collect::<Vec<_>>()})
            }
        };
        Self {
            fixture,
            scene,
            params,
            ids,
            originals,
        }
    }

    pub async fn run(&mut self) -> Result<Value, RuntimeError> {
        let params = std::mem::take(&mut self.params);
        let verb = match self.scene {
            Scene::Complete => "gtd.complete",
            Scene::Census => "gtd.census",
            Scene::Repair => "gtd.repair",
            Scene::RoundTrip => {
                let created = self
                    .fixture
                    .registry
                    .dispatch("gtd.assign", black_box(params))
                    .await?;
                let raw_id = created["full_id"]
                    .as_str()
                    .expect("created id for completion");
                self.ids
                    .push(Uuid::parse_str(raw_id).expect("created UUID"));
                return self
                    .fixture
                    .registry
                    .dispatch("gtd.complete", json!({"id":raw_id}))
                    .await;
            }
        };
        self.fixture
            .registry
            .dispatch(verb, black_box(params))
            .await
    }

    pub async fn check(&self, result: &Value) {
        match self.scene {
            Scene::Complete | Scene::RoundTrip => {
                assert_eq!(result["completed"], true);
                assert_eq!(result["full_id"], self.ids[0].to_string());
                assert_eq!(result["from"], "inbox");
                assert_eq!(result["to"], "done");
                assert_eq!(result["is_terminal"], true);
                assert_eq!(result["audit_persisted"], true);
                let note = self.fixture.stored(self.ids[0]).await;
                let properties = note.properties.as_ref().expect("task properties");
                assert_eq!(properties["status"], "done");
                assert_eq!(properties["assignee"], ACTOR);
                assert!(note.deleted_at.is_none());
                let audits = self.fixture.audit_rows().await;
                assert_eq!(audits.len(), 1);
                assert_eq!(text(&audits[0], "note_id"), self.ids[0].to_string());
                assert_eq!(text(&audits[0], "from_state"), "inbox");
                assert_eq!(text(&audits[0], "to_state"), "done");
            }
            Scene::Census => {
                assert_eq!(result["schema_version"], 1);
                assert_eq!(
                    result["scope"],
                    json!({"kind":"task","rows":"live_only","namespaces":[ACTOR,"local"]})
                );
                assert_eq!(result["total_tasks"], 1000);
                assert_eq!(result["created_at"]["magnitude_16_digits"], 1000);
                assert_eq!(result["archived_at"]["null"], 1000);
                for field in ["created_at", "archived_at"] {
                    assert_eq!(
                        result[field]
                            .as_object()
                            .expect("histogram")
                            .values()
                            .map(|count| count.as_u64().expect("integer count"))
                            .sum::<u64>(),
                        1000
                    );
                }
                assert_eq!(result["created_at_gt_archived_at_raw"], 0);
                assert!(result.get("candidates").is_none());
                for original in &self.originals {
                    assert_eq!(self.fixture.stored(original.id).await, *original);
                }
            }
            Scene::Repair => self.check_repair(result).await,
        }
    }

    async fn check_repair(&self, result: &Value) {
        assert_eq!(result["apply"], true);
        assert_eq!(result["accepted"], 100);
        assert_eq!(result["applied"], 100);
        assert_eq!(result["refused"], 0);
        let rows = result["results"].as_array().expect("repair receipts");
        assert_eq!(rows.len(), self.originals.len());
        for (row, original) in rows.iter().zip(&self.originals) {
            assert_eq!(row["id"], original.id.to_string());
            assert_eq!(row["accepted"], true);
            assert_eq!(row["applied"], true);
            assert!(row["reason"].is_null());
            let mut stored = self.fixture.stored(original.id).await;
            assert_eq!(stored.created_at, MICROS);
            assert_eq!(stored.updated_at, MICROS);
            assert_eq!(stored.version, original.version + 1);
            let properties = stored.properties.as_mut().unwrap().as_object_mut().unwrap();
            assert_eq!(properties["status"], "done");
            let history = properties
                .remove("gtd_repair")
                .expect("persisted repair history");
            for (field, observed) in [
                ("created_at", SECONDS.to_string()),
                ("updated_at", MILLIS.to_string()),
                ("status", "\"archived\"".into()),
            ] {
                assert_eq!(history["originals"][field]["value"], observed);
                assert_eq!(history["originals"][field]["actor"], ACTOR);
            }
            properties.insert("status".into(), json!("archived"));
            stored.created_at = SECONDS;
            stored.updated_at = MILLIS;
            stored.version = original.version;
            assert_eq!(
                stored, *original,
                "repair changes only named fields and history"
            );
        }
        let audits = self.fixture.audit_rows().await;
        assert_eq!(audits.len(), 100);
        let ids: BTreeSet<_> = audits
            .iter()
            .map(|row| {
                assert_eq!(text(row, "from_state"), "archived");
                assert_eq!(text(row, "to_state"), "done");
                assert_eq!(text(row, "namespace"), "local");
                let payload: Value =
                    serde_json::from_str(text(row, "note")).expect("repair audit JSON");
                assert_eq!(payload["operation"], "gtd.repair");
                assert_eq!(payload["actor"], ACTOR);
                Uuid::parse_str(text(row, "note_id")).expect("audit UUID")
            })
            .collect();
        assert_eq!(ids, self.originals.iter().map(|note| note.id).collect());
    }

    pub async fn close(self) {
        self.fixture.close().await;
    }
}
