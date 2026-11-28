# IMAP backoff classification

Source: `crates/khive-channel-email/src/backoff.rs`

## Why this module exists

Per-credential IMAP guardrail: single-flight connection cap + jittered exponential
backoff on connect/auth and command transport failures (#605).

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

Classifies whether a `ChannelError` from an IMAP poll represents connection pressure
that should back off, versus a failure that should keep the normal poll cadence.

Grounded in the actual errors `crates/khive-channel-email`'s IMAP connect/auth flow
produces (see `connector/imap.rs::LiveImap::connect` and `fetch_since`):

- `ChannelError::Auth` — TLS handshake, `LOGIN`, and `XOAUTH2` failures. This is exactly
  the credential/slot-exhaustion class from the 2026-07-04 outage ("User is authenticated
  but not connected" surfaces here when Exchange rejects the handshake outright).
- `ChannelError::Transport` — TCP connect, greeting read, `EXAMINE`, `UID SEARCH`, and
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

## Command time budgets

After authentication, `EXAMINE INBOX`, `UID SEARCH` and the `RFC822.SIZE` fetch each
have a fresh 15-second elapsed budget. The budget includes writing the command and
consuming its entire response, including the final tagged completion. A response
that arrives in pieces does not renew the budget.

Each selected message's `BODY.PEEK` fetch has a separate budget based on its
validated `RFC822.SIZE`:

```text
15 seconds + ceil(RFC822.SIZE / 524288) seconds
```

The transfer allowance is one second per 512 KiB, rounded upward. A 1 MiB message
has 17 seconds; a 25 MiB message has 65 seconds. This budget includes the command
write, the complete literal and the final tagged completion. It is an elapsed
budget, not an inactivity timer. Each UID starts a new budget, so a page may take
longer than one command's allowance.

A command timeout returns `ChannelError::Transport` identifying the phase and
budget. It follows the existing backoff classification. The incomplete page is
rejected without advancing its progress, even if an earlier UID on that page
was fetched successfully. Dropping the session releases the credential's
single-flight permit so a later poll can reconnect and retry the same progress.
The permit is acquired before connecting and remains held through logout.

Logout also has a 15-second elapsed budget on all successful page branches:
an exhausted UID range, an empty search and a populated page. A logout error or
timeout is ignored after the page is complete; it does not discard that page.

The size allowance sets a finite transfer-rate floor. A server that repeatedly
takes longer than the base allowance plus the 512 KiB/s transfer allowance can
time out on every poll. That message remains retryable at the same progress;
timeouts do not automatically skip or quarantine it. These limits do not add a
whole-page deadline, a memory limit or external poll-loop liveness monitoring.
