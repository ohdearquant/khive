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
///
/// The `idx_comm_message_external_id` UNIQUE index is NOT listed here; it is
/// created by the V5 schema migration (`005-unique-comm-external-id.sql`), which
/// is the sole durable authority for that index.
pub(crate) static COMM_SCHEMA_PLAN_STMTS: [&str; 4] = [
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
    COMM_CHANNEL_CURSOR_SCHEMA_STMT,
];

/// Pack-owned auxiliary cursor table for durable channel poll progress
/// (issue #449): one row per `(channel_kind, channel_slug)`, holding the
/// transport-neutral checkpoint fields from `khive_channel::ChannelCheckpoint`.
/// For IMAP, `generation` is `UIDVALIDITY` and `high_water` is the greatest
/// durably handled UID. `source` detects a host/port/mailbox/folder change
/// under the same registry identity, so a stale checkpoint is never applied
/// to a different configuration.
///
/// Idempotent (`CREATE TABLE IF NOT EXISTS`), applied at boot via
/// `schema_plan` and shared verbatim with `handle_cursor_get`/
/// `handle_cursor_commit`'s lazy bootstrap for in-memory/test runtimes that
/// never run the boot-time schema plan.
pub(crate) const COMM_CHANNEL_CURSOR_SCHEMA_STMT: &str =
    "CREATE TABLE IF NOT EXISTS comm_channel_cursor (\
    channel_kind TEXT NOT NULL CHECK (length(trim(channel_kind)) > 0),\
    channel_slug TEXT NOT NULL CHECK (length(trim(channel_slug)) > 0),\
    source TEXT NOT NULL CHECK (length(trim(source)) > 0),\
    generation INTEGER NOT NULL CHECK (generation > 0),\
    high_water INTEGER CHECK (high_water IS NULL OR high_water > 0),\
    updated_at INTEGER NOT NULL,\
    PRIMARY KEY (channel_kind, channel_slug)\
)";

pub(crate) static COMM_HANDLERS: [HandlerDef; 11] = [
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
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Structured provenance tags (e.g. run id, job id, traffic class), persisted verbatim to `properties[\"tags\"]` on both the outbound and inbound copies.",
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
            ParamDef {
                name: "from_actor",
                param_type: "string",
                required: false,
                description: "Exact match on the sender's actor label (`properties.from_actor`). Mutually exclusive with `from_prefix`.",
            },
            ParamDef {
                name: "from_prefix",
                param_type: "string",
                required: false,
                description: "Prefix match on the sender's actor label (e.g. `\"agent:khive:\"` selects all agents under one namespace). Mutually exclusive with `from_actor`.",
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
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Structured provenance tags, persisted verbatim to `properties[\"tags\"]` on both the outbound and inbound copies of the reply.",
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
                description: "Max messages to return. Default 100, max 500. Truncation applies after ordering, so order=\"desc\" + limit returns the newest `limit` messages.",
            },
            ParamDef {
                name: "order",
                param_type: "string",
                required: false,
                description: "Ordering of returned messages: \"asc\" (default, chronological) | \"desc\" (newest first).",
            },
            ParamDef {
                name: "after",
                param_type: "string",
                required: false,
                description: "Cursor: a message id (short prefix or full UUID) or an RFC 3339 timestamp (any valid form, e.g. whole-second `Z` or `+00:00` offset). An id cursor ties-break on (created_at, full_id) so equal-timestamp messages are never skipped or duplicated. Only messages strictly after that point in the chosen `order` are returned; an unparseable value is a hard error.",
            },
        ],
    },
    HandlerDef {
        name: "comm.ingest",
        description: "Ingest an inbound message from a channel adapter. Subhandler — not callable on the MCP wire.",
        visibility: Visibility::Subhandler,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: true,
                description: "Target namespace for the ingested message note.",
            },
            ParamDef {
                name: "from",
                param_type: "string",
                required: true,
                description: "Sender address in `channel-kind:addr` form (e.g. `email:alice@example.com`).",
            },
            ParamDef {
                name: "to",
                param_type: "string",
                required: true,
                description: "Recipient address in `channel-kind:addr` form.",
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Message body text.",
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
                description: "Optional internal thread UUID. When absent, a new thread root is created.",
            },
            ParamDef {
                name: "channel_kind",
                param_type: "string",
                required: false,
                description: "Channel kind identifier (e.g. `email`).",
            },
            ParamDef {
                name: "external_id",
                param_type: "string",
                required: false,
                description: "Stable transport dedup key. For email: `imap:{host}:{uidvalidity}:{uid}`. Duplicate messages are silently ignored.",
            },
            ParamDef {
                name: "sent_at",
                param_type: "string",
                required: false,
                description: "RFC 3339 timestamp of the original message.",
            },
            ParamDef {
                name: "correlation_external_id",
                param_type: "string",
                required: false,
                description: "External correlation key used to resolve the thread (e.g. X-Khive-Thread-ID or In-Reply-To header value).",
            },
            ParamDef {
                name: "wire_message_id",
                param_type: "string",
                required: false,
                description: "This message's own RFC 822 Message-ID (including angle brackets), distinct from `external_id` (the transport dedup key). Persisted so a later reply can set In-Reply-To/References.",
            },
            ParamDef {
                name: "wire_references",
                param_type: "string",
                required: false,
                description: "This message's own RFC 822 References header value, verbatim. Persisted so a later reply can extend the full ancestor chain instead of truncating it to the immediate parent.",
            },
            ParamDef {
                name: "metadata",
                param_type: "object",
                required: false,
                description: "Optional transport-layer metadata passthrough, merged additively into the stored note's properties (never overrides an already-set field). Generic and channel-agnostic; the email channel uses it for quarantine markers (quarantined, quarantine_reason, quarantine_claimed_from — ADR-056 Amendment 2026-07-02).",
            },
        ],
    },
    HandlerDef {
        name: "comm.heartbeat",
        description: "Persist a per-channel-credential heartbeat row after a poll attempt. \
                       Subhandler — not callable on the MCP wire; only the daemon's channel \
                       poll loop calls this (khive #606). The row is ALWAYS pinned to \
                       `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` (\"local\") regardless of \
                       the caller's dispatch namespace — heartbeat rows are an operational \
                       surface, not message data, so they never follow \
                       `KHIVE_EMAIL_INGEST_NAMESPACE` or any other caller-chosen namespace.",
        visibility: Visibility::Subhandler,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: true,
                description: "Dispatch routing key (ADR-007 Rule 3 explicit escape) consumed \
                              by `VerbRegistry::dispatch` to mint the call's `NamespaceToken`. \
                              Callers should always pass \
                              `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE`: the persisted row's \
                              actual namespace is hardcoded to that constant inside the \
                              handler regardless of this value, so a different value here \
                              does not redirect where the row lands.",
            },
            ParamDef {
                name: "channel_kind",
                param_type: "string",
                required: true,
                description: "Channel kind identifier (e.g. `email`).",
            },
            ParamDef {
                name: "channel_slug",
                param_type: "string",
                required: true,
                description: "Stable per-credential identifier distinguishing accounts of the same kind (e.g. the mailbox address). Never `channel_kind` alone — two accounts of the same kind must not collapse into one row.",
            },
            ParamDef {
                name: "outcome",
                param_type: "string",
                required: true,
                description: "Poll outcome for this attempt: \"success\" or \"failure\".",
            },
            ParamDef {
                name: "error_class",
                param_type: "string",
                required: false,
                description: "Error class, required when outcome is \"failure\". Open string enum; v1 values: auth | transport | config. Callers must tolerate unknown classes.",
            },
            ParamDef {
                name: "error_message",
                param_type: "string",
                required: false,
                description: "Human-readable error detail when outcome is \"failure\". Must carry a message class, never raw secrets or wire headers.",
            },
            ParamDef {
                name: "at",
                param_type: "string",
                required: false,
                description: "RFC 3339 timestamp of this poll attempt. Defaults to now.",
            },
        ],
    },
    HandlerDef {
        name: "comm.health",
        description: "Read-only per-channel health snapshot (khive #606). Returns the \
                       daemon-persisted heartbeat row for every known channel: timestamps \
                       and consecutive-failure counts only — never a computed healthy bool. \
                       Health judgment belongs to the caller. Reads from the caller's injected \
                       namespace (khive #877) — `token.namespace()`, the same explicit \
                       `namespace=` escape / \"local\" default every other comm verb resolves \
                       (ADR-007 Rev 6 Rule 3). An unscoped call defaults to \"local\", matching \
                       the namespace heartbeat rows are persisted under; a call with an \
                       explicit non-local `namespace=` sees only that namespace's rows, never \
                       \"local\"'s.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "comm.probe",
        description: "Read-only poll for new inbound message metadata and stale unread count.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "actor",
                param_type: "string",
                required: true,
                description: "Actor label whose inbound queue is probed, e.g. \"lambda:leo\".",
            },
            ParamDef {
                name: "since_us",
                param_type: "integer",
                required: false,
                description: "Opaque cursor round-tripped from a previous comm.probe response's cursor_us; only messages committed after it are returned. Omit for a baseline-first probe. Not a computable timestamp.",
            },
            ParamDef {
                name: "stale_minutes",
                param_type: "integer",
                required: false,
                description: "Unread age threshold in minutes. Default 20.",
            },
        ],
    },
    HandlerDef {
        name: "comm.cursor_get",
        description: "Read the persisted channel poll checkpoint for (channel_kind, channel_slug), \
                       or null if none exists. Subhandler — not callable on the MCP wire; only the \
                       daemon's channel poll loop (khive #449) calls this.",
        visibility: Visibility::Subhandler,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "channel_kind",
                param_type: "string",
                required: true,
                description: "Channel kind identifier (e.g. `email`).",
            },
            ParamDef {
                name: "channel_slug",
                param_type: "string",
                required: true,
                description: "Stable per-credential identifier distinguishing accounts of the same kind.",
            },
        ],
    },
    HandlerDef {
        name: "comm.cursor_commit",
        description: "Persist a channel poll checkpoint for (channel_kind, channel_slug), replacing \
                       any prior row for that identity. Subhandler — not callable on the MCP wire; \
                       only the daemon's channel poll loop calls this, and only after every envelope \
                       in the page has been durably accepted by comm.ingest (khive #449).",
        visibility: Visibility::Subhandler,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "channel_kind",
                param_type: "string",
                required: true,
                description: "Channel kind identifier (e.g. `email`).",
            },
            ParamDef {
                name: "channel_slug",
                param_type: "string",
                required: true,
                description: "Stable per-credential identifier distinguishing accounts of the same kind.",
            },
            ParamDef {
                name: "source",
                param_type: "string",
                required: true,
                description: "Stable, non-secret identity of the remote source/configuration (e.g. \
                               `imap+tls:{host}:{port}:{mailbox}:INBOX`). A mismatch against the \
                               stored row's source is how the caller detects a configuration change.",
            },
            ParamDef {
                name: "generation",
                param_type: "integer",
                required: true,
                description: "Remote identity epoch (e.g. IMAP UIDVALIDITY). Must be a positive integer.",
            },
            ParamDef {
                name: "high_water",
                param_type: "integer",
                required: false,
                description: "Greatest durably handled remote sequence value (e.g. IMAP UID). Omit \
                               or null to reset progress within the generation (e.g. right after a \
                               UIDVALIDITY change with no messages selected yet).",
            },
        ],
    },
];
