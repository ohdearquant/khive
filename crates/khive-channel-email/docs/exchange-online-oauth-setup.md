# Exchange Online OAuth setup (Microsoft 365)

This guide wires the khive email channel to a Microsoft 365 / Exchange Online
mailbox using **app-only OAuth (client credentials, XOAUTH2)**. The channel
authenticates itself with a registered application identity, so it sends and
receives unattended with no interactive human login and no stored mailbox
password. It reuses the channel's `lettre` (SMTP) and `imap` (IMAP) transports;
no Microsoft Graph rewrite is required.

The same setup works for any agent that needs its own mailbox. "Email the agent"
is a low-friction front door: a user sends a message to an address, the agent
receives it through IMAP, acts, and replies through SMTP.

All commands below are reproduced verbatim from Microsoft Learn; only the
identifiers are placeholders. Replace every `<...>` token and the example
address `agent@example.com` with your own values.

## Prerequisites

- A Microsoft 365 / Exchange Online tenant.
- A mailbox for the agent (a licensed user mailbox or a shared mailbox; app-only
  OAuth supports both).
- Administrator access: an account that can register an Entra application and
  grant admin consent, and an account with the Exchange administrator role.
- PowerShell with the `ExchangeOnlineManagement` module (installed in Part 2).

## Part 0 — Values you will collect

| Value | Where to find it | Sensitivity |
| --- | --- | --- |
| Application (client) ID | Entra → App registrations → your app → Overview | Not secret |
| Directory (tenant) ID | Same Overview page | Not secret |
| Enterprise application Object ID | Entra → **Enterprise applications** → your app → Object ID | Not secret |
| Client secret **Value** | App → Certificates & secrets (shown once) | **Secret — store in env only** |

> **The Enterprise-application Object ID is not the App-registration Object ID.**
> They are two different values. The `New-ServicePrincipal` command in Part 2
> requires the one from **Enterprise applications**. Using the wrong one fails
> authentication.

## Part 1 — Register the application in Entra

At [entra.microsoft.com](https://entra.microsoft.com), signed in as an
administrator:

1. **Identity → Applications → App registrations → New registration.**
2. Set **Name** (for example `khive-mail`), **Supported account types** to
   *Accounts in this organizational directory only (Single tenant)*, and leave
   **Redirect URI** blank. Click **Register**.
3. On the app's **Overview** page, copy the **Application (client) ID** and the
   **Directory (tenant) ID**.
4. **Certificates & secrets → Client secrets → New client secret.** Choose an
   expiry and **copy the Value immediately** — it is shown only once.
5. **API permissions → Add a permission → APIs my organization uses → Office 365
   Exchange Online → Application permissions.** Check `IMAP.AccessAsApp`, click
   **Add**, then **Grant admin consent**.
6. Go to **Enterprise applications**, find the app, and copy its **Object ID**
   (the value flagged in Part 0).

> Add **only** `IMAP.AccessAsApp` here (for receiving). Do **not** add
> `SMTP.SendAsApp` as an Entra API permission. Sending is authorized by the
> Exchange RBAC role assignment in Part 2; adding it here triggers a redundant
> mailbox-permission check and causes a conflict.

Part 1 is complete when you have the three IDs and a saved client secret, and
`IMAP.AccessAsApp` shows admin consent granted.

## Part 2 — Exchange Online PowerShell

This binds the application to a single mailbox. Replace `<TENANT_ID>`,
`<CLIENT_ID>`, `<ENTERPRISE_APP_OBJECT_ID>`, and `agent@example.com` throughout.

### 1. Connect to Exchange Online

```powershell
Install-Module -Name ExchangeOnlineManagement
Import-Module ExchangeOnlineManagement
Connect-ExchangeOnline -Organization <TENANT_ID>
```

`Connect-ExchangeOnline` prompts for an interactive administrator sign-in. In
environments where the browser sign-in is intercepted or unavailable (for
example a tenant whose admin portal is managed by a third-party reseller), use
the device-code flow instead:

```powershell
Connect-ExchangeOnline -Device -ShowBanner:$false
```

Fully unattended app-only `Connect-ExchangeOnline` requires a **certificate**,
not a client secret. The client secret created in Part 1 authenticates the mail
transports (SMTP/IMAP), not the PowerShell management session.

### 2. Enable SMTP AUTH submission on the mailbox

SMTP AUTH submission is **disabled by default** at the tenant level (a Microsoft
secure default). This is a **separate** control from the OAuth/RBAC permissions:
the RBAC role in step 4 authorizes *what* may send as the mailbox; this setting
authorizes the mailbox to use the SMTP AUTH submission protocol *at all*. Both
are required. OAuth/XOAUTH2 is subject to this gate, not exempt from it. Without
it, authentication fails with `535 5.7.139 SmtpClientAuthentication is disabled
for the Tenant` before send-as is ever checked.

Enable it **per-mailbox**, which overrides the tenant-wide default (no need to
open SMTP AUTH tenant-wide):

```powershell
Set-CASMailbox -Identity agent@example.com -SmtpClientAuthenticationDisabled $false
```

`$false` means the *disabled* flag is off — that is, SMTP AUTH is **enabled**.
The per-mailbox value takes precedence over the organization setting. Verify:

```powershell
Get-CASMailbox -Identity agent@example.com | Format-List SmtpClientAuthenticationDisabled
# expect:  SmtpClientAuthenticationDisabled : False
```

A portal alternative exists: Microsoft 365 admin center → Users → Active users →
select the mailbox → Mail → Manage email apps → check **Authenticated SMTP** →
Save.

> **If sending still returns `535` after this**, two tenant-level policies can
> independently block SMTP AUTH, both above the per-mailbox override:
>
> 1. **Security defaults** (Entra → Overview → Properties → Manage security
>    defaults). This is the most common cause and it sits *above* the per-mailbox
>    setting. Microsoft's own statement: "If security defaults is enabled in your
>    organization, SMTP AUTH is already disabled in Exchange Online. To use SMTP
>    AUTH, you need to disable security defaults." The per-mailbox `$false` only
>    overrides the organization `Set-TransportConfig` setting, not security
>    defaults, so an error that still says "disabled for the Tenant" after a
>    verified per-mailbox enable points here. Disabling security defaults needs
>    the Conditional Access Administrator (or Global Administrator) role and turns
>    off tenant-wide enforced MFA and legacy-auth blocking; the supported
>    replacement is a Conditional Access policy (Entra ID P1). Treat it as a
>    security-posture decision, not a config tweak.
> 2. An **authentication policy** that disables basic auth for SMTP also blocks
>    the protocol "even if you enable the settings outlined in this article."
>    Check with `Get-AuthenticationPolicy`.
>
> A per-mailbox change can take a few minutes to propagate, so retry once; if it
> persists, security defaults is the first thing to check.

#### Finding and disabling security defaults

The security-defaults toggle is buried, and direct deep-links to it tend to
404. The reliable path:

1. Sign in at [entra.microsoft.com](https://entra.microsoft.com) as an
   administrator.
2. In the left nav, **Entra ID → Overview**, then open the **Properties** tab.
   (If the search bar is faster, type "security defaults" and pick the
   **Properties** result — the section lives at the bottom of that page.)
3. Scroll to the **Security defaults** section at the bottom. It shows the
   current state ("Your organization is protected by security defaults" when
   enabled).
4. Click **Manage security defaults**. A panel opens on the right with a single
   dropdown.
5. Set the dropdown to **Disabled**, click **Save**, and choose a reason when
   prompted (for example, "My organization is using Conditional Access" or
   "Other").

The change takes effect within a minute or two; re-test the SMTP send after.

> Disabling security defaults removes tenant-wide **enforced** MFA. On a
> single-admin tenant, turn MFA back on for the admin account manually
> afterward (an Authenticator-app method on the account), so disabling the
> blanket enforcement does not leave the admin login unprotected.

### 3. Register the application's service principal in Exchange

Run once; the service principal is shared by send and receive.

```powershell
New-ServicePrincipal -AppId <CLIENT_ID> -ObjectId <ENTERPRISE_APP_OBJECT_ID> -DisplayName "khive-mail"
```

`-ObjectId` uses the **Enterprise applications** Object ID, not the App
registration one.

### 4. Grant send permission (RBAC, scoped to the mailbox)

```powershell
New-ManagementScope -Name "agent-mail-scope" `
  -RecipientRestrictionFilter "PrimarySmtpAddress -eq 'agent@example.com'"

New-ManagementRoleAssignment -Name "agent-smtp-rbac" `
  -Role "Application SMTP.SendAsApp" `
  -App <CLIENT_ID> `
  -CustomResourceScope "agent-mail-scope"
```

This RBAC role — not an Entra API permission — is what authorizes the app to
send as the mailbox over OAuth. It lives in Exchange, deliberately not in the
Entra app registration.

### 5. Grant receive permission (mailbox access for the service principal)

```powershell
$sp = Get-ServicePrincipal -Identity "khive-mail"
Add-MailboxPermission -Identity "agent@example.com" -User $sp.Identity -AccessRights FullAccess
```

`-User` must be `$sp.Identity` (returned by `Get-ServicePrincipal`), not the raw
Entra Object ID.

### 6. Verify

```powershell
Test-ServicePrincipalAuthorization -Identity <CLIENT_ID> -Resource "agent@example.com"
```

Part 2 is complete when step 2 shows `SmtpClientAuthenticationDisabled : False`
and this command returns `Granted = True` on the SMTP row. RBAC is cached; full
propagation can take 30 minutes to 2 hours, so a correct configuration may still
need time before live sends succeed.

## Part 3 — SPF (deliverability)

If the domain's SPF record does not authorize Exchange Online, mail from the
mailbox tends to land in spam. Add the Outlook include to the existing record at
your DNS provider:

```
v=spf1 include:spf.protection.outlook.com -all
```

If the domain already sends through another provider, keep that provider's
include alongside the Outlook one, for example:

```
v=spf1 include:spf.protection.outlook.com include:<existing-provider> -all
```

This is a deliverability fix, not an authentication blocker.

## Part 4 — Configure the channel

The channel reads its configuration from environment variables (see
[`.env.example`](../.env.example) for the full template). The `kkernel` binary
loads `~/.khive/.env` at startup; real process environment variables take
precedence over the file.

```bash
# ~/.khive/.env  (never committed to git)
KHIVE_EMAIL_SMTP_HOST=smtp.office365.com
KHIVE_EMAIL_IMAP_HOST=outlook.office365.com
KHIVE_EMAIL_USERNAME=agent@example.com
KHIVE_EMAIL_MAILBOX=agent@example.com
KHIVE_EMAIL_MAINTAINER_ADDRESS=operator@example.com

# OAuth app-only mode — set all three
KHIVE_EMAIL_OAUTH_TENANT_ID=<tenant id>
KHIVE_EMAIL_OAUTH_CLIENT_ID=<client id>
KHIVE_EMAIL_OAUTH_CLIENT_SECRET=<secret value>
```

> **Keep the client secret out of chat, git, and any khive store.** The three
> IDs (tenant, client, enterprise-object) are not secrets, but the secret
> **Value** belongs only in the environment. Reference secrets by env-var name,
> never by value.

## Appendix — OAuth / XOAUTH2 details (reference)

The channel performs these steps; they are documented here for reference, not as
manual setup.

### Token request

```
POST https://login.microsoftonline.com/<TENANT_ID>/oauth2/v2.0/token
grant_type=client_credentials
client_id=<CLIENT_ID>
client_secret=<SECRET>
scope=https://outlook.office365.com/.default
```

SMTP and IMAP use the same scope, `https://outlook.office365.com/.default`. The
token lives 3600 seconds with no refresh token; the channel re-requests on
expiry.

### XOAUTH2 SASL string (same for send and receive)

```
base64("user=agent@example.com\x01auth=Bearer <token>\x01\x01")

IMAP wire:  AUTHENTICATE XOAUTH2 <base64>
SMTP wire:  AUTH XOAUTH2 <base64>
```

`\x01` is byte `0x01` (Ctrl-A). `lettre` provides `Mechanism::Xoauth2`
natively; the IMAP path uses a custom authenticator returning this string. The
SMTP transport uses **port 587 with STARTTLS** (port 465 would be implicit TLS).

## Known gotchas

- **`535 5.7.139 SmtpClientAuthentication is disabled`** — the SMTP AUTH
  submission gate (Part 2, step 2), distinct from OAuth/RBAC. Fix with
  `Set-CASMailbox -SmtpClientAuthenticationDisabled $false`; the per-mailbox
  value overrides the tenant default. If it persists, check security defaults and
  authentication policy (Part 2 callout).
- **RBAC propagation delay** — a role assignment takes 30 minutes to 2 hours to
  take effect. `Test-ServicePrincipalAuthorization` returning `True` means the
  configuration is correct; live sends may still wait on the cache.
- **Enterprise-app vs App-registration Object ID** — `New-ServicePrincipal`
  needs the Enterprise-applications Object ID. The wrong one fails silently at
  authentication time.
- **`IMAP.AccessAsApp` is a tenant-wide grant.** The real per-mailbox gate for
  receiving is `Add-MailboxPermission`. Scope mailbox access deliberately.
- **Do not use Application Access Policy** for scoping. It is marked legacy and
  pending deprecation. The RBAC path above is the current recommended approach.

## Sources

All commands and behavior are from Microsoft Learn: *Authenticate an IMAP, POP,
or SMTP connection using OAuth*; *Enable or disable SMTP AUTH in Exchange
Online*; *Configure SMTP onboarding to App RBAC*; *Security defaults in Microsoft
Entra ID*.
