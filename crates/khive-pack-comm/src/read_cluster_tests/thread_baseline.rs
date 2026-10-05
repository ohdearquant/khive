//! Quiescent handler oracles; statement starts are not SQLite work counters.

use super::*;
use khive_runtime::micros_to_iso;
use std::collections::BTreeMap;
use std::sync::Mutex;

#[path = "thread_baseline/fold.rs"]
mod fold;
#[path = "thread_baseline/selection.rs"]
mod selection;
#[path = "thread_baseline/work.rs"]
mod work;

#[derive(Clone, Default, serde::Serialize)]
struct Observation {
    physical_pages: usize,
    fetched_rows: usize,
    rendered_rows: usize,
    retained_rows: BTreeMap<String, usize>,
    peak_physical_rows: usize,
    final_owner_ids: Vec<String>,
    reader_acquisitions: u64,
    statement_starts: BTreeMap<String, usize>,
}

impl Observation {
    fn observe(&mut self, phase: Phase) {
        match phase {
            Phase::ThreadPage(ids) => {
                self.physical_pages += 1;
                self.fetched_rows += ids.len();
            }
            Phase::ThreadRendered(count) => self.rendered_rows += count,
            Phase::ThreadRetained { stage, rows } => {
                if matches!(stage, "physical" | "with_selected_root") {
                    self.peak_physical_rows = self.peak_physical_rows.max(rows);
                }
                self.retained_rows.insert(stage.into(), rows);
            }
            Phase::ThreadOwners(ids) => {
                self.final_owner_ids = ids.into_iter().map(|id| id.to_string()).collect();
            }
            _ => {}
        }
    }
}

async fn observed_as(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    params: Value,
    actor: Option<&str>,
) -> (Value, Observation) {
    let observation = Arc::new(Mutex::new(Observation::default()));
    let callback = hook({
        let observation = Arc::clone(&observation);
        move |phase| {
            let observation = Arc::clone(&observation);
            Box::pin(async move { observation.lock().unwrap().observe(phase) })
        }
    });
    let statements = runtime
        .backend()
        .pool()
        .observe_test_statement_starts(20_000)
        .expect("private pool's existing statement observer");
    let before = reader_acquisitions(runtime);
    let response = PHASE_HOOK
        .scope(callback, async {
            match actor {
                Some(actor) => {
                    registry
                        .dispatch_as(
                            "comm.thread",
                            params,
                            khive_runtime::VerifiedActor::new(actor).unwrap(),
                        )
                        .await
                }
                None => registry.dispatch("comm.thread", params).await,
            }
        })
        .await
        .expect("real thread handler");
    let mut result = observation.lock().unwrap().clone();
    result.reader_acquisitions = reader_acquisitions(runtime) - before;
    for statement in statements
        .started_statements()
        .expect("complete SQL starts")
    {
        assert!(
            statement.readonly,
            "thread must not write: {}",
            statement.sql
        );
        *result.statement_starts.entry(statement.sql).or_default() += 1;
    }
    assert_eq!(
        result.final_owner_ids.len(),
        response["count"].as_u64().unwrap() as usize
    );
    assert!(
        !result.statement_starts.is_empty(),
        "positive executed-SQL control"
    );
    (response, result)
}

async fn observed(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    params: Value,
) -> (Value, Observation) {
    observed_as(runtime, registry, params, None).await
}

fn row(index: u32, created_at: i64, root: Uuid) -> Note {
    let mut note = message(index);
    note.created_at = created_at;
    note.updated_at = created_at;
    note.properties.as_mut().unwrap()["thread_id"] = json!(root.to_string());
    note
}

fn outbound(index: u32, created_at: i64, root: Uuid) -> Note {
    let mut note = row(index, created_at, root);
    let props = note.properties.as_mut().unwrap();
    props["direction"] = json!("outbound");
    props["from_actor"] = json!("lambda:reader");
    props["to_actor"] = json!("lambda:sender");
    note
}

fn inbound(index: u32, created_at: i64, root: Uuid, owner: u32, read: Value) -> Note {
    let mut note = row(index, created_at, root);
    let props = note.properties.as_mut().unwrap();
    props["outbound_ref"] = json!(id(owner).to_string());
    props["read"] = read;
    note
}

fn expect_ids(response: &Value, observation: &Observation, ids: &[Uuid]) {
    let expected: Vec<_> = ids.iter().map(Uuid::to_string).collect();
    assert_eq!(thread_messages(response), expected);
    assert_eq!(observation.final_owner_ids, expected);
}

fn emit(label: &str, params: &Value, response: &Value, observation: &Observation) {
    println!(
        "COMM_THREAD_BASELINE {}",
        json!({
            "fixture": label,
            "request": params,
            "observation": observation,
            "returned_ids": thread_messages(response),
            "uninstrumented": ["bound SQLite values", "EXPLAIN QUERY PLAN", "VM steps", "fullscan steps", "sort counters", "SQLite rows stepped", "retained bytes", "attachment SQL owner requests"],
        })
    );
}
