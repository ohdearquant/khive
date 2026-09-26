# khive-channel-email

Email channel for the khive comm layer (ADR-056): polls an IMAP mailbox for
inbound messages and sends outbound mail over SMTP. Authenticates with OAuth2
(XOAUTH2) or basic auth. Built on `lettre` (SMTP) and `imap` (IMAP); no Graph
API dependency.

The channel is single-mailbox today. Multi-tenant, multi-provider operation is
under design in
[#371](https://github.com/ohdearquant/khive/issues/371).

## Configuration

All configuration is read from environment variables; see
[`.env.example`](.env.example) for the full, annotated template. The `kkernel`
binary loads `~/.khive/.env` at startup.

Inbound IMAP fetches use `RFC822.SIZE` before requesting bodies. Set
`KHIVE_EMAIL_IMAP_MAX_MESSAGE_BYTES` (default 25 MiB) and
`KHIVE_EMAIL_IMAP_MAX_PAGE_BYTES` (default 50 MiB) to limit memory used per
message and per poll page. The page limit must be at least the message limit.
Larger messages produce a quarantine marker without downloading the body;
messages deferred by the page limit resume on the next poll.

## Provider setup guides

- [Exchange Online / Microsoft 365 (OAuth)](docs/exchange-online-oauth-setup.md)
- [Gmail / Google Workspace](docs/gmail-setup.md)
