# khive-pack-session

Session pack: registers the `session` note kind and a background transcript
mirror that live-tails Claude Code and Codex CLI session logs into two auxiliary
SQL tables (ADR-080).

## What ships this milestone

The pack declares three verbs — `session.store`, `session.list`, `session.get` —
over the notes substrate, but all three are `Visibility::Subhandler`: reachable
via the runtime and `kkernel call`, but **withheld from the agent-facing MCP
`request` surface** until the session-continuity query UX is designed. They add
zero verbs to what an agent sees in the `request` tool's catalog today.

The pack's active feature this milestone is the **mirror service**, started from
the `warm()` pack hook independent of verb visibility. It polls
`$HOME/.claude/projects/**/*.jsonl` (Claude Code) and
`$HOME/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (Codex CLI), tracks a
byte-offset cursor per file (`session_mirror_cursor`), and appends parsed
messages into `sessions` / `session_messages` — both idempotent
(`INSERT OR IGNORE`) so restarting the daemon or double-processing a file is
safe. The service is opt-in via environment variable, disabled by default.

```text
KHIVE_MIRROR_ENABLED=true        # enable the Claude Code mirror
KHIVE_MIRROR_PROJECTS_DIR=...    # default: $HOME/.claude/projects
KHIVE_MIRROR_CODEX_ENABLED=true  # enable the Codex CLI mirror (independent flag)
KHIVE_MIRROR_CODEX_DIR=...       # default: $HOME/.codex/sessions
KHIVE_MIRROR_POLL_SECS=2         # polling interval
KHIVE_MIRROR_BACKFILL=true       # mirror existing files from byte 0 vs. current EOF
```

## Usage

The mirror is started automatically by the runtime's pack warm-up; it is not
called directly. The parse and config surfaces are public for testing and
alternate embedding:

```rust
use khive_pack_session::mirror::{parse_cc_line, MirrorConfig};

let config = MirrorConfig::from_env();
if config.enabled {
    // a JSONL line from a Claude Code transcript; returns None on a blank or
    // unparseable line
    if let Some(event) = parse_cc_line(line) {
        // event: ParsedEvent — session_id, uuid, role, text, raw JSON, ...
    }
}
```

Once the `session.store`/`list`/`get` verbs are flipped to `Visibility::Verb`,
the intended call shape is:

```text
request(ops="session.list(agent_id=\"lambda:khive\", limit=20)")
```

## Storage

Three tables, applied idempotently via the pack's `schema_plan` hook:
`sessions` (one row per transcript: `provider_session_id`, `source`, `cwd`,
`git_branch`, `message_count`, first/last-seen timestamps), `session_messages`
(one row per parsed message: `seq`, `parent_uuid`, `role`, `msg_type`, `text`,
raw JSON), and `session_mirror_cursor` (byte-offset bookkeeping per file).

## Where this sits

`khive-pack-session` sits in the pack tier alongside
[`khive-pack-kg`](https://crates.io/crates/khive-pack-kg) (a `REQUIRES`
dependency for the `session` note kind's substrate) and is one of the eight
packs loaded by default in `khive-mcp`. This crate is the OSS storage mechanism
half of session continuity — the design record explicitly moved the scope
boundary so the mirror mechanism (this crate, the `session.*` verb surface, and
the note-kind registration) ships in the open-source repository rather than
staying a deployment-only concern. Governing ADR:
[ADR-080 (Session Pack — OSS Storage Mechanism)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-080-session-pack-oss-storage-mechanism.md).

## License

Apache-2.0.
