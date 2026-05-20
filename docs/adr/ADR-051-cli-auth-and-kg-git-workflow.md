# ADR-051: CLI Authentication and KG Git Workflow Commands

**Status**: proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 defines the `.khive/kg/` file format (sorted NDJSON + `schema.yaml`) and establishes git as
the versioning layer for knowledge graphs. It specifies four CLI commands — `init`, `export`,
`import`, `validate` — that handle the SQLite-to-file round trip. What ADR-048 deliberately defers
is the **git workflow layer**: the commands that wrap git operations with KG-aware semantics, and
the **authentication layer** that connects the CLI to khive.ai's cloud services.

Without this layer, users must manually run `git add .khive/kg/ && git commit` after every export,
call `git push` separately, remember to import after pulling, and have no way to notify khive.ai
that a push happened. This is workable for developers who already know git, but it defeats the goal
of making khive.ai "GitHub for knowledge graphs" — a surface that is recognizable and natural to
researchers who have never opened a terminal.

The reference model is the `git` + `gh` pairing. `git` provides the VCS primitives; `gh` provides
the cloud-connected workflow (auth, PR creation, repo management). For khive:

- `khive kg commit/push/pull/status/branch/log` — VCS workflow primitives wrapping git
- `khive auth login/status/logout` — cloud authentication wrapping OAuth/API-key flows

ADR-044 specified a Deno HTTP API layer. ADR-049 specified the frontend workspace. This ADR
specifies the CLI layer that bridges the git-native KG workflow (ADR-048) with the khive.ai cloud
(ADR-027 API surface).

### What changes and what does not

- ADR-048 (`export`, `import`, `validate`, `diff`, `update`): unchanged. This ADR adds commands
  that call these; it does not modify them.
- ADR-027 (HTTP API layer): extended with one new sync endpoint (`POST /v1/projects/:ns/sync`).
  All other API surface is unchanged.
- ADR-044 (HTTP API layer): unaffected. The Deno gateway is unchanged.
- ADR-003 (four-layer architecture): the CLI is a new binary in the Deno layer. No new layers.

## Decision

### 1. CLI authentication (`khive auth`)

#### Commands

```
khive auth login              # browser OAuth → stores token
khive auth login --token PAT  # personal access token / API key
khive auth status             # show current authentication state
khive auth logout             # clear stored credentials
```

#### Browser OAuth flow

The browser flow is the primary authentication path. It mirrors `gh auth login`:

1. The CLI generates a random local port in the range 9000–9999 and starts a minimal HTTP server
   that listens for exactly one request on that port.
2. The CLI opens `https://khive.ai/auth/cli?port=<port>` in the system browser (via
   `Deno.run(["open", url])` on macOS, `xdg-open` on Linux, `start` on Windows).
3. The user authenticates via Google or GitHub on khive.ai.
4. khive.ai redirects to `http://localhost:<port>/callback?token=<access_token>&refresh=<refresh_token>`.
5. The CLI receives the tokens, writes `~/.khive/auth.json`, stops the local server, and prints a
   confirmation.

If the browser does not open within 30 seconds (headless environment, SSH session), the CLI prints
the full URL for manual opening and waits up to 5 minutes for the callback.

#### Personal access token flow

```
khive auth login --token <token>
```

Writes `~/.khive/auth.json` with the token as the `access_token` value. No `refresh_token` is
set; the token is treated as long-lived (no expiry check). This is the primary path for CI
environments and automation.

#### Token storage

```
~/.khive/auth.json
```

```json
{
    "api_url": "https://api.khive.ai",
    "access_token": "eyJ...",
    "refresh_token": "...",
    "expires_at": "2026-05-20T15:00:00Z",
    "user": {
        "namespace": "ocean",
        "email": "ocean@example.com"
    }
}
```

File permissions are set to `0600` (owner read/write only) on creation and verified on every read.
If permissions are wider than `0600`, the CLI prints a warning and refuses to use the file.

The `api_url` field allows pointing the CLI at a self-hosted khive.ai instance or a staging
environment without recompilation. Default: `https://api.khive.ai`.

#### Token refresh

Before any API call that requires authentication, the CLI checks `expires_at`. If the current time
is within 60 seconds of expiry or past expiry, and a `refresh_token` is present, the CLI:

1. Calls `POST <api_url>/v1/auth/refresh` with `{"refresh_token": "..."}`.
2. Receives `{"access_token": "...", "expires_at": "..."}`.
3. Writes the new values to `auth.json` (preserving all other fields).
4. Proceeds with the original API call.

If refresh fails (network error, expired refresh token), the CLI prints an authentication error and
instructs the user to run `khive auth login` again.

#### `khive auth status` output

```
Authenticated to khive.ai
  API URL:   https://api.khive.ai
  User:      ocean (ocean@example.com)
  Token:     eyJ... (expires in 6 days)
  Namespace: ocean
```

Or, if not authenticated:

```
Not authenticated. Run 'khive auth login' to sign in.
```

### 2. Namespace detection

KG workflow commands need to resolve the current project's namespace. The resolution order is:

1. **Explicit flag**: `--namespace <ns>` on any `khive kg *` command.
2. **`.khive/settings.json`**: `actor.name` field, as specified in ADR-048 §D3.
3. **Git remote URL**: `git remote get-url origin` → extract the repository name
   (e.g., `git@github.com:ocean/khive.git` → `khive`).
4. **Directory name**: `basename $(git rev-parse --show-toplevel)`.

The resolved namespace is validated against `^[a-z0-9][a-z0-9_-]{0,62}[a-z0-9]$` (ADR-048 §D3).
If no namespace can be resolved, the command errors with instructions to set `actor.name` in
`.khive/settings.json`.

### 3. `khive kg status`

Summarizes the state of `.khive/kg/` against the last git commit. No git writes occur.

```
khive kg status
```

**Output format:**

```
KG Status (namespace: khive)
  Schema: valid (6 kinds, 13 relations, 2 remotes)
  Entities: 472 (12 modified since last commit)
  Edges: 1,111 (3 new since last commit)

  Modified files:
    M .khive/kg/entities.ndjson (12 entities changed)
    M .khive/kg/edges.ndjson (3 edges added)

  Validation: pass
```

The counts "modified since last commit" and "new since last commit" come from parsing the output of
`git diff HEAD -- .khive/kg/entities.ndjson` and `git diff HEAD -- .khive/kg/edges.ndjson` — the
CLI counts `+` and `-` lines per entity/edge UUID, not raw line diffs.

If `.khive/kg/` does not exist: prints `KG not initialized. Run 'khive kg init' to start.`

If validation fails, the Validation line reads `fail — N errors` and lists the first 5 errors.

### 4. `khive kg commit`

Exports the live SQLite state to NDJSON, validates it, and creates a git commit.

```
khive kg commit -m "message"
khive kg commit              # prompts for message if -m is omitted
```

**Execution sequence:**

1. Run `khive kg export` (DB → `.khive/kg/entities.ndjson` + `edges.ndjson`).
2. Run `khive kg validate` (check consistency against `schema.yaml`).
3. If validation fails: abort with the validation errors printed. No git operations are performed.
4. `git add .khive/kg/entities.ndjson .khive/kg/edges.ndjson .khive/kg/schema.yaml`
5. `git commit -m "<message>"`
6. Print: `[<branch> <short-sha>] <message>` + entity/edge counts.

**Counts in output:**

```
[main a1b2c3d] add LoRA and QLoRA concepts
  472 entities, 1,111 edges (12 changed, 3 added)
```

The counts come from the same `git diff` analysis as `khive kg status`.

If there are no changes to `.khive/kg/` since the last commit, the command prints
`Nothing to commit (KG is clean)` and exits with 0.

### 5. `khive kg push`

Pushes the current branch to the git remote and optionally notifies khive.ai.

```
khive kg push
```

**Execution sequence:**

1. `git push origin <current-branch>` (standard git push; respects upstream tracking).
2. Resolve the current commit SHA: `git rev-parse HEAD`.
3. If authenticated to khive.ai:
   ```
   POST <api_url>/v1/projects/<namespace>/sync
   {
     "ref": "<branch-name>",
     "commit": "<full-sha>",
     "repo": "<git-remote-url>"
   }
   ```
4. Print push result. If sync succeeds: `Synced to khive.ai (namespace: <ns>)`.
   If sync fails: print a warning, but do not fail the command — the git push already succeeded.

The sync call is advisory: khive.ai uses it to trigger an import of the pushed NDJSON files into
the cloud-hosted KG. If the call fails (network, not authenticated, or cloud error), the local git
push is still complete and the user's data is safe. The next `khive kg push` or a manual
`khive kg sync` can retry.

### 6. `khive kg pull`

Fetches from the git remote, fast-forwards the working branch, and imports any KG changes into the
local SQLite database.

```
khive kg pull
```

**Execution sequence:**

1. `git pull origin <current-branch>` (standard git pull; fast-forward only by default).
2. Check whether `.khive/kg/` files changed in the pull:
   `git diff HEAD@{1} HEAD -- .khive/kg/` (non-empty output → files changed).
3. If `.khive/kg/` files changed:
   Run `khive kg import --on-conflict update`.
4. Print a summary of what changed:
   ```
   Pulled main from origin (a1b2c3d → f9e8d7c)
   KG updated: +3 entities, +7 edges, 0 conflicts
   ```

If `git pull` produces a merge conflict, the command aborts after the pull step with the standard
git merge-conflict message. The user must resolve the conflict, run `khive kg validate`, and
re-commit before the import step can proceed. No partial import is performed.

If `.khive/kg/` was not modified by the pull, no import is run.

### 7. `khive kg branch`

Creates or lists KG-aware git branches.

```
khive kg branch <name>        # create a new branch
khive kg branch               # list local branches (highlights current)
khive kg branch -d <name>     # delete a branch
```

This is a thin wrapper around `git branch`. The only KG-specific behavior is:

- On branch creation: run `khive kg status` and print a summary so the developer knows what state
  they are branching from.
- On branch listing: annotate each branch with its last KG commit message if the last commit
  touched `.khive/kg/`.

### 8. `khive kg log`

Shows git log filtered to KG-relevant commits.

```
khive kg log
khive kg log --limit 20
```

This is equivalent to `git log --oneline -- .khive/kg/` with KG-aware annotation:

```
f9e8d7c add LoRA and QLoRA concepts (+2 entities, +3 edges)
a1b2c3d initial KG import (472 entities, 1,111 edges)
```

The entity/edge counts per commit are parsed from the NDJSON line-count delta in the diff for that
commit. If the counts cannot be parsed (e.g., for a commit that modified `schema.yaml` only), the
annotation is omitted.

### 9. Cloud sync endpoint (new API surface)

One new endpoint is added to the khive.ai HTTP API:

```
POST /v1/projects/:namespace/sync
Authorization: Bearer <access_token>

{
  "ref": "main",
  "commit": "f9e8d7c6b5a4321098765432109876543210fedc",
  "repo": "https://github.com/ocean/khive"
}

Response 202 Accepted
{
  "job_id": "sync-a1b2c3d4",
  "status": "queued"
}
```

The sync job pulls the NDJSON files from the git repository at the specified commit and imports
them into the cloud namespace. The job runs asynchronously; completion is visible in the khive.ai
dashboard.

Job status polling (optional, phase C4):

```
GET /v1/projects/:namespace/sync/:job_id
Response 200
{ "status": "complete|running|failed", "imported": N, "errors": [] }
```

### 10. Phasing

| Phase | What | Target version |
|-------|------|----------------|
| C1 | `khive auth login/status/logout` + `~/.khive/auth.json` with `0600` permissions + token refresh | v0.3 |
| C2 | `khive kg status` + `khive kg commit` | v0.3 |
| C3 | `khive kg push` (git push only, no cloud sync) + `khive kg pull` (git pull + import) | v0.4 |
| C4 | Cloud sync on push (`POST /v1/projects/:ns/sync`) + job status polling | v0.5 |
| C5 | `khive kg branch` + `khive kg log` | v0.5 |

Phases C1 and C2 are independently shippable and cover the primary daily workflow: authenticate
once, then `khive kg commit` instead of the manual export-validate-add-commit sequence.

## Rationale

### Why `khive auth` mirrors `gh auth` rather than inventing a new model

`gh auth login` is the reference implementation that millions of developers have used. The browser
OAuth flow (CLI starts local server → opens browser → receives callback) is well-understood,
security-reviewed, and does not require the user to copy-paste tokens from a web UI. Implementing
the same UX means developers can transfer their `gh` mental model directly to `khive`.

The `--token` flag provides an escape hatch for CI and environments without a browser. This matches
`gh auth login --with-token`, and covers the same use cases (GitHub Actions, Docker containers,
automated pipelines).

### Why `~/.khive/auth.json` rather than system keychain

The system keychain (macOS Keychain, GNOME Keyring, Windows Credential Manager) provides stronger
isolation but introduces platform-specific code paths, failure modes, and dependencies. `gh` uses
a custom credential helper abstraction for the same reason: the keychain is ideal but not always
present (headless Linux, Docker containers, minimal VMs).

A `0600` file at a predictable path is auditable, portable, and deletable with `rm`. Developers
who want keychain integration can configure credential helpers at the OS level. The `api_url` field
in `auth.json` also makes the file useful as a configuration file (not just a secret store), which
keychain entries are not designed for.

### Why `khive kg commit` runs export and validate before `git commit`

The invariant this enforces: git never contains a NDJSON file that is out of sync with the local
SQLite database, and never contains a NDJSON file that fails schema validation. Both conditions
would make `khive kg import` on another machine produce an inconsistent database.

The alternative — let the user run `khive kg export` and `git commit` separately — is correct but
fragile. A developer who forgets to re-export after a late entity edit commits stale NDJSON. A
developer who forgets to validate before committing may push a graph that fails CI. `khive kg
commit` makes the right path the easy path.

### Why `khive kg push` does not fail if the cloud sync call fails

The git push is the durable operation. If the push to the remote succeeds, the user's data is safe
and replicated to GitHub. The cloud sync is an advisory notification that allows khive.ai to
proactively import the new state into its hosted KG. If the sync fails (transient network error,
khive.ai downtime), the user can re-trigger it later or let khive.ai pick it up via a webhook on
the next push event.

Treating the sync as a hard dependency would make `khive kg push` fail in environments without
internet access to khive.ai (air-gapped labs, offline development), which is contrary to the
OSS-first philosophy: the CLI should work fully locally without a cloud account.

### Why `khive kg pull` runs `import --on-conflict update`

After a pull, the NDJSON files represent the definitive agreed state of the KG (the state that
other contributors committed and pushed). Local edits that conflict with the pulled state lose: the
pull wins. This matches git's semantics — after a pull, the working tree reflects the merged
history.

`--on-conflict error` (the default) would abort the import if any UUID already exists in the local
database, which would happen on every pull for any entity that was present before the pull. That
would make `khive kg pull` useless in practice.

`--on-conflict skip` would silently ignore incoming changes, which would leave the local database
stale.

`--on-conflict update` overwrites local records with the pulled state, which is the correct
semantics for a pull: the pulled state supersedes whatever was local.

### Why namespace detection falls back to git remote URL

The namespace identifies which khive.ai project the local repo maps to. In the common case, the
namespace matches the git repository name, because repositories and projects have a 1:1 mapping on
khive.ai. The fallback chain allows `khive kg push` to work correctly in a freshly cloned repo
where `.khive/settings.json` has not yet been created, without requiring the developer to manually
configure the namespace.

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
|---|---|---|---|
| Device-flow OAuth (no local HTTP server) | Works in headless environments without a port | Requires khive.ai to implement the device authorization endpoint; longer user flow (poll loop) | Browser flow is simpler for the common case; `--token` covers the headless case |
| Store tokens in system keychain | Stronger isolation than file permissions | Platform-specific code; unavailable in Docker/CI; not portable for `api_url` config field | File with `0600` is portable and auditable; keychain can be layered on top |
| `khive kg commit` without automatic export | User controls export timing | Developer forgets to export after last edit; commits stale NDJSON | Correctness invariant: commit ≡ export + validate + git commit. Auto-export is the right default. |
| `khive kg push` fails on cloud sync error | Stricter guarantee that khive.ai is in sync | Breaks offline workflow; couples git operation to cloud availability | OSS-first: CLI must work without cloud. Sync is advisory. |
| `khive kg pull` with `--on-conflict error` | Explicit: surfaces every conflict | Requires manual resolution of every import after every pull; unworkable | Pulled state must win; `update` is the correct semantic |
| Separate `khive sync` command instead of integrating into push/pull | Explicit: user controls when to sync | Adds friction; developers forget to sync; status diverges | Integrating sync into push/pull matches the `git push` UX expectation |
| Reuse `gh` for auth | No additional auth code | Couples khive to GitHub specifically; excludes Google-auth users; khive.ai is its own identity provider | khive.ai is a platform, not a GitHub wrapper; must support its own OAuth |

## Consequences

### Positive

- `khive kg commit -m "message"` replaces the manual export → validate → git-add → git-commit
  sequence. Four commands become one with the same correctness guarantees.
- `khive kg push` and `khive kg pull` give developers a workflow that is structurally identical to
  `git push` / `git pull`, with the KG import automatically handled.
- `khive auth login` follows the `gh auth login` precedent that millions of developers recognize.
  Authentication is a one-time setup, not a per-command burden.
- The cloud sync call (`POST /v1/projects/:ns/sync`) enables khive.ai to maintain an up-to-date
  hosted mirror of every project's KG without requiring webhook configuration on the git host.
- All commands degrade gracefully without a cloud account: `khive kg commit/push/pull/status` work
  fully locally. Authentication adds the cloud sync step but is not required for any local
  workflow.

### Negative

- `khive kg commit` runs `khive kg export` unconditionally, even if the live database has not
  changed since the last export. This is a minor inefficiency for a no-op export (milliseconds
  for typical graph sizes), but it is simpler than maintaining a dirty flag across command
  invocations.
- The browser OAuth flow requires the CLI to open a browser, which fails silently in some
  environments (e.g., remote SSH sessions without X11 forwarding). The CLI must detect this case
  and print the URL for manual opening.
- Token storage in `~/.khive/auth.json` is not encrypted at rest. A developer with read access to
  the home directory can read the access token. This is the same exposure as `~/.config/gh/hosts.yml`
  (the `gh` credentials file). The `0600` permissions prevent access by other OS users but do not
  protect against the current user's own processes or physical access.
- `khive kg pull` uses `--on-conflict update`, which means local edits that were not yet exported
  and committed will be overwritten by the pulled state. This is correct git semantics but
  surprising to developers who modified the live database without running `khive kg commit` first.
  Mitigation: `khive kg status` shows uncommitted local changes; `khive kg pull` should print a
  warning if there are uncommitted changes before running the pull.

### Neutral

- The `khive auth` commands have no Rust component. They are Deno (TypeScript) commands in the same
  Deno CLI binary as the existing `khive kg` commands.
- The sync endpoint (`POST /v1/projects/:ns/sync`) is the only new server-side surface. All other
  HTTP API surface (ADR-044) is unchanged.
- `khive kg branch` and `khive kg log` (Phase C5) are thin wrappers around `git branch` and
  `git log`. They add no new state.

## Implementation

### Deno CLI structure

The existing Deno CLI binary gains two new command trees:

```
cli/
  commands/
    auth/
      login.ts        — browser OAuth + --token PAT path
      status.ts       — read and display auth.json
      logout.ts       — delete auth.json
    kg/
      status.ts       — git diff analysis + validation summary (existing: validate)
      commit.ts       — export → validate → git-add → git-commit
      push.ts         — git push + optional cloud sync
      pull.ts         — git pull + conditional import
      branch.ts       — git branch wrapper with KG annotation
      log.ts          — git log --oneline -- .khive/kg/ with count annotation
  lib/
    auth.ts           — read/write/refresh auth.json; HttpAuthClient
    git.ts            — thin wrappers: exec git commands, parse output
    namespace.ts      — namespace resolution chain
    sync.ts           — POST /v1/projects/:ns/sync; job polling
```

The `lib/auth.ts` module is shared between `auth/` and `kg/` commands (push uses `auth.ts` to get
the bearer token; status uses it to display the current user).

### Token security implementation

```typescript
// lib/auth.ts
const AUTH_PATH = Deno.env.get("HOME") + "/.khive/auth.json";
const REQUIRED_MODE = 0o600;

async function readAuth(): Promise<AuthFile | null> {
  try {
    const stat = await Deno.stat(AUTH_PATH);
    const mode = stat.mode & 0o777;
    if (mode !== REQUIRED_MODE) {
      console.warn(`Warning: ${AUTH_PATH} has permissions ${mode.toString(8)}, expected 600`);
      return null;
    }
    const text = await Deno.readTextFile(AUTH_PATH);
    return JSON.parse(text) as AuthFile;
  } catch {
    return null;
  }
}

async function writeAuth(auth: AuthFile): Promise<void> {
  await Deno.writeTextFile(AUTH_PATH, JSON.stringify(auth, null, 4));
  await Deno.chmod(AUTH_PATH, REQUIRED_MODE);
}
```

### Cloud sync implementation

```typescript
// lib/sync.ts
export async function notifySync(
  auth: AuthFile,
  namespace: string,
  ref: string,
  commit: string,
  repo: string,
): Promise<void> {
  const resp = await fetch(
    `${auth.api_url}/v1/projects/${namespace}/sync`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${auth.access_token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ ref, commit, repo }),
    },
  );
  if (!resp.ok) {
    // Non-fatal: print warning, do not throw
    console.warn(`Warning: cloud sync failed (${resp.status}). Run 'khive kg push' again to retry.`);
  }
}
```

### Phasing (implementation order)

Phase C1 (auth) is a prerequisite for Phase C4 (cloud sync) but not for C2 or C3. Phases C2 and
C3 can be implemented without any authentication code. The recommended implementation sequence is
C2 → C3 → C1 → C4 → C5, delivering the most-used local workflow first.

## References

- ADR-048: Git-Native KG Versioning — file format, `export`/`import`/`validate` commands, and
  namespace detection that this ADR builds on
- ADR-044: HTTP API Layer — Deno + Hono REST layer extended with the sync endpoint
- ADR-027: Single Tool MCP Surface — unchanged; KG workflow commands are CLI-only, not MCP tools
- ADR-003: Four-Layer Architecture — CLI is in the Deno layer; no new architecture layers introduced
- ADR-029: Authorization Gate — access token validation for the sync endpoint uses the Gate trait
- ADR-007: Namespace as Open String — namespace regex `^[a-z0-9][a-z0-9_-]{0,62}[a-z0-9]$`
- `gh auth login` — reference implementation for browser OAuth CLI flow
- OAuth 2.0 Authorization Code Flow: https://datatracker.ietf.org/doc/html/rfc6749#section-4.1
