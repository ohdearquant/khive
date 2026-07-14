# `Authentication-Results` header parsing (RFC 8601 structural subset)

Source: `crates/khive-channel-email/src/auth_results.rs` (all items in this module are
`pub(crate)`/private — internal to the crate, not part of the published API).

## Why this module exists

`mail-parser` has no structured support for the `Authentication-Results` header — it is
not one of the RFC 5322 shapes it recognizes, so it always falls through to
`HeaderValue::Text` (or `TextList` when duplicated) via the generic `Other` header path.
This module hand-parses only what the attribution gate needs: the `authserv-id`, and the
`dmarc`/`spf`/`dkim` method verdicts plus the `header.d` / `smtp.mailfrom` / `header.from`
alignment properties.

## Segment splitting (`split_top_level_segments`)

The top-level split into `resinfo` segments is CFWS-aware: it walks the raw header value
once, tracking quoted-string state (honoring `\` escapes) and `(...)` comment nesting
(RFC 5322 comments nest), and only treats a `;` as a segment boundary when it appears
outside both. Comments are stripped entirely before segment text is retained;
quoted-string content is kept verbatim (including any `;` or `=` it contains) as part of
the single segment it belongs to. This guarantees a `;` inside a `reason="..."` quoted
pvalue or inside a `(...)` comment can never manufacture an additional, unintended
`method=result` segment — see `parse_header_reason_quoted_semicolon_does_not_forge_dmarc_pass`
and `parse_header_comment_semicolon_does_not_forge_dmarc_pass` in the test module. It does
not attempt full ABNF conformance beyond that (e.g. `ptype.property` values containing bare
`=`) — those are tolerated as harmless unmatched tokens, never as false positives against
the three keys this module actually reads.

## Whitespace tokenizing (`split_top_level_ws`)

The second of two delimiter layers: the segment scanner above finds `;` boundaries; this
one finds whitespace boundaries within a segment. Both share the same quoted-pair
semantics (`\` plus the following character is one atomic unit, regardless of what that
character is), but this layer never sees `(...)` comments — those were already discarded
by the segment scanner. A token is returned verbatim, including any retained quote and
backslash characters; it is never unquoted.

Malformed input (an unmatched `"`, or a `\` as the final character while quoted) is
handled conservatively: the remainder of the segment is retained as one atomic token
through EOF rather than resuming whitespace splitting, so a malformed quoted tail can
never be reinterpreted as additional tokens — see
`split_top_level_ws_keeps_malformed_quoted_tail_atomic`.

## `contains_unquoted`

Returns true if `target` occurs anywhere in `token` *outside* of a quoted-string span,
using the same quoted-pair (`\`) escaping semantics as `split_top_level_ws`. A plain
`str::contains` would treat a `=` inside a quoted value (e.g. a quoted `authserv-id` like
`"id=foo"`, which RFC 8601 §2.2 permits as a valid `value`) the same as an unquoted `=`,
even though the tokenizer that produced `token` already knows the difference — this keeps
the classification consistent with that state.

## `parse_header`

Detects two shapes for the first `resinfo`-or-authserv-id segment:

- **RFC 8601 form**: the first whitespace token of the first top-level segment is the
  `authserv-id` (a dot-atom/value that can never contain an unquoted `=`). The remaining
  segments are parsed as `resinfo` entries.
- **No-authserv-id form** (observed from Exchange Online's internal-hop stamp): the first
  whitespace token of the first segment itself contains an unquoted `=`, which is
  impossible for a valid authserv-id and unambiguous for a `resinfo`
  (`method[/version]=result`). In this case `authserv_id` is set to `None` and segment 0
  is parsed as a `resinfo` entry through the *same* loop as every other segment — it is
  never discarded as a (nonexistent) authserv-id.

Returns `None` when no signal can be extracted at all: an empty/whitespace header (no
first token), or — in the no-authserv-id form only — a header whose segments contain no
recognized `dmarc`/`spf`/`dkim` method entry anywhere. In the RFC 8601 form, a non-empty
authserv-id with no method the gate recognizes still parses successfully to an
`AuthResults` with empty method vectors (unchanged from prior behavior) — the gate treats
"no recognized passing method" uniformly regardless of which of those it was.

The unquoted-`=` detection (`is_no_authserv_id_form`) must use the same quote-state
machine as `split_top_level_ws` (via `contains_unquoted`), not a raw `str::contains`: RFC
8601 §2.2 permits a quoted-string `authserv-id` value, and a `=` sealed inside that
quoting is not a resinfo delimiter — treating it as one would let a quoted authserv-id be
misclassified as the no-authserv-id form, which `TrustAnchor::TopmostNoAuthservId` treats
as a strictly weaker, position-only trust signal than a real authserv-id match.

The optional method-version suffix (`method/version=result`, RFC 8601 §2.2) is supported
only for version `1` (absent suffix is implicitly version 1); §2.6 requires consumers to
IGNORE resinfo for a method version they do not support, so anything else skips the whole
segment rather than being silently trusted as the current version.

## `select_trusted`

Selects the trusted `Authentication-Results` header per the configured `TrustAnchor`
(ADR-056 Amendment 2026-07-03, "EXO no-authserv-id trust anchor"):

- `TrustAnchor::AuthservId`: the first (topmost) header, in document order, whose
  `authserv-id` matches the configured id (case-insensitive). Topmost wins: a receiving
  MTA prepends its own stamp on each hop, so the header nearest the top of the document is
  the one added by the final, trusted receiving boundary — PROVIDED that boundary strips
  or renames any pre-existing header already claiming its own `authserv-id` before adding
  its stamp. That stripping is an operational precondition of the receiving MTA, verified
  by deployment configuration, not re-derived from message content here.
- `TrustAnchor::TopmostNoAuthservId`: the boundary emits no authserv-id at all (e.g.
  Exchange Online's internal-hop stamp), so position is the *sole* discriminator. Only the
  literal topmost `Authentication-Results` header (`raw_headers[0]`) is ever considered —
  never a later one, even if the topmost fails to parse. It is trusted only if it parses
  AND is itself in the no-authserv-id form (`authserv_id.is_none()`); if the topmost
  carries any authserv-id, or fails to parse, that violates the invariant that this
  boundary's own stamp is topmost and unadorned, so the message quarantines (fails closed)
  rather than falling through to a lower header.
