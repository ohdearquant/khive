# External inbound email

External replies are enabled by the daemon's boot-resolved external-sender policy.
While disabled, inbound routing settings are inert and non-maintainer mail keeps
the existing `off-allowlist` disposition.

When enabled, `KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR` is required and nonempty.
`KHIVE_EMAIL_INBOUND_ROUTES` optionally contains comma-separated `address=actor`
pairs. Addresses are normalized and matched exactly; duplicate addresses and
malformed entries are startup errors. A verified outbound owner wins over an
alias. Otherwise the first configured alias across `Delivered-To`, `To`, then
`Cc` wins; unmatched mail goes to the external inbound actor.

Authentication and inbound suppression run before deterministic MIME screening.
Delivered external messages retain `origin=external`, `trust=untrusted`, their
normalized email sender and rule identifiers. Routing never grants trust.
Withheld messages contain a placeholder naming the labels; original RFC5322 bytes
are referenced from their atomic replay attachment for explicit `blob.get` reads.
Ordinary inbox, read, thread and notice surfaces do not carry those bytes.
Executable attachments or two withholding families are quarantined as `screened`.

An authenticated opt-out on a marked or withheld delivery commits an outbound
address suppression through the real ingest path before duplicate acknowledgement
or message creation. Store failure retains the message's existing bounded failure
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
