//! Schedule pack vocabulary — handler definitions, param schemas, and auxiliary SQL.

use khive_types::{HandlerDef, ParamDef, Visibility};

/// Pack-auxiliary index for agenda() efficiency.
///
/// Uses `WHERE deleted_at IS NULL` instead of `WHERE kind = 'scheduled_event'` so
/// that the parameterized `kind = ?N` predicate in `build_note_filter_where` can
/// use this index.  A literal-value partial condition (`WHERE kind = 'scheduled_event'`)
/// is invisible to the planner when the query uses a bound parameter for `kind`.
/// `namespace` and `kind` are included as indexed columns for efficient namespace+kind
/// range scans.  The statement is idempotent (`CREATE INDEX IF NOT EXISTS`) and is NOT
/// part of the core versioned migration chain.
pub(crate) static SCHEDULE_SCHEMA_PLAN_STMTS: [&str; 1] =
    ["CREATE INDEX IF NOT EXISTS idx_schedule_trigger \
        ON notes(namespace, kind, json_extract(properties, '$.trigger_at')) \
        WHERE deleted_at IS NULL"];

pub(crate) static SCHEDULE_HANDLERS: [HandlerDef; 4] = [
    HandlerDef {
        name: "schedule.remind",
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
                description: "Recurrence: \"daily\" | \"weekly\" | \"monthly\" | limited 5-field form using only '*' or one in-range integer per field (e.g. \"0 9 * * 1\"); cron operators (steps, ranges, lists) are not accepted.",
            },
        ],
    },
    HandlerDef {
        name: "schedule.schedule",
        description: "Schedule a future verb dispatch.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "action",
                param_type: "string",
                required: true,
                description: "Verb dispatch payload to execute at the trigger time (e.g. \"schedule.remind(content=\\\"hello\\\")\"). Must not be empty.",
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
                description: "Recurrence: \"daily\" | \"weekly\" | \"monthly\" | limited 5-field form using only '*' or one in-range integer per field (e.g. \"0 9 * * 1\"); cron operators (steps, ranges, lists) are not accepted.",
            },
        ],
    },
    HandlerDef {
        name: "schedule.agenda",
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
        name: "schedule.cancel",
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
