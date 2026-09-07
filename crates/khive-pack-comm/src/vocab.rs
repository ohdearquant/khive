//! Static vocabulary: handler definitions and schema indexes for the comm pack.

use khive_types::{HandlerDef, IdResolutionMode, ParamDef, Visibility};

/// Pack-auxiliary indexes for comm inbox and thread queries (idempotent). See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#vocabrscomm_schema_plan_stmts for
/// why they filter on `deleted_at IS NULL` rather than a literal `kind` value,
/// and why `idx_comm_message_external_id` is deliberately absent from this list.
pub(crate) static COMM_SCHEMA_PLAN_STMTS: [&str; 5] = [
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
    "CREATE INDEX IF NOT EXISTS idx_comm_message_outbound_ref \
        ON notes(namespace, kind, json_extract(properties, '$.direction'), \
        json_extract(properties, '$.from_actor'), \
        json_extract(properties, '$.outbound_ref')) \
        WHERE deleted_at IS NULL",
    COMM_CHANNEL_CURSOR_SCHEMA_STMT,
];

/// Pack-owned auxiliary cursor table for durable channel poll progress (issue
/// #449), idempotent (`CREATE TABLE IF NOT EXISTS`). See
/// crates/khive-pack-comm/docs/api/probe-cursor.md#vocabrscomm_channel_cursor_schema_stmt
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

pub(crate) static COMM_HANDLERS: [HandlerDef; 14] = [
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Message body. Must not be empty.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "subject",
                param_type: "string",
                required: false,
                description: "Optional subject line.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "thread_id",
                param_type: "uuid",
                required: false,
                description: "Optional full UUID to group messages into a thread. A short prefix would require scoped resolution and is rejected because the thread root is an explicit stable reference.",
                resolution_mode: IdResolutionMode::FullUuidOnlyScopedToPrimary,
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Structured provenance tags (e.g. run id, job id, traffic class), persisted verbatim to `properties[\"tags\"]` on both the outbound and inbound copies.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "self_send",
                param_type: "boolean",
                required: false,
                description: "Explicitly allow delivery when `to` matches the configured sender actor. Defaults to false: such self-addressed sends are rejected unless this is true. The anonymous `local` fallback is exempt.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "comm.delivered",
        description: "Confirm whether the caller's internal inbound sibling exists for an \
                      outbound comm.send/comm.reply UUID. This is a read-only sender-scoped \
                      dual-write confirmation, not external transport (for example SMTP) \
                      delivery status.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[ParamDef {
            name: "id",
            param_type: "uuid",
            required: true,
            description: "Full UUID returned as full_id by comm.send or comm.reply, or surfaced \
                          as outbound_id in an ambiguous atomic-write error. A full UUID is \
                          required because it is the exact correlation key.",
            resolution_mode: IdResolutionMode::FullUuidOnlyScopedToPrimary,
        }],
    },
    HandlerDef {
        name: "comm.inbox",
        description: "List and page through the caller's filtered inbound or sent messages, optionally waiting for a new matching message. Defaults to the inbound inbox.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max messages to return. Default 20, max 200.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "box",
                param_type: "string",
                required: false,
                description: "Message box: \"inbox\" (default, actor-addressed inbound rows) | \"sent\" (outbound rows authored by the caller).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Zero-based offset in the fully-filtered newest-first result set. Default 0. Follow `next_offset` until it is null to enumerate every match without changing read state.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "status",
                param_type: "string",
                required: false,
                description: "Inbox-only read-status filter: \"unread\" (default) | \"read\" | \"all\". Rejected for box=\"sent\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "wait_ms",
                param_type: "integer",
                required: false,
                description: "Long-poll budget in milliseconds. Returns immediately when the initial query matches; otherwise waits for a new matching message, up to 30000 ms. Default 0 (no wait).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "from_actor",
                param_type: "string",
                required: false,
                description: "Inbox-only exact match on the sender's actor label (`properties.from_actor`). Mutually exclusive with `from_prefix`.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "from_prefix",
                param_type: "string",
                required: false,
                description: "Inbox-only prefix match on the sender's actor label (e.g. `\"agent:khive:\"` selects all agents under one namespace). Mutually exclusive with `from_actor`.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "exclude_from_actor",
                param_type: "string",
                required: false,
                description: "Inbox-only exclusion of an exact sender actor label.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "to_actor",
                param_type: "string",
                required: false,
                description: "Exact recipient actor filter for box=\"sent\". Rejected for the default inbox box.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "since",
                param_type: "string",
                required: false,
                description: "Inclusive RFC 3339 lower bound on message `created_at`.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "before",
                param_type: "string",
                required: false,
                description: "Exclusive RFC 3339 upper bound on message `created_at`.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "subject_contains",
                param_type: "string",
                required: false,
                description: "Case-insensitive non-empty substring match on subject. Messages with no subject do not match.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "content_contains",
                param_type: "string",
                required: false,
                description: "Case-insensitive non-empty substring match on message content.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "fields",
                param_type: "array of string",
                required: false,
                description: "Non-empty message-field projection shared with comm.thread. Unknown fields are rejected. Omit for the full message view.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "comm.read",
        description: "Compatibility mark-read verb for one or up to 500 inbound messages; it does not retrieve message content. Mark writes are best-effort: each result carries status=success|failed|unknown (unknown means the write's execution seam terminated after the request was accepted, so it may already have applied — re-check with comm.inbox before deciding whether to re-issue; re-issuing is safe, marking read is idempotent), and bulk responses carry status=success|partial|failed|unknown.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "string",
                required: false,
                description: "Short 8-char prefix or full UUID of one inbound message to mark read. Mutually exclusive with `ids`.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "ids",
                param_type: "array of string",
                required: false,
                description: "One to 500 inbound message ids to mark read in one operation. Mutually exclusive with `id`; all targets are validated before mutation and duplicate resolved ids are updated once.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "comm.mark_read",
        description: "Mark up to 500 inbound messages as read; use comm.inbox or comm.thread to retrieve content. Best-effort responses carry status=success|partial|failed; atomic=true provides all-or-nothing mutation.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "ids",
                param_type: "array of string",
                required: true,
                description: "One to 500 inbound message ids. Every target is validated before mutation and duplicate resolved ids are updated once.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "atomic",
                param_type: "boolean",
                required: false,
                description: "All-or-nothing cross-message mutation. Defaults to false (best-effort per-target storage updates after complete validation).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "comm.unread",
        description: "Bounded count-only view of the caller's unread inbound messages (same filter as inbox(status=\"unread\"), no message payloads). Exact below count_cap=1000; count_saturated=true means at least that many.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[],
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Reply body. Must not be empty.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Structured provenance tags, persisted verbatim to `properties[\"tags\"]` on both the outbound and inbound copies of the reply.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max messages to return. Default 100, max 500. Truncation applies after ordering, so order=\"desc\" + limit returns the newest `limit` messages.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "order",
                param_type: "string",
                required: false,
                description: "Ordering of returned messages: \"asc\" (default, chronological) | \"desc\" (newest first).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "after",
                param_type: "string",
                required: false,
                description: "Cursor: a message id (short prefix or full UUID) or an RFC 3339 timestamp (any valid form, e.g. whole-second `Z` or `+00:00` offset). An id cursor ties-break on (created_at, full_id) so equal-timestamp messages are never skipped or duplicated. Only messages strictly after that point in the chosen `order` are returned; an unparseable value is a hard error.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "fields",
                param_type: "array of string",
                required: false,
                description: "Non-empty message-field projection shared with comm.inbox. Unknown fields are rejected. Omit for the full message view.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "from",
                param_type: "string",
                required: true,
                description: "Sender address in `channel-kind:addr` form (e.g. `email:alice@example.com`).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "to",
                param_type: "string",
                required: true,
                description: "Recipient address in `channel-kind:addr` form.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Message body text.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "subject",
                param_type: "string",
                required: false,
                description: "Optional subject line.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "thread_id",
                param_type: "uuid",
                required: false,
                description: "Optional full internal thread UUID. A short prefix would require scoped resolution and is rejected because the thread root is an explicit stable reference. The value is validated syntactically only — ingest performs no existence or namespace lookup on it. When absent, a new thread root is created.",
                resolution_mode: IdResolutionMode::UnscopedFullUuidOnly,
            },
            ParamDef {
                name: "channel_kind",
                param_type: "string",
                required: false,
                description: "Channel kind identifier (e.g. `email`).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "channel_slug",
                param_type: "string",
                required: false,
                description: "Stable channel credential/account identity returned by `Channel::slug`. Channel pollers supply it with `channel_kind` so quarantine health can be grouped without trusting free-form metadata (khive #1383).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "external_id",
                param_type: "string",
                required: false,
                description: "Stable transport dedup key. For email: `imap:{host}:{uidvalidity}:{uid}`. Duplicate messages are silently ignored.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "sent_at",
                param_type: "string",
                required: false,
                description: "RFC 3339 timestamp of the original message.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "correlation_external_id",
                param_type: "string",
                required: false,
                description: "External correlation key used to resolve the thread (e.g. X-Khive-Thread-ID or In-Reply-To header value).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "wire_message_id",
                param_type: "string",
                required: false,
                description: "This message's own RFC 822 Message-ID (including angle brackets), distinct from `external_id` (the transport dedup key). Persisted so a later reply can set In-Reply-To/References.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "wire_references",
                param_type: "string",
                required: false,
                description: "This message's own RFC 822 References header value, verbatim. Persisted so a later reply can extend the full ancestor chain instead of truncating it to the immediate parent.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "metadata",
                param_type: "object",
                required: false,
                description: "Optional transport-layer metadata passthrough, merged additively into the stored note's properties (never overrides an already-set field). Generic and channel-agnostic; the email channel uses it for quarantine markers (quarantined, quarantine_reason, quarantine_claimed_from — ADR-056 Amendment 2026-07-02).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "comm.heartbeat",
        description: "Persist a per-channel-credential heartbeat row after a poll attempt. \
                       Subhandler — not callable on the MCP wire; only trusted internal Rust \
                       callers holding a `VerbRegistry` handle can dispatch this (khive #606). \
                       The row persists under the caller's dispatch-authorized namespace \
                       (`token.namespace()`, khive #917) — the same explicit `namespace=` \
                       escape / `\"local\"` default every other comm verb resolves. The local \
                       single-tenant channel poll loop always dispatches with \
                       `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` (\"local\") as its explicit \
                       `namespace`, so its heartbeat rows are unaffected and never follow \
                       `KHIVE_EMAIL_INGEST_NAMESPACE`. An authorized per-tenant writer instead \
                       dispatches via `VerbRegistry::dispatch_as` with a `VerifiedActor` (an \
                       out-of-band authenticated tenant principal) and passes its own tenant \
                       namespace in this same `namespace` field, producing heartbeat rows under \
                       that tenant's own namespace.",
        visibility: Visibility::Subhandler,
        category: khive_types::VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: true,
                description: "Dispatch routing key (ADR-007 Rule 3 explicit escape) consumed \
                              by `VerbRegistry::dispatch` to mint the call's `NamespaceToken` \
                              — the namespace the persisted row lands under. The local \
                              single-tenant poll loop always passes \
                              `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE`. An authorized \
                              per-tenant writer passes its own tenant namespace here via \
                              `VerbRegistry::dispatch_as` with a `VerifiedActor`, since this \
                              verb has no wire path for an untrusted caller to reach.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "channel_kind",
                param_type: "string",
                required: true,
                description: "Channel kind identifier (e.g. `email`).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "channel_slug",
                param_type: "string",
                required: true,
                description: "Stable per-credential identifier distinguishing accounts of the same kind (e.g. the mailbox address). Never `channel_kind` alone — two accounts of the same kind must not collapse into one row.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "poll_interval_secs",
                param_type: "integer",
                required: false,
                description: "Positive nominal poll cadence for this channel. New pollers supply it on every heartbeat; omission remains accepted for mixed-version internal writers.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "outcome",
                param_type: "string",
                required: true,
                description: "Poll outcome for this attempt: \"success\" or \"failure\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "error_class",
                param_type: "string",
                required: false,
                description: "Error class, required when outcome is \"failure\". Open string enum; v1 values: auth | transport | config. Callers must tolerate unknown classes.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "error_message",
                param_type: "string",
                required: false,
                description: "Human-readable error detail when outcome is \"failure\". Must carry a message class, never raw secrets or wire headers.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "at",
                param_type: "string",
                required: false,
                description: "RFC 3339 timestamp of this poll attempt. Defaults to now.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "comm.health",
        description: "Read-only per-channel health snapshot (khive #606, #1383, #1472). Returns \
                       daemon-persisted heartbeat rows plus exact channel identities found on \
                       live quarantine notes. Every channel entry includes `quarantined_count`; \
                       the response also includes namespace-wide `quarantined_count` and \
                       `unattributed_quarantined_count`. Quarantine-only entries do not fabricate \
                       daemon ownership: their heartbeat facts include \
                       `consecutive_failures: null`. Returns at most 200 channels. Heartbeat rows \
                       take precedence and retain persisted order; quarantine-only identities \
                       fill remaining capacity in lexical channel-identity order, while top-level \
                       counts remain namespace-wide. Heartbeat entries include their \
                       nominal `poll_interval_secs` and a nullable advisory `stalled` schedule \
                       fact. `stalled` becomes true after three missed nominal intervals; it is \
                       null for legacy/malformed rows and known failure/backoff state. This is \
                       not a computed healthy bool; overall \
                       health judgment belongs to the caller. Reads from the caller's injected \
                       namespace (khive #877) — `token.namespace()`, the same explicit \
                       `namespace=` escape / \"local\" default every other comm verb resolves \
                       (ADR-007 Rev 6 Rule 3). An unscoped call defaults to \"local\", matching \
                       the namespace heartbeat rows are persisted under; a call with an \
                       explicit non-local `namespace=` sees only that namespace's rows, never \
                       \"local\"'s. The response carries a `namespace` field naming the \
                       namespace actually read, so `role: \"client\"` means no heartbeat rows \
                       exist under THAT namespace (even if quarantine-only channels exist), not \
                       necessarily that no daemon exists anywhere. `comm.heartbeat` (khive #917) \
                       persists under the caller's dispatch-authorized namespace, so a \
                       non-local `namespace=` scope returns that namespace's rows once an \
                       authorized per-tenant writer has run. Without a heartbeat it may still \
                       return quarantine-only channel evidence while the local poll loop is \
                       actively heartbeating under \"local\".",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "comm.probe",
        description: "Read-only poll for new inbound message metadata and stale unread count. \
                      Unlike comm.inbox, the actor is not inferred from the caller — pass it \
                      explicitly via the required `actor` param (khive #93).",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "actor",
                param_type: "string",
                required: true,
                description: "Actor label whose inbound queue is probed, e.g. \"lambda:leo\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "since_us",
                param_type: "integer",
                required: false,
                description: "Opaque cursor round-tripped from a previous comm.probe response's cursor_us; only messages committed after it are returned. Omit for a baseline-first probe. Not a computable timestamp.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "stale_minutes",
                param_type: "integer",
                required: false,
                description: "Unread age threshold in minutes. Default 20.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "channel_slug",
                param_type: "string",
                required: true,
                description: "Stable per-credential identifier distinguishing accounts of the same kind.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "channel_slug",
                param_type: "string",
                required: true,
                description: "Stable per-credential identifier distinguishing accounts of the same kind.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "source",
                param_type: "string",
                required: true,
                description: "Stable, non-secret identity of the remote source/configuration (e.g. \
                               `imap+tls:{host}:{port}:{mailbox}:INBOX`). A mismatch against the \
                               stored row's source is how the caller detects a configuration change.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "generation",
                param_type: "integer",
                required: true,
                description: "Remote identity epoch (e.g. IMAP UIDVALIDITY). Must be a positive integer.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "high_water",
                param_type: "integer",
                required: false,
                description: "Greatest durably handled remote sequence value (e.g. IMAP UID). Omit \
                               or null to reset progress within the generation (e.g. right after a \
                               UIDVALIDITY change with no messages selected yet).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
];
