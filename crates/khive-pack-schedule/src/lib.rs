//! pack-schedule — Schedule pack (ADR-040).
pub mod handlers;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, Visibility};

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

static SCHEDULE_HANDLERS: [HandlerDef; 4] = [
    HandlerDef {
        name: "remind",
        description: "Create a time-triggered reminder.",
        visibility: Visibility::Verb,
    },
    HandlerDef {
        name: "schedule",
        description: "Schedule a future verb dispatch.",
        visibility: Visibility::Verb,
    },
    HandlerDef {
        name: "agenda",
        description: "List upcoming scheduled events.",
        visibility: Visibility::Verb,
    },
    HandlerDef {
        name: "cancel",
        description: "Cancel a scheduled event.",
        visibility: Visibility::Verb,
    },
];

impl SchedulePack {
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

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "remind" => handlers::handle_remind(self.runtime(), token, params).await,
            "schedule" => handlers::handle_schedule(self.runtime(), token, params).await,
            "agenda" => handlers::handle_agenda(self.runtime(), token, params).await,
            "cancel" => handlers::handle_cancel(self.runtime(), token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "schedule pack does not handle verb {verb:?}"
            ))),
        }
    }
}
