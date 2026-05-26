//! pack-schedule — Schedule pack (ADR-040).
pub mod handlers;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, SchemaPlan, VerbRegistry};
use khive_types::{HandlerDef, Pack, ParamDef, Visibility};

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

/// ADR-040 §283-291: pack-auxiliary index for agenda() efficiency.
///
/// A partial index on `properties.trigger_at` (JSON-extracted) scoped to
/// `scheduled_event` notes enables efficient range scans in `agenda()`.
/// The statement is idempotent (`CREATE INDEX IF NOT EXISTS`) and is NOT
/// part of the core versioned migration chain (ADR-015).
pub(crate) static SCHEDULE_SCHEMA_PLAN_STMTS: [&str; 1] =
    ["CREATE INDEX IF NOT EXISTS idx_schedule_trigger \
        ON notes(json_extract(properties, '$.trigger_at')) \
        WHERE kind = 'scheduled_event'"];

static SCHEDULE_HANDLERS: [HandlerDef; 4] = [
    HandlerDef {
        name: "remind",
        description: "Create a time-triggered reminder.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Reminder message. Must not be empty.",
            },
            ParamDef {
                name: "at",
                param_type: "string",
                required: true,
                description: "Trigger time in RFC 3339 format (e.g. \"2026-06-01T09:00:00Z\"). Must not be empty.",
            },
            ParamDef {
                name: "repeat",
                param_type: "string",
                required: false,
                description: "Recurrence: \"daily\" | \"weekly\" | \"monthly\" | 5-field cron expression (e.g. \"0 9 * * 1\").",
            },
        ],
    },
    HandlerDef {
        name: "schedule",
        description: "Schedule a future verb dispatch.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "action",
                param_type: "string",
                required: true,
                description: "Verb dispatch payload to execute at the trigger time (e.g. \"remind(content=\\\"hello\\\")\"). Must not be empty.",
            },
            ParamDef {
                name: "at",
                param_type: "string",
                required: true,
                description: "Trigger time in RFC 3339 format (e.g. \"2026-06-01T09:00:00Z\"). Must not be empty.",
            },
            ParamDef {
                name: "repeat",
                param_type: "string",
                required: false,
                description: "Recurrence: \"daily\" | \"weekly\" | \"monthly\" | 5-field cron expression (e.g. \"0 9 * * 1\").",
            },
        ],
    },
    HandlerDef {
        name: "agenda",
        description: "List upcoming scheduled events.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "from",
                param_type: "string",
                required: false,
                description: "Start of time window in RFC 3339 format. Omit to start from earliest pending event.",
            },
            ParamDef {
                name: "to",
                param_type: "string",
                required: false,
                description: "End of time window in RFC 3339 format. Omit to include all future events.",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max events to return. Default 20, max 200.",
            },
        ],
    },
    HandlerDef {
        name: "cancel",
        description: "Cancel a scheduled event.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[ParamDef {
            name: "id",
            param_type: "string",
            required: true,
            description: "Full UUID of the scheduled event to cancel.",
        }],
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

#[cfg(test)]
mod help_tests {
    use super::*;
    use khive_types::Pack;

    fn find_handler(name: &str) -> &'static HandlerDef {
        SchedulePack::HANDLERS
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("handler {name:?} not found in schedule pack"))
    }

    #[test]
    fn remind_has_required_content_and_at() {
        let h = find_handler("remind");
        assert!(!h.params.is_empty(), "remind must have non-empty params");
        let content = h
            .params
            .iter()
            .find(|p| p.name == "content")
            .expect("remind must have 'content'");
        assert!(content.required, "remind.content must be required");
        let at = h
            .params
            .iter()
            .find(|p| p.name == "at")
            .expect("remind must have 'at'");
        assert!(at.required, "remind.at must be required");
    }

    #[test]
    fn remind_has_optional_repeat() {
        let h = find_handler("remind");
        let repeat = h
            .params
            .iter()
            .find(|p| p.name == "repeat")
            .expect("remind must have 'repeat'");
        assert!(!repeat.required, "remind.repeat must be optional");
    }

    #[test]
    fn schedule_has_required_action_and_at() {
        let h = find_handler("schedule");
        assert!(!h.params.is_empty(), "schedule must have non-empty params");
        let action = h
            .params
            .iter()
            .find(|p| p.name == "action")
            .expect("schedule must have 'action'");
        assert!(action.required, "schedule.action must be required");
        let at = h
            .params
            .iter()
            .find(|p| p.name == "at")
            .expect("schedule must have 'at'");
        assert!(at.required, "schedule.at must be required");
    }

    #[test]
    fn schedule_has_optional_repeat() {
        let h = find_handler("schedule");
        let repeat = h
            .params
            .iter()
            .find(|p| p.name == "repeat")
            .expect("schedule must have 'repeat'");
        assert!(!repeat.required, "schedule.repeat must be optional");
    }

    #[test]
    fn agenda_has_optional_from_to_limit() {
        let h = find_handler("agenda");
        assert!(!h.params.is_empty(), "agenda must have non-empty params");
        for name in ["from", "to", "limit"] {
            let p = h
                .params
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("agenda must have {name:?}"));
            assert!(!p.required, "agenda.{name} must be optional");
        }
    }

    #[test]
    fn cancel_has_required_id() {
        let h = find_handler("cancel");
        assert!(!h.params.is_empty(), "cancel must have non-empty params");
        let id = h
            .params
            .iter()
            .find(|p| p.name == "id")
            .expect("cancel must have 'id'");
        assert!(id.required, "cancel.id must be required");
    }

    #[test]
    fn all_schedule_handlers_have_non_empty_params() {
        for handler in SchedulePack::HANDLERS {
            assert!(
                !handler.params.is_empty(),
                "schedule handler {:?} must have non-empty params",
                handler.name
            );
        }
    }
}
