# Off-Studio box bootstrap kit

Bootstrap kit for running the khive substrate (the `kkernel` daemon plus
`khive.db`) on a standalone Linux box, off the Mac.

Status: the scripts and the systemd unit in this directory were authored and
reviewed on macOS. They have not been executed on a target Linux box. Treat
the first run as a validation pass, not a known-good deploy, and read
`bootstrap.sh` before running it.

## Contents

- `bootstrap.sh`: installs build dependencies, Rust, clones/updates the repo,
  builds `kkernel` in release mode with the `channel-email` feature, installs
  the binary, and installs/starts a systemd `--user` unit.
- `kkernel.service`: the systemd `--user` unit installed by `bootstrap.sh`.
- `env.template`: placeholder environment file to copy to `~/.khive/.env`.
- `README.md`: this file.

## Prerequisites

- A Linux box with `sudo` access for the account that will run `kkernel`
  (needed once, for apt package installation).
- Outbound HTTPS access, at minimum to fetch the rustup installer, clone the
  repo, and pull crates.io dependencies during the build.
- An SSH key already authorized on the box, if you plan to use the SSH
  forwarding pattern described under "Mac-side access" below.

## Running bootstrap.sh

```bash
git clone https://github.com/ohdearquant/khive.git
cd khive
scripts/box/bootstrap.sh
```

By default this clones/builds `~/khive` at `main` and installs the binary to
`~/.cargo/bin/kkernel`. Override with a positional argument or environment
variables:

```bash
# Build a different ref into a different directory
REF=v0.3.0 scripts/box/bootstrap.sh /opt/khive

# Build from a fork
REPO_URL=https://github.com/<you>/khive.git scripts/box/bootstrap.sh
```

The script is idempotent: re-running it after a `git` update on the box (or
after changing `REF`) rebuilds, reinstalls the binary, and restarts the
systemd unit so the new build takes effect. It does not touch `~/.khive/.env`
or `~/.khive/khive.db`.

Unlike the Mac `make local` target, `bootstrap.sh` does not `pkill` running
`kkernel` processes before installing the new binary. On this Mac, `make
local` killing every `kkernel` process was flagged as a production-impacting
step; on Linux the atomic `mv` used here is safe against a running process
(the old process keeps its already-open handle to the old inode), and
`systemctl --user restart kkernel.service` is what actually cuts over to the
new binary, on the operator's terms.

## Placing secrets

`bootstrap.sh` does not write `~/.khive/.env`. After the first bootstrap run:

```bash
cp scripts/box/env.template ~/.khive/.env
chmod 600 ~/.khive/.env
# edit ~/.khive/.env with real values
scripts/box/bootstrap.sh
```

See `env.template` for the full variable list and which combinations are
required. The email channel (`channel-email` feature, built in by
`bootstrap.sh`) is inert until this file has valid SMTP/IMAP/auth values; an
incomplete config disables the email channel with a warning in the logs
rather than crashing the daemon. Re-running `bootstrap.sh`, rather than only
`systemctl --user restart kkernel.service`, both picks up the new secrets
and exercises the OAuth token-endpoint check described below, so a bad or
incomplete secret is caught at bootstrap time instead of discovered later
from a silently-disabled channel.

## Actor identity

This is a separate requirement from the email secrets above, and lives in a
different file: TOML config, not `.env`.

`comm.send` stamps `from_actor` from the caller's dispatch token
(`crates/khive-pack-comm/src/handlers.rs`). A daemon with no actor identity
configured mints anonymous tokens, so every `comm.send` stamps
`from_actor="local"`. A reply routed back to that outbound note then
resolves `to_actor="local"` instead of this box's inbox: a silent misroute,
not an error either side would notice on its own (tracked as issue #200 in
source). If this box is standing up as a specific lambda in the fleet
(`lambda:leo`, `lambda:khive`, or a sub-lambda), it needs its own identity
set before it sends or expects to receive addressed mail.

Set it in `~/.khive/config.toml` (create the file if it does not exist yet):

```toml
[actor]
id = "lambda:<you>"                # e.g. "lambda:leo", "lambda:khive"
display_name = "<optional human label>"
```

Then restart the daemon so it picks up the change:

```bash
systemctl --user restart kkernel.service
```

A `KHIVE_ACTOR` environment variable is also read as a fallback when no
`id` is set in `config.toml`, but when both are present the TOML value
wins, so treat `config.toml` as the source of truth for this box.

`bootstrap.sh` checks `~/.khive/config.toml` for a non-empty `[actor] id` as
the last step of its smoke test and prints a loud warning if it is missing,
listing the exact fix. It does not invent a default value: the runtime
itself treats a missing actor id as valid (an anonymous, single-tenant
"local" identity, the pre-issue-#200 default), so this box can legitimately
run without one if that is genuinely the intended deployment. The check
exists so that omission is a deliberate choice, not an oversight discovered
later from misrouted mail.

This kit installs one systemd unit, so this box runs as one actor identity.
If more than one lambda needs an independent identity on the same physical
box, that is a separate per-agent `~/.khive` home and daemon instance, not
covered by this single-daemon bootstrap flow.

## Email channel auth smoke

The email channel does not crash the daemon when its OAuth configuration is
incomplete or its client secret is invalid or expired; it disables itself
with a warning in the logs (`crates/khive-channel-email/src/config.rs`). On
a headless box nobody is watching those logs by default, so a rotated or
expired client secret otherwise means email quietly stops working with
nothing loud to notice it. `bootstrap.sh` closes that gap by calling the
same OAuth2 `client_credentials` token endpoint the daemon itself would use,
at bootstrap time.

While the client secret is resolved and in scope, the script also suspends
any shell tracing that was already active (`set -x`, or running the whole
script as `bash -x scripts/box/bootstrap.sh`) and restores it only after the
secret variable has been unset, so tracing output cannot print the secret.

What it checks: whether `KHIVE_EMAIL_OAUTH_CLIENT_ID`,
`KHIVE_EMAIL_OAUTH_CLIENT_SECRET`, and `KHIVE_EMAIL_OAUTH_TENANT_ID` (read
the same way the daemon reads them: a real process environment variable
wins, otherwise `~/.khive/.env`) resolve to a working client secret against
`https://login.microsoftonline.com/<tenant>/oauth2/v2.0/token`.

When it runs: on every `bootstrap.sh` run, including re-runs, not on a bare
`systemctl --user restart`. Before secrets are placed, all three variables
are absent and the check prints a one-line skip note; this is the normal
first-run path, not a warning. See "Placing secrets" above: the documented
flow after placing secrets is re-running `bootstrap.sh`, not only
restarting the unit, specifically so this check runs against the new
values.

What PASS/FAIL mean:

- **PASS**: the token endpoint returned an access token. The script prints
  only the client id, masked (first 6 characters then an ellipsis); it
  never prints the client secret or the token.
- **Partial config** (only one or two of the three variables set): a loud
  warning, since this is the exact condition under which the channel
  silently disables itself.
- **Malformed id**: if `KHIVE_EMAIL_OAUTH_CLIENT_ID` or
  `KHIVE_EMAIL_OAUTH_TENANT_ID` is set but is not shaped like a Microsoft
  GUID (`8-4-4-4-12` hex digits), a warning names the affected variable
  (value masked, never raw) and the smoke is skipped non-fatally without
  calling the token endpoint.
- **FAIL**: a loud warning containing the `error` and `error_description`
  fields from Microsoft's response. That text is Microsoft's own
  diagnostic and is safe to print, but since it can echo a configured
  client id or tenant id verbatim (for example "Application with
  identifier 'xxx' was not found in the directory"), the script scrubs any
  occurrence of the configured client id or tenant id from it first,
  replacing each with its masked form. `AADSTS7000222` means the client
  secret has expired; `AADSTS7000215` means it is invalid. Either way, the
  fix is in the Entra portal, under App registrations, then the
  application, then Certificates & secrets: client secrets have bounded
  lifetimes, and rotating one means updating `~/.khive/.env` and re-running
  `bootstrap.sh`.

Like the `stats()` smoke test and the actor-identity check, this is
non-fatal: a box without email configured, or one with a transient network
blip during bootstrap, is not a bootstrap failure.

This smoke test does not cover Basic auth mode
(`KHIVE_EMAIL_PASSWORD`). There is no equivalent safe check for it: probing
would mean either attempting a live IMAP login from a bootstrap script or
inventing a lighter check that does not actually verify the password. For
Basic auth mode, the daemon's own startup log is the check: watch
`journalctl --user -u kkernel.service` after restarting for whether the
email channel reports itself enabled or disabled.

## Verifying boot persistence

A systemd `--user` unit stops when the owning user's last session ends,
unless lingering is enabled for that user:

```bash
loginctl enable-linger $USER
```

Run this once. Without it, the daemon dies when the SSH session that
installed it closes, and does not come back on reboot.

## Moving the database

To move an existing `khive.db` from the Mac (or another box) onto this one:

1. On the **source** machine, stop everything writing to the database first
   (the MCP client process, any `kkernel mcp --daemon`, `li play`/loop
   processes). SQLite's WAL mode tolerates concurrent readers but a mid-copy
   write from a live process can hand you a torn snapshot.

2. Still on the source machine, checkpoint the WAL back into the main
   database file so the copy is self-contained:

   ```bash
   sqlite3 ~/.khive/khive.db 'PRAGMA wal_checkpoint(TRUNCATE);'
   ```

3. Copy `khive.db` **and its sidecar files**. `~/.khive/khive.db` may have
   `khive.db-wal`, `khive.db-shm`, and `khive.db-journal` alongside it
   depending on state at copy time, so take the whole `~/.khive/` directory
   rather than a single file so nothing is left behind:

   ```bash
   scp -r source-host:~/.khive/ ~/.khive-incoming
   # then merge the DB (and only the DB) into the box's ~/.khive/, with
   # kkernel stopped on the box:
   systemctl --user stop kkernel.service
   cp ~/.khive-incoming/khive.db* ~/.khive/
   systemctl --user start kkernel.service
   ```

4. Do not run `bootstrap.sh` and a manual DB copy concurrently against the
   same `~/.khive/khive.db`: stop the service, copy, then start the service.

This procedure moves data, it does not merge two independently-written
databases. If both the Mac and the box have accumulated writes since the
last sync, this is a one-way overwrite; treat the box's prior `khive.db` (if
any) as disposable before doing this, or back it up first.

## Timezone (schedule pack)

Fresh Linux VPS images commonly default to UTC. The `schedule` pack's
`remind`/`schedule` verbs take an ISO datetime in the `at` field; when that
datetime carries no explicit UTC offset, the box's system timezone
determines how it resolves. Set the box to the timezone the fleet actually
reasons in to avoid a reminder landing hours off from what an operator
expected:

```bash
sudo timedatectl set-timezone America/New_York
```

Where possible, prefer ISO datetimes with an explicit offset (e.g.
`2026-07-15T09:00:00-04:00`) when scheduling from another machine, so the
result does not depend on which box's local time zone is in effect.

## Embedding first-warm smoke test

`bootstrap.sh` runs `kkernel exec 'stats()'` as its own smoke test, which
only confirms the daemon answers requests and does not exercise the
embedding path. Once secrets are in place, run a query that forces an
embedding call:

```bash
~/.cargo/bin/kkernel exec 'knowledge.search(query="off studio box bootstrap smoke test", limit=3)'
```

The embedding backend (`lattice-embed`) may need to acquire model state on
first use. This kit does not assert exactly how that acquisition works; if
this first call is unexpectedly slow or fails, check
`journalctl --user -u kkernel.service -n 100 --no-pager` for network activity
or errors before assuming the box is misconfigured, and confirm the box has
outbound HTTPS egress.

## Email round-trip smoke test

The outbound path is a poll loop (`channel_outbox_loop` in
`crates/khive-mcp/src/serve.rs`): every 5 seconds it lists notes with
`kind="message"`, `direction="outbound"`, `delivered=false` in the
configured ingest namespace, and sends any whose `to_actor` property starts
with `email:`. To smoke-test sending, create such a note directly through
the substrate:

```bash
~/.cargo/bin/kkernel exec 'create(kind="message", content="box bootstrap smoke test", properties={"direction":"outbound","to_actor":"email:<your-address>","subject":"khive box smoke test"})'
```

Then watch the logs for the outcome:

```bash
journalctl --user -u kkernel.service -f
```

A successful send logs `outbox loop: delivered` with the note id, recipient,
and generated Message-ID, and stamps a `delivered_at` property on the note
(the delivery loop is at-least-once: a crash between the SMTP send
succeeding and that stamp being written causes a retry with the same
Message-ID on the next pass, which most receiving mail servers collapse as a
duplicate). If `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS` is set, the recipient
address must be on that list or the send is skipped with a warning log
instead of an error.

Inbound polling is a separate loop (`channel_poll_loop`). Send a message to
the configured mailbox from another account and look for a matching inbound
note via `kkernel exec 'list(kind="message", direction="inbound", limit=5)'`.

## Monitor dependency on sqlite3

The fleet's standing inbox-wakeup pattern (a background `Monitor` polling
`MAX(created_at)` on inbound mail) queries `~/.khive/khive.db` directly with
the `sqlite3` CLI rather than through the MCP surface, so that a wake check
is a single lightweight read with no daemon round-trip. `bootstrap.sh`
installs `sqlite3` via apt for exactly this reason, in addition to its use
in the WAL-checkpoint step above. If you strip packages from this kit,
`sqlite3` is not optional for any lambda that arms that Monitor pattern on
this box.

## Mac-side access

To reach this box's daemon socket directly from the Mac (rather than
running a second `kkernel` there), forward it over SSH. OpenSSH supports
local-socket-to-remote-socket forwarding directly:

```bash
ssh -N -L /tmp/khived.sock:/home/<remote-user>/.khive/khived.sock <remote-user>@<box-host>
```

Then, in the Mac shell that should talk to the box, point the client at the
forwarded socket instead of the local one:

```bash
KHIVE_SOCKET=/tmp/khived.sock kkernel exec 'stats()'
```

`KHIVE_SOCKET` overrides the default `~/.khive/khived.sock` path on the
client side; it is meant for exactly this kind of operational override, not
for normal use. Close the `ssh -N -L` session when done; it holds no shell,
only the forwarded socket.
