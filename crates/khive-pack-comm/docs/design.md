# khive-pack-comm Design

## ADR Compliance

### ADR-040: Communication Pack

This crate is the primary implementation of ADR-040. It provides five `comm.*` verbs over the
standard `message` note kind stored in the notes table.

Key design decisions from ADR-040:
- **Dual-write delivery**: every `comm.send` writes an outbound copy (caller namespace) and an
  inbound copy (recipient namespace). If the inbound write fails, the outbound note is rolled back
  before returning the error.
- **Cross-namespace delivery is DENIED**: pending ACL policy specification (ADR-018). The recipient
  namespace must equal the caller namespace. This prevents unauthorized writes into arbitrary
  recipient namespaces (issue #481 fix).
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

### ADR-025: Verb Categories

Handler definitions in `vocab.rs` assign each verb a `VerbCategory` matching the speech-act
taxonomy from ADR-025. The mapping is enforced by the `verb_categories_match_spec` unit test.

### ADR-017: Pack Standard

`CommPack` implements the `Pack` trait with:
- `NOTE_KINDS = ["message"]`
- `ENTITY_KINDS = []`
- `REQUIRES = ["kg"]`
- `HANDLERS = COMM_HANDLERS` (5 entries)

The pack self-registers via `inventory::submit!` so it is available when loaded by name.

### ADR-018: ACL Policy (not yet implemented)

Cross-namespace messaging is currently denied (fail-closed). The `dual_write_message` function
returns `CrossNamespaceWrite` for any send where `from != to`. This will be relaxed once ADR-018
specifies the ACL policy for inter-namespace writes.

## Consistency Notes

- **Cross-namespace delivery**: the current deny-all policy is more restrictive than what ADR-040
  specifies for the long-term vision. This is intentional and documented in the code: the
  `dual_write_message` function explains that full cross-namespace delivery awaits ADR-018 ACL
  policy. No inconsistency with the current spec; both ADR-040 and the code agree on the interim
  fail-closed posture.
- **Thread root resolution**: `comm.thread` resolves the canonical thread root by inspecting
  `properties.thread_id` on the resolved note. If the stored `thread_id` differs from the note's
  own UUID (inbound copy case), it uses the stored value. This cross-namespace root resolution
  is a refinement on the base ADR-040 spec and enables `thread(id=inbound_id)` to return the
  full conversation.
- **Reply routing (UE6-H1)**: reply routing is direction-aware: if the reply caller is the original
  sender, the reply goes to the original recipient, and vice versa. This is an implementation
  refinement not explicitly specified in ADR-040.
