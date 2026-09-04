# Secret Gate — Credential Detection Algorithm

`secret_gate.rs` scans transcript/audit text for accidentally-embedded credentials (API keys,
tokens, passwords) before it is persisted or logged, and masks any it finds. This document is the
full algorithm spec: the allowlist layers that suppress false positives, the trigger-word
matching rules, and the per-function shape criteria used by the detection helpers. The in-source
module doc-comment carries only a concise summary and points here.

## Module-level detection algorithm

Allowlist (false-positive suppression) — **all of the following are prose-context exemptions,
not unconditional passes: a credential trigger word in the surrounding window dominates, with
exactly two narrow trigger-context exceptions (file paths and VCS revisions, defined below),
both of which run only after the reconstruction checks and only outside credential-value syntax
per the clause-label guard.** A UUID or a sha-prefixed content hash sitting directly beside
"api_key"/"secret"/"auth" is exactly as ambiguous as any other high-entropy candidate and falls
through to explicit detection instead of being silently allowed.

- Pure hex strings (sha256, git SHA) — passed when not near a trigger.
- UUID canonical form (`xxxxxxxx-xxxx-…`) — passed when not near a trigger.
- Base64/base64url content hashes with an explicit `sha<N>-` prefix (SRI hashes, npm lockfile
  integrity) — passed when not near a trigger and not preceded by a known-vendor prefix. Bare
  base64 tokens without the `sha<N>-` prefix are NOT passed.
- Strings that are entirely ASCII punctuation/whitespace (e.g. code) — not subject to the entropy
  heuristic, only the literal-prefix checks apply.
- Non-ASCII characters (CJK prose, accented text, emoji) act as token delimiters for the entropy
  heuristic: only maximal ASCII runs are entropy-checked. Real base64/hex/base64url credentials
  are ASCII, and `shannon_entropy` runs over UTF-8 bytes — multibyte codepoints inflate the
  byte-wise entropy and false-positive on natural-language non-Latin content. Treating non-ASCII
  as a delimiter (rather than skipping any whitespace token that merely contains it) keeps CJK
  prose unflagged while still catching an ASCII credential glued to CJK text/punctuation/fullwidth
  whitespace. The literal-prefix checks (Layer 1) treat any non-ASCII-alphanumeric char (CJK,
  accented text, emoji) as a token boundary, so a known-prefix secret is caught whether the
  adjacent non-ASCII sits before the prefix (`数据AKIA…`) or after it (`AKIA…数据`).
- Known provider prefixes (Layer 1) require the configured minimum token length. Fine-grained
  GitHub PATs require 93 total characters, OpenAI project keys require 88, and Anthropic keys
  require 108. A registered `sk-` vendor prefix remains governed by its specific threshold rather
  than falling through to the generic `sk-` detector. Prefix detectors also reject one narrow
  filename shape: after the prefix, a payload ending in `.py`, `.rs`, `.ts`, `.js`, `.sh`, `.md`,
  `.toml`, or `.json` is treated as a source filename only when its stem contains lowercase ASCII
  letters, contains at least one filename separator, and otherwise consists solely of lowercase
  letters plus `_`, `-`, `/`, and `.`. This admits ordinary names such as
  `vercel_deployment_monitor.py`. An uppercase letter, digit, or separator-free payload is
  independent value-shape evidence and preserves the prefix match even when the token ends in a
  source extension. Markdown/prose punctuation around the filename is ignored. This check only
  suppresses the matching prefix detector; every other detector layer still evaluates the token.
- Structured identifiers: a token is only considered for this exemption when it contains at least
  one of `/`, `-`, `_`, or `.` (the gate); it is then decomposed into maximal alphanumeric runs by
  splitting on _every_ non-alphanumeric character (not just the four gating separators — any other
  ASCII punctuation glued into the same whitespace token, e.g. a stray `:` or `,`, also acts as a
  run boundary). A token exempts when it decomposes into two or more such runs and every run is
  letters-then-digits or pure digits, at most 24 chars long, with a low case-transition density.
  This covers content like `fable-ops/ADR-DRAFT-adr079.md` or `local workspace artifact`, which is
  otherwise indistinguishable from a high-entropy secret once glued into one whitespace token.
  Random base64/base62 secrets do not decompose this way: their case and digit placement is
  effectively uniform rather than word-shaped, so a hyphenated or underscored secret still fails
  this check and remains subject to the entropy heuristic below.

  **This exemption applies ONLY outside an explicit credential trigger context.** Signals that
  measure Shannon entropy over an attacker-chosen run boundary (e.g. requiring a trailing file
  extension, or an average per-run letter entropy below a threshold) are not sound near a trigger
  word: an attacker who controls where a credential's separators fall can always choose run
  lengths whose entropy reads no higher than an ordinary short English path segment, since the
  measure only sees a character-frequency histogram, never word semantics. So near a trigger word,
  THIS exemption does not apply: a structured-identifier-shaped token falls through to the entropy
  heuristic like any other token, and only the separate, narrower file-path exemption below (which
  requires path shape, runs after every reconstruction check, and is refused in credential-value
  syntax) can still admit it. This is an accepted false-positive tradeoff on a small number of
  genuine paths/doc-slugs that happen to sit near a trigger word AND read above the entropy
  threshold on their own without qualifying for the narrow path exemption — see
  `accepted_false_positive_adr_draft_path_near_trigger` and its siblings for the specific repro
  cases this blocks, and the call site in `check_entropy_heuristic`. A path that qualifies for the
  narrow exemption can still block when a trigger word sits attributively ahead of a value
  delimiter ("see the docs for auth setup: <path>") — the clause walk cannot distinguish an
  attributive trigger from a label head without reopening labeled-value bypasses; pinned as
  `accepted_false_positive_docs_path_behind_attributive_trigger_and_delimiter`.

- File paths (trigger-context, narrow): a path-shaped token (two or more `/` segments; optional
  angle-bracket wrapping; optional `:line`/`:line-range` suffix) is exempted near a trigger word
  ONLY after the per-run entropy/hex-length checks, normalized-hex reconstruction, and
  multi-fragment bridge reconstruction have all run against it — a path-shaped anchor must not be
  able to skip a chain that reconstructs a blocked credential — AND only when the token is not in
  credential-value syntax (see the clause-label guard below).
- VCS revisions (trigger-context, narrow): a 40-hex value attached to an explicit VCS coordinate
  marker (`commit`, `revision`, `rev`, `sha` — immediately preceding word, or `marker:value` in
  one token) is treated as a public VCS coordinate near a trigger word, again only outside
  credential-value syntax. The exemption is a _flag over the hex-credential-shape checks only_,
  never an early skip of the whole check sequence. For the bare-marker form (`commit <hex>`) the
  exempt hex value is a plain alphanumeric token, so it still participates in fragment
  reconstruction anchored at neighboring tokens: a split credential hiding one fragment behind
  the marker is accumulated and blocked from the other fragments' anchors. That symmetric-anchor
  compensation is **guaranteed only for that topology**: the inline `marker:value` form is a
  colon-bearing token that neighboring bridge anchors reject (fragments must be
  alphanumeric-only), and a chain probing across a bare marker word terminates at the marker. A
  split credential whose fragments are reachable only through an inline-marker token or across a
  marker word is therefore a bounded-fragment residual (see the reconstruction bounds above), not
  a covered topology — unless a credential label is in clause range, in which case the clause
  guard below disables the exemption and the shape checks fire directly. The bare marker word
  itself (form `commit <hex>`) is skipped entirely — a fixed English marker word is not
  attacker-controlled credential material. Generic `hash`/`sha256` prose does not rescue a token.

Both narrow exemptions above are gated by a **clause-label guard** (`has_clause_credential_label`):
the exemption is refused when the candidate carries an inline credential shape
(`api_key=<value>`) or when a credential label is reachable by walking backwards through the
current clause. The walk steps over connector words that commonly sit between a label and its
value (`is`, `was`, `value`, articles, the VCS marker words themselves so a marker cannot
shield an earlier label, and prepositions/determiners/possessives — the glue of noun-compound
qualifiers), version fragments (`v1.2` splits into version-shaped identifiers), and long hex
fragments (a separator-split payload piece is value material, not a label word), up to a
bounded number of identifiers. Crossing a value delimiter (`:` or `=`, including one attached
to a VCS marker: `deploy sha: <hex>` is assignment syntax like any other) additionally lets the
walk step over CONTENT words outside those sets — "label with qualifiers: value" (`api key for
production deploy: <value>`, `api key for shared encrypted deploy: <value>`) names the value
regardless of how many qualifier nouns the label carries. Content words after a delimiter are
bounded only by the overall walk limit, the sentence boundary, and the past-participle stop; a
per-clause content-word cap was tried and removed, since any cap re-admits the labeled-value
bypass one natural qualifier past the cap. For the same reason, EXHAUSTING the walk limit after
crossing a delimiter fails CLOSED: the clause is assignment-shaped and its head was never
scanned, so it is treated as credential-labeled — clause length cannot launder a labeled value
into the exemptions (`api key for the new shared encrypted regional staging deploy: <value>`
blocks even though the trigger sits past the walk budget).

A regular `-ed` past-participle content word ends the walk: verb-phrase prose narrates an action
on the value rather than labeling it (`the auth scanner flagged this file: <path>`, `one extra
token was introduced by sha: <hex>` stay exempt). The walk stops at a sentence/paragraph boundary
(`;`, `!`, `?`, blank line; `.` only when not immediately followed by an alphanumeric character,
so a dotted version qualifier does not read as a sentence end). The participle stop is
position-sensitive: it applies only in verb position — the participle followed (in reading order)
by a glue word or the value itself (`flagged this file:`, `introduced by sha:`, `key updated:
<v>`). For a no-delimiter file-path candidate, however, adjacency to the value is not sufficient:
the reverse walk must already have processed a real value-side identifier and remain in connector
position. Thus `api key leaked <value>` blocks, while `the auth scanner flagged this file <path>`
retains the stop after processing `file` and `this`. Delimiter-bearing clauses and VCS coordinates
retain their existing direct-participle behavior. Followed by a content noun, a participle is an
ADJECTIVE inside a label qualifier (`shared deploy:`, `encrypted backup:`) and walks like any other
qualifier noun. Coordinating conjunctions (`and`, `or`) are transparent to this classification: in
`shared and encrypted staging deploy: <value>` the coordination as a whole is followed by a content
noun, so both participles read as adjectives and walk. A participle BEFORE the trigger word never
matters — the walk reaches the trigger first (`generated api key: <v>` blocks). The regular-suffix
proxy has an explicit lexical exception set: `hundred` is not treated as a participle merely because
its bytes end in `ed`. Irregular `found` intentionally is not a narrative stop, so it cannot shield
the direct label in `api key found <value>`.

Without a `:`/`=`, the two exemptions deliberately use different tiers. VCS coordinates retain
the strict connector-only walk; the first unknown content word ends it, preserving ordinary prose
such as `the key changes are in commit <hex>`. This leaves the accepted VCS residual `api key pour
commit <hex>`. File-path candidates may instead cross at most two content words, in addition to
the closed connector sets, so direct labels remain reachable: `api key found <slash-base64>`,
`auth scanner found <slash-base64>`, and `secret note <high-entropy path>` all block, as do the
corresponding path/slash-base64 combinations. The position-sensitive regular-participle stop applies
to this bounded bridge only after real value-side walk progress, so `the auth scanner flagged this
file <path>` stays exempt but `api key leaked <value>` blocks. The file-path tier has one narrower
regular-`-ing` narrative stop: it applies only when the reverse walk has just crossed the literal
value-side preposition `in`, preserving technical citations such as `api_key handling in <path>`.
Adjacency is not enough: `api key handling <value>` keeps walking to the direct trigger and blocks.
`see` has no special stop; `key: see <path>` likewise blocks, while a citation whose path precedes a
later topical trigger (`see <path> ... key`) stays exempt because the backward clause contains no
credential label.

A single-identifier lookback is deliberately NOT the contract: `api key value is commit <hex>` is
a labeled credential wearing a marker, and one connector word must not hide the label. A label on
the far side of a sentence boundary is prose context (the `near_trigger` window models that), not
this value's label. The other accepted participle residual remains: `api key updated: commit
<hex>` reads as changelog prose; without the VCS marker the raw hex still blocks under the
near-trigger rule. Ordering remains significant: `updated api key: <hex>` blocks because the walk
meets the trigger first.

Accepted false positives,
conservative direction: the walk has no grammar — ANY trigger word reachable inside the
pre-delimiter clause (absent a sentence boundary or verb-position participle) is treated as a
credential label, whether it is attributive (`see the docs for auth setup: <path>`, `secret
scanner archive notes: <path>`, `writing up the secret gate false positive repro: <path>`) or
a topical object (`results from testing auth against parser: <path>`); distinguishing these
from a label head would reopen the labeled-value bypasses. Likewise a delimiter-bearing clause
that exhausts the walk limit blocks regardless of whether a trigger was reached (exhaustion
fails closed).

Trigger-word matching only fires on genuine mentions, not substring collisions: trigger words
(`key`, `secret`, `password`, `passwd`, `credential`, `bearer`, `auth`, `apikey`) are matched at a
word boundary (`contains_bounded_word`), so `auth` does not fire inside `authorized` or
`authentication`, nor `key` inside `monkey`/`keyword`. The candidate token is excluded from its
own surrounding context. This prevents an internal path segment such as `cli-auth-and-kg` from
making the path self-trigger. Assignment-shaped candidates such as `auth=<value>` and
`api_key=<value>` are checked separately, including when whitespace splits the label from the
value, so the exclusion does not weaken credential-shaped writes.

A structured-identifier-shaped token sitting near a **genuinely standalone** trigger word (e.g.
`auth work saved at .../repo-audit.md`, where `auth` is an actual topical mention rather than a
substring collision) is an accepted false positive: no window-narrowing or exemption-widening
scheme survives the adversarial regression corpus without also reopening a real bypass, because
the caller (or an attacker) fully controls the prose between a trigger word and a payload:
narrowing `TRIGGER_WINDOW` or reinstating the structured-identifier exemption near "bare" trigger
mentions both fail the same known bypass strings that motivated closing them.

The word-boundary rule above treats underscore as a BOUNDARY for bare `TRIGGER_WORDS`
(`contains_bounded_word`): deliberately different from `has_standalone_token`'s rule for the word
`token`, which treats underscore as a continuation so `tokenizer`/`next_token`/`token_count` stay
exempt. Treating underscore as a boundary for the bare set is what lets common underscore-joined
credential-config compounds keep firing: `SECRET_KEY=...` (Django/Flask-style config),
`auth_token=...`, `session_secret_...`, `signing_key=...` all match on the `secret`/`key`/`auth`
half. This is implemented by parameterizing the boundary rule (`contains_word`'s
`underscore_is_word_char` argument) rather than sharing one rule between the two callers.

## find_prefix_token

Known provider-prefix matching remains context-free and requires both a token boundary and the
detector's configured minimum total length. Before returning a match, `find_prefix_token` applies
`is_filename_shaped_prefix_match` to the payload after the prefix. The helper recognizes only the
closed source-extension set and a stem made entirely from lowercase ASCII letters plus filename
punctuation (`_`, `-`, `/`, `.`), with at least one letter and at least one such separator. Outer
backticks, quotes, brackets, and sentence punctuation do not become payload evidence. Any digit,
uppercase byte, or separator-free payload rejects the filename shape, so a value-shaped provider
token still matches even if `.py` or another known extension is appended. Suppressing this one
prefix match is not an allow decision: the remaining known-shape and entropy detectors still scan
the token.

## value_candidates

Yields every candidate value that an assignment/wrapper-glued whitespace token could contain, so
shape allowlists that require an EXACT match (`is_uuid_canonical`, `is_base64_content_hash`) still
recognize the credential once it is glued to normal storage syntax: `key=value`, `(value)`,
`{"key":"value"}`, `key1=key2=value`, a trailing sentence period, or a label itself containing
`:`/`=` (`{"api:key":"value"}`). Used only to derive candidates for the near-trigger
UUID/content-hash checks in `check_entropy_heuristic` — it does NOT replace `token` for the
entropy, hex, or structured-identifier paths, none of which require an exact shape match.

Strips wrapper punctuation from both ends first, then yields the wrapper-stripped whole token,
plus the wrapper-stripped suffix after EVERY internal `=`/`:` occurrence (skipping empty
suffixes). No single separator position can be assumed correct: the true key/value or JSON-label
boundary might be the first separator (`secret=sha256-...`), but a base64/base64url value can
itself end in `=` padding — for a padded content hash that padding IS the last `=` in the token,
so a last-separator split would land on the padding boundary instead. A label can also itself
contain `:`/`=` (`{"api:key":"<uuid>"}`) or the assignment can be doubled
(`key=label=<uuid>`), so neither "first" nor "last" is a sound single choice. Emitting every
suffix and letting the caller test each one is the only choice that is sound in all these shapes:
the true value always appears as _some_ suffix, and a `=`/`:` that lands inside padding or a label
simply yields a non-matching suffix that the caller's shape check harmlessly rejects.

Byte-scan via `char_indices` over an already-short token (whitespace-delimited, so bounded by
realistic line length) — no allocation, since this runs in the hot scan path.

## contains_word

`underscore_is_word_char` selects which of two, deliberately different, boundary rules the caller
needs:

- `true` (used by `has_standalone_token` / `has_token_assignment` for `token`): underscore is a
  continuation of the same identifier, so `next_token`, `tokenizer`, and `token_count` do NOT
  match — a prior, deliberate decision that must not change.
- `false` (used by `contains_bounded_word` for the bare `TRIGGER_WORDS`): underscore IS a
  boundary, so `secret_key=`/`auth_token=`/`signing_key=` still match on the
  `secret`/`auth`/`key` half of the compound — these underscore-joined credential-config
  compounds (Django/Flask `SECRET_KEY`, OAuth `auth_token`, JWT `signing_key`) are exactly the
  shape a credential trigger must not lose. Only _letter_-joined collisions (`authorized`,
  `authentication`, `monkey`, `keyword`) are meant to stop matching.

CJK/accented prose always counts as a boundary in both modes (only ASCII alphanumerics — plus
underscore when `underscore_is_word_char` is `true` — are treated as word characters).

## Named redaction surfaces

ADR-115 Amendment 2 declares three permanent mask-only surfaces through the public
`RedactionSurface` enum and `redaction_surface_contract`:

- `GitIngest` stores masked commit/issue/pull-request entity and note fields.
- `SessionMirror` stores masked `session_messages.text`, `session_messages.raw`, `sessions.cwd`,
  `sessions.git_branch`, and `sessions.slug` projections — the latter covers every parsed
  title/slug field (ChatGPT export `title`, Claude Code `slug`, claude.ai export `name`/`summary`),
  not just the message body columns. `cwd`/`git_branch` are session-keyed, not message-keyed:
  `session_messages` carries no columns of its own for them.
- `McpDiagnostic` returns a bounded masked diagnostic and has no durable stored target. Backend
  error message masking and backend-id/key masking both go through `mask_bounded`
  (see [Bounded masking](#bounded-masking) below): the masker's own input is capped to
  `MASK_WINDOW_CHARS` (4,096) BEFORE masking runs, not the full, untruncated text — cost scales with
  the window, never with the caller's raw input length. A token straddling the window boundary,
  including every fragment of a bridged multi-fragment credential chained to it, is dropped rather
  than echoed unmasked; see [Bounded masking](#bounded-masking) for why. The kkernel coordinator's
  pre-MCP diagnostic logging (`bounded_backend_cause_for_log`, `bounded_backend_id_for_log` in
  `crates/kkernel/src/coordinator/dispatch.rs`) is also a named `McpDiagnostic` caller, applying the
  same bounded-window masking before the same backend cause/id text reaches a log record ahead of
  the MCP wire boundary.

Each call site uses `mask_for_redaction_surface`. Every contract has mode `PermanentMaskOnly`, no
stamp property, and no atomic exemption-success event. The wrapper has no manifest input and cannot
return an exemption outcome. The Git and session surfaces persist only their masked values; MCP
diagnostics are response data and are not durable records. Adding an admission mode is an ADR-level
contract change, not a caller-selectable sensitivity option. `redaction_surface_contract` sources its
`final_stored_target` strings from the `GIT_INGEST_STORED_TARGET` and `SESSION_MIRROR_STORED_TARGET`
constants, and the contract test compares against those same constants rather than a second copy of
the prose.

## Bounded masking

`mask_bounded(surface, text, window_chars, output_cap_chars)` is what every diagnostic-boundary
caller (MCP backend error/key masking, the kkernel coordinator's pre-MCP logging) actually calls,
not `mask_for_redaction_surface` directly. It caps the masker's own input to `window_chars` BEFORE
masking runs — `MASK_WINDOW_CHARS` (4,096) for every current caller — so scan cost is bounded by the
window regardless of the caller's raw input length, then caps the masked output to
`output_cap_chars` (`output_cap_chars` is clamped to `window_chars` in every build, not just under
`debug_assert!`, so a misconfigured call site cannot cap tighter than it windows).

A window cut mid-token would let a masker that never saw the token's terminating shape emit the
token's visible prefix unmasked, so any token straddling the boundary is dropped whole — back to
the last whitespace inside the window — rather than masked. The same drop extends to a chain of
`bridge_fragment_chain`-reconstructible fragments straddling the boundary, not just the one token
touching it: the gap between two fragments is deliberately unbounded in byte length (see
[Bridge fragment reconstruction](#bridge-fragment-reconstruction)), so no finite forward lookahead
past the window can guarantee seeing every fragment of a chain that starts before the cut. Instead
of scanning past the boundary, `mask_bounded` walks BACKWARD from it — over data already inside the
window, so this adds no lookahead and stays bounded by `window_chars` alone — dropping every further
bridge-fragment-shaped token chained to the one already removed, within the same
`MAX_BRIDGE_FRAGMENTS`/`MAX_BRIDGE_GLUE_TOKENS` budgets `bridge_fragment_chain` itself uses.

This walk runs on every truncated window, regardless of whether the window itself carries
trigger-word context. The entropy detector's own bridge reconstruction (see
[Bridge fragment reconstruction](#bridge-fragment-reconstruction)) admits a trigger word from either
side of a fragment chain, so a credential such as `<frag> <frag> <frag> is the api key for ...` is
reconstructed and masked by the unbounded masker even though its only trigger sits after the last
fragment. A window cut inside that fragment chain may never contain the trigger at all — it can sit
past the boundary, in text `mask_bounded` has already decided to discard — so gating the backward
walk on an in-window trigger check leaves exactly that case unprotected: no visible trigger, no
walk, and any whole fragments already read into the window leak. The walk cannot distinguish a
genuine chained fragment from an unrelated fragment-shaped word sitting at the tail of an
untriggered window either; the trade this makes is dropping that word too rather than risking a
leaked credential fragment. The cost is bounded: at most `MAX_BRIDGE_FRAGMENTS - 1` tokens of a tail
that `mask_bounded` has already decided to truncate.

## mask_secrets

A transcript line cannot be rejected wholesale, so each credential span is replaced in place
while the surrounding prose is preserved. Spans are discovered left to right against the ORIGINAL
text via `scan_from`: each scan advances a `from` cursor past the previous span but always
evaluates trigger context over the full input. This closes the entropy-context gap — a
high-entropy value whose only trigger word sits to the left of an earlier-redacted secret is
still detected, because the trigger window is never sliced away. The entropy detector tokenizes
the full input once per masking call, then uses the first token at or after the cursor on each
pass; the known-prefix detectors (real API keys: `sk-ant-`, `sk-proj-`, `AKIA`/`ASIA`, GitHub,
Stripe, …) remain context-free and scan the suffix. Masking limits cumulative suffix bytes
submitted to those repeated detector sweeps to 2 MiB; the first sweep is always allowed for
larger or multibyte callers. If dense credential-shaped input reaches that work budget with text
remaining, the last confirmed secret span is extended through the rest of the input. This
fail-closed tail redaction bounds repeated scan work without allowing an unscanned credential to
survive.

## trigger_words

Bare English words that can otherwise appear as a pure substring collision inside unrelated
identifiers or prose: `auth` inside `authorized`/`authentication`, `key` inside
`monkey`/`turkey`/`keyword`, `secret` inside `secretary`. Design decision (see the module doc): a
substring collision like this poisons the trigger window on prose that never mentions credentials
at all, which is a distinct failure mode from a genuine (if topical) mention of the word — see
issues #577 / #632. Matching these words at a word boundary removes the substring-collision false
positives while changing nothing about detection of a genuine standalone mention: `auth` as its
own word (`auth header`, `auth:`) still triggers exactly as before.

The bare substring `token` is NOT in this list because it fires on benign terms like `tokenizer`,
`token_count`, and `next_token`. Instead the dedicated boundary-aware helpers `has_standalone_token`
(standalone word) and `has_token_assignment` (`token=` / `token:` with word boundary before) are
used.

## is_base64_content_hash

Criteria:

- Token starts with `sha<digits>-` (e.g. `sha256-`, `sha384-`, `sha512-`).
- The body after the prefix matches a SHA-family length (43, 64, or 86–88 unpadded chars).
- Every byte in the body is a standard-base64 or URL-safe-base64 character.
- Does NOT start with a known vendor-token prefix (those are credentials regardless of alphabet).

Bare base64 tokens of those lengths WITHOUT the `sha<N>-` prefix are NOT allowlisted here — a
43-char base64url API token near the word "key" is indistinguishable from a sha256 hash body
without the prefix, so the explicit prefix is required to avoid false-negative credential
escapes.

## check_entropy_heuristic — per-token flagging sequence

For each token, in order:

1. **UUID / content-hash near trigger.** Both exact-shape checkers (`is_uuid_canonical`,
   `is_base64_content_hash`) require the WHOLE candidate to match, so `value_candidates` is used
   instead of the raw token to reach a credential glued to storage syntax (`api_key=<uuid>`,
   `(<uuid>)`, `{"api_key":"<uuid>"}`, a doubled assignment, a trailing sentence period, or a
   label itself containing `:`/`=`) — `strip_delimiters` only trims outer punctuation, not an
   internal separator from an assignment form. `value_candidates` is used only for this pair of
   checks; it does not replace `token` for entropy, hex, or structured-identifier checks, none of
   which require an exact shape match. This is a small bounded iteration over separator positions
   in one token, not an allocation-heavy scan. Off-trigger, a UUID or content hash is allowlisted
   outright.
2. **Pure hex off-trigger** is allowlisted (git SHA, checksum digests). Trigger-adjacent hex
   requires an explicit VCS coordinate marker (`commit`, `revision`, `rev`, `sha`) to earn the
   same exemption, and only when the surrounding clause carries no credential label (`api key
   value is commit <hex>` is a labeled credential wearing a marker, not a VCS citation). The
   exemption is a flag over the hex-credential-shape checks only, never an early skip of fragment
   reconstruction — an exempted anchor that skipped reconstruction would let a marker-adjacent
   fragment hide a split credential. The bare-marker-word form (`commit <hex>`) is skipped
   entirely via `continue`, since a fixed marker word is not attacker-controlled material and
   letting it anchor fragment reconstruction would re-accumulate its own legitimate neighboring
   revision; the hex value itself is a separate token checked on its own iteration.
3. **Hex credential shape near trigger.** The entropy heuristic cannot catch hex API keys (AWS
   secret access key, Stripe test keys): hex's alphabet maxes at log2(16) = 4.0 bits/char, always
   below `ENTROPY_THRESHOLD` (4.5). A credential-shaped hex token (32/40/64/128 chars,
   `HEX_CREDENTIAL_LENGTHS`) near a trigger word is flagged directly.
4. **Per-run hex/entropy re-check (issue #1044).** A genuine credential can dilute below the
   whole-token-average checks above when it shares a whitespace token with low-entropy filler
   segments (`vault/<payload>/rotate.md`). Decomposing the token on every non-alphanumeric
   separator (the same split `is_structured_identifier` uses) and re-running the hex-length and
   entropy checks against each run independently closes that gap. Only `MIN_ENTROPY_LEN`+ runs
   are considered — the #1040 measurement corpus confirmed no real path false positive contains a
   single run that long. This does not touch the `is_structured_identifier` exemption itself,
   which stays scoped to `!near_trigger`; a run this long clearing its own check is evidence
   independent of that exemption's word-shape rule.
5. **Normalized hex concatenation (issue #1062).** The per-run loop above still misses a
   credential hex payload split into multiple runs each individually below `MIN_ENTROPY_LEN`
   (e.g. two 20-char hex runs joined by `/`). Concatenating consecutive pure-hex runs (dropping
   separators) and re-checking the combined length against `HEX_CREDENTIAL_LENGTHS` closes this
   without widening the allowlist.
6. **Multi-fragment bridge (issue #1062, Unicode variant).** A non-ASCII separator (e.g. U+200B)
   is a tokenizer delimiter, so it splits the payload into separate tokens instead of leaving it
   inside one — the concatenation check above never sees the halves together. Bridging only one
   adjacent pair and bounding the gap by raw byte length is insufficient (repeating the delimiter
   defeats a byte-length bound, and a three-way split defeats single-pair bridging).
   `bridge_fragment_chain` fixes both by walking outward in both directions across a bounded
   chain of fragments (see [Bridge fragment reconstruction](#bridge-fragment-reconstruction)); a
   delimiter-only token between two gaps is transparent to the walk (absorbed as glue, see
   `is_delimiter_only_token`). Every fragment found while extending from the anchor must itself
   be bridge-fragment-shaped (alphanumeric, `MIN_BRIDGE_FRAGMENT_LEN`+) — this stops the walk at
   a short trigger/glue word (`key`, `api`, `for`) rather than dragging prose into the
   reconstruction. The anchor itself is included unconditionally, so a shape exemption able to
   admit a long non-fragment anchor must run after this reconstruction, not before.

   **Actual guarantee** (narrower than "every three-or-more-way split is reconstructed"):
   coverage extends to splits of up to `MAX_BRIDGE_FRAGMENTS` real fragments (each individually
   meeting `MIN_BRIDGE_FRAGMENT_LEN`), where a single gap between two fragments may be any byte
   length but spans at most `MAX_BRIDGE_GLUE_TOKENS` delimiter-only tokens per probe direction. A
   split into more fragments, into fragments individually below the length floor, or across more
   glue tokens in one gap, is an accepted residual limitation of the local-neighborhood bound —
   per ADR-096 / ADR-115 this gate is accidental-persistence hygiene on a single-principal
   same-uid host, not defense against a same-uid adversary hand-splitting a credential to evade
   it (that adversary can write the DB directly). This module does not itself establish that the
   host is same-uid; it inherits that from the daemon's peer-credential check
   (`khive-runtime/src/daemon.rs`, refusal of any peer uid other than its own before the first
   frame is read) — if that refusal is ever removed or weakened, this residual limitation stops
   being residual. See the `allows_seven_way_hex_split_beyond_fragment_cap_documented_limitation`
   and `allows_six_way_sub_floor_hex_split_documented_limitation` tests.

   The bridged chain is checked two ways: `contains_normalized_hex_credential` over fragments
   joined by a plain space (a non-alphanumeric separator, so it accumulates only genuinely
   adjacent hex runs the same way it does for one token's internal `/`-split runs), and
   separately the fragments concatenated WITHOUT a separator against the same whole-token entropy
   decision a single-token high-entropy candidate must clear — this catches a
   base64/base64url-shaped credential split by the same delimiter mechanism, which isn't pure hex
   so the hex-length check alone misses it. A vcs-exempt anchor skips reconstruction from itself
   (else the chain would re-accumulate the anchor's own legitimate 40-hex and re-flag every
   benign marker-adjacent revision); a genuinely split credential hiding one fragment behind a
   marker is still caught because every OTHER fragment anchors its own chain and accumulates the
   exempted fragment's hex into the total (`blocks_split_hex_credential_with_marker_adjacent_fragment`).
7. **File-path exemption** (`is_plausible_file_path`, gated by `has_clause_credential_label`)
   applies after all of the above, never before — a path-shaped anchor must not be able to skip a
   chain that would otherwise reconstruct a blocked credential.
8. **Structured-identifier exemption** off-trigger only — must come after the UUID/content-hash
   and hex-credential-token checks (neither of which it weakens) and before the entropy
   computation, since an identifier can exceed `ENTROPY_THRESHOLD` on Shannon entropy alone.

## Bridge fragment reconstruction

`bridge_fragment_chain` checks whether a trigger-adjacent short token is one piece of a
delimiter-split credential by walking outward and concatenating neighboring fragments.
`MAX_BRIDGE_FRAGMENTS` bounds the walk by FRAGMENT COUNT, deliberately not by gap byte length: a
byte-length gap bound is defeated by repeating the delimiter (e.g. three U+200B in a row instead
of one), while a fragment count cannot be bypassed that way since repeating a delimiter inside a
single gap never creates a new fragment. Each extension consumes one real fragment plus up to
`MAX_BRIDGE_GLUE_TOKENS` delimiter-only glue tokens crossed to reach it, and the walk stops the
moment a gap contains an ASCII alphanumeric character — this keeps reconstruction to a small
local neighborhood rather than a document-wide scan, even for a document seeded with a long run
of punctuation-only tokens (`--- --- --- ...`).

`MIN_BRIDGE_FRAGMENT_LEN` is the shortest bare token treated as a plausible fragment (the
`is_bridge_candidate` check in `check_entropy_heuristic`). Below this length, common short words
(`dead`, `beef`, `cafe`, or their base64-alphabet equivalents) would let ordinary prose feed the
bridge checks. It applies equally to hex and generic alphanumeric fragments — a
base64/base64url-shaped credential half is exactly as plausible a bridge candidate as a hex half;
the entropy/length decision made over the reconstructed chain, not this floor, is what keeps
ordinary short prose fragments from being flagged.

## is_structured_identifier

A structured identifier decomposes into two or more maximal ASCII-alphanumeric "runs" separated
by `/`, `-`, `_`, or `.`, where every run is word-shaped: letters-then-digits (`adr079`,
`slices234`, `R1`) or pure digits (`20260701`), at most `MAX_RUN_LEN` chars, with a low
case-transition density in the letter portion. Random base64/base62 secrets glued between
separators reliably fail this shape check: their case and digit placement is essentially uniform
rather than word-like, so a run either exceeds the length cap or mixes case too densely to pass.

Outside credential-trigger context this shape check alone is sufficient to exempt a token from
the entropy heuristic. In trigger context the caller grants NO exemption at all: see the module
doc and the call site in `check_entropy_heuristic`.
