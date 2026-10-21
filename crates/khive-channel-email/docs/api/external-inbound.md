# External inbound email

External inbound is enabled by the daemon's boot-resolved external-sender policy,
or on its own by `KHIVE_EMAIL_EXTERNAL_INBOUND=enabled`, which receives external
mail while outbound external sending stays disabled. Any other non-empty value
of that variable refuses start-up. While external inbound is disabled, inbound
routing settings are inert and non-maintainer mail keeps the existing
`off-allowlist` disposition.

When enabled, `KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR` is required and nonempty.
`KHIVE_EMAIL_INBOUND_ROUTES` optionally contains comma-separated `address=actor`
pairs. Addresses are normalized and matched exactly; duplicate addresses and
malformed entries are startup errors. A verified outbound owner wins over an
alias. Otherwise the first configured alias across `Delivered-To`, `To`, then
`Cc` wins; unmatched mail goes to the external inbound actor.

A thread UUID in `X-Khive-Thread-ID` counts as a verified outbound owner only when
the named thread holds an outbound email whose Message-ID verifies under the
configured mailbox domains (ADR-200 Amendment 1 decision 4). An `In-Reply-To` or
`References` Message-ID is verified the same way. A UUID naming a thread with no
such email is not an owner, so routing falls through to the recipient aliases and
then the external inbound actor.

Authentication and inbound suppression run before deterministic MIME screening.
Delivered external messages retain `origin=external`, `trust=untrusted`, their
normalized email sender and rule identifiers. Routing never grants trust.
Withheld messages contain a placeholder naming the labels; original RFC5322 bytes
are owned by their atomic replay attachment and read through `comm.withheld_body`,
open to the message's recipient actor and the maintainer. The delivered message
carries no blob reference, and ordinary inbox, read, thread and notice surfaces do
not carry those bytes.
Executable attachments or two withholding families are quarantined as `screened`.

Quarantine metadata preserves the claimed sender as `quarantine_claimed_from`.
When a parsed message has `To` recipients, `quarantine_claimed_to` is a JSON array
encoded as a string, for example `["first@example.com","second@example.com"]`.
It preserves the order and duplicates in the existing parsed, lowercased `To`
list; it excludes `Delivered-To` and `Cc` and performs no additional normalization.
Both claimed fields are sender-asserted information for inspection. They do not
change quarantine attribution, its maintainer address, the resolved recipient
actor, or the replay notification recipient, and do not grant trust or route mail.
An empty `To` list omits the key. Malformed, oversized or missing-body envelopes
also omit it: those envelopes have no parsed `RawEmail` recipient list, and replay
bytes, when available, are not parsed again to infer one.

An authenticated opt-out on a marked or withheld delivery, or on a message
quarantined as `screened`, commits an outbound address suppression through the
real ingest path before duplicate acknowledgement or message creation. A
`screened` quarantine carries `screening_opt_out = "true"` when screening found
the opt-out, and the suppressed address is its `quarantine_claimed_from`: screening
runs only after the sender authenticated as external. No other quarantine reason
records an opt-out. Store failure retains the message's existing bounded failure
identity and holds the coordinator's checkpoint. The channel does not acknowledge
that obligation from its poll alone.

Plaintext opt-out detection retains its first nonquoted line rule. With no actual
plain part, the first nonempty line of the parser's HTML text conversion counts
as nonquoted; HTML blockquote false positives intentionally run toward suppression.
Extracted HTML is used only for opt-out and delivery text, preserving the distinct
`html_only` and hidden-HTML classifier inputs.

A failed owner lookup carries its ordered candidates to ingest without persisting
error text. Ingest repeats the verified ownership lookup so a typed failure keeps
the selected UID in the existing per-message 60/5 failure ledger. Healthy lookups
are not repeated, and sender-asserted correlation cannot override their result.

See [ADR-200](../../../../docs/adr/ADR-200-external-email-policy.md) sections 7–8
and [ADR-056](../../../../docs/adr/ADR-056-channel-transport-layer.md), October 14
Decisions 1–3, for the conditional withheld-custody and bounded failure outcomes.
