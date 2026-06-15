//! Static vocabulary: handler definitions and schema indexes for the comm pack.

use khive_types::{HandlerDef, ParamDef, Visibility};

/// Pack-auxiliary indexes for comm inbox and thread queries.
///
/// Indexes use `WHERE deleted_at IS NULL` (not `WHERE kind = 'message'`) so that
/// SQLite's index planner can match them when queries contain the parameterized
/// `kind = ?N` predicate emitted by `build_note_filter_where`.  A literal-value
/// partial index (`WHERE kind = 'message'`) cannot be used for a parameterized
/// comparison — the planner sees different predicates and falls back to a table scan.
/// `deleted_at IS NULL` is always present in filtered queries, so the partial
/// condition is always satisfied and the index is eligible.
/// `kind` is included as an indexed column so the `kind = ?N` predicate is covered.
/// Statements are idempotent (`CREATE INDEX IF NOT EXISTS`).
pub(crate) static COMM_SCHEMA_PLAN_STMTS: [&str; 3] = [
    "CREATE INDEX IF NOT EXISTS idx_comm_message_direction \
        ON notes(namespace, kind, json_extract(properties, '$.direction'), \
        json_extract(properties, '$.read'), created_at DESC) \
        WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_comm_message_thread \
        ON notes(namespace, kind, json_extract(properties, '$.thread_id'), created_at DESC) \
        WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_comm_message_to_actor \
        ON notes(namespace, kind, \
        json_extract(properties, '$.to_actor'), \
        json_extract(properties, '$.direction'), \
        json_extract(properties, '$.read'), \
        created_at DESC) \
        WHERE deleted_at IS NULL",
];

pub(crate) static COMM_HANDLERS: [HandlerDef; 5] = [
    HandlerDef {
        name: "comm.send",
        description: "Send a message, optionally threaded.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "to",
                param_type: "string",
                required: true,
                description: "Actor label to send to (e.g. \"lambda:leo\"). Both copies land in the caller's namespace; no cross-namespace write occurs.",
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Message body. Must not be empty.",
            },
            ParamDef {
                name: "subject",
                param_type: "string",
                required: false,
                description: "Optional subject line.",
            },
            ParamDef {
                name: "thread_id",
                param_type: "uuid",
                required: false,
                description: "Optional UUID to group messages into a thread.",
            },
        ],
    },
    HandlerDef {
        name: "comm.inbox",
        description: "List inbound messages for the caller.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max messages to return. Default 20, max 200.",
            },
            ParamDef {
                name: "status",
                param_type: "string",
                required: false,
                description: "Filter by read status: \"unread\" (default) | \"read\" | \"all\".",
            },
        ],
    },
    HandlerDef {
        name: "comm.read",
        description: "Mark an inbound message as read.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[ParamDef {
            name: "id",
            param_type: "string",
            required: true,
            description: "Short 8-char prefix or full UUID of the inbound message to mark read. Outbound messages cannot be marked read.",
        }],
    },
    HandlerDef {
        name: "comm.reply",
        description: "Reply to a message, threading linkage.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "string",
                required: true,
                description: "Short 8-char prefix or full UUID of the message being replied to.",
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Reply body. Must not be empty.",
            },
        ],
    },
    HandlerDef {
        name: "comm.thread",
        description: "Retrieve all messages in a conversation thread, ordered chronologically.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "string",
                required: true,
                description: "Thread root: short 8-char prefix or full UUID of the originating message.",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max messages to return. Default 100, max 500.",
            },
        ],
    },
];
