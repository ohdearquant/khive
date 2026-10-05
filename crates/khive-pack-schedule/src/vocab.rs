//! Schedule pack vocabulary — handler definitions and param schemas.

use khive_types::{HandlerDef, IdResolutionMode, ParamDef, Visibility};

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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "at",
                param_type: "string",
                required: true,
                description: "Trigger time in RFC 3339 format (e.g. \"2026-06-01T09:00:00Z\"). Must not be empty.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "repeat",
                param_type: "string",
                required: false,
                description: "Recurrence, evaluated in UTC (never local time): \"daily\" | \"weekly\" | \"monthly\" | \"every:<N><s|m|h|d>\" (an interval from the previous trigger, e.g. \"every:15m\") | a five-field cron expression, minute hour day-of-month month day-of-week in UTC (e.g. \"0 9 * * 1\" is 09:00 UTC on Mondays). Anything else is rejected at creation, so a stored recurrence is always one the executor can advance.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "schedule.schedule",
        description: "Schedule a future verb dispatch. NESTED-ACTION EXAMPLE: \
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "at",
                param_type: "string",
                required: true,
                description: "Trigger time in RFC 3339 format (e.g. \"2026-06-01T09:00:00Z\") — \
                               when THIS schedule fires and `action` gets dispatched. This is \
                               independent of any `at` the nested `action` verb itself \
                               requires (see the handler description). Must not be empty.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "repeat",
                param_type: "string",
                required: false,
                description: "Recurrence, evaluated in UTC (never local time): \"daily\" | \"weekly\" | \"monthly\" | \"every:<N><s|m|h|d>\" (an interval from the previous trigger, e.g. \"every:15m\") | a five-field cron expression, minute hour day-of-month month day-of-week in UTC (e.g. \"0 9 * * 1\" is 09:00 UTC on Mondays). Anything else is rejected at creation, so a stored recurrence is always one the executor can advance.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "schedule.agenda",
        description: "List upcoming scheduled events ordered by UTC trigger instant, stored timestamp text, then UUID. Round-trip non-null next.after verbatim and next.after_id with the same time window until an empty page returns next=null; continuation is exclusive and does not pin a snapshot or promise more rows.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "from",
                param_type: "string",
                required: false,
                description: "Inclusive start of time window in RFC 3339 format. Omit to start from earliest pending event.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "to",
                param_type: "string",
                required: false,
                description: "Inclusive end of time window in RFC 3339 format. Omit to include all future events.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "after",
                param_type: "string",
                required: false,
                description: "Exclusive continuation timestamp in RFC 3339 format, returned as next.after. Preserve its original text and supply together with after_id; existing from/to filters still apply.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "after_id",
                param_type: "string",
                required: false,
                description: "Continuation UUID, returned as next.after_id. Supply together with after; UUID breaks ties with the same UTC instant and stored timestamp text.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max events to return. Default 20, max 200.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
            description: "Complete UUID or unique 8+ hex prefix of the scheduled event to cancel. \
                          Prefix resolution searches the caller's primary namespace.",
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    },
];
