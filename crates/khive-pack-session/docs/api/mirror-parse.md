# Session mirror parse

Technical reference for the ChatGPT-export parsing helpers in
`crates/khive-pack-session/src/mirror/parse.rs` — the DFS conversation walk, per-conversation
context, and text/block extraction that turn a raw export tree into mirrored session events.

## `parse_chatgpt_export` — DFS walk and per-conversation isolation

Unlike `parse_cc_line`/`parse_codex_line` (one JSONL line in, one event out),
a ChatGPT export is a single static JSON array of conversation objects — this
function parses the whole file at once and returns every message-bearing
event across every conversation it contains.

Returns `None` when `content` is not valid JSON or the top level is not a
JSON array. The caller treats that as a per-file error so the mirror cursor
does not advance: a partially-downloaded export is retried whole on the next
tick, never half-consumed. A malformed *conversation* inside an otherwise-valid
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
null/absent), and `slug`.

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

## `extract_text` / `extract_block` (Claude Code + Codex block extraction)

`extract_text` handles both the string form and the structured-block array
form of a message `content` value.

`extract_block` extracts a display string from a single content block:
- `"text"` — Claude Code plain text block.
- `"input_text"` / `"output_text"` — Codex user and assistant text blocks
  (same field, `text`, as the Claude Code `"text"` block, hence shared
  extraction logic).
- `"tool_use"` — tool invocation (name + input JSON, truncated to 500 chars).
- `"tool_result"` — tool output (content string, truncated to 500 chars).

## `extract_chatgpt_text`

Extracts display text from a ChatGPT message `content` object per its
`content_type`: for `"text"`, joins string `parts` with `"\n"` (non-string
parts ignored defensively); for anything else (`"code"`, `"execution_output"`,
…), prefers `content.text` if present, else falls back to joined string
`parts`, else `None`.
