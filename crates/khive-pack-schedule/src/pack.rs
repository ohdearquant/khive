//! `SchedulePack` implementation — registers schema, vocab, and verb handlers with the runtime.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, SchemaPlan, VerbRegistry};
use khive_types::{HandlerDef, Pack};

use crate::handlers;
use crate::vocab::{SCHEDULE_HANDLERS, SCHEDULE_SCHEMA_PLAN_STMTS};

/// Schedule pack — stores time-triggered reminders and verb dispatches.
///
/// Intent storage only: the pack creates and queries `scheduled_event` notes.
/// The execution environment delivers reminder content to the creating actor's
/// inbox and dispatches scheduled payloads when their triggers become due.
pub struct SchedulePack {
    runtime: KhiveRuntime,
}

impl Pack for SchedulePack {
    const NAME: &'static str = "schedule";
    const NOTE_KINDS: &'static [&'static str] = &["scheduled_event"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &SCHEDULE_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl SchedulePack {
    /// Create a new `SchedulePack` backed by the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

struct SchedulePackFactory;

impl khive_runtime::PackFactory for SchedulePackFactory {
    fn name(&self) -> &'static str {
        "schedule"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(SchedulePack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&SchedulePackFactory) }

#[async_trait]
impl PackRuntime for SchedulePack {
    fn name(&self) -> &str {
        <SchedulePack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <SchedulePack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <SchedulePack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &SCHEDULE_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <SchedulePack as Pack>::REQUIRES
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "schedule",
            statements: &SCHEDULE_SCHEMA_PLAN_STMTS,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "schedule.remind" => handlers::handle_remind(self.runtime(), token, params).await,
            "schedule.schedule" => {
                handlers::handle_schedule(self.runtime(), token, registry, params).await
            }
            "schedule.agenda" => handlers::handle_agenda(self.runtime(), token, params).await,
            "schedule.cancel" => handlers::handle_cancel(self.runtime(), token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "schedule pack does not handle verb {verb:?}"
            ))),
        }
    }
}
