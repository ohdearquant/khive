//! `TaskHook` — gtd's per-kind specialization for the `task` note kind.
//!
//! Implements the `KindHook` extension point for the pack standard. Normalises
//! user-facing GTD fields into the kg storage shape on `prepare_create`, and
//! creates `depends_on` graph edges on `after_create` (best-effort). GTD
//! lifecycle semantics are documented in `docs/design.md`.

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, Namespace, RuntimeError};

use crate::task_create::{link_depends_on_edges, prepare_task_create, TaskCreateInput};

#[derive(Debug, Default)]
/// KindHook implementation for the `task` note kind; normalises GTD fields on create.
pub struct TaskHook;

#[async_trait]
impl KindHook for TaskHook {
    async fn prepare_create(
        &self,
        runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let token = args
            .get("namespace")
            .and_then(Value::as_str)
            .and_then(|s| Namespace::parse(s).ok())
            .map(|ns| runtime.authorize(ns))
            .unwrap_or_else(|| runtime.authorize(Namespace::local()))?;

        // #625/#626: this generic `create(kind="note", note_kind="task")`
        // entry point and `gtd.assign` (`GtdPack::handle_assign` in
        // handlers.rs) both normalize/validate through
        // `task_create::prepare_task_create` so status/priority checks,
        // `depends_on` resolution, and `context_entity_id` handling can't
        // drift between the two paths again.
        let input = TaskCreateInput::from_create_args(args)?;
        let prepared = prepare_task_create(runtime, &token, input).await?;
        prepared.apply_to_create_args(args)?;

        Ok(())
    }

    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError> {
        let token = args
            .get("namespace")
            .and_then(Value::as_str)
            .and_then(|s| Namespace::parse(s).ok())
            .map(|ns| runtime.authorize(ns))
            .unwrap_or_else(|| runtime.authorize(Namespace::local()))?;

        if let Some(properties) = args.get("properties") {
            link_depends_on_edges(runtime, &token, id, properties, "task hook").await;
        }

        Ok(())
    }
}
