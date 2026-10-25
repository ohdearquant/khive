use std::any::Any;
use std::collections::BTreeSet;
use std::error::Error as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use khive_db::StorageBackend;
use khive_pack_kg::KgPack;
use khive_pack_tool::{
    policy::{decide, grant_is_active_on, Decision},
    RegistryPin, ToolPack,
};
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::error::{SqliteWriteFailure, SqliteWriteStage};
use khive_storage::types::SqlColumn;
use khive_storage::{
    CapacityUnavailablePhase, ContentRef, PageRequest, SqlReader, SqlRow, SqlStatement, SqlValue,
    StorageCapability, StorageError, StorageResult, WriterTaskRequestState,
};
use serde_json::{json, Value};
use uuid::Uuid;

const NOW: i64 = 100;
const ACTOR: &str = "agent:requester";
const TOOL: &str = "held-tool";
const HELD: &str = "m-held";
const CHILD_ENV: &str = "KHIVE_AM7_QUEUED_CHILD";
const DATABASE_ENV: &str = "KHIVE_AM7_QUEUED_DATABASE";

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.into())
}
fn statement(sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params,
        label: None,
    }
}
fn unbox<T: Any + Send>(value: Box<dyn Any + Send>) -> T {
    *value
        .downcast::<T>()
        .unwrap_or_else(|_| panic!("unexpected atomic-unit result type"))
}
fn pin(id: u128, source: &str) -> RegistryPin {
    RegistryPin::from_canonical_bytes(Uuid::from_u128(id), khive_types::canonical_json_bytes(&json!({
        "source": source, "side_effect": "write", "trust": "first_party", "schema": {"type": "object"},
    })).expect("canonical policy inputs"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Live,
    PinnedLive,
    Requested,
    Denied,
    Revoked,
    Expired,
    InvalidatedBy,
    InvalidatedAt,
    RegistryInvalidated,
    RegistryMismatch,
    DigestMismatch,
    RegistryOnly,
    DigestOnly,
    BothPins,
    ActorMismatch,
    ToolMismatch,
    Missing,
    TextExpiry,
    TextRequested,
    Decode(&'static str, bool),
}
impl State {
    fn registration(self) -> Option<RegistryPin> {
        matches!(
            self,
            Self::PinnedLive | Self::RegistryMismatch | Self::DigestMismatch
        )
        .then(|| pin(1, "mcp:original"))
    }
    fn error_column(self) -> Option<&'static str> {
        match self {
            Self::TextExpiry => Some("expires_at"),
            Self::TextRequested => Some("requested_at"),
            Self::Decode(column, _) => Some(column),
            _ => None,
        }
    }
    fn active(self) -> bool {
        matches!(self, Self::Live | Self::PinnedLive)
    }
    fn underlying_matches(self) -> bool {
        self.active()
            || matches!(
                self,
                Self::TextExpiry | Self::TextRequested | Self::Decode(_, _)
            )
    }
    fn patch(self) -> Option<Patch> {
        match self {
            Self::Decode(column, missing) => Some(Patch {
                column,
                value: if missing {
                    None
                } else {
                    Some(if column == "requested_at" || column == "expires_at" {
                        text("not-a-time")
                    } else {
                        SqlValue::Integer(7)
                    })
                },
            }),
            _ => None,
        }
    }
}
fn stored_states() -> Vec<State> {
    vec![
        State::Live,
        State::PinnedLive,
        State::Requested,
        State::Denied,
        State::Revoked,
        State::Expired,
        State::InvalidatedBy,
        State::InvalidatedAt,
        State::RegistryInvalidated,
        State::RegistryMismatch,
        State::DigestMismatch,
        State::RegistryOnly,
        State::DigestOnly,
        State::BothPins,
        State::ActorMismatch,
        State::ToolMismatch,
        State::Missing,
        State::TextExpiry,
        State::TextRequested,
    ]
}
fn all_states() -> Vec<State> {
    let mut states = stored_states();
    for column in [
        "id",
        "actor",
        "tool",
        "status",
        "requested_at",
        "expires_at",
    ] {
        states.push(State::Decode(column, true));
        states.push(State::Decode(column, false));
    }
    states
}

#[derive(Clone)]
struct Grant {
    id: String,
    namespace: String,
    actor: String,
    tool: String,
    status: String,
    requested_at: SqlValue,
    expires_at: SqlValue,
    registry_id: SqlValue,
    digest: SqlValue,
    invalidated_by: SqlValue,
    invalidated_at: SqlValue,
}
impl Grant {
    fn live(id: &str, at: i64, registration: Option<&RegistryPin>) -> Self {
        let mut row = Self {
            id: id.into(),
            namespace: "local".into(),
            actor: ACTOR.into(),
            tool: TOOL.into(),
            status: "granted".into(),
            requested_at: SqlValue::Integer(at),
            expires_at: SqlValue::Null,
            registry_id: SqlValue::Null,
            digest: SqlValue::Null,
            invalidated_by: SqlValue::Null,
            invalidated_at: SqlValue::Null,
        };
        if let Some(pin) = registration {
            row.registry_id = text(&pin.registry_id().to_string());
            row.digest = text(pin.definition_digest());
        }
        row
    }
    fn state(id: &str, at: i64, state: State, registration: Option<&RegistryPin>) -> Self {
        let mut row = Self::live(id, at, registration);
        match state {
            State::Requested => row.status = "requested".into(),
            State::Denied => row.status = "denied".into(),
            State::Revoked => row.status = "revoked".into(),
            State::Expired => row.expires_at = SqlValue::Integer(NOW),
            State::InvalidatedBy => row.invalidated_by = text(&Uuid::from_u128(2).to_string()),
            State::InvalidatedAt => row.invalidated_at = SqlValue::Integer(1),
            State::RegistryInvalidated => {
                row.invalidated_by = text(&Uuid::from_u128(2).to_string());
                row.invalidated_at = SqlValue::Integer(1);
            }
            State::RegistryMismatch => row.registry_id = text(&Uuid::from_u128(2).to_string()),
            State::DigestMismatch => row.digest = text(pin(1, "mcp:changed").definition_digest()),
            State::RegistryOnly => row.registry_id = text(&Uuid::from_u128(1).to_string()),
            State::DigestOnly => row.digest = text(pin(1, "mcp:original").definition_digest()),
            State::BothPins => {
                row.registry_id = text(&Uuid::from_u128(1).to_string());
                row.digest = text(pin(1, "mcp:original").definition_digest());
            }
            State::ActorMismatch => row.actor = "agent:someone-else".into(),
            State::ToolMismatch => row.tool = "other-tool".into(),
            State::TextExpiry => row.expires_at = text("not-a-time"),
            State::TextRequested => row.requested_at = text("not-a-time"),
            _ => {}
        }
        row
    }
    fn params(&self) -> Vec<SqlValue> {
        vec![
            text(&self.id),
            text(&self.namespace),
            text(&self.actor),
            text(&self.tool),
            text(&self.status),
            self.requested_at.clone(),
            self.expires_at.clone(),
            self.registry_id.clone(),
            self.digest.clone(),
            self.invalidated_by.clone(),
            self.invalidated_at.clone(),
        ]
    }
}

struct Fixture {
    rt: KhiveRuntime,
    registry: VerbRegistry,
}
impl Fixture {
    fn from_runtime(rt: KhiveRuntime) -> Self {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("am7-test-author".into()));
        builder.register(KgPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        let registry = builder.build().expect("real tool registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), rt.backend())
            .expect("actual tool schema");
        rt.install_edge_rules(registry.all_edge_rules());
        Self { rt, registry }
    }
    fn new() -> Self {
        let rt = KhiveRuntime::memory().expect("private memory runtime");
        assert!(rt.sql().database_path().is_none());
        Self::from_runtime(rt)
    }
    async fn write(&self, sql: &str, params: Vec<SqlValue>) -> u64 {
        self.rt
            .sql()
            .writer()
            .await
            .expect("fixture writer")
            .execute(statement(sql, params))
            .await
            .expect("fixture DML")
    }
    async fn clear(&self) {
        self.write("DELETE FROM tool_grants", vec![]).await;
    }
    async fn seed(&self, row: &Grant) {
        assert_eq!(self.write("INSERT INTO tool_grants (id,namespace,actor,tool,status,requested_at,expires_at,registry_id,definition_digest,invalidated_by_registry_id,invalidated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", row.params()).await, 1);
    }
    async fn rewrite_in_unit(&self, row: &Grant) {
        let update = statement("UPDATE tool_grants SET actor=?3,tool=?4,status=?5,requested_at=?6,expires_at=?7,registry_id=?8,definition_digest=?9,invalidated_by_registry_id=?10,invalidated_at=?11 WHERE id=?1 AND namespace=?2", row.params());
        let result = self
            .rt
            .sql()
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    let changed = writer.execute(update).await?;
                    Ok(Box::new(changed) as Box<dyn Any + Send>)
                })
            }))
            .await
            .expect("fixture change commits in actual unit");
        assert_eq!(unbox::<u64>(result), 1);
    }
    async fn unit_check(
        &self,
        namespace: &str,
        id: &str,
        actor: &str,
        tool: &str,
        now: i64,
        registration: Option<&RegistryPin>,
    ) -> Result<bool, RuntimeError> {
        let (namespace, id, actor, tool, registration) = (
            namespace.to_string(),
            id.to_string(),
            actor.to_string(),
            tool.to_string(),
            registration.cloned(),
        );
        let result = self
            .rt
            .sql()
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    let answer = grant_is_active_on(
                        writer,
                        &namespace,
                        &id,
                        &actor,
                        &tool,
                        now,
                        registration.as_ref(),
                    )
                    .await;
                    Ok(Box::new(answer) as Box<dyn Any + Send>)
                })
            }))
            .await
            .expect("by-ID actual atomic unit completes on first poll");
        unbox::<Result<bool, RuntimeError>>(result)
    }
    async fn row(&self, id: &str) -> SqlRow {
        self.rt
            .sql()
            .reader()
            .await
            .expect("stored row reader")
            .query_row(statement(
                "SELECT * FROM tool_grants WHERE namespace='local' AND id=?1",
                vec![text(id)],
            ))
            .await
            .expect("stored row query")
            .expect("named row remains present")
    }
    async fn snapshot(&self) -> Value {
        let rows = self
            .rt
            .sql()
            .reader()
            .await
            .expect("snapshot reader")
            .query_all(statement("SELECT * FROM tool_grants ORDER BY id", vec![]))
            .await
            .expect("actual grant census");
        serde_json::to_value(rows).expect("typed rows serialize")
    }
    async fn registered_invalidation(&self) -> Value {
        self.registry.dispatch("tool.register", json!({"name":TOOL,"source":"mcp:fixture","side_effect":"write","trust":"first_party","schema":{"type":"object"}})).await.expect("real registry insert invalidates grant")
    }
}
fn assert_decode<T: std::fmt::Debug>(
    result: Result<T, RuntimeError>,
    column: &str,
    id: &str,
    label: &str,
) {
    let message = match result {
        Err(RuntimeError::Internal(message)) => message,
        other => panic!("named decode must refuse ({label}): {other:?}"),
    };
    assert!(
        message.contains(id),
        "named decode lost row id ({label}): {message}"
    );
    assert!(
        message.contains(&format!("SQL column {column}")),
        "named decode lost column ({label}): {message}"
    );
}
fn assert_named(result: Result<bool, RuntimeError>, state: State, id: &str, label: &str) {
    if let Some(column) = state.error_column() {
        assert_decode(
            result,
            column,
            if matches!(state, State::Decode("id", _)) {
                "<unreadable id>"
            } else {
                id
            },
            label,
        );
    } else {
        assert_eq!(
            result.unwrap_or_else(|e| panic!("named query failed ({label}): {e}")),
            state.active(),
            "named by-ID answer ({label})"
        );
    }
}
fn assert_decision(result: Result<Decision, RuntimeError>, expected: Option<&str>, label: &str) {
    let decision = result.unwrap_or_else(|e| panic!("decide failed ({label}): {e}"));
    assert_eq!(
        decision.grant_id.as_deref(),
        expected,
        "independent decide grant ({label})"
    );
    assert_eq!(
        decision.source,
        if expected.is_some() {
            "grant"
        } else {
            "default"
        },
        "independent decide source ({label})"
    );
    assert_eq!(
        decision.decision,
        if expected.is_some() { "allow" } else { "ask" },
        "independent decide outcome ({label})"
    );
}

#[derive(Clone)]
struct Patch {
    column: &'static str,
    value: Option<SqlValue>,
}
struct ObservedReader {
    inner: Box<dyn SqlReader>,
    calls: Vec<&'static str>,
    patch: Option<Patch>,
    first_error: Option<StorageError>,
}
impl ObservedReader {
    async fn new(f: &Fixture) -> Self {
        Self {
            inner: f.rt.sql().reader().await.expect("real supplied reader"),
            calls: vec![],
            patch: None,
            first_error: None,
        }
    }
    fn enter(&mut self, method: &'static str) -> StorageResult<()> {
        self.calls.push(method);
        if let Some(error) = self.first_error.take() {
            Err(error)
        } else {
            Ok(())
        }
    }
    fn patch_rows(&self, rows: &mut [SqlRow]) {
        if let Some(patch) = &self.patch {
            for row in rows {
                if row.text("id").ok() == Some(HELD) {
                    row.columns.retain(|column| column.name != patch.column);
                    if let Some(value) = &patch.value {
                        row.columns.push(SqlColumn {
                            name: patch.column.into(),
                            value: value.clone(),
                        });
                    }
                }
            }
        }
    }
}
#[async_trait]
impl SqlReader for ObservedReader {
    async fn query_row(&mut self, s: SqlStatement) -> StorageResult<Option<SqlRow>> {
        self.enter("query_row")?;
        let row = self.inner.query_row(s).await?;
        let mut rows: Vec<_> = row.into_iter().collect();
        self.patch_rows(&mut rows);
        Ok(rows.into_iter().next())
    }
    async fn query_all(&mut self, s: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.enter("query_all")?;
        let mut rows = self.inner.query_all(s).await?;
        self.patch_rows(&mut rows);
        Ok(rows)
    }
    async fn query_page(
        &mut self,
        s: SqlStatement,
        page: PageRequest,
    ) -> StorageResult<Vec<SqlRow>> {
        self.enter("query_page")?;
        let mut rows = self.inner.query_page(s, page).await?;
        self.patch_rows(&mut rows);
        Ok(rows)
    }
    async fn query_scalar(&mut self, s: SqlStatement) -> StorageResult<Option<SqlValue>> {
        self.enter("query_scalar")?;
        self.inner.query_scalar(s).await
    }
    async fn count(&mut self, s: SqlStatement) -> StorageResult<u64> {
        self.enter("count")?;
        self.inner.count(s).await
    }
    async fn explain(&mut self, s: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.enter("explain")?;
        self.inner.explain(s).await
    }
}
async fn observed_check(
    f: &Fixture,
    state: State,
    registration: Option<&RegistryPin>,
    label: &str,
) -> Result<bool, RuntimeError> {
    let mut reader = ObservedReader::new(f).await;
    reader.patch = state.patch();
    let answer =
        grant_is_active_on(&mut reader, "local", HELD, ACTOR, TOOL, NOW, registration).await;
    assert_eq!(
        reader.calls.len(),
        1,
        "exactly one supplied-reader query ({label}): {:?}",
        reader.calls
    );
    answer
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_states_match_decide_on_atomic_writer() {
    let f = Fixture::new();
    f.seed(&Grant::live(HELD, 10, None)).await;
    assert!(f
        .unit_check("local", HELD, ACTOR, TOOL, NOW, None)
        .await
        .unwrap());
    assert_decision(
        decide(&f.rt, "local", ACTOR, TOOL, None, None).await,
        Some(HELD),
        "before actual registry invalidation",
    );
    let registered = f.registered_invalidation().await;
    let invalidated = f.row(HELD).await;
    assert_eq!(
        invalidated.text("invalidated_by_registry_id").unwrap(),
        registered["tool"]["full_id"].as_str().unwrap()
    );
    assert!(invalidated.opt_i64("invalidated_at").unwrap().is_some());
    assert!(
        !f.unit_check("local", HELD, ACTOR, TOOL, NOW, None)
            .await
            .unwrap(),
        "actual registry invalidation blocks named grant"
    );
    assert_decision(
        decide(&f.rt, "local", ACTOR, TOOL, None, None).await,
        None,
        "actual registry invalidation",
    );
    for state in stored_states() {
        f.clear().await;
        let registration = state.registration();
        if state != State::Missing {
            f.seed(&Grant::state(HELD, 10, state, registration.as_ref()))
                .await;
        }
        let before = f.snapshot().await;
        let answer = f
            .unit_check("local", HELD, ACTOR, TOOL, NOW, registration.as_ref())
            .await;
        let decision = decide(&f.rt, "local", ACTOR, TOOL, None, registration.as_ref()).await;
        assert_named(answer, state, HELD, &format!("atomic {state:?}"));
        if let Some(column) = state.error_column() {
            assert_decode(decision, column, HELD, &format!("decide {state:?}"));
        } else {
            assert_decision(
                decision,
                state.active().then_some(HELD),
                &format!("decide {state:?}"),
            );
        }
        assert_eq!(
            f.snapshot().await,
            before,
            "by-ID/decide must not mutate grants: {state:?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_reader_states_keep_shared_predicate() {
    let f = Fixture::new();
    f.seed(&Grant::live(HELD, 10, None)).await;
    assert_decision(
        decide(&f.rt, "local", ACTOR, TOOL, None, None).await,
        Some(HELD),
        "reader before actual registry invalidation",
    );
    let registered = f.registered_invalidation().await;
    let invalidated = f.row(HELD).await;
    assert_eq!(
        invalidated.text("invalidated_by_registry_id").unwrap(),
        registered["tool"]["full_id"].as_str().unwrap()
    );
    assert!(invalidated.opt_i64("invalidated_at").unwrap().is_some());
    assert_decision(
        decide(&f.rt, "local", ACTOR, TOOL, None, None).await,
        None,
        "reader actual registry invalidation",
    );
    for state in stored_states() {
        f.clear().await;
        let registration = state.registration();
        if state != State::Missing {
            f.seed(&Grant::state(HELD, 10, state, registration.as_ref()))
                .await;
        }
        let answer = decide(&f.rt, "local", ACTOR, TOOL, None, registration.as_ref()).await;
        if let Some(column) = state.error_column() {
            assert_decode(answer, column, HELD, &format!("reader-only {state:?}"));
        } else {
            assert_decision(
                answer,
                state.active().then_some(HELD),
                &format!("reader-only {state:?}"),
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_malformed_rows_refuse_without_borrowing_a_companion() {
    let f = Fixture::new();
    for state in [State::TextExpiry, State::TextRequested] {
        f.clear().await;
        f.seed(&Grant::live(HELD, 10, None)).await;
        assert!(f
            .unit_check("local", HELD, ACTOR, TOOL, NOW, None)
            .await
            .unwrap());
        f.rewrite_in_unit(&Grant::state(HELD, 10, state, None))
            .await;
        let column = state.error_column().unwrap();
        let classes =
            f.rt.sql()
                .reader()
                .await
                .unwrap()
                .query_row(statement(
                    &format!("SELECT typeof({column}) AS class FROM tool_grants WHERE id=?1"),
                    vec![text(HELD)],
                ))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(classes.text("class").unwrap(), "text");
        assert_decode(
            f.unit_check("local", HELD, ACTOR, TOOL, NOW, None).await,
            column,
            HELD,
            "real malformed named row",
        );
        assert_decode(
            decide(&f.rt, "local", ACTOR, TOOL, Some("read"), None).await,
            column,
            HELD,
            "real malformed decide row",
        );
    }
    // These BLOB cells still match pattern_match_sql's byte casts, so both
    // public consumers must select the malformed row and refuse its text decode.
    for (column, value) in [("actor", ACTOR), ("tool", TOOL)] {
        f.clear().await;
        f.seed(&Grant::live(HELD, 10, None)).await;
        assert!(f
            .unit_check("local", HELD, ACTOR, TOOL, NOW, None)
            .await
            .unwrap());
        assert_decision(
            decide(&f.rt, "local", ACTOR, TOOL, None, None).await,
            Some(HELD),
            "before selected BLOB pattern cell",
        );
        assert_eq!(
            f.write(
                &format!("UPDATE tool_grants SET {column}=?1 WHERE id=?2"),
                vec![SqlValue::Blob(value.as_bytes().to_vec()), text(HELD)]
            )
            .await,
            1
        );
        let classes =
            f.rt.sql()
                .reader()
                .await
                .unwrap()
                .query_row(statement(
                    &format!("SELECT typeof({column}) AS class FROM tool_grants WHERE id=?1"),
                    vec![text(HELD)],
                ))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(classes.text("class").unwrap(), "blob");
        assert_decode(
            f.unit_check("local", HELD, ACTOR, TOOL, NOW, None).await,
            column,
            HELD,
            "real selected BLOB named pattern",
        );
        assert_decode(
            decide(&f.rt, "local", ACTOR, TOOL, None, None).await,
            column,
            HELD,
            "real selected BLOB decide pattern",
        );
    }
    f.clear().await;
    f.seed(&Grant::state(HELD, 10, State::TextExpiry, None))
        .await;
    f.seed(&Grant::live("a-valid", 10, None)).await;
    assert_decode(
        f.unit_check("local", HELD, ACTOR, TOOL, NOW, None).await,
        "expires_at",
        HELD,
        "malformed named behind valid tie",
    );
    assert_decision(
        decide(&f.rt, "local", ACTOR, TOOL, None, None).await,
        Some("a-valid"),
        "decide selects valid earlier tie ID",
    );
    for column in [
        "id",
        "actor",
        "tool",
        "status",
        "requested_at",
        "expires_at",
    ] {
        for missing in [true, false] {
            f.clear().await;
            f.seed(&Grant::live(HELD, 10, None)).await;
            let state = State::Decode(column, missing);
            assert_named(
                observed_check(&f, state, None, "required-column supplement").await,
                state,
                HELD,
                &format!("{state:?}"),
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_terms_stay_on_the_named_row_across_age_and_ties() {
    let f = Fixture::new();
    let terms = [
        State::Revoked,
        State::Expired,
        State::InvalidatedBy,
        State::InvalidatedAt,
        State::RegistryMismatch,
        State::DigestMismatch,
        State::RegistryOnly,
        State::DigestOnly,
        State::ActorMismatch,
        State::ToolMismatch,
    ];
    let mut cases = 0;
    for state in terms {
        for tied in [false, true] {
            for fail_first in [true, false] {
                f.clear().await;
                let registration = state.registration();
                let second_at = if tied { 10 } else { 20 };
                f.seed(&Grant::live("a-first", 10, registration.as_ref()))
                    .await;
                f.seed(&Grant::live("z-second", second_at, registration.as_ref()))
                    .await;
                assert!(f
                    .unit_check("local", "a-first", ACTOR, TOOL, NOW, registration.as_ref())
                    .await
                    .unwrap());
                assert!(f
                    .unit_check("local", "z-second", ACTOR, TOOL, NOW, registration.as_ref())
                    .await
                    .unwrap());
                assert_decision(
                    decide(&f.rt, "local", ACTOR, TOOL, None, registration.as_ref()).await,
                    Some(if tied { "a-first" } else { "z-second" }),
                    "two live positive",
                );
                let (failed, at, live) = if fail_first {
                    ("a-first", 10, "z-second")
                } else {
                    ("z-second", second_at, "a-first")
                };
                f.rewrite_in_unit(&Grant::state(failed, at, state, registration.as_ref()))
                    .await;
                let label = format!("{state:?} tied={tied} failed={failed}");
                assert!(
                    !f.unit_check("local", failed, ACTOR, TOOL, NOW, registration.as_ref())
                        .await
                        .unwrap(),
                    "failed named row: {label}"
                );
                assert!(
                    f.unit_check("local", live, ACTOR, TOOL, NOW, registration.as_ref())
                        .await
                        .unwrap(),
                    "other live row: {label}"
                );
                assert_decision(
                    decide(&f.rt, "local", ACTOR, TOOL, None, registration.as_ref()).await,
                    Some(live),
                    &label,
                );
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 40);
    for failed in ["a-first", "z-second"] {
        f.clear().await;
        f.seed(&Grant::live("a-first", 10, None)).await;
        f.seed(&Grant::live("z-second", 10, None)).await;
        f.rewrite_in_unit(&Grant::state(failed, 10, State::BothPins, None))
            .await;
        let other = if failed == "a-first" {
            "z-second"
        } else {
            "a-first"
        };
        assert!(
            !f.unit_check("local", failed, ACTOR, TOOL, NOW, None)
                .await
                .unwrap(),
            "both pin columns on named tied row {failed}"
        );
        assert!(f
            .unit_check("local", other, ACTOR, TOOL, NOW, None)
            .await
            .unwrap());
        assert_decision(
            decide(&f.rt, "local", ACTOR, TOOL, None, None).await,
            Some(other),
            "both pin columns tied supplement",
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_answer_is_invariant_under_companion_placement() {
    let f = Fixture::new();
    f.seed(&Grant::live(HELD, 10, None)).await;
    assert!(f
        .unit_check("local", HELD, ACTOR, TOOL, NOW, None)
        .await
        .unwrap());
    let registered = f.registered_invalidation().await;
    let invalidated = f.row(HELD).await;
    let actual_registry_marker = invalidated
        .text("invalidated_by_registry_id")
        .unwrap()
        .to_string();
    let actual_time_marker = invalidated
        .opt_i64("invalidated_at")
        .unwrap()
        .expect("real registry invalidation time");
    assert_eq!(
        actual_registry_marker,
        registered["tool"]["full_id"].as_str().unwrap()
    );
    for state in all_states() {
        let registration = state.registration();
        let mut placements = vec![None];
        for companion in ["a-companion", "z-companion"] {
            for at in if state == State::TextRequested {
                vec![9]
            } else {
                vec![9, 10, 11]
            } {
                placements.push(Some((companion, at)));
            }
        }
        for placement in placements {
            f.clear().await;
            if state != State::Missing {
                let mut named = Grant::state(HELD, 10, state, registration.as_ref());
                if state == State::RegistryInvalidated {
                    // Reuse both typed marker values from the actual registry
                    // change, rather than inventing a different placement state.
                    named.invalidated_by = text(&actual_registry_marker);
                    named.invalidated_at = SqlValue::Integer(actual_time_marker);
                }
                f.seed(&named).await;
            }
            if let Some((id, at)) = placement {
                f.seed(&Grant::live(id, at, registration.as_ref())).await;
            }
            let label = format!("named={state:?} companion={placement:?}");
            let answer = if state.patch().is_some() {
                observed_check(&f, state, registration.as_ref(), &label).await
            } else {
                f.unit_check("local", HELD, ACTOR, TOOL, NOW, registration.as_ref())
                    .await
            };
            assert_named(answer, state, HELD, &label);
            let named_wins = state.underlying_matches()
                && (state == State::TextRequested
                    || placement.is_none_or(|(id, at)| at < 10 || at == 10 && HELD < id));
            let reader_answer =
                decide(&f.rt, "local", ACTOR, TOOL, None, registration.as_ref()).await;
            if named_wins && matches!(state, State::TextExpiry | State::TextRequested) {
                assert_decode(reader_answer, state.error_column().unwrap(), HELD, &label);
            } else {
                assert_decision(
                    reader_answer,
                    if named_wins {
                        Some(HELD)
                    } else {
                        placement.map(|(id, _)| id)
                    },
                    &label,
                );
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_actor_tool_patterns_and_namespace_are_exact() {
    let f = Fixture::new();
    for column in ["actor", "tool"] {
        for (pattern, value, matches) in [
            ("*", "anything", true),
            ("exact", "exact", true),
            ("exact", "exactly", false),
            ("Exact", "exact", false),
            ("pre*", "prefix", true),
            ("Pre*", "prefix", false),
            ("%_*", "%_literal", true),
            ("%_*", "wildcards", false),
            ("a*b", "axxb", false),
            ("a*b", "a*b", true),
            ("é*", "éclair", true),
            ("nul\0*", "nul\0tail", true),
            ("nul\0exact", "nul\0other", false),
        ] {
            f.clear().await;
            let mut row = Grant::live(HELD, 10, None);
            if column == "actor" {
                row.actor = pattern.into();
            } else {
                row.tool = pattern.into();
            }
            f.seed(&row).await;
            let (actor, tool) = if column == "actor" {
                (value, TOOL)
            } else {
                (ACTOR, value)
            };
            let label = format!("{column} pattern={pattern:?} value={value:?}");
            assert_eq!(
                f.unit_check("local", HELD, actor, tool, NOW, None)
                    .await
                    .unwrap(),
                matches,
                "named pattern answer {label}"
            );
            assert_decision(
                decide(&f.rt, "local", actor, tool, None, None).await,
                matches.then_some(HELD),
                &label,
            );
        }
    }
    f.clear().await;
    f.seed(&Grant::live(HELD, 10, None)).await;
    assert!(
        !f.unit_check("other", HELD, ACTOR, TOOL, NOW, None)
            .await
            .unwrap(),
        "grant scope differs"
    );
    assert_decision(
        decide(&f.rt, "other", ACTOR, TOOL, None, None).await,
        None,
        "grant scope differs",
    );
    assert!(
        !f.unit_check("local", "missing-id", ACTOR, TOOL, NOW, None)
            .await
            .unwrap(),
        "another live grant cannot stand in for a missing id"
    );
}

async fn consume(f: &Fixture, marker: &str) -> (bool, u64) {
    let marker = marker.to_string();
    let result = f.rt.sql().atomic_unit(Box::new(move |writer| Box::pin(async move {
        let outcome: Result<(bool,u64),RuntimeError> = async {
            let active = grant_is_active_on(writer,"local",HELD,ACTOR,TOOL,NOW,None).await?;
            let changed = if active { writer.execute(statement("UPDATE tool_grants SET decision_note=?1 WHERE namespace='local' AND id=?2",vec![text(&marker),text(HELD)])).await? } else {0};
            Ok((active,changed))
        }.await;
        Ok(Box::new(outcome) as Box<dyn Any + Send>)
    }))).await.expect("check plus dependent write completes on first poll");
    unbox::<Result<(bool, u64), RuntimeError>>(result).expect("grant check")
}
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_sees_committed_revoke_and_guards_consumption() {
    let f = Fixture::new();
    f.seed(&Grant::live(HELD, 10, None)).await;
    assert_eq!(consume(&f, "healthy-consumption").await, (true, 1));
    assert_eq!(
        f.row(HELD).await.text("decision_note").unwrap(),
        "healthy-consumption"
    );
    f.rewrite_in_unit(&Grant::state(HELD, 10, State::Revoked, None))
        .await;
    let before = serde_json::to_value(f.row(HELD).await).unwrap();
    assert_eq!(f.row(HELD).await.text("status").unwrap(), "revoked");
    assert_eq!(
        consume(&f, "must-not-consume").await,
        (false, 0),
        "prior committed revoke blocks dependent write"
    );
    assert_eq!(serde_json::to_value(f.row(HELD).await).unwrap(), before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_reader_uses_one_query_for_each_outcome() {
    let f = Fixture::new();
    for state in [
        State::Live,
        State::Missing,
        State::Revoked,
        State::TextExpiry,
    ] {
        f.clear().await;
        if state != State::Missing {
            f.seed(&Grant::state(HELD, 10, state, None)).await;
        }
        let label = format!("one-query {state:?}");
        assert_named(
            observed_check(&f, state, None, &label).await,
            state,
            HELD,
            &label,
        );
    }
}

#[derive(Debug)]
struct DriverMarker(&'static str);
impl std::fmt::Display for DriverMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for DriverMarker {}
fn variant(error: &StorageError) -> &'static str {
    match error {
        StorageError::SqliteWrite { .. } => "SqliteWrite",
        StorageError::NotFound { .. } => "NotFound",
        StorageError::AlreadyExists { .. } => "AlreadyExists",
        StorageError::Conflict { .. } => "Conflict",
        StorageError::InvalidInput { .. } => "InvalidInput",
        StorageError::Unsupported { .. } => "Unsupported",
        StorageError::BlobTooLarge { .. } => "BlobTooLarge",
        StorageError::BlobSizeMismatch { .. } => "BlobSizeMismatch",
        StorageError::BlobDigestMismatch { .. } => "BlobDigestMismatch",
        StorageError::Pool { .. } => "Pool",
        StorageError::Timeout { .. } => "Timeout",
        StorageError::AdmissionTimeout { .. } => "AdmissionTimeout",
        StorageError::Transaction { .. } => "Transaction",
        StorageError::ReadTransactionAgeEvicted { .. } => "ReadTransactionAgeEvicted",
        StorageError::ReadTransactionAgeEvictionCleanupFailed { .. } => {
            "ReadTransactionAgeEvictionCleanupFailed"
        }
        StorageError::Serialization { .. } => "Serialization",
        StorageError::IndexMaintenance { .. } => "IndexMaintenance",
        StorageError::Driver { .. } => "Driver",
        StorageError::WriteQueueFull { .. } => "WriteQueueFull",
        StorageError::WriterTaskBusy { .. } => "WriterTaskBusy",
        StorageError::WriterTaskRequestFailed { .. } => "WriterTaskRequestFailed",
        StorageError::WriterTaskTerminated { .. } => "WriterTaskTerminated",
        StorageError::Internal(..) => "Internal",
        StorageError::WriterTaskNoRuntime => "WriterTaskNoRuntime",
        StorageError::CapacityFloor { .. } => "CapacityFloor",
        StorageError::CapacityUnavailable { .. } => "CapacityUnavailable",
    }
}
fn fingerprint(error: &StorageError) -> (&'static str, String, String, Vec<(String, String)>) {
    let mut chain = vec![];
    let mut source = error.source();
    while let Some(value) = source {
        chain.push((format!("{value:?}"), value.to_string()));
        source = value.source();
    }
    (
        variant(error),
        format!("{error:?}"),
        error.to_string(),
        chain,
    )
}
fn reader_errors() -> Vec<StorageError> {
    let capability = StorageCapability::Sql;
    let a = ContentRef::from_digest_bytes(&[1; 32]);
    let b = ContentRef::from_digest_bytes(&[2; 32]);
    let mut errors = vec![
        StorageError::Timeout {
            operation: "timeout-op".into(),
        },
        StorageError::SqliteWrite {
            failure: SqliteWriteFailure {
                stage: SqliteWriteStage::Statement,
                primary_code: 19,
                extended_code: 2067,
                settlement_unknown: false,
            },
            source: Box::new(StorageError::Timeout {
                operation: "nested-timeout".into(),
            }),
        },
        StorageError::NotFound {
            capability,
            resource: "fixture",
            key: "missing-key".into(),
        },
        StorageError::AlreadyExists {
            capability,
            resource: "fixture",
            key: "duplicate-key".into(),
        },
        StorageError::Conflict {
            capability,
            operation: "conflict-op".into(),
            message: "conflict-payload".into(),
        },
        StorageError::InvalidInput {
            capability,
            operation: "input-op".into(),
            message: "input-payload".into(),
        },
        StorageError::Unsupported {
            capability,
            operation: "unsupported-op".into(),
            message: "unsupported-payload".into(),
        },
        StorageError::BlobTooLarge {
            content_ref: a.clone(),
            max_bytes: 7,
            observed_at_least: 9,
        },
        StorageError::BlobSizeMismatch {
            content_ref: a.clone(),
            metadata_bytes: 3,
            actual_bytes: 4,
        },
        StorageError::BlobDigestMismatch {
            expected: a,
            actual: b,
        },
        StorageError::Pool {
            operation: "pool-op".into(),
            message: "pool-payload".into(),
        },
        StorageError::AdmissionTimeout {
            operation: "admission-op".into(),
            timeout_ms: 17,
            pool_identity: Some("private-test-pool".into()),
        },
        StorageError::Transaction {
            operation: "transaction-op".into(),
            message: "transaction-payload".into(),
        },
        StorageError::ReadTransactionAgeEvicted {
            operation: "age-op".into(),
            max_age_secs: 23,
        },
        StorageError::ReadTransactionAgeEvictionCleanupFailed {
            operation: "cleanup-op".into(),
            max_age_secs: 29,
            message: "cleanup-payload".into(),
        },
        StorageError::Serialization {
            capability,
            message: "serialize-payload".into(),
        },
        StorageError::IndexMaintenance {
            capability,
            message: "index-payload".into(),
        },
        StorageError::driver(
            capability,
            "driver-op",
            DriverMarker("driver-source-payload"),
        ),
        StorageError::WriteQueueFull { timeout_ms: 31 },
        StorageError::WriterTaskBusy { timeout_ms: 37 },
        StorageError::WriterTaskRequestFailed {
            request_state: WriterTaskRequestState::TransactionRolledBack,
            source: Box::new(StorageError::Transaction {
                operation: "nested-op".into(),
                message: "nested-payload".into(),
            }),
        },
        StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
            sqlite_full_codes: Some((13, 778)),
        },
        StorageError::Internal("internal-payload".into()),
        StorageError::WriterTaskNoRuntime,
        StorageError::CapacityFloor {
            capability,
            volume: "private-test-volume".into(),
            available_bytes: 41,
            floor_bytes: 43,
            required_headroom_bytes: 47,
        },
    ];
    for phase in [
        CapacityUnavailablePhase::Identity,
        CapacityUnavailablePhase::Lock,
        CapacityUnavailablePhase::LockContended,
        CapacityUnavailablePhase::Probe,
    ] {
        errors.push(StorageError::CapacityUnavailable {
            capability,
            phase,
            message: format!("capacity-{phase}"),
        });
    }
    errors.push(StorageError::WriterTaskRequestFailed {
        request_state: WriterTaskRequestState::SideEffectsUnknown,
        source: Box::new(StorageError::driver(
            capability,
            "nested-driver",
            DriverMarker("nested-driver-payload"),
        )),
    });
    errors.push(StorageError::SqliteWrite {
        failure: SqliteWriteFailure {
            stage: SqliteWriteStage::Commit,
            primary_code: 13,
            extended_code: 778,
            settlement_unknown: true,
        },
        source: Box::new(StorageError::driver(
            capability,
            "commit-driver",
            DriverMarker("commit-source"),
        )),
    });
    errors
}
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_preserves_every_first_reader_error_without_retry() {
    let f = Fixture::new();
    f.seed(&Grant::live(HELD, 10, None)).await;
    assert!(f
        .unit_check("local", HELD, ACTOR, TOOL, NOW, None)
        .await
        .unwrap());
    let errors = reader_errors();
    let covered: BTreeSet<_> = errors.iter().map(variant).collect();
    assert_eq!(covered.len(), 26);
    let mut retryable_capacity = false;
    let mut nonretryable_capacity = false;
    for error in errors {
        if let StorageError::CapacityUnavailable { phase, .. } = &error {
            if *phase == CapacityUnavailablePhase::LockContended {
                assert!(error.is_retryable());
                retryable_capacity = true;
            }
            if *phase == CapacityUnavailablePhase::Probe {
                assert!(!error.is_retryable());
                nonretryable_capacity = true;
            }
        }
        let expected = fingerprint(&error);
        let mut reader = ObservedReader::new(&f).await;
        reader.first_error = Some(error);
        let result = grant_is_active_on(&mut reader, "local", HELD, ACTOR, TOOL, NOW, None).await;
        assert_eq!(
            reader.calls.len(),
            1,
            "first reader error must not be retried: {expected:?}"
        );
        let returned = match result {
            Err(RuntimeError::Storage(error)) => error,
            other => panic!("first reader error must remain typed Storage: {other:?}"),
        };
        assert_eq!(
            fingerprint(&returned),
            expected,
            "first reader error payload/source must stay unchanged"
        );
    }
    assert!(retryable_capacity && nonretryable_capacity);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn named_grant_queued_unit_completes_on_first_poll() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let path =
            std::path::PathBuf::from(std::env::var_os(DATABASE_ENV).expect("owned child database"));
        let backend =
            Arc::new(StorageBackend::sqlite_for_test(&path).expect("private queued backend"));
        backend.prepare_core_schema().expect("core schema");
        let rt = KhiveRuntime::from_backend(
            backend,
            RuntimeConfig {
                db_path: Some(path),
                events_split: None,
                actor_id: Some("am7-queued-author".into()),
                packs: vec!["kg".into(), "tool".into()],
                ..RuntimeConfig::no_embeddings()
            },
        );
        let f = Fixture::from_runtime(rt);
        let pool = f.rt.backend().pool_arc();
        assert_eq!(pool.config().write_queue_enabled, Some(true));
        assert!(
            pool.writer_task_handle().unwrap().is_some(),
            "actual queued writer is required"
        );
        f.seed(&Grant::live(HELD, 10, None)).await;
        assert_eq!(
            consume(&f, "queued-consumption").await,
            (true, 1),
            "queued unit must complete without Pending refusal"
        );
        assert_eq!(
            f.row(HELD).await.text("decision_note").unwrap(),
            "queued-consumption"
        );
        return;
    }
    let temp = tempfile::tempdir().expect("owned child directory");
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut command =
        tokio::process::Command::new(std::env::current_exe().expect("test executable"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    command
        .args([
            "--exact",
            "named_grant_queued_unit_completes_on_first_poll",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(temp.path())
        .env("HOME", &home)
        .env("KHIVE_TEST_HARNESS", "1")
        .env(CHILD_ENV, "1")
        .env(DATABASE_ENV, temp.path().join("grant.sqlite3"))
        .env("KHIVE_WRITE_QUEUE", "1")
        .env("KHIVE_WRITE_ROUTING", "compat")
        .kill_on_drop(true);
    let mut child = command.spawn().expect("owned queued test child");
    let status = match tokio::time::timeout(Duration::from_secs(120), child.wait()).await {
        Ok(result) => result.expect("owned child exits"),
        Err(_) => {
            child
                .start_kill()
                .expect("issue kill to timed-out owned child");
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(result) => {
                    result.expect("reap killed owned child");
                }
                Err(_) => panic!(
                    "queued test child exceeded120seconds; kill issued but reap exceeded5seconds"
                ),
            }
            panic!("queued test child exceeded120seconds");
        }
    };
    assert!(
        status.success(),
        "queued child must run the exact test successfully: {status}"
    );
}
