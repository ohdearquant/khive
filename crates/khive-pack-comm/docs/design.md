# khive-pack-comm Design

## ADR Compliance

### Communication Pack (ADR-040)

This crate is the primary implementation of ADR-040. It provides five `comm.*` verbs over the
standard `message` note kind stored in the notes table.

Key design decisions from ADR-040:
- **Dual-write delivery**: every `comm.send` writes an outbound copy (caller namespace) and an
  inbound copy (recipient namespace). If the inbound write fails, the outbound note is rolled back
  before returning the error.
- **Cross-namespace delivery**: controlled by the sender-side `actor.allowed_outbound_namespaces`
  allowlist. An empty allowlist (the default) reproduces the prior deny-all behavior. Namespaces
  listed in the allowlist may receive inbound notes via `dual_write_message`. Unlisted namespaces
  receive `RuntimeError::PermissionDenied { verb: "comm.send" }` (issue #481 fix, updated by the
  cross-namespace ACL PR).
- **Canonical thread_id**: when a root message is sent (`thread_id` is `None`), both the outbound
  and inbound copies share the same canonical `thread_id` — the sender's outbound UUID. This
  ensures `comm.thread(id=outbound_id)` can find replies across namespaces.
- **Verb categories (speech-act classification)**:
  - `comm.send` — Commissive (the sender commits to delivery)
  - `comm.inbox` — Assertive (queries state)
  - `comm.read` — Declaration (changes the read/unread state)
  - `comm.reply` — Commissive (the sender commits to a reply)
  - `comm.thread` — Assertive (queries state)
- **Pack-auxiliary indexes**: two partial indexes on the `notes` table (`idx_comm_message_direction`
  and `idx_comm_message_thread`) are declared via `schema_plan()`. These use
  `WHERE deleted_at IS NULL` rather than `WHERE kind = 'message'` so that the SQLite query planner
  can match them when queries use a parameterized `kind = ?N` predicate.
- **`read()` is a recipient-only action**: marking an outbound (sent) message as read is rejected.
  Only inbound messages (direction=inbound) can be marked read.

### Verb Categories (ADR-025)

Handler definitions in `vocab.rs` assign each verb a `VerbCategory` matching the speech-act
taxonomy from ADR-025. The mapping is enforced by the `verb_categories_match_spec` unit test.

### ADR-017: Pack Standard

`CommPack` implements the `Pack` trait with:
- `NOTE_KINDS = ["message"]`
- `ENTITY_KINDS = []`
- `REQUIRES = ["kg"]`
- `HANDLERS = COMM_HANDLERS` (5 entries)

The pack self-registers via `inventory::submit!` so it is available when loaded by name.

### Cross-namespace delivery policy (OSS, specified 2026-06-15)

Cross-namespace messaging is controlled by the **sender-side outbound allowlist**. The
`dual_write_message` function returns `RuntimeError::PermissionDenied` when the recipient
namespace is not in the sender's `actor.allowed_outbound_namespaces` allowlist. An empty
allowlist (default) reproduces the prior deny-all behavior.

## Consistency Notes

- **Cross-namespace delivery**: the sender-side `actor.allowed_outbound_namespaces` allowlist is
  the OSS ACL gate (2026-06-15). The default is empty (deny-all). ADR-040 is updated to match.
- **Thread root resolution**: `comm.thread` resolves the canonical thread root by inspecting
  `properties.thread_id` on the resolved note. If the stored `thread_id` differs from the note's
  own UUID (inbound copy case), it uses the stored value. This cross-namespace root resolution
  is a refinement on the base ADR-040 spec and enables `thread(id=inbound_id)` to return the
  full conversation.
- **Reply routing (UE6-H1)**: reply routing is direction-aware: if the reply caller is the original
  sender, the reply goes to the original recipient, and vice versa. This is an implementation
  refinement not explicitly specified in ADR-040.
