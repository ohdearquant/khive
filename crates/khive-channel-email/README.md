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

## Provider setup guides

- [Exchange Online / Microsoft 365 (OAuth)](docs/exchange-online-oauth-setup.md)
- [Gmail / Google Workspace](docs/gmail-setup.md)
