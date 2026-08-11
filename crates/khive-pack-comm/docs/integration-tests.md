# Comm Pack -- Integration Test Notes

Design narrative and regression context moved out of `crates/khive-pack-comm/tests/integration.rs` so the source file keeps only tightened per-test proof statements and invariant notes. Each test's inline comment still states what it proves in one line; this document carries the longer *why*.

## Why this file is not split into submodules

The public comm verbs share a single in-memory runtime fixture. Splitting into per-verb files would require duplicating the fixture and lose cross-verb invariant tests (e.g. send -> inbox -> read -> reply -> thread roundtrip and thread-isolation assertions) that exercise interactions between verbs.

## Design notes and regression context

Background for clusters of related tests, in source-file order.

### ADR-057 (Comm Actor-Addressed Delivery): actor-addressed send allows lambda↔lambda messaging

Before ADR-057, comm.send(to="lambda:leo") from lambda:khive was denied by the cross-namespace ACL gate (#481 fix). ADR-057 supersedes that gate for actor- addressed sends: both copies land in the caller's namespace (lambda:khive). The recipient namespace (lambda:leo) receives nothing — isolation is preserved.

### T1 — within-namespace send unchanged by the allowlist feature.

ADR-007 Rev 2: all storage routes to "local". Both copies land in "local".

### T2 — cross-ns send is actor-addressed (ADR-057): succeeds regardless of allowlist.

ADR-007 Rev 2: both copies land in "local" (the shared storage namespace). Actor labels (from_actor/to_actor) in note properties distinguish sender and recipient.

### T3 — actor-addressed send (ADR-057): both copies land in "local" (ADR-007 Rev 2).

Actor labels from_actor/to_actor in note properties identify routing participants.

### T4 — inbound note's namespace column is "local" (ADR-007 Rev 2 all-local model).

ADR-057 actor-addressed delivery: both copies land in "local", identified by actor labels.

### T5 — ADR-057 §(c): actor-addressed delivery with configured identity.

A sender with actor_id="lambda:khive" sends to "lambda:leo". Both copies land in the "local" namespace (ADR-007 all-local). The recipient (actor_id="lambda:leo") sees the message in their inbox because the to_actor filter matches. An anonymous caller on the same backend sees 0 messages (inbox leak closed, #199).

### T5b — ADR-057: comm.reply always writes same-namespace.

Reply on a configured-actor setup proves the fail-closed reply path: after the fix, handle_reply ALWAYS passes caller_ns as both `from` and `to` to dual_write_message and always sets from_actor/to_actor. No path through handle_reply can cause dual_write_message to mint a token in a foreign namespace. We use actor_id="lambda:khive" (self-send to "lambda:khive") so that the inbox filter correctly surfaces both the original inbound and the reply inbound, both of which have to_actor="lambda:khive".

### T6 — inbox isolation: sender does NOT see inbound copy addressed to another actor.

An anonymous sender (no actor_id) sends from "lambda:leo" namespace to "lambda:khive". The inbound note has to_actor="lambda:khive". The sender's inbox uses EqOrMissing("local") filter (anonymous), so it sees 0 messages. The inbound copy is invisible to the sender. This is the CORRECT post-#199-fix behavior. The old behavior (seeing 1) was the leak.

### T7 — white-box: with_namespace token scoping (realigned to ADR-007 by-ID contract, #148).

`NamespaceToken::with_namespace(recipient)` produces a token scoped to the recipient namespace.  It is an ordinary NamespaceToken — NOT a type-enforced write-only capability. Under ADR-007 rule 2 (PR #148), by-ID operations are namespace-blind: the token's namespace is used for WRITE attribution and multi-record LIST filtering only. A `get_note_including_deleted` call resolves a globally-unique UUID and returns the record regardless of which namespace the token carries. (a) The minted token CAN read the SENDER-namespace note by UUID (by-ID reads are namespace-blind; the gate, not the token's visible set, is the auth boundary). (b) The minted token CAN read the RECIPIENT-namespace note by UUID (same contract). The security boundary remains the sender-side allowlist check on comm.send; the token type does not enforce read isolation on by-ID fetches.

### T8 — ADR-007 Rev 2: inbound note is in "local" (all-local model).

A local-namespace token CAN read the inbound note. No separate recipient namespace exists. Recipient isolation is provided by actor labels (to_actor), not namespace partitioning.

### T9 — actor-addressed reply (ADR-057) with ADR-007 Rev 2 all-local model.

ADR-007: all writes go to "local". ADR-057: actor labels distinguish routing. example actor (registry_shared) sends to khive (both copies in "local"). Then replies to the inbound copy, verifying reply inherits the canonical thread_id.

### T10 — ADR-057: reply to a non-existent message ID fails with NotFound.

Under actor-addressed delivery, the inbound note is in the SENDER's namespace (lambda:leo), not the recipient's (lambda:khive). A reply attempt by khive using a random ID fails because the note is not visible in khive's namespace.

### T11 — ADR-057: actor-addressed send always succeeds (allowlist no longer gates comm.send).

ADR-007 Rev 2: both notes land in "local" (all-local model). The rollback path (dual_write_message) is tested by T13/T14 via FTS/vector injection.

### T12 — ADR-057: both directions succeed (actor-addressed, allowlist no longer gates comm.send).

ADR-007 Rev 2: each send produces 2 notes in "local" (all-local model). After 2 sends (leo→khive and khive→leo), "local" has 4 notes total. Actor labels distinguish the sender/recipient for each pair.

### T13 — FTS failure on note write leaves no stranded row.

Under ADR-007 Rev 2 dispatch pins the storage token to Namespace::local(). arm_fts_fail("local") would race against every other concurrent test that writes a note to "local". To preserve namespace-targeting isolation, this test uses a unique UUID namespace via rt.create_note() directly (bypassing dispatch). This validates the same create_note_inner rollback behavior — commit row → FTS error → compensate (delete row) → return Err — without the cross-test injection race that "local" would introduce.

### T14 — vector insertion failure on note write leaves no stranded row.

Under ADR-007 Rev 2 dispatch pins the storage token to Namespace::local(). arm_vector_fail("local") would race against every other concurrent test that writes a note with an embedder registered. To preserve namespace-targeting isolation, this test uses a unique UUID namespace via rt.create_note() directly (bypassing dispatch). This validates the same create_note_inner rollback behavior — commit row → FTS ok → vector error → compensate (delete row + FTS) → return Err — without the cross-test injection race.

### Issue #75 regression: actor-identity filter (ADR-057)

Root cause: handle_inbox read caller_actor from token.namespace() (always "local") instead of token.actor().id. The to_actor guard was permanently dormant. After the fix, when RuntimeConfig.actor_id is set, authorize() mints a token carrying that actor label, activating the filter.

### TOML wiring test — KhiveConfig parsed from TOML with

`actor.allowed_outbound_namespaces = [...]` must land those values in RuntimeConfig.allowed_outbound_namespaces.

### Cluster-2 isolation tests

These tests cover the decision-independent (no ADR required) half of the isolation story: #199 (comm.inbox actor-filter bypass) -- the to_actor filter must isolate tenants when actor_id IS configured; #224 (gate actor identity gap) -- the GateRequest.actor must carry the configured actor identity, not ActorRef::anonymous(), so a cloud TenantGate can act on it. Fixed in PR #271, which removed the `#[ignore]` attribute; the test now passes unconditionally.

### Issue #199 / #200 regression: actor attribution and inbox isolation

These tests reproduce two fixed bugs: #200: from_actor stamped as 'local' when sender has no actor.id configured but sends to a specific actor label.  Addressed sends from anonymous callers must be rejected; party-line self-sends (to="local") still work. #199: inbox actor-filter skipped when caller resolves to anonymous/'local'. An unconfigured caller must NOT see messages addressed to other actors; they must only see messages whose to_actor is "local" (or absent/NULL).

### X-Khive-Thread-ID header correlation (thread-UUID fallback)

When our own outbound email carries X-Khive-Thread-ID = <thread_uuid>, a reply that preserves that header arrives with correlation_external_id = <thread_uuid>. The existing pass-1 (external_id match) finds nothing because thread_uuid ≠ the note's external_id (which is a Message-ID).  The new pass-2 matches $.thread_id on an outbound note to recover from_actor and route the reply back to the original sender's actor.

### issue #403: In-Reply-To/References on outbound replies (native MUA threading)

khive's own thread continuity uses X-Khive-Thread-ID / external_id correlation (tested above); native mail clients (iPhone Mail, Gmail) instead group conversations by RFC 5322 Message-ID ancestry, which these tests cover.

### issue #403: References must carry the full ancestor chain

The prior implementation set References from the single `in_reply_to` value, dropping any ancestors before the immediate parent. These tests assert the exact serialized References/In-Reply-To values (not just presence) for each required case.

### #494: comm.thread tail pagination (order + after cursor)

NOTE: `comm.send`/`comm.reply` targeting the caller's own namespace ("local") write BOTH an outbound and an inbound copy of every logical message into that same namespace (dual_write_message, ADR-057) — so each `content` string below appears TWICE in an unfiltered thread(), consecutively (outbound then inbound), since the inbound copy is always written a moment after the outbound copy in the same call. Tests account for this pairing explicitly rather than assuming one physical note per logical send (matches the existing #485/H3 tests' use of tolerant `>=` counts for the same reason).

### Issue #820: child-process self-address must be loud, not silent

A child process spawned in the same project scope resolves its actor identity from the same worktree-scoped `.khive/config.toml` as its parent process (ADR-096 Fork 2: `[actor] id` is a per-project, not per-session, injection tier). When the child addresses that shared label expecting to reach a distinct parent principal, `from_actor` and `to_actor` collapse onto the identical string with no error and no distinct inbox.

### send-single-txn: atomic dual-write coverage

`dual_write_message` now commits both message copies (row + FTS + one vector row per registered embedding model) through `khive_runtime::create_notes_atomic` — ONE writer transaction for the pair instead of one writer acquisition per row/FTS/vector write. This test covers the multi-model vector fan-out count landing inside the single atomic unit.

## Extended per-test notes

Additional rationale for individual tests beyond the one-line proof statement kept inline, keyed by test function name, in source-file order.

### `delivered_rejects_outbound_only_identical_body`

Content is never used as a delivery heuristic.

### `delivered_confirms_inbound_after_outbound_disappears`

It must not require an outbound row to resolve first, which keeps the read useful for legacy/imported states and direct ambiguity fixtures.

### `delivered_ignores_matching_inbound_in_another_namespace`

An unrelated namespace cannot manufacture a positive result for the caller by reusing its UUID.

### `test_send_writes_outbound_in_caller_ns`

Cross-namespace sends are denied (issue #481 fix). Same-namespace sends must produce both outbound and inbound copies.

### `test_send_writes_inbound_in_recipient_ns`

Cross-namespace sends are denied (issue #481 fix). Same-namespace send creates both copies in the caller's namespace.

### `test_inbox_returns_inbound_for_recipient`

A session with actor_id="lambda:khive" sends to itself; the inbound copy has to_actor="lambda:khive" and is visible to the same registry's inbox (filter matches).

### `test_reply_from_sender_routes_to_recipient`

Within the same namespace: A sends to self (from=A, to=A). Sender replies to the outbound copy. Because from==to, the reply routes back to the same namespace (which is correct — there is no other party in a self-send). Cross-namespace send is denied (issue #481 fix).

### `test_reply_from_recipient_routes_to_sender`

Within same-namespace: both are the same namespace so the routing is always self. This test verifies reply() works on an inbound message and preserves the metadata. Cross-namespace send is denied (issue #481 fix).

### `test_reply_marks_directionless_legacy_original`

Before this, requiring a literal "inbound" made a directionless legacy record report `marked_read: null`, which is specified to mean "outbound".

### `test_reply_read_patch_preserves_concurrent_properties`

Both writes use the storage layer's one-statement JSON-property setter, so the invariant does not depend on a best-effort re-read immediately before replacement.

### `test_send_inbound_failure_rolls_back_outbound`

We simulate inbound failure by passing an invalid recipient namespace string (khive namespace syntax forbids control characters). The outbound note must not be persisted either.

### `test_inbox_returns_self_send_as_inbound`

Before the fix, inbox always returned 0 for self-sends because no inbound note was written.

### `test_list_message_thread_id_filter`

Before the fix, thread_id was silently ignored and all messages were returned.

### `test_list_message_direction_filter`

Before the fix, direction was silently ignored and all messages were returned.

### `test_read_rejects_outbound_message`

Before the fix, read() silently mutated outbound messages, corrupting the read/unread invariant. Cross-namespace send is denied (issue #481 fix). Same-namespace send is used here; the outbound copy stays in lambda:khive.

### `t87_non_addressee_read_rejected_and_stays_unread`

The message must stay unread after the rejected attempt.

### `t87_legacy_message_without_to_actor_reads_fail_open`

Decision: fail-open (with a tracing warning) — mirrors the inbox `EqOrMissing` filter precedent (#199), where legacy to_actor-less messages stay visible to any caller. Failing closed here would leave such messages permanently unreadable and stuck "unread", defeating the unread-based wake/sweep logic this fix protects.

### `test_thread_verb_returns_threaded_messages`

Before the fix, the thread verb was not registered, causing "unknown verb" error.

### `test_reply_delivers_inbound_to_recipient`

Before the fix, reply() created only an outbound note via a single create_note call, so inbox() would not surface the reply. Cross-namespace send is denied (issue #481 fix). Same-namespace send is used here — both copies land in the caller's namespace.

### `test_thread_rejects_nonexistent_root`

Before the fix, thread() accepted any resolvable UUID and returned Ok with count=0.

### `test_inbox_paginated_scan_finds_message_beyond_prefetch_window`

Before the fix, inbox() fetched at most limit*4 notes and applied in-memory filtering — if all newest notes were outbound, older inbound messages were invisible. This test creates 25 outbound-only messages before the inbound message to push it outside the old window.

### `test_cross_namespace_thread_query_finds_reply`

Before the fix, dual_write_message did not stamp the outbound copy with a canonical thread_id. The reply's thread_id was then set to the inbound copy UUID, causing thread(id=outbound_id) to miss the reply. After the fix, both copies share the same canonical thread_id (outbound UUID), and all replies carry that thread_id so the thread query finds them. Cross-namespace send is denied (issue #481 fix). Same-namespace send is used to test the canonical thread_id invariant.

### `test_list_message_finds_match_beyond_1000_backlog`

Before the fix, the handler fetched at most (limit*10).min(1000) rows and applied an in-memory filter — a single matching message buried beyond 1000 non-matching rows would be silently missed. After the fix, the handler paginates through the store in 200-row chunks until either `limit` filtered matches are collected or the scan ceiling (10000) is reached.

### `test_inbox_read_filter_json_type_truth_table`

Seeds messages with $.read set to: missing, bool false, bool true, string "true", integer 1. Verifies that inbox(status=unread) and inbox(status=read) classify each case correctly.

### `send_response_thread_id_round_trips_root_and_continuation`

A root send reports the note's own UUID; a continuation send echoes the caller-supplied root.

### `t_actor_inbox_filters_to_actor`

B's inbox should see the message; A's inbox should not.

### `t_anonymous_actor_inbox_filters_addressed_messages`

Messages sent to specific actor labels (e.g. "lambda:x", "lambda:y") are NOT visible to anonymous callers — this closes the cross-actor inbox read leak. Prior behavior (pre-fix): all messages were visible to anonymous callers ("party-line fallback"). That behavior was the bug.

### `t_c2_inbox_isolation_cross_actor`

This is the end-to-end isolation assertion for #199. It PASSES today when `actor_id` is properly configured per-registry. The test documents that the to_actor filter is active and working — the misconfiguration footgun (missing actor_id → party-line) is addressed separately by the strict-mode startup gate (`KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`).

### `t_c2_gate_receives_configured_actor_not_anonymous`

When `actor_id = "lambda:tenant-x"` is set on the `VerbRegistryBuilder`, the `GateRequest.actor.id` must equal `"lambda:tenant-x"` so that a cloud `TenantGate` can enforce per-actor policies. Fixed in PR #234 by threading the configured actor into `VerbRegistry::dispatch` before the gate consult. See: https://github.com/ohdearquant/khive/issues/224

### `i199_200_anonymous_send_to_specific_actor_is_warned`

The send is NOT rejected (to preserve backward compatibility with sessions that set default_namespace but not actor_id), but attribution is mis-stamped. A tracing::warn! is emitted. This is a known limitation pending issue #75 (actor identity per request). The important invariant: even with the corrupted from_actor, the message is stored and the #199 inbox fix prevents OTHER anonymous callers from reading messages with to_actor set to a specific label.

### `i199_200_anonymous_send_to_local_still_works`

The fix must not break OSS single-tenant deployments where everyone is 'local'.

### `i199_anonymous_inbox_cannot_read_messages_addressed_to_other_actor`

Before the fix, `comm.inbox` with an unconfigured caller (actor="local") returned ALL inbound messages regardless of `to_actor`, leaking cross-actor inbox content. After the fix, the anonymous caller only sees messages with to_actor="local" or to_actor absent/NULL.

### `i199_anonymous_inbox_sees_local_messages`

Party-line messages (to_actor="local" or to_actor absent) must remain visible to anonymous callers — this is the OSS single-tenant case.

### `list_message_thread_filter_matches_legacy_hex_label_and_uuid_prefix`

A genuine UUID prefix must still resolve.

### `list_thread_prefix_resolution_ignores_non_message_notes`

A non-message note carrying a `thread_id` property that shares the queried prefix must not inject a second candidate (which would surface as a false "ambiguous thread_id prefix" error) when the caller lists with the substrate-level kind that leaves the note kind unbound.

### `list_thread_prefix_resolves_across_configured_visible_namespaces`

A thread stored only in a configured visible namespace is returned by the list path, so its prefix must resolve rather than error "no message thread matches".

### `ingest_routing_reply_correlates_bracket_free_in_reply_to`

Pass-1 must still correlate the reply back to the original sender. Before the bracket-toggle fix this fell through to `default_inbound_actor` (lambda:leo) with a fresh thread — the exact failure seen on the live round-trip.

### `ingest_routing_reply_matches_legacy_urn_and_upper_hex_thread_id`

A canonical incoming correlation must still find a legacy outbound row whose `thread_id` was persisted in one of those forms.

### `reply_sets_in_reply_to_for_inbound_originated_parent`

The reply must read `wire_message_id` and wrap it for the wire.

### `reply_sets_in_reply_to_for_outbound_minted_parent`

The reply must reuse it verbatim.

### `reply_extends_references_chain_for_outbound_parent`

A reply must extend THAT chain, proving the direction-aware read is wired through `comm.reply` end-to-end, not just unit-tested on `parent_references_chain`.

### `reply_dedups_tainted_parent_references_chain_containing_parent_id`

The duplicate is dropped and first-seen order is preserved: the parent's id keeps its original position in the chain rather than being appended again at the end.

### `ingest_rejects_malformed_thread_id_without_writing_note`

Before the fix, an invalid thread_id was silently filtered out and replaced with a fresh UUID, splitting the message into the wrong conversation while still reporting success.

### `ingest_correlation_canonicalizes_legacy_compact_root_for_thread_lookup`

The new inbound row must canonicalize that root, and a thread lookup through a pre-v1 child id must still include every spelling.

### `thread_from_canonical_rows_includes_all_legacy_uuid_spellings_once`

Starting from either the canonical root or a new v1 child must recover every legacy formatter spelling, not only the spelling carried by the selected row.

### `health_includes_resource_self_report`

No computed `healthy` field, matching the rest of this verb's contract.

### `health_reports_null_stalled_for_legacy_heartbeat`

Their staleness is unknown, not false: `false` would misreport an old frozen row as current.

### `health_scoped_to_injected_namespace_sees_only_its_own_rows`

Plants one row directly under `"local"` and one directly under a non-local `"tenant-a"` namespace, planting directly rather than via `comm.heartbeat` to isolate the `comm.health` read path under test — the heartbeat write-path namespace is covered by the #917 writer tests below. An unscoped call defaults to `"local"` and must see only the local row; a call with an explicit `namespace="tenant-a"` must see only tenant-a's row, never local's. Also asserts the response's `namespace` field (khive #877) names the namespace actually read for both the unscoped and the explicitly-scoped call, so a caller can tell "no daemon anywhere" apart from "no rows under my scope yet" instead of the two cases being indistinguishable client-role/empty-channels responses.

### `authorized_writer_persists_heartbeat_under_its_own_tenant_namespace`

A tenant-scoped `comm.health` read now sees that row (closing the "reads an empty set by construction" gap #917 reports), while the default (local-scoped) `comm.health` read is unaffected — heartbeat rows for different namespaces do not bleed into each other.

### `two_tenants_same_channel_get_distinct_heartbeat_rows`

Were the namespace dropped from the id hash, both writes would resolve to one UUID and the second would replace the first (`upsert_note`'s `INSERT OR REPLACE` keys on the row id), so `comm.health(namespace="tenant-a")` would return an empty set instead of tenant-a's own heartbeat — and every existing #917 test would still pass. Drives the real handler via `dispatch_as` (not direct note planting) so it pins the write-path id derivation the plant-based read-path test cannot.

### `t494_thread_after_id_cursor_returns_strictly_later_messages`

The cursor resolves to the OUTBOUND copy's `full_id` (what `comm.reply` returns), which post-#94 is also the canonical id of the "reply-1" logical message itself — so it is excluded by the strict `>` comparison, and there is nothing after it (reply-1 was the last message sent).

### `t494_thread_order_desc_with_after_id_cursor_returns_strictly_older_in_desc_sequence`

"After" in desc order means further along the desc traversal, i.e. strictly older.

### `t94_thread_round_trip_returns_deduped_logical_messages`

`comm.thread` must return exactly 3 logical messages (not 6 ADR-057 dual-write physical copies), in chronological order, each attributed to the actor that actually sent it, with no duplicate entries (issue #94 symptom 3).

### `i820_child_to_parent_delivery_with_distinct_identities_succeeds`

This is the "no bug" baseline the fix must not regress.

### `i820_unflagged_self_address_is_a_loud_error`

This must now be a loud error, never a silent delivery into the sender's own inbox.

### `send_lands_outbound_inbound_fts_and_vectors_with_multi_model_counts`

Two stub models are registered: the vector-row count for each model must be exactly 2 (outbound + inbound).

### `update_refuses_to_forge_owner_established_properties_on_message_note`

This is the central regression test — send a message, forge via update, assert the forgery is refused and the stored value is unchanged. Table-driven over every key in `OWNER_ESTABLISHED_PROPERTIES` (khive-runtime's `curation.rs`, kept in sync by hand here since the const is crate-private and this is a different crate). Nothing here detects that drift: a key added to the const without an arm added below leaves this test green and that key uncovered, so a change that protects a new key adds its arm here in the same change. For each key, a complete snapshot of the note's stored `properties` is compared before and after the refused attempt — not just a handful of named fields — so a forgery that lands on any untested field is still caught.

### `create_derives_from_actor_overwriting_a_forged_value`

This is not a refusal: the create succeeds and the identity property is silently corrected to the value the authorization token actually names.

### `comm_send_still_stamps_from_actor_with_validator_installed`

A guard that broke the writer that is supposed to set `from_actor` would fail closed into an outage — prove it does not. MECHANISM SENSITIVITY: this arm stays green even with the atomic multi-note writer's own derivation call removed entirely, because `comm.send`'s handler (`crates/khive-pack-comm/src/handlers.rs`) derives and stamps `from_actor` onto the message spec BEFORE it ever reaches `khive-runtime`'s `create_notes_atomic_with_report`. A failure here means the send handler itself, or the ordinary (non-atomic) write path, broke — it says nothing about the atomic writer's own guard. That coverage lives in `khive-runtime`'s `atomic_message::tests::create_notes_atomic_derives_from_actor_overwriting_a_forged_value`, which calls the atomic writer directly with a forged property and would fail if this arm alone were relied on.

### `create_leaves_generic_kind_properties_untouched`

MECHANISM SENSITIVITY: the foreign-kind passthrough assertion alone would stay green even if the validator were never installed at all — with no validator, every kind's properties pass through untouched, so that assertion by itself cannot tell "validator installed and correctly scoped to `message`" apart from "no validator at all". The paired `message` assertion below closes that gap: it only passes if a validator is installed AND correctly scoped, so this arm fails if the validator is missing, not just if it is mis-scoped.

### `merge_preserves_into_note_from_actor_under_prefer_into`

Note (PR #1690): this arm is NOT sensitive to the preservation step being removed. `PreferInto`'s fold (`merge_json` in `khive-runtime`'s `curation.rs`) only ever inserts a `from`-note key that is absent on `into` — `from_actor` is already present on X's into-note before the merge runs, so the fold itself never touches it. This arm stays green with `preserve_owner_established_properties` deleted entirely; it is a legitimate control (it proves the merge doesn't silently overwrite under this strategy) but it is NOT evidence the guard works. `merge_preserves_into_note_from_actor_under_prefer_from` above is the arm carrying the security-relevant assertion: `PreferFrom`'s fold does overwrite `from_actor` with Y's value, so that arm only passes because the preserve step reverts it.

### `merge_reports_properties_merged_for_key_that_actually_survives`

Only the genuinely new non-owned key (`added`) contributed to the fold, so `properties_merged` must report 1, not 0, and the non-owned key must actually survive on the merged note.

### `merge_reports_zero_properties_merged_for_nested_union_reversion`

The round-2 fix above corrected the flat case (`merge_reports_properties_merged_for_key_that_actually_survives`); this is the nested case that fix left uncorrected. Under `union` the fold recurses into `thread_id` and counts the absorbed note's nested key as a merged contribution, but restoration then reverts `thread_id` wholesale back to the into-note's pre-merge value — so nothing the fold counted actually survived, and `properties_merged` must report 0.

### `merge_reports_zero_properties_merged_when_restoration_reverts_the_only_new_key_through_the_route`

`external_id` is an `OWNER_ESTABLISHED_PROPERTIES` key `comm.send` never sets, so the into-note's property map genuinely lacks the key entirely (unlike `subject`, which `comm.send` always writes, even as `null` — a present-but-null key would already be "in" the into-note's map and wouldn't exercise the "absent from into" removal path). The from-note is built with `create` so `properties` can name `external_id` directly. Under `prefer_from` the fold treats `external_id` as a genuinely new key — the into-note's property map does not have it — and counts it as one contribution. Restoration then reverts it: `external_id` is absent on the into-note, so it is removed from the merged result rather than kept. Nothing the fold counted actually survives, so `properties_merged` must report 0, and the into-note's owner-established properties (here `from_actor`) must still read as the into-note's own, not the absorbed note's.

### `i1471_attributed_sent_box_excludes_legacy_rows`

Only the anonymous `"local"` actor gets the EqOrMissing fallback.

### `sent_box_rejects_empty_to_actor_filter`

It is rejected with the same shape as the empty substring-filter validations.

## Helper function notes

### `build_crossns_registry`

`dispatch_ns` — the default namespace used for dispatch (the caller identity). `allowed_outbound` — namespaces this sender may deliver into cross-namespace. Both registries in a cross-ns pair must share the same `Arc<khive_db::StorageBackend>` so that outbound notes written in one namespace are visible via the other's token.

### `build_actor_registry`

The minted token's actor.id will equal `actor_id`, activating the to_actor filter in handle_inbox.

### `insert_thread_message`

Cursor/tie-break/ordering tests need exact control over timestamps (including two rows sharing the same microsecond) that racing the wall clock through the normal dispatch path cannot guarantee.

### `build_registry_with_owned_kinds`

Without this the runtime never learns `message` is pack-owned and the guard stays inert.

### `build_registry_with_owned_kinds_and_validator`

A test exercising the CREATE- or MERGE-path guard against a registry that skips this call proves nothing: the derive/preserve step is inert on an unwired runtime exactly like the update-path refusal was inert before `install_pack_owned_note_kinds` existed.
