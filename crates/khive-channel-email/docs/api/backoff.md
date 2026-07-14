# IMAP backoff classification

Source: `crates/khive-channel-email/src/backoff.rs`

## Why this module exists

Per-credential IMAP guardrail: single-flight connection cap + jittered exponential
backoff on connect/auth failure (#605).

The 2026-07-04 inbound-email outage (#602) was amplified by the poll loop's flat ~5s
retry cadence with no per-credential concurrency cap: nine concurrent pollers on
`recipient@example.com` exhausted Exchange Online's per-mailbox connection slots, and the
flat retry hammer kept the slots saturated for ~19h. `#602`/`#610` fixed the multi-process
spawn that caused the concurrency; the types here make the channel degrade gracefully if
polling pressure ever returns.

## `ImapBackoff::record_failure` clamp regression guard

`delay` is clamped to `max` (regression guard): jitter is additive noise on top of
`step`, but `step` itself can already equal `max` once escalation saturates, so an
unclamped `step + jitter` would let a "~10min cap" reach 750s at the default 25% jitter
window. Clamping keeps `delay <= max` a hard invariant regardless of where jitter lands.

## `is_backoff_eligible`

Classifies whether a `ChannelError` from an IMAP poll represents connect/auth pressure
that should back off, versus a failure that should keep the normal poll cadence.

Grounded in the actual errors `crates/khive-channel-email`'s IMAP connect/auth flow
produces (see `connector/imap.rs::LiveImap::connect` and `fetch_since`):

- `ChannelError::Auth` — TLS handshake, `LOGIN`, and `XOAUTH2` failures. This is exactly
  the credential/slot-exhaustion class from the 2026-07-04 outage ("User is authenticated
  but not connected" surfaces here when Exchange rejects the handshake outright).
- `ChannelError::Transport` — TCP connect, greeting read, `SELECT`, `UID SEARCH`, and
  `UID FETCH` failures. Exchange's connection-slot exhaustion can also surface here (a
  post-login command failing because the mailbox has no free slot), so this class backs
  off too.

Not backoff-eligible:

- `ChannelError::Config` — static misconfiguration; backing off would only delay an
  operator noticing and fixing it faster.
- `ChannelError::UnauthorizedSender` — a per-message attribution gate failure, not a
  connectivity failure; never produced by `poll`/`connect`.
- `ChannelError::InvalidEnvelope` — malformed data, not connectivity; never produced by
  `poll`/`connect`.
