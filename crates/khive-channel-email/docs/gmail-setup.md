# Gmail / Google Workspace setup

This guide wires the khive email channel to a Gmail or Google Workspace mailbox.
Like the [Exchange Online guide](exchange-online-oauth-setup.md), the goal is an
agent mailbox the channel can send from and receive into unattended.

Google offers two relevant authentication styles, with different support status
in the channel today:

| Path | Works with the channel today | Best for |
| --- | --- | --- |
| **App Password (basic auth)** | **Yes** — the channel's basic-auth mode | A single mailbox, fastest to stand up |
| **OAuth / XOAUTH2** (service account or refresh token) | **Not yet** — needs a Google token provider in the channel (see note) | Workspace fleets, no per-mailbox password |

> **OAuth support status.** The channel's OAuth token acquisition currently
> implements only Microsoft's client-credentials flow
> (`crates/khive-channel-email/src/oauth.rs`, endpoint
> `login.microsoftonline.com`, scope `outlook.office365.com/.default`). Google
> uses different token flows (service-account JWT bearer, or refresh-token
> exchange against `oauth2.googleapis.com`). Adding a Google token provider is
> tracked as part of the multi-provider / multi-tenant channel design
> ([#371](https://github.com/ohdearquant/khive/issues/371)). Until then, use the
> App Password path below for Gmail.

## Path A — App Password (works today)

App Passwords let the channel authenticate over SMTP and IMAP with basic auth,
without exposing the account's primary password. They require 2-Step
Verification to be enabled on the account.

### 1. Enable IMAP and 2-Step Verification

- **Consumer Gmail:** Gmail → Settings → **Forwarding and POP/IMAP** → **Enable
  IMAP** → Save. Then enable **2-Step Verification** in the Google Account
  security settings.
- **Google Workspace:** a super admin must allow IMAP access for the
  organizational unit (Admin console → Apps → Google Workspace → Gmail →
  End User Access → **IMAP access**), and 2-Step Verification must be on for the
  account.

### 2. Generate an App Password

In the Google Account security settings, open **App passwords**, create a new
app password (name it for the agent, for example `khive-mail`), and copy the
16-character value. It is shown once.

### 3. Configure the channel

```bash
# ~/.khive/.env  (never committed to git)
KHIVE_EMAIL_SMTP_HOST=smtp.gmail.com
KHIVE_EMAIL_IMAP_HOST=imap.gmail.com
KHIVE_EMAIL_USERNAME=agent@example.com
KHIVE_EMAIL_MAILBOX=agent@example.com
KHIVE_EMAIL_MAINTAINER_ADDRESS=operator@example.com

# Basic-auth mode (Mode B): set the password, set NONE of the OAuth vars.
KHIVE_EMAIL_PASSWORD=<16-char app password>
```

The channel's SMTP transport uses port 587 with STARTTLS by default
(`smtp.gmail.com:587`); IMAP uses port 993 (`imap.gmail.com:993`). Both are the
defaults, so the port variables can be left unset.

> **Keep the App Password out of chat, git, and any khive store.** It is a
> credential. Put it only in the environment, referenced by var name.

## Path B — OAuth / XOAUTH2 (Google-side setup, channel support pending)

Use this when a per-mailbox password is unacceptable, typically a Workspace
fleet. The Google-side setup is documented here so it is ready when the channel
gains a Google token provider. Both sub-paths produce an OAuth access token for
the scope `https://mail.google.com/`, which the channel would present over
XOAUTH2 exactly as it does for Exchange.

### Option 1 — Service account with domain-wide delegation (Workspace)

The unattended, app-only analog to Exchange app-only OAuth. Requires Google
Workspace (not consumer Gmail) and a super admin.

1. In the **Google Cloud Console**, create or select a project and **enable the
   Gmail API**.
2. Create a **service account**. Under its details, enable **domain-wide
   delegation** and note the numeric **client ID**. Create and download a **JSON
   key**.
3. In the **Workspace Admin console** → Security → Access and data control →
   **API controls** → **Domain-wide delegation**, add the service account's
   client ID and authorize the scope `https://mail.google.com/`.
4. The service account then mints OAuth tokens that **impersonate** the target
   Workspace mailbox (a JWT bearer grant against `oauth2.googleapis.com`), and
   the channel uses those tokens over XOAUTH2.

### Option 2 — OAuth client with a refresh token (single account)

Works for a single consumer or Workspace account without domain-wide delegation.

1. In the **Google Cloud Console**, configure the **OAuth consent screen** and
   create an **OAuth client ID**.
2. Perform a **one-time user consent** for the scope
   `https://mail.google.com/` to obtain an authorization code, then exchange it
   for a **refresh token**.
3. The channel exchanges the refresh token for short-lived access tokens and
   presents them over XOAUTH2.

### XOAUTH2 SASL string (for reference)

Identical in shape to the Exchange path; only the token source differs:

```
base64("user=agent@example.com\x01auth=Bearer <token>\x01\x01")
```

## Known gotchas

- **App Passwords require 2-Step Verification.** Without 2SV enabled, the App
  Passwords option does not appear.
- **"Less secure app access" is gone.** Google removed the legacy toggle; App
  Passwords (with 2SV) are the supported basic-auth path.
- **IMAP must be enabled.** Consumer Gmail disables IMAP by default; Workspace
  admins can restrict it per organizational unit.
- **Domain-wide delegation needs Workspace + super admin.** It does not apply to
  consumer Gmail accounts.
- **Send-as restrictions.** To send as a non-primary address, the address must
  be a verified send-as alias on the account, or a Workspace alias.

## Sources

Google Workspace Admin help and Google Identity documentation: *Sign in with app
passwords*; *Use OAuth 2.0 for server-to-server applications* (domain-wide
delegation); *Authorize with a refresh token*; *Gmail IMAP/SMTP settings*.
