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
- A fetch response for a UID **outside** `selected_uids` is unrequested (e.g. a stray server
  response) and is ignored with a `warn!`; it never affects page validity or the candidate
  high-water mark.

See `crates/khive-channel-email/src/channel.rs`'s
`poll_page_malformed_uid_produces_a_stable_external_id_and_quarantine_metadata` test for the
end-to-end proof that a `Malformed` disposition survives into the `ChannelEnvelope` handed
to `comm.ingest`, not just this function's intermediate value.
