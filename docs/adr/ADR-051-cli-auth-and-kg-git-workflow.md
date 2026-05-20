# ADR-051: CLI Authentication and KG Git Workflow Commands

**Status**: proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 defines the `.khive/kg/` file format (sorted NDJSON + `schema.yaml`) and establishes git as
the versioning layer for knowledge graphs. It specifies low-level CLI commands — `init`, `export`,
`import`, `validate` — that handle the SQLite-to-file round trip. What ADR-048 deliberately defers
is the **git workflow layer**: a small set of commands that automate the KG-specific parts of the
git workflow that users cannot do with raw git alone.

The key insight: most git operations (push, pull, branch, checkout, merge, log) work correctly on
NDJSON files without any KG awareness. The only KG-specific operations are:

1. **Commit**: exporting the DB to NDJSON and validating before committing (users forget this).
2. **Sync**: rebuilding the DB from NDJSON after any git operation that changes the files.
3. **Status**: showing entity/edge counts, not just file-level changes.
4. **Resolve**: entity-level conflict resolution when git merge produces NDJSON conflicts.

Everything else — push, pull, branch, checkout, merge, stash, log — is standard git. The CLI
should not wrap these; users already know git, and wrapping it adds maintenance cost, behavioral
surprises, and documentation burden for zero value.

**Platform model**: khive.ai is a KG intelligence layer over GitHub-hosted repos, not a git host.
KG repos live on GitHub (or any git host). khive.ai connects via a GitHub App: it receives push
webhooks, imports NDJSON into its cloud KG index, enriches PRs with entity-level diffs, and
provides cross-project entity resolution and global search. The OSS CLI (`khive kg`) works fully
without a khive.ai account — it is a pure git wrapper with KG-aware semantics. Authentication is
optional, for platform features only (global entity search, project dashboard).

### What changes and what does not

- ADR-048 (`init`, `export`, `import`, `validate`, `diff`, `update`): unchanged. This ADR adds
  `commit`, `sync`, and `resolve` that compose these; it does not modify them.
- ADR-044 (HTTP API layer): unchanged. khive.ai receives events via GitHub App webhooks, not CLI
  HTTP calls. No new API endpoints are added by this ADR.
- ADR-027 (Single Tool MCP Surface): unaffected. KG workflow commands are CLI-only, not MCP tools.
- ADR-003 (four-layer architecture): the CLI is in the Deno layer. No new layers.

## Decision

### 1. CLI authentication (`khive auth`) — optional

Authentication is **not required** for any `khive kg` command. All local operations work without
a khive.ai account. Auth enables optional platform features: global entity search from the CLI,
project dashboard queries, and verifying that the GitHub App is connected to a repo.

#### Commands

```
khive auth login              # GitHub OAuth → khive.ai API token
khive auth login --token PAT  # khive.ai API token for CI/automation
khive auth status             # show current authentication state + connected repos
khive auth logout             # clear stored credentials
```

#### GitHub OAuth flow

The browser flow is the primary authentication path. It mirrors `gh auth login`, using GitHub
as the identity provider:

1. The CLI generates a random local port in the range 9000–9999 and starts a minimal HTTP server
   that listens for exactly one request on that port.
2. The CLI opens `https://khive.ai/auth/cli?port=<port>` in the system browser (via
   `Deno.run(["open", url])` on macOS, `xdg-open` on Linux, `start` on Windows).
3. The user authenticates via GitHub OAuth on khive.ai. khive.ai uses GitHub as the sole identity
   provider (support for additional providers may be added later based on demand).
4. khive.ai redirects to `http://localhost:<port>/callback?token=<access_token>&refresh=<refresh_token>`.
5. The CLI receives the tokens, writes `~/.khive/auth.json`, stops the local server, and prints a
   confirmation.

If the browser does not open within 30 seconds (headless environment, SSH session), the CLI prints
the full URL for manual opening and waits up to 5 minutes for the callback.

The GitHub OAuth scope includes `read:user` (identity) and `repo` (so khive.ai can install its
GitHub App on the user's repos during the onboarding flow). The GitHub App installation is a
separate step on khive.ai: after login, `khive auth status` shows which repos are connected and
prompts to install the App if none are.

#### API token flow (CI)

```
khive auth login --token <token>
```

Writes `~/.khive/auth.json` with the token as the `access_token` value. No `refresh_token` is
set; the token is treated as long-lived (no expiry check). Tokens are generated on
`khive.ai/settings/tokens`. This is the primary path for CI environments and automation.

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
  API URL:      https://api.khive.ai
  User:         ocean (ocean@example.com)
  Token:        eyJ... (expires in 6 days)
  GitHub App:   installed on 3 repos (ocean/khive, ocean/lattice, ocean/styx)
```

Or, if not authenticated:

```
Not authenticated. Run 'khive auth login' to sign in.
All 'khive kg' commands work without authentication.
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

The counts "modified since last commit" and "new since last commit" are computed by exporting the
current working DB to a temporary NDJSON snapshot and comparing it against the committed NDJSON
files using the diff algorithm defined in ADR-052. This catches changes made through
`khive create/update/delete` that have not been exported yet, and does not rely on the git working
tree being dirty. The comparison is a content diff on UUID-keyed records, not a raw line count.

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

The counts come from the same DB-vs-NDJSON comparison as `khive kg status` (see ADR-052 for the
diff algorithm).

If there are no changes to `.khive/kg/` since the last commit, the command prints
`Nothing to commit (KG is clean)` and exits with 0.

### 5. `khive kg sync`

Rebuilds `working.db` from the current NDJSON files. This is the KG-aware operation that must
run after any git command that changes `.khive/kg/` files (pull, checkout, merge, rebase, stash
pop, cherry-pick).

```
khive kg sync
khive kg sync --quiet        # suppress output (for git hooks)
```

**Execution sequence:**

1. Check whether `.khive/kg/entities.ndjson` or `.khive/kg/edges.ndjson` have changed relative to
   the current `working.db` state (same diff as ADR-052 §6, but in the opposite direction: files
   are the source of truth, DB is the target).
2. If no changes: print `DB is up to date` and exit.
3. Run the ADR-052 atomic rebuild: validate NDJSON → import into temp DB → atomic rename into
   `.state/working.db`.
4. Print a summary: `Synced: 472 entities, 1,111 edges`.

`khive kg sync` is idempotent: running it twice produces the same result. It is safe to run at any
time — if the DB already matches the NDJSON files, it is a no-op.

**Error handling**: If NDJSON validation fails (corrupt file, schema violation), `sync` prints the
errors and exits non-zero without modifying `working.db`. The user must fix the NDJSON files
(manually or via `khive kg resolve` for merge conflicts) before sync will succeed.

### 6. Git hooks — automatic sync

`khive kg init` installs git hooks that run `khive kg sync --quiet` automatically after operations
that may change NDJSON files:

```bash
# .git/hooks/post-checkout
#!/bin/sh
# Rebuild working.db after branch switch or file checkout
khive kg sync --quiet 2>/dev/null || true

# .git/hooks/post-merge
#!/bin/sh
# Rebuild working.db after pull (with merge) or explicit merge
khive kg sync --quiet 2>/dev/null || true

# .git/hooks/post-rewrite
#!/bin/sh
# Rebuild working.db after rebase or amend
khive kg sync --quiet 2>/dev/null || true
```

The hooks are installed in `.git/hooks/` (not committed to the repo). `khive kg init` writes them
only if the hook file does not already exist — it does not overwrite user-customized hooks. If a
hook already exists, `khive kg init` prints a message instructing the user to add
`khive kg sync --quiet` to their existing hook.

The `|| true` ensures that a sync failure (e.g., NDJSON has merge conflicts) does not block the
git operation. The user resolves conflicts, then runs `khive kg sync` explicitly.

**Why hooks instead of wrapping git**: Git hooks are the standard mechanism for extending git with
project-specific behavior. They work with all git interfaces (CLI, IDE, GUI clients), not just
`khive kg` commands. A developer using `git pull` in VS Code's Source Control panel gets automatic
DB sync without knowing about `khive kg`.

### 7. Phasing

| Phase | What                                                                | Target version |
| ----- | ------------------------------------------------------------------- | -------------- |
| C1    | `khive kg commit` + `khive kg sync` + `khive kg status` + git hooks | v0.3           |
| C2    | `khive auth login/status/logout` + `~/.khive/auth.json` (optional)  | v0.4           |
| C3    | `khive kg resolve` (entity-level conflict resolution, see ADR-053)  | v0.5           |

C1 is the complete local workflow: commit, sync, and status. This covers 100% of the solo-user
use case and 90% of the multi-user workflow (sync handles pull/checkout; only merge conflicts
need `resolve`). C2 adds optional platform features. C3 adds the KG-aware merge tool.

## Rationale

### Why `khive kg commit` but not `khive kg push/pull`

`khive kg commit` adds value that raw git cannot provide: it automates the export → validate →
stage → commit pipeline and enforces the invariant that committed NDJSON is always consistent with
the live DB. Forgetting to export before committing is the #1 user error in the manual workflow.

`khive kg push` would be `git push` with zero KG-specific logic. khive.ai receives push events
via GitHub App webhooks, not CLI HTTP calls — there is nothing for the CLI to do beyond what
`git push` already does. Wrapping it adds maintenance cost, behavioral surprises (does it push
all branches? does it set upstream?), and forces users to learn a khive-specific command for a
universal git operation.

`khive kg pull` would be `git pull` + `khive kg sync`. With git hooks installed, `git pull`
triggers `post-merge`, which runs `khive kg sync` automatically. The user gets the same result
with standard git.

The design principle: wrap git only where the wrapper adds KG-specific intelligence. Transport
(push/pull) and branching (branch/checkout/merge) are not KG-specific.

### Why git hooks instead of wrapper commands

Git hooks work with every git interface: CLI, VS Code, JetBrains, GitHub Desktop, `tig`,
`lazygit`. Wrapper commands only work when the user remembers to use them. A developer who runs
`git pull` in VS Code and forgets to run `khive kg sync` afterward has a stale DB — silent and
hard to diagnose. A hook makes sync automatic regardless of how the user invokes git.

The tradeoff: hooks are per-clone (not committed), so each new clone must run `khive kg init` to
install them. This is acceptable because `khive kg init` is already the required first step after
cloning a KG repo (to bootstrap `.state/working.db` from the committed NDJSON files).

### Why `khive auth` uses GitHub OAuth

khive.ai's value proposition is being a platform layer over GitHub repos. GitHub OAuth provides:

1. **Identity**: the user's GitHub username becomes their khive.ai namespace.
2. **Repo access**: the OAuth scope allows installing the khive.ai GitHub App on repos.
3. **Zero new accounts**: researchers already have GitHub accounts. No new signup flow.

The `--token` flag covers CI and headless environments. Additional OAuth providers (GitLab,
Bitbucket) can be added later based on demand — the auth.json format is provider-agnostic.

### Why `~/.khive/auth.json` rather than system keychain

The system keychain (macOS Keychain, GNOME Keyring, Windows Credential Manager) provides stronger
isolation but introduces platform-specific code paths, failure modes, and dependencies. `gh` uses
a custom credential helper abstraction for the same reason: the keychain is ideal but not always
present (headless Linux, Docker containers, minimal VMs).

A `0600` file at a predictable path is auditable, portable, and deletable with `rm`. Developers
who want keychain integration can configure credential helpers at the OS level. The `api_url` field
in `auth.json` also makes the file useful as a configuration file (not just a secret store), which
keychain entries are not designed for.

### Why namespace detection falls back to git remote URL

The namespace identifies which khive.ai project the local repo maps to. In the common case, the
namespace matches the git repository name, because repositories and projects have a 1:1 mapping on
khive.ai. The fallback chain allows `khive kg` commands to work correctly in a freshly cloned repo
where `.khive/settings.json` has not yet been created, without requiring the developer to manually
configure the namespace.

## Alternatives Considered

| Alternative                                                             | Pros                                          | Cons                                                                                                                                      | Why rejected                                                                                                                   |
| ----------------------------------------------------------------------- | --------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| Full git wrapper (`khive kg push/pull/branch/checkout/merge/stash/log`) | Familiar "all-in-one" surface                 | Massive maintenance cost; wrapping git introduces behavioral surprises; users already know git; git hooks achieve the same automatic sync | The wrapper adds no KG-specific value for transport/branching; git hooks + `sync` is simpler and works with all git interfaces |
| Device-flow OAuth (no local HTTP server)                                | Works in headless environments without a port | Requires khive.ai to implement the device authorization endpoint; longer user flow (poll loop)                                            | Browser flow is simpler for the common case; `--token` covers the headless case                                                |
| Store tokens in system keychain                                         | Stronger isolation than file permissions      | Platform-specific code; unavailable in Docker/CI; not portable for `api_url` config field                                                 | File with `0600` is portable and auditable; keychain can be layered on top                                                     |
| `khive kg commit` without automatic export                              | User controls export timing                   | Developer forgets to export after last edit; commits stale NDJSON                                                                         | Correctness invariant: commit ≡ export + validate + git commit. Auto-export is the right default.                              |
| Advisory sync POST on push                                              | khive.ai updates immediately without webhook  | Couples CLI to khive.ai availability; breaks offline workflow; redundant when webhooks exist                                              | GitHub App webhooks handle push notification without any CLI-side code                                                         |
| Pre-commit hook instead of `khive kg commit`                            | Transparent: `git commit` auto-exports        | Complex hook logic; silent failures confuse users; `git commit --no-verify` skips it                                                      | Explicit `khive kg commit` is more predictable and its failures are visible                                                    |

## Consequences

### Positive

- `khive kg commit -m "message"` replaces the manual export → validate → git-add → git-commit
  sequence. Four commands become one with the same correctness guarantees.
- `khive kg sync` + git hooks give automatic DB rebuild after any git operation, regardless of
  which git interface the user prefers (CLI, IDE, GUI).
- The CLI surface is small: `init`, `commit`, `sync`, `status` are the only KG-specific commands.
  Users use standard git for everything else. No new mental model for transport or branching.
- `khive auth login` follows the `gh auth login` precedent, but is entirely optional. The local
  workflow is complete without a khive.ai account.
- khive.ai receives push events via GitHub App webhooks. No CLI-side cloud sync code is needed,
  eliminating a failure mode and a network dependency from the CLI.

### Negative

- `khive kg commit` runs `khive kg export` unconditionally, even if the live database has not
  changed since the last export. This is a minor inefficiency for a no-op export (milliseconds
  for typical graph sizes), but it is simpler than maintaining a dirty flag across command
  invocations.
- Git hooks are per-clone, not committed to the repo. Each new clone must run `khive kg init`
  to install hooks and bootstrap `working.db`. This is the expected onboarding path, but
  developers who forget `khive kg init` will have a stale or missing DB.
- The browser OAuth flow requires the CLI to open a browser, which fails silently in some
  environments (e.g., remote SSH sessions without X11 forwarding). The CLI must detect this case
  and print the URL for manual opening.
- Token storage in `~/.khive/auth.json` is not encrypted at rest. A developer with read access to
  the home directory can read the access token. This is the same exposure as `~/.config/gh/hosts.yml`
  (the `gh` credentials file). The `0600` permissions prevent access by other OS users but do not
  protect against the current user's own processes or physical access.
- Requires a GitHub account for khive.ai platform features (authentication, GitHub App
  installation). Users on GitLab or other hosts can use the local CLI fully but cannot connect
  to khive.ai until additional OAuth providers are supported.

### Neutral

- The `khive auth` commands have no Rust component. They are Deno (TypeScript) commands in the same
  Deno CLI binary as the existing `khive kg` commands.
- Standard git commands (`push`, `pull`, `branch`, `checkout`, `merge`, `log`) are not wrapped.
  Users use git directly. This is a deliberate design choice, not a missing feature.

## Implementation

### Deno CLI structure

The existing Deno CLI binary gains two new command trees:

```
cli/
  commands/
    auth/
      login.ts        — GitHub OAuth + --token PAT path
      status.ts       — read and display auth.json + connected repos
      logout.ts       — delete auth.json
    kg/
      status.ts       — entity/edge diff analysis + validation summary
      commit.ts       — export → validate → git-add → git-commit
      sync.ts         — atomic DB rebuild from NDJSON (ADR-052 §5)
  lib/
    auth.ts           — read/write/refresh auth.json
    git.ts            — thin wrappers: exec git commands, parse output
    namespace.ts      — namespace resolution chain
    hooks.ts          — install/check git hooks
```

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

### Hook installation

```typescript
// lib/hooks.ts
const HOOKS = {
  "post-checkout": "khive kg sync --quiet 2>/dev/null || true",
  "post-merge": "khive kg sync --quiet 2>/dev/null || true",
  "post-rewrite": "khive kg sync --quiet 2>/dev/null || true",
};

async function installHooks(gitDir: string): Promise<void> {
  const hooksDir = `${gitDir}/hooks`;
  for (const [name, command] of Object.entries(HOOKS)) {
    const path = `${hooksDir}/${name}`;
    try {
      await Deno.stat(path);
      console.log(`Hook ${name} already exists — add 'khive kg sync --quiet' manually.`);
    } catch {
      await Deno.writeTextFile(path, `#!/bin/sh\n${command}\n`);
      await Deno.chmod(path, 0o755);
      console.log(`Installed ${name} hook.`);
    }
  }
}
```

## References

- [ADR-048](ADR-048-git-native-kg-versioning.md) — NDJSON format, `init`/`export`/`import`/`validate`
- [ADR-052](ADR-052-kg-storage-model.md) — DB/file reconciliation, atomic rebuild, status diff
- [ADR-053](ADR-053-kg-branching-and-merge.md) — `khive kg resolve` (entity-level merge conflicts)
- [ADR-044](ADR-044-http-api-layer.md) — HTTP API layer (unchanged by this ADR)
