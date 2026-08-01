//! Schedule pack vocabulary — handler definitions, param schemas, and auxiliary SQL.

use khive_types::{HandlerDef, ParamDef, Visibility};

/// Pack-auxiliary indexes for agenda scans and creator-provenance lookup.
///
/// Uses `WHERE deleted_at IS NULL` instead of `WHERE kind = 'scheduled_event'` so
/// that the parameterized `kind = ?N` predicate in `build_note_filter_where` can
/// use this index.  A literal-value partial condition (`WHERE kind = 'scheduled_event'`)
/// is invisible to the planner when the query uses a bound parameter for `kind`.
/// `namespace` and `kind` are included as indexed columns for efficient namespace+kind
/// range scans. The statements are idempotent (`CREATE INDEX IF NOT EXISTS`) and are NOT
/// part of the core versioned migration chain. The second index binds provenance lookup
/// to namespace + internal verb + target note + outcome, avoiding an event-history scan
/// for each fired row.
pub(crate) static SCHEDULE_SCHEMA_PLAN_STMTS: [&str; 2] = [
    "CREATE INDEX IF NOT EXISTS idx_schedule_trigger \
        ON notes(namespace, kind, json_extract(properties, '$.trigger_at')) \
        WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_schedule_creator_provenance \
        ON events(namespace, verb, target_id, outcome)",
];

pub(crate) static SCHEDULE_HANDLERS: [HandlerDef; 4] = [
    HandlerDef {
        name: "schedule.remind",
        description: "Deliver a time-triggered reminder to the creating actor's inbox.",
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
        description: "Schedule a future verb dispatch. NESTED-ACTION EXAMPLE (issue #110): \
                       schedule.schedule(action=\"schedule.remind(content=\\\"renew the \
                       domain\\\", at=\\\"2027-06-01T09:00:00Z\\\")\", \
                       at=\"2027-05-25T09:00:00Z\") — the OUTER `at` (2027-05-25) is when \
                       THIS schedule fires and the stored `action` gets dispatched (i.e. when \
                       the reminder gets CREATED); the INNER `at` (2027-06-01) is the nested \
                       `schedule.remind` call's own required argument and becomes THAT \
                       reminder's trigger time (i.e. when the newly-created reminder itself \
                       fires). The two are independent and commonly differ — this schedules \
                       the creation of a reminder a week ahead of when the reminder should go \
                       off. If the nested action's own verb requires `at` (or any other \
                       required param), it must be supplied on the nested call, exactly as if \
                       that verb were being called directly — replay dispatches the stored \
                       `action` string verbatim with no injection from the outer `at`.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "action",
                param_type: "string",
                required: true,
                description: "Verb dispatch payload to execute at the trigger time — a \
                               complete, self-sufficient verb call including ALL of that \
                               verb's OWN required params (e.g. \
                               \"schedule.remind(content=\\\"hello\\\", \
                               at=\\\"2027-06-01T09:00:00Z\\\")\" — `schedule.remind` requires \
                               its own `at`, separate from and independent of this verb's `at` \
                               below; see the handler description for the full worked \
                               example). Must not be empty.",
            },
            ParamDef {
                name: "at",
                param_type: "string",
                required: true,
                description: "Trigger time in RFC 3339 format (e.g. \"2026-06-01T09:00:00Z\") — \
                               when THIS schedule fires and `action` gets dispatched. This is \
                               independent of any `at` the nested `action` verb itself \
                               requires (see the handler description). Must not be empty.",
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
