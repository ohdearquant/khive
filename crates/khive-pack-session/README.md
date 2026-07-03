# khive-pack-session

Session pack: registers the `session` note kind and four agent-facing verbs
for storing and retrieving agent-session records (transcripts or summaries)
over the notes substrate (ADR-083).

## Verbs

- `session.store(content, title?, provider?, provider_session_id?, tags?)` —
  persist a session record. `provider_session_id` is the provider-native
  continuity anchor (not a khive UUID); `(provider, provider_session_id)` is
  the strongest grouping key when both are present.
- `session.list(limit?, offset?, provider?)` — browse stored sessions, newest
  first. Summaries omit `content`.
- `session.resume(id)` — fetch one session's full content by full UUID or
  8+ hex short prefix.
- `session.export(id, format?)` — serialize a session as `json` (default) or
  `markdown`.

```text
request(ops="session.store(content=\"...\", provider=\"codex\", provider_session_id=\"abc\")")
request(ops="session.list(limit=20, provider=\"codex\")")
request(ops="session.resume(id=\"a1b2c3d4\")")
request(ops="session.export(id=\"a1b2c3d4\", format=\"markdown\")")
```

## Storage

Sessions are stored as `kind=session` notes on the shared `notes` substrate —
no pack-private schema or migration. `notes.name` holds the optional title,
`notes.content` holds the verbatim payload, and `notes.properties` holds
`provider`, `provider_session_id`, and `tags`. Handlers go through the public
runtime seam (`runtime.core()`, `create_note`, `query_notes_filtered`,
`resolve_prefix`, `resolve_primary`) rather than direct SQL.

## Out of scope for this slice

The digester/summarization pipeline is cloud-side. `session.import`, tiering,
and billing are deferred; see
[ADR-083](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-083-session-pack-t1-verbs.md).
The session mirror (transcript parsing and ingestion into `session_messages`)
is a separate, already-shipped concern — see
[ADR-080 §6](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-080-session-pack-oss-storage-mechanism.md#6-the-session-mirror-amendment-2026-07-02).

## Where this sits

`khive-pack-session` sits in the pack tier alongside
[`khive-pack-kg`](https://crates.io/crates/khive-pack-kg) (a `REQUIRES`
dependency for the `session` note kind's substrate) and is one of the packs
loaded by default in `khive-mcp`.

## License

Apache-2.0.
