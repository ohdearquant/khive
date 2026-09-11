# Session mirror parse

Technical reference for the provider-export parsing helpers in
`crates/khive-pack-session/src/mirror/parse.rs` — the conversation walks, per-conversation
context, and text/block extraction that turn ChatGPT and claude.ai exports into mirrored
session events.

## `parse_chatgpt_export` — DFS walk and per-conversation isolation

Unlike `parse_cc_line`/`parse_codex_line` (one JSONL line in, one event out),
a ChatGPT export is a single static JSON array of conversation objects — this
function parses the whole file at once and returns every message-bearing
event across every conversation it contains.

Returns `None` when `content` is not valid JSON or the top level is not a
JSON array. The caller treats that as a per-file error so the mirror cursor
does not advance: a partially-downloaded export is retried whole on the next
tick, never half-consumed. A malformed _conversation_ inside an otherwise-valid
array is skipped individually (`parse_conversation`) so one bad entry cannot
sink the rest of the file.

Each conversation's `mapping` forms a tree; events are emitted in
deterministic DFS preorder from the root, following each node's `children`
array order (never JSON object key order). Nodes off the `current_node`
root-to-tip path are flagged `is_sidechain`, mirroring how Claude Code flags
abandoned/regenerated branches.

## `ConvContext`

Context threaded through node visitation for one conversation — the pieces
that don't change as the DFS walks the mapping tree: `mapping`,
`current_path` (the current-node root-to-tip set), `session_id`,
`conv_created_at_micros` (conversation-level `create_time` in micros, 0 if
absent — the fallback used when a message's own `create_time` is
null/absent), and `slug`. The conversation `title` is passed through the same
`SessionMirror` permanent mask-only surface as `text` and `raw` before it
becomes `slug`, both on the per-event projection and on the `sessions.slug`
column it feeds — a credential-bearing title cannot reach storage unmasked.

## `parse_claude_ai_export` — `chat_messages` and active branches

A claude.ai export is also a top-level JSON array, but its shape is distinct:
conversation identity and title are `uuid` and `name`, and messages live in
`chat_messages`. Each message's `uuid` becomes the event id, the conversation
`uuid` becomes `session_id` / `provider_session_id`, `human` is normalized to
the `user` role, and ISO-8601 `created_at` values are converted to microseconds.

Messages are emitted in ascending `index` order with source-array position as
the deterministic fallback and tie-breaker. When the export supplies
`current_leaf_message_uuid`, the parser follows `parent_message_uuid` to mark
the active path and retains other messages as sidechains. Older flat exports
without an active-leaf field keep every message on the main path. Root
sentinels and parents that did not produce a stored event become `None`, so
stored rows never gain a dangling provider-export parent reference.

Visible text comes from structured `content` blocks using the same text,
voice-note, and tool-block extraction as the CLI parsers; `thinking` and
unknown internal blocks are not display text. The parser then appends a
distinct top-level `text` value because agent turns can carry tool blocks and
a separate final answer at the same time; when `text` exactly duplicates a
text block it is emitted only once. Older exports that use `role` instead of
`sender` retain the same user/assistant normalization. Every extracted text and serialized raw
message is passed through the typed `SessionMirror` permanent mask-only surface before it becomes a
`ParsedEvent`. Per ADR-115 Amendment 2 the final stored targets are `session_messages.text` and
`session_messages.raw`; the surface has no exemption lookup, posture stamp, or atomic
exemption-success event.
Malformed conversations are skipped individually; invalid JSON or a non-array
top level returns `None` so the ingest cursor cannot advance.

The ingest-facing `parse_claude_ai_export_with_sessions` result carries one
`ParsedSession` for every valid conversation separately from its retained
`ParsedEvent`s. Consequently, an empty `chat_messages` array—or one containing
only thinking/unknown blocks—still creates the conversation's session metadata
without manufacturing a message row. The public `parse_claude_ai_export`
helper remains the event-only view used by parser tests and callers that do not
need session metadata.

Both provider exports use the filename `conversations.json`. Each parser also
rejects a file containing the other provider's conversation shape (including a
mixed file). This keeps one source from advancing the shared path cursor first
if an operator accidentally configures overlapping export roots.

## `parse_conversation`

Parses one ChatGPT export conversation object, appending its message-bearing
nodes (deterministic DFS preorder from the mapping root) to `out`. Skips the
whole conversation on a missing/empty `id` or missing `mapping` so one
malformed entry cannot sink the rest of the file.

- **current-path set**: walks `current_node` → `parent` → ... → root. Off-path
  nodes (abandoned/regenerated branches) are flagged `is_sidechain`, mirroring
  how Claude Code flags sidechains. A cycle guard (`current_path.insert`
  returning `false`) protects against malformed mapping data.
- **DFS preorder from the root, following `children` order**: uses an
  explicit stack, not recursion — a long linear conversation can nest
  thousands of turns deep and would risk overflowing a worker-thread stack.
  Children are pushed in reverse so the first child in the array is popped
  (and thus visited) first, preserving `children` order as preorder.

## `build_chatgpt_event`

Builds a `ParsedEvent` for a single message-bearing mapping node. Returns
`None` when the message carries no `id`, or when the extracted text is
empty/whitespace-only (ChatGPT scaffolding nodes, e.g. system prompts with
`parts: [""]`).

`parent_uuid` is `Some(parent_node_id)` only when that parent node itself
carries a (non-null) message — the ChatGPT root is normally `message: null`,
so its children correctly get `parent_uuid: None`. A parent that DOES carry a
message but was itself skipped as an event (e.g. empty-parts scaffolding)
still counts — this is provenance linkage, matching how CC parent chains can
reference events that were never mirrored.

## `extract_text` / `extract_block` (Claude Code, Codex, and claude.ai blocks)

`extract_text` handles both the string form and the structured-block array
form of a message `content` value.

`extract_block` extracts a display string from a single content block:

- `"text"` — Claude Code plain text block.
- `"voice_note"` — claude.ai voice-note transcript text.
- `"input_text"` / `"output_text"` — Codex user and assistant text blocks
  (same field, `text`, as the Claude Code `"text"` block, hence shared
  extraction logic).
- `"tool_use"` — tool invocation (name + input JSON, masked through the `SessionMirror` surface
  then truncated to 500 chars). Masking runs before truncation: a detector's terminating span can
  sit past the 500-char cut, and a masker that only sees a truncated prefix cannot recognize a
  match it cannot see the end of.
- `"tool_result"` — tool output (content string, masked through the `SessionMirror` surface then
  truncated to 500 chars, for the same mask-before-truncate reason).

## `extract_chatgpt_text`

Extracts display text from a ChatGPT message `content` object per its
`content_type`: for `"text"`, joins string `parts` with `"\n"` (non-string
parts ignored defensively); for anything else (`"code"`, `"execution_output"`,
…), prefers `content.text` if present, else falls back to joined string
`parts`, else `None`.
