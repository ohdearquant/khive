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
- Structured identifiers: a token is only considered for this exemption when it contains at least
  one of `/`, `-`, `_`, or `.` (the gate); it is then decomposed into maximal alphanumeric runs by
  splitting on *every* non-alphanumeric character (not just the four gating separators — any other
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
  credential-value syntax. The exemption is a *flag over the hex-credential-shape checks only*,
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
walk step over a small bounded number of CONTENT words outside those sets — "label with
qualifiers: value" (`api key for production deploy: <value>`) names the value regardless of
which qualifier nouns the label carries. A past-participle content word ends the walk:
verb-phrase prose narrates an action on the value rather than labeling it (`the auth scanner
flagged this file: <path>`, `one extra token was introduced by sha: <hex>` stay exempt). The
walk stops at a sentence/paragraph boundary (`;`, `!`, `?`, blank line; `.` only when not
immediately followed by an alphanumeric character, so a dotted version qualifier does not read
as a sentence end). The past-participle stop is position-sensitive: it applies only in verb
position — the participle followed (in reading order) by a glue word or the value itself
("flagged this file:", "introduced by sha:", "key updated: <v>"). Followed by a content noun it
is a participial ADJECTIVE inside a label qualifier ("shared deploy:", "encrypted backup:") and
walks like any other qualifier noun. A participle BEFORE the trigger word never matters — the
walk reaches the trigger first ("generated api key: <v>" blocks). A single-identifier lookback
is deliberately NOT the contract: `api key value is commit <hex>` is a labeled credential
wearing a marker, and one connector word must not hide the label. A label on the far side of a
sentence boundary is prose context (the `near_trigger` window models that), not this value's
label. Known residuals, accepted under the threat model: a non-connector qualifier without any
delimiter (`api key pour commit <hex>`), a participle in verb position directly after the
trigger (`api key updated: <hex>` reads as changelog prose — note the ordering: `updated api
key: <hex>` blocks, since the walk meets the trigger first), and label clauses exceeding the
walk or content-word bounds.

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
the true value always appears as *some* suffix, and a `=`/`:` that lands inside padding or a label
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
  shape a credential trigger must not lose. Only *letter*-joined collisions (`authorized`,
  `authentication`, `monkey`, `keyword`) are meant to stop matching.

CJK/accented prose always counts as a boundary in both modes (only ASCII alphanumerics — plus
underscore when `underscore_is_word_char` is `true` — are treated as word characters).

## mask_secrets

A transcript line cannot be rejected wholesale, so each credential span is replaced in place
while the surrounding prose is preserved. Spans are discovered left to right against the ORIGINAL
text via `scan_from`: each scan advances a `from` cursor past the previous span but always
evaluates trigger context over the full input. This closes the entropy-context gap — a
high-entropy value whose only trigger word sits to the left of an earlier-redacted secret is
still detected, because the trigger window is never sliced away. The known-prefix detectors (real
API keys: `sk-ant-`, `sk-proj-`, `AKIA`/`ASIA`, GitHub, Stripe, …) are context-free and matched the
same way.

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
