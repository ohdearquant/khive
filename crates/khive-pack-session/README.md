# khive-pack-session

Session pack: registers the `session` note kind and five agent-facing verbs
for storing and retrieving agent-session records (transcripts or summaries)
over the notes substrate (ADR-083).

## Verbs

- `session.store(content, title?, provider?, provider_session_id?, tags?)` —
  persist a session record. `provider_session_id` is the provider-native
  continuity anchor (not a khive UUID); `(provider, provider_session_id)` is
  the strongest grouping key when both are present.
- `session.list(limit?, offset?, provider?, agent_id?, since?)` — browse
  stored sessions, newest first. `agent_id` matches the legacy
  `properties.agent_id` field and `since` is an inclusive RFC 3339 creation
  timestamp. Summaries omit `content`.
- `session.resume(id)` — fetch one session's full content by full UUID or
  8+ hex short prefix.
- `session.export(id, format?)` — serialize a session as `json` (default) or
  `markdown`.
- `session.search(query, limit?, since?, source?, cwd?)` — tenant-scoped mirror
  search. The public handler remains unavailable until transcript deletion and
  resume/export continuity support are available.

```text
request(ops="session.store(content=\"...\", provider=\"codex\", provider_session_id=\"abc\")")
request(ops="session.list(limit=20, provider=\"codex\")")
request(ops="session.list(agent_id=\"lambda:worker\", since=\"2026-08-01T00:00:00Z\")")
request(ops="session.resume(id=\"a1b2c3d4\")")
request(ops="session.export(id=\"a1b2c3d4\", format=\"markdown\")")
```

## Storage

Sessions stored through the four note verbs are `kind=session` notes on the
shared `notes` substrate. `notes.name` holds the optional title,
`notes.content` holds the verbatim payload, and `notes.properties` holds
`provider`, `provider_session_id`, and `tags`. Handlers go through the public
runtime seam (`runtime.core()`, `create_note`, `query_notes_filtered`,
`resolve_prefix`, `resolve_primary`) rather than direct SQL. The separate
session mirror has pack-owned SQL tables, a versioned identity migration,
and an FTS5 index; see [ADR-117a mirror identity](docs/api/adr117a-identity.md).

## Out of scope for this slice

The digester/summarization pipeline is cloud-side. `session.import`, tiering,
and billing are deferred; see
[ADR-083](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-083-session-pack-t1-verbs.md).
The session mirror (transcript parsing and ingestion into `session_messages`)
is a separate, already-shipped concern — see
[ADR-080 §6](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-080-session-pack-oss-storage-mechanism.md#6-the-session-mirror-amendment-2026-07-02).
After its startup discovery pass, the mirror caches directory listings, polls
actively growing transcripts directly, and samples cold transcripts through
fixed round-robin budgets. Quiet-tick metadata work therefore stays bounded as
the historical transcript corpus grows; directory changes prioritize their
cold files, with the ordinary bounded sweep covering filesystems where append
does not update parent-directory mtime. The priority ordering is bounded but
not fair under continuous directory churn: repeated changes can keep the
priority queue ahead of the ordinary cold sweep, while productive cold-file
metadata probes remain capped at 256 per tick.

## Where this sits

`khive-pack-session` sits in the pack tier alongside
[`khive-pack-kg`](https://crates.io/crates/khive-pack-kg) (a `REQUIRES`
dependency for the `session` note kind's substrate) and is one of the packs
loaded by default in `khive-mcp`.

## License

Apache-2.0.
