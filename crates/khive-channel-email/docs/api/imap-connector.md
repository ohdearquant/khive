# IMAP connector: page selection and poison-UID handling

Source: `crates/khive-channel-email/src/connector/imap.rs`. Covers how a fetched IMAP page
is validated and turned into per-message dispositions.

## `process_selected_page`

Validates a fully-fetched selected page and builds the `SelectedMessage` list, in
`selected_uids` order — exactly one entry per selected UID.

Every UID in `selected_uids` must appear exactly once in `fetched_raw`:

- A **gap** (a UID absent from the fetch response entirely) or a **duplicate** response for
  the same UID fails the whole page — no partial advancement. These are treated as protocol
  anomalies rather than permanent per-message failures, since a genuinely expunged message
  will not be re-selected on the next poll.
- A **missing or unparseable RFC822 body**, by contrast, is a permanent per-UID failure
  (khive #449 High fix): rather than failing the whole page and re-selecting the same
  poison UID forever, that UID gets a durable `SelectedMessage::Malformed` disposition so
  the caller can quarantine it and advance past it.
- A fetch response for a UID **outside** `selected_uids` is unrequested (e.g. a stray server
  response) and is ignored with a `warn!`; it never affects page validity or the candidate
  high-water mark.

See `crates/khive-channel-email/src/channel.rs`'s
`poll_page_malformed_uid_produces_a_stable_external_id_and_quarantine_metadata` test for the
end-to-end proof that a `Malformed` disposition survives into the `ChannelEnvelope` handed
to `comm.ingest`, not just this function's intermediate value.
