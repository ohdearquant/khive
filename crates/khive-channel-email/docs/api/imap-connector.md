# IMAP connector: page selection and poison-UID handling

Source: `crates/khive-channel-email/src/connector/imap.rs`. Covers how a fetched IMAP page
is validated and turned into per-message dispositions.

The live poll opens INBOX read-only with `EXAMINE` and fetches bodies with bounded
`BODY.PEEK[]` requests. Neither operation marks a message Seen; durable UID progress,
not server read flags, controls retries and checkpoint advancement.

## `process_selected_page`

Validates a selected page and builds the `SelectedMessage` list, in
`selected_uids` order — exactly one entry per selected UID.

Every UID that passed the size preflight must appear exactly once in `fetched_raw`:

- A **gap** (a UID absent from the fetch response entirely) or a **duplicate** response for
  the same UID fails the whole page — no partial advancement. These are treated as protocol
  anomalies rather than permanent per-message failures, since a genuinely expunged message
  will not be re-selected on the next poll.
- A **missing or unparseable RFC822 body**, by contrast, is a permanent per-UID failure
  (khive #449 High fix): rather than failing the whole page and re-selecting the same
  poison UID forever, that UID gets a durable `SelectedMessage::Malformed` disposition so
  the caller can quarantine it and advance past it.
- Before body fetch, `RFC822.SIZE` is required for each selected UID. A message above
  `KHIVE_EMAIL_IMAP_MAX_MESSAGE_BYTES` is quarantined with reason `too-large` and
  an empty replay body. Only the remaining UIDs are fetched, in order, with a
  bounded partial `BODY.PEEK[]` request. Aggregate fetched body bytes stay within
  `KHIVE_EMAIL_IMAP_MAX_PAGE_BYTES`, apart from at most one probe byte used to
  detect a body that grew after the size check. The first UID that would exceed the page
  budget is left for the next poll, and the checkpoint advances only through
  the processed prefix. Missing or duplicate size responses reject the page.
- `KHIVE_EMAIL_IMAP_MAX_MESSAGE_BYTES` is accepted up to 64 MiB, and a larger value refuses
  at startup. A quarantined message's original bytes are stored as one blob before the
  cursor advances past it, and a blob object holds at most 64 MiB, so a larger ceiling
  would admit a message whose quarantine could never be stored.
- Quarantine storage is bounded in aggregate as well as per message.
  `KHIVE_EMAIL_QUARANTINE_MAX_RETAINED` (default 256) caps how many live quarantine
  records in the ingest namespace may hold a stored original; the poller counts them
  before each `blob.put`, both for a message the adapter quarantined and for one it
  quarantines because `comm.ingest` refused it (the `quarantined_count` that `comm.health` reports, so the
  cap survives restarts and frees up as expired records are cleaned up). Once the cap
  is reached, a quarantined message is still ingested and the cursor still advances,
  but its bytes are not stored: the record has no `quarantine_content_ref` and carries
  `quarantine_original_retained: "false"` with `quarantine_original_not_retained_reason:
  "retention-limit"`. `0` stores no originals. If the count cannot be read the page is
  held and retried; an original is never dropped silently. Records stored without an
  original count toward the cap too, so it is conservative.
- Inbound polling also needs the blob pack's runtime to accept writes. When comm is
  writable but blob is read-only, the daemon does not start the poll and logs an error
  naming the blob pack runtime, rather than failing every `blob.put` and retrying the
  same message forever.
- A fetch response for a UID **outside** `selected_uids` is unrequested (e.g. a stray server
  response) and is ignored with a `warn!`; it never affects page validity or the candidate
  high-water mark.

See `crates/khive-channel-email/src/channel.rs`'s
`poll_page_malformed_uid_produces_a_stable_external_id_and_quarantine_metadata` test for the
end-to-end proof that a `Malformed` disposition survives into the `ChannelEnvelope` handed
to `comm.ingest`, not just this function's intermediate value.
