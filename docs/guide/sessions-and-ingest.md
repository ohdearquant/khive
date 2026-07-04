# Agent Sessions and Data Ingest

This guide covers the session pack (`crates/khive-pack-session/`), which has
two independent parts that are easy to conflate but serve different
purposes:

- **Session verbs** (`session.store`, `session.list`, `session.resume`,
  `session.export`): explicit, caller-driven persistence of a session record
  as a khive note.
- **Provider mirror ingest**: an optional background service that tails
  agent CLI transcript files and ChatGPT data exports into their own SQL
  tables, entirely separate from the note substrate.

Do not assume these two share storage. A session stored via `session.store`
is a `kind=session` note, queryable like any other note. A conversation
picked up by the mirror service lands in the `sessions` /
`session_messages` tables and is not a note at all.

## Session verbs

`session.store` persists a session as a note with `kind="session"`. The pack
declares `NOTE_KINDS = &["session"]` and requires the `kg` pack.

### Store

| Param                 | Type            | Required | Notes                                                      |
| --------------------- | --------------- | -------- | ---------------------------------------------------------- |
| `content`             | string          | yes      | Verbatim transcript or summary content. Must not be empty. |
| `title`               | string          | no       | Stored as the note's `name`.                               |
| `provider`            | string          | no       | Provider label, e.g. `codex`, `claude_code`, `openai`.     |
| `provider_session_id` | string          | no       | Provider-native continuity anchor.                         |
| `tags`                | array of string | no       | Stored in `properties.tags`.                               |

```
request(ops="session.store(content=\"...\", title=\"PR #610 review session\", provider=\"claude_code\")")
```

Any string field that is supplied must be non-empty after trimming; an empty
`content`, or a present-but-blank `title`/`provider`/`provider_session_id`, or
an empty tag entry, is rejected with a validation error naming the offending
field.

### List

| Param      | Type    | Required | Notes                                        |
| ---------- | ------- | -------- | -------------------------------------------- |
| `limit`    | integer | no       | 1 to 200; default 20.                        |
| `offset`   | integer | no       | Default 0.                                   |
| `provider` | string  | no       | Exact match filter on `properties.provider`. |

```
request(ops="session.list(provider=\"codex\", limit=10)")
```

Results are ordered newest first and returned as summaries (no `content`
field) with a `total` count when the underlying store can supply one.

### Resume

Fetches one session's full content, including `content`, by id.

```
request(ops="session.resume(id=\"<session_id_or_8+char_hex_prefix>\")")
```

`id` accepts a full UUID or an 8-or-more character hex prefix. A prefix that
matches no record, an id that resolves to a note of any kind other than
`session`, or a well-formed id with no matching record all produce distinct
errors (a resolution error naming the mismatched kind, or a not-found error),
so a caller can tell "wrong kind of record" apart from "does not exist" apart
from "malformed id".

### Export

Serializes one stored session as `json` (the default) or `markdown`.

```
request(ops="session.export(id=\"<session_id>\", format=\"markdown\")")
```

`format` is validated before id resolution, so an invalid format is rejected
even if the id itself would not resolve. The markdown form renders a heading
from the title (or `Session {first 8 chars of id}` if untitled), a metadata
list (`id`, `provider`, `provider_session_id`, `created_at`, `updated_at`,
`tags`), and a `## Content` section containing the raw stored content
verbatim.

## Provider mirror ingest

The mirror service (`crates/khive-pack-session/src/mirror/`) is a background
task, distinct from the four verbs above, that discovers and tails local
transcript files and writes their events into three dedicated tables
(`sessions`, `session_messages`, `session_mirror_cursor`) created by the
pack's schema plan. It never calls `session.store` and does not create
`session` notes.

### Supported providers

Only three sources are implemented (`MirrorSource` in `mirror/ingest.rs`):

- **Claude Code CLI transcripts** (`claude_code`): JSONL files under
  `KHIVE_MIRROR_PROJECTS_DIR`.
- **Codex CLI transcripts** (`codex`): JSONL files under
  `KHIVE_MIRROR_CODEX_DIR`.
- **ChatGPT data exports** (`chatgpt_export`): a `conversations.json` file
  (the format ChatGPT's "export data" produces) under
  `KHIVE_MIRROR_CHATGPT_DIR`.

There is no ingest path for claude.ai (the web product) conversation history.
If a future provider adds that, it belongs here as a fourth `MirrorSource`
variant, not folded into the existing ChatGPT or Claude Code parsers.

### Enabling it

The mirror service only starts if at least one enable flag is true. This is
checked once, in the session pack's `warm()` lifecycle hook, and `warm()` is
only invoked by the persistent daemon's startup path
(`crates/khive-runtime/src/daemon.rs`), not by a plain stdio client. So, like
the email channel loops described in
[Communication and Email](communication.md), running `kkernel mcp --daemon`
is what actually starts background ingestion; a stdio session never spawns
its own mirror poller.

| Variable                       | Default                  |
| ------------------------------ | ------------------------ |
| `KHIVE_MIRROR_ENABLED`         | `false`                  |
| `KHIVE_MIRROR_PROJECTS_DIR`    | `$HOME/.claude/projects` |
| `KHIVE_MIRROR_CODEX_ENABLED`   | `false`                  |
| `KHIVE_MIRROR_CODEX_DIR`       | `$HOME/.codex/sessions`  |
| `KHIVE_MIRROR_CHATGPT_ENABLED` | `false`                  |
| `KHIVE_MIRROR_CHATGPT_DIR`     | `$HOME/.chatgpt/exports` |
| `KHIVE_MIRROR_POLL_SECS`       | `2`                      |
| `KHIVE_MIRROR_BACKFILL`        | `true`                   |

`KHIVE_MIRROR_POLL_SECS=0` is rejected (falls back to the default rather than
busy-looping); a non-numeric value likewise falls back to the default, with a
warning logged in both cases.

### provider_session_id and idempotency

Each mirrored file is tracked in `session_mirror_cursor` by file path and byte
offset, so re-running the service resumes tailing from where it left off
rather than re-reading from the start. Writes into `sessions` and
`session_messages` are transactional and idempotent (`INSERT OR IGNORE` /
`ON CONFLICT DO NOTHING` on stable keys), so a crash mid-write or a
re-processed byte range does not duplicate rows. `provider_session_id`
values are the mirror's continuity anchor across restarts. Reads are bounded
in size (a byte/line/event cap per read) so a single oversized file cannot
stall the tailer; the ChatGPT export path additionally caps the whole file
at `KHIVE_MIRROR_CHATGPT_MAX_BYTES` (default 256 MiB).

### Worked example

Enable Codex mirroring only, then list the mirrored provider labels via
`session.list` filtered by provider (the mirror writes `source` into the
`sessions` table, not `properties.provider` on a note, so `session.list`
filters khive-native `session.store` records, not mirrored rows; the two are
queried separately today):

```bash
KHIVE_MIRROR_ENABLED=false \
KHIVE_MIRROR_CODEX_ENABLED=true \
KHIVE_MIRROR_CODEX_DIR="$HOME/.codex/sessions" \
kkernel mcp --daemon
```

To store a manual session summary through the note-based verbs instead
(unaffected by any mirror configuration):

```
request(ops="session.store(content=\"Reviewed PR #610, confirmed daemon-only gating\", title=\"PR 610 review\", provider=\"claude_code\")")
request(ops="session.list(provider=\"claude_code\", limit=5)")
```

## See also

- [Communication and Email](communication.md): the other daemon-only
  background loop (email channel polling), and the same warm-hook /
  daemon-role pattern.
- [Specialized Packs](specialized-packs.md): packs beyond the eight loaded
  by default.
