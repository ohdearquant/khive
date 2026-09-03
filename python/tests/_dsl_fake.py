"""A small parser mirroring the khive-cloud request DSL grammar.

Used only to make the offline fake REST/MCP servers in `conftest.py` enforce
the real wire contract (`ops` is one request DSL string, not the client's
internal JSON ops-array form) instead of silently accepting whatever the
renderer under test happens to send.

Scope of the mirror: the fake pins whether an input is accepted or rejected
and, where `docs/DSL_WIRE_CONTRACT.md` names one for the shape under test,
the error variant class the real parser reports (`DslParseError.variant`,
spelled as the `DslError` variant name in `crates/khive-request`). Error
message text is a diagnostic for the test reader and pins nothing; the real
parser's reason strings are not part of the wire contract. A raise without a
`variant` is a rejection the fake does not classify, which is deliberate:
mirroring every positional arm of the real parser is not this file's job.
"""

from __future__ import annotations

import json
import re
from typing import Any

_IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
_JSON_NUMBER_RE = re.compile(r"^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$")

# Mirrors `crates/khive-request/src/types.rs::MAX_OPS`.
MAX_OPS = 100
# Mirrors `crates/khive-request/src/types.rs::MAX_OPS_INPUT_LEN`.
MAX_OPS_INPUT_LEN = 1024 * 1024
# Mirrors `crates/khive-request/src/types.rs::NESTING_DEPTH_LIMIT`.
NESTING_DEPTH_LIMIT = 64
# The exact range `serde_json::Value::Number` (default features) can hold as
# an integer; outside it, serde decodes the literal as `f64`.
_MIN_SIGNED_64 = -(2**63)
_MAX_UNSIGNED_64 = 2**64 - 1


class DslParseError(ValueError):
    """A rejection by the fake. `variant`, when set, names the `DslError`
    variant the real parser reports for the same input."""

    def __init__(self, message: str, variant: str | None = None) -> None:
        super().__init__(message)
        self.variant = variant


class PrevRef:
    """A resolved `$prev` chain reference (mirrors `ArgValue::PrevRef`)."""

    __slots__ = ("path",)

    def __init__(self, path: str) -> None:
        self.path = path

    def __eq__(self, other: object) -> bool:
        return isinstance(other, PrevRef) and self.path == other.path

    def __repr__(self) -> str:
        return f"PrevRef({self.path!r})"


def _reject_non_finite_constant(name: str) -> float:
    raise DslParseError(
        f"non-finite constant {name!r} has no representation in the request DSL",
        variant="InvalidValue",
    )


def _skip_ws(text: str, pos: int) -> int:
    n = len(text)
    while pos < n and text[pos] in " \t\n\r\f\v":
        pos += 1
    return pos


def _split_top_level(text: str, seps: str) -> list[str]:
    """Split `text` on any of `seps` at bracket/quote depth 0."""
    parts: list[str] = []
    depth = 0
    in_string = False
    buf: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if in_string:
            buf.append(ch)
            if ch == "\\" and i + 1 < n:
                buf.append(text[i + 1])
                i += 2
                continue
            if ch == '"':
                in_string = False
            i += 1
            continue
        if ch == '"':
            in_string = True
            buf.append(ch)
            i += 1
            continue
        if ch in "([{":
            depth += 1
            buf.append(ch)
            i += 1
            continue
        if ch in ")]}":
            depth -= 1
            buf.append(ch)
            i += 1
            continue
        if depth == 0 and ch in seps:
            parts.append("".join(buf))
            buf = []
            i += 1
            continue
        buf.append(ch)
        i += 1
    parts.append("".join(buf))
    return parts


def _parse_tool_name(text: str, pos: int) -> tuple[str, int]:
    """Parses `ident` or `ident.ident` — a second dot is unsupported verb
    nesting (`parser_impl.rs::parse_op`)."""
    pos = _skip_ws(text, pos)
    m = _IDENT_RE.match(text, pos)
    if not m:
        raise DslParseError(f"invalid identifier at {pos}: {text!r}", variant="InvalidIdentifier")
    tool = m.group(0)
    pos = m.end()
    if pos < len(text) and text[pos] == ".":
        pos += 1
        m2 = _IDENT_RE.match(text, pos)
        if not m2:
            raise DslParseError(
                f"invalid identifier at {pos}: {text!r}", variant="InvalidIdentifier"
            )
        tool = f"{tool}.{m2.group(0)}"
        pos = m2.end()
        if pos < len(text) and text[pos] == ".":
            raise DslParseError(
                f"unsupported verb nesting: {tool}{text[pos:]!r}", variant="UnsupportedVerbNesting"
            )
    return tool, pos


def _is_ascii_digit(c: str) -> bool:
    """`str.isdigit()` accepts non-ASCII digits (e.g. `١`); the Rust source
    (`parser_impl.rs::string_as_prev_ref`/`parse_prev_ref`) requires
    `char::is_ascii_digit`, so an index built from a non-ASCII digit must
    leave the whole value a literal string, not a reference."""
    return c.isascii() and c.isdigit()


def _quoted_prev_path_is_valid(path: str) -> bool:
    """Mirrors `parser_impl.rs::quoted_prev_path_is_valid`: a malformed
    `[...]` segment (or a stray `]`) keeps the whole value literal."""
    i = 0
    n = len(path)
    while i < n:
        c = path[i]
        if c == "[":
            i += 1
            start = i
            while i < n and _is_ascii_digit(path[i]):
                i += 1
            if i == start or i >= n or path[i] != "]":
                return False
            i += 1
        elif c == "]":
            return False
        else:
            i += 1
    return True


def _string_as_prev_ref(s: str, *, in_chain: bool) -> Any:
    """Mirrors `parser_impl.rs::string_as_prev_ref` plus the outside-chain
    check `dispatch.rs` applies to a fully-parsed op: a decoded string equal
    to `$prev`, or starting with `$prev.`/`$prev[` and shaped as a valid
    path, is a reference (rejected here if `not in_chain`, matching
    `PrevRefOutsideChain`); the same shape preceded by a literal backslash is
    the escaped-literal form and decodes to the text with that backslash
    stripped. Anything else is an ordinary string."""
    if s.startswith("\\"):
        rest = s[1:]
        if rest == "$prev" or rest.startswith(("$prev.", "$prev[")):
            return rest
    if s == "$prev":
        if not in_chain:
            raise DslParseError(
                "$prev reference used outside a chain", variant="PrevRefOutsideChain"
            )
        return PrevRef("")
    if s.startswith("$prev."):
        rest = s[len("$prev.") :]
        if rest and _quoted_prev_path_is_valid(rest):
            if not in_chain:
                raise DslParseError(
                    "$prev reference used outside a chain", variant="PrevRefOutsideChain"
                )
            return PrevRef(rest)
        return s
    if s.startswith("$prev["):
        after = s[len("$prev[") :]
        close = after.find("]")
        if close != -1:
            index_str = after[:close]
            if index_str and all(_is_ascii_digit(c) for c in index_str):
                tail = after[close + 1 :]
                if _quoted_prev_path_is_valid(tail):
                    if not in_chain:
                        raise DslParseError(
                            "$prev reference used outside a chain", variant="PrevRefOutsideChain"
                        )
                    return PrevRef(f"[{index_str}]{tail}")
        return s
    return s


def _parse_bare_prev_ref(
    text: str, in_chain: bool, *, followed_by_close_paren: bool = False
) -> PrevRef:
    """Parses the primary (unquoted) `$prev` reference syntax — mirrors
    `parser_impl.rs::parse_prev_ref`, triggered when a value starts with the
    `$` sigil directly, as opposed to a quoted string that merely looks like
    one (`_string_as_prev_ref`'s job). `text` is the whole isolated value
    token, so a full parse must consume it exactly. `followed_by_close_paren`
    says the call's own `)` came right after `text` in the real source (see
    `_parse_value`), which decides whether an index left open at the end of
    `text` met that `)` or true end-of-input."""
    if not (text == "$prev" or text.startswith(("$prev.", "$prev["))):
        raise DslParseError(f"expected '$prev', found {text!r}", variant="InvalidValue")
    pos = len("$prev")
    n = len(text)
    path = ""
    while pos < n:
        c = text[pos]
        if c == ".":
            pos += 1
            m = _IDENT_RE.match(text, pos)
            if not m:
                raise DslParseError(
                    f"expected identifier after '.' in {text!r}", variant="InvalidIdentifier"
                )
            if path:
                path += "."
            path += m.group(0)
            pos = m.end()
        elif c == "[":
            pos += 1
            idx_start = pos
            while pos < n and _is_ascii_digit(text[pos]):
                pos += 1
            if pos == idx_start:
                raise DslParseError(
                    f"malformed index in $prev path: {text!r}", variant="InvalidValue"
                )
            if pos >= n:
                raise DslParseError(
                    f"malformed index in $prev path: {text!r}",
                    variant="UnexpectedChar" if followed_by_close_paren else "UnexpectedEof",
                )
            if text[pos] != "]":
                raise DslParseError(
                    f"malformed index in $prev path: {text!r}", variant="UnexpectedChar"
                )
            index_str = text[idx_start:pos]
            pos += 1
            if path:
                path += "."
            path += f"[{index_str}]"
        else:
            raise DslParseError(
                f"unexpected {c!r} in $prev path: {text!r}", variant="UnexpectedChar"
            )
    if not in_chain:
        raise DslParseError("$prev reference used outside a chain", variant="PrevRefOutsideChain")
    return PrevRef(path)


_RAW_CONTROL_ESCAPE = {"\n": "\\n", "\r": "\\r", "\t": "\\t"}


def _string_end(text: str, start: int) -> int | None:
    """Mirrors `parser/scan.rs::scan_string_end`: `start` sits on the opening
    quote; returns the index just past the closing quote, skipping backslash
    pairs, or None when the span never closes."""
    i = start + 1
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "\\":
            i += 2
            continue
        i += 1
        if ch == '"':
            return i
    return None


def _parse_string(text: str) -> str:
    """Mirrors `parser_impl.rs::decode_quoted_json_string`: a raw newline/CR/
    tab byte may appear literally inside the quoted span and is rewritten to
    its JSON escape before the whole literal is decoded strictly with
    `json.loads` (this also makes `\\uXXXX` decode, unlike a hand-rolled
    escape table). Any other raw control byte is left as-is, which
    `json.loads` rejects exactly as strict JSON does. JSON does not define
    `\\'`, so that sequence is rejected too, not decoded to `'`. A span that
    never closes is the scanner's `UnclosedString`; one that closes with
    bytes left over fails the strict decode like any malformed literal."""
    end = _string_end(text, 0) if text[:1] == '"' else None
    if end is None:
        raise DslParseError(f"unterminated string: {text!r}", variant="UnclosedString")
    normalized: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "\\" and i + 1 < n:
            normalized.append(ch)
            normalized.append(text[i + 1])
            i += 2
            continue
        normalized.append(_RAW_CONTROL_ESCAPE.get(ch, ch))
        i += 1
    try:
        return json.loads("".join(normalized))
    except ValueError as exc:
        raise DslParseError(f"malformed string literal: {text!r}", variant="InvalidValue") from exc


def _coerce_out_of_range_int(value: Any) -> Any:
    """Mirrors `serde_json::Value::Number`'s exact integer range: Python's
    `json` module always returns an arbitrary-precision `int` for an
    integer-looking literal, but serde (default features, no
    `arbitrary_precision`) only holds i64/u64 exactly — a literal outside
    `[_MIN_SIGNED_64, _MAX_UNSIGNED_64]` decodes as `f64` there, so this
    coerces the same literal to a Python `float` here."""
    if (
        isinstance(value, int)
        and not isinstance(value, bool)
        and (value < _MIN_SIGNED_64 or value > _MAX_UNSIGNED_64)
    ):
        return float(value)
    return value


def _parse_object(text: str, in_chain: bool, depth: int = 0) -> dict[str, Any]:
    """Mirrors `parser_impl.rs::Parser::parse_object_arg_body`: each value is
    parsed through the same value grammar as an argument's own top level
    (`_parse_value`, recursively) rather than decoded as one JSON blob — so a
    bare `$prev` reference, or a quoted string with a raw newline/CR/tab
    byte, is accepted at any object depth exactly as it is at the top
    level. A key must be a quoted string (`parser_impl.rs` rejects a bare
    key with `UnexpectedChar { expected: "quoted string key" }`). `depth` is
    the container depth already entered for this object (see `_parse_value`
    — the `{` branch increments before calling here)."""
    inner = text[1:-1].strip()
    if not inner:
        return {}
    obj: dict[str, Any] = {}
    for entry in _split_top_level(inner, ","):
        entry = entry.strip()
        if not entry:
            raise DslParseError(f"malformed object literal: {text!r}", variant="UnexpectedChar")
        parts = _split_top_level(entry, ":")
        if len(parts) != 2:
            raise DslParseError(f"malformed object entry: {entry!r}", variant="UnexpectedChar")
        key_text = parts[0].strip()
        if not key_text.startswith('"'):
            raise DslParseError(
                f"object key must be a quoted string: {key_text!r}", variant="UnexpectedChar"
            )
        key_end = _string_end(key_text, 0)
        if key_end is not None and key_end < len(key_text):
            # The key decoded; the real parser then asks for `:` and meets
            # the leftover byte instead.
            raise DslParseError(
                f"unexpected text after object key: {key_text!r}", variant="UnexpectedChar"
            )
        key = _parse_string(key_text)
        obj[key] = _parse_value(parts[1].strip(), in_chain, depth)
    return obj


def _parse_value(
    text: str,
    in_chain: bool = False,
    depth: int = 0,
    *,
    followed_by_close_paren: bool = False,
) -> Any:
    """`depth` is the container depth already entered before `text`'s own
    value (see `_dsl_fake.NESTING_DEPTH_LIMIT`); entering `text`, when it is
    itself an array or object, is one more increment, checked before
    recursing — mirrors `parser_impl.rs::Parser::enter_container` (depth 64
    accepted, 65 refused). `followed_by_close_paren` is set only for the
    last argument of a well-formed call (`_parse_op`'s `text[-1] == ")"`
    branch, via `_parse_args`): the call's own `)` was sliced off before
    `text` ever reached here, so an unterminated `{` at the end of `text`
    was, in the real source, immediately followed by that `)` rather than by
    true end-of-input — the same distinction `parser_impl.rs::
    parse_object_arg_body`'s `Some(c)` vs `None` arms (about 271-282) make."""
    text = text.strip()
    if not text:
        raise DslParseError("empty value", variant="InvalidValue")
    if text[0] == "$":
        return _parse_bare_prev_ref(text, in_chain, followed_by_close_paren=followed_by_close_paren)
    if text[0] == "[":
        if text[-1] != "]":
            raise DslParseError(f"unterminated array: {text!r}")
        child_depth = depth + 1
        if child_depth > NESTING_DEPTH_LIMIT:
            raise DslParseError(
                f"container nesting depth {child_depth} exceeds max {NESTING_DEPTH_LIMIT}",
                variant="NestingTooDeep",
            )
        inner = text[1:-1].strip()
        if not inner:
            return []
        return [_parse_value(p, in_chain, child_depth) for p in _split_top_level(inner, ",")]
    if text[0] == "{":
        child_depth = depth + 1
        if child_depth > NESTING_DEPTH_LIMIT:
            raise DslParseError(
                f"container nesting depth {child_depth} exceeds max {NESTING_DEPTH_LIMIT}",
                variant="NestingTooDeep",
            )
        if text[-1] != "}":
            # Mirrors `parser_impl.rs::parse_object_arg_body`'s key-position
            # match (P113): the byte after `{` decides the class. True
            # end-of-input is the `None` arm; any present byte that is not a
            # `"` — the call's own `)` when `followed_by_close_paren`, or a
            # `,` kept inside this slice by `_split_top_level` — is the
            # `Some(c)` arm. A quoted key that then runs off the end is left
            # unclassified: the real parser fails somewhere after the key,
            # at an arm this fake does not mirror.
            after_brace = text[1:].lstrip()
            if not after_brace:
                if followed_by_close_paren:
                    raise DslParseError(
                        f"unexpected ')' while expecting a quoted string key: {text!r}",
                        variant="UnexpectedChar",
                    )
                raise DslParseError(
                    f"unexpected end of input while expecting an object key: {text!r}",
                    variant="UnexpectedEof",
                )
            if after_brace[0] != '"':
                raise DslParseError(
                    f"unexpected {after_brace[0]!r} while expecting a quoted string key: {text!r}",
                    variant="UnexpectedChar",
                )
            raise DslParseError(f"unterminated object: {text!r}")
        return _parse_object(text, in_chain, child_depth)
    # A scalar: the real parser first scans to the value's boundary
    # (`parser_impl.rs::scan_value_end`), decodes that slice, and only then
    # lets the enclosing grammar see what follows — so a leftover byte is
    # rejected as unexpected only once the slice before it decoded.
    boundary = _scan_value_end(text)
    if boundary < len(text):
        _parse_scalar(text[:boundary].strip(), in_chain)
        raise DslParseError(
            f"unexpected {text[boundary]!r} after a value: {text!r}", variant="UnexpectedChar"
        )
    return _parse_scalar(text, in_chain)


def _parse_scalar(text: str, in_chain: bool) -> Any:
    """Decodes one already-bounded scalar slice the way `parse_value` hands
    it to `decode_quoted_json_string` or `serde_json`."""
    if not text:
        raise DslParseError("empty value", variant="InvalidValue")
    if text[0] == '"':
        return _string_as_prev_ref(_parse_string(text), in_chain=in_chain)
    if text == "true":
        return True
    if text == "false":
        return False
    if text == "null":
        return None
    if _JSON_NUMBER_RE.match(text):
        parsed = json.loads(text, parse_constant=_reject_non_finite_constant)
        return _coerce_out_of_range_int(parsed)
    raise DslParseError(f"unparseable value: {text!r}", variant="InvalidValue")


def _scan_value_end(text: str) -> int:
    """Mirrors `parser_impl.rs::scan_value_end` over a raw scalar slice:
    returns the index where the value ends — a `]`, `}`, `)` or `,` met with
    no local container open, else the end of the slice. A `}` or `)` met
    while a *different* local container is still open is `UnclosedBracket`
    at once (P116, e.g. the `1[}` slice of `v(a=1[})`); a `[` or `{` still
    open at the end of the slice is the same class. A `"` hands off to the
    string scan first, so a bracket inside a quoted span is never counted
    and a span that never closes is `UnclosedString`."""
    depth_paren = 0
    depth_brack = 0
    depth_brace = 0
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            end = _string_end(text, i)
            if end is None:
                raise DslParseError(f"unterminated string: {text!r}", variant="UnclosedString")
            i = end
            continue
        if ch == "[":
            depth_brack += 1
        elif ch == "]":
            if depth_brack == 0:
                return i
            depth_brack -= 1
        elif ch == "{":
            depth_brace += 1
        elif ch == "}":
            if depth_brace == 0:
                if depth_paren == 0 and depth_brack == 0:
                    return i
                raise DslParseError(
                    f"unclosed bracket: unmatched '{{' while scanning {text!r}",
                    variant="UnclosedBracket",
                )
            depth_brace -= 1
        elif ch == "(":
            depth_paren += 1
        elif ch == ")":
            if depth_paren == 0 and depth_brack == 0 and depth_brace == 0:
                return i
            if depth_paren == 0:
                raise DslParseError(
                    f"unclosed bracket: unmatched '(' while scanning {text!r}",
                    variant="UnclosedBracket",
                )
            depth_paren -= 1
        elif ch == "," and depth_paren == 0 and depth_brack == 0 and depth_brace == 0:
            return i
        i += 1
    if depth_brack > 0 or depth_brace > 0:
        raise DslParseError(
            f"unclosed bracket: container left open in {text!r}", variant="UnclosedBracket"
        )
    return i


def _parse_args(argtext: str, in_chain: bool, *, closed: bool = False) -> dict[str, Any]:
    """`closed` marks a well-formed call's args slice (`_parse_op`'s
    `text[-1] == ")"` branch): that trailing `)` was sliced off before
    `argtext` got here, so only the LAST piece could possibly have been
    immediately followed by it in the real source (any earlier piece is
    followed by a `,` instead, inside `argtext` itself) — see
    `_parse_value`'s `followed_by_close_paren` parameter."""
    args: dict[str, Any] = {}
    if not argtext:
        return args
    pieces = _split_top_level(argtext, ",")
    last_index = len(pieces) - 1
    for index, piece in enumerate(pieces):
        piece = piece.strip()
        if not piece:
            # Mirrors `parser_impl.rs::parse_op`: after a `,` the loop
            # always expects another `name=value`, so a trailing comma
            # (or any empty piece) is rejected, not skipped.
            raise DslParseError(
                f"malformed arg list: trailing or empty argument in {argtext!r}",
                variant="InvalidIdentifier",
            )
        if "=" not in piece:
            # The real parser asks for `=` after the name: it meets the next
            # `,` or the call's `)` unless this piece is the open tail.
            raise DslParseError(
                f"malformed arg: {piece!r}",
                variant="UnexpectedChar" if closed or index < last_index else "UnexpectedEof",
            )
        key, _, val = piece.partition("=")
        key = key.strip()
        if not _IDENT_RE.fullmatch(key):
            # An identifier that starts well but carries junk fails at the
            # `=` delimiter; one that never starts is an invalid identifier.
            head = _IDENT_RE.match(key)
            raise DslParseError(
                f"invalid argument name: {key!r}",
                variant="UnexpectedChar" if head else "InvalidIdentifier",
            )
        if key in args:
            # Mirrors `parser_impl.rs::parse_op`'s `DuplicateArg`.
            raise DslParseError(f"duplicate argument: {key!r}", variant="DuplicateArg")
        args[key] = _parse_value(
            val.strip(),
            in_chain,
            followed_by_close_paren=closed and index == last_index,
        )
    return args


def _parse_op(text: str, in_chain: bool = False) -> tuple[str, dict[str, Any]]:
    text = text.strip()
    tool, pos = _parse_tool_name(text, 0)
    pos = _skip_ws(text, pos)
    if pos >= len(text) or text[pos] != "(":
        raise DslParseError(f"not a call: {text!r}")
    if text[-1] != ")":
        # Not a well-formed call. Still route the tail through the same
        # argument/value grammar a real call uses so a shape like an object
        # literal reaching end-of-input while expecting a key (P113) fails
        # with the reason `parser_impl.rs::parse_object_arg_body` gives,
        # rather than a generic "not a call" that never inspects the tail.
        _parse_args(text[pos + 1 :].strip(), in_chain)
        raise DslParseError(f"not a call: {text!r}", variant="UnclosedCall")
    args = _parse_args(text[pos + 1 : -1].strip(), in_chain, closed=True)
    return tool, args


def parse_dsl_with_mode(text: str) -> tuple[list[tuple[str, dict[str, Any]]], str]:
    """Parses a request DSL string into `([(verb, args), ...], mode)`, where
    `mode` is `"single"`, `"parallel"`, or `"chain"` — a `$prev` reference is
    accepted only in the last of these (`dispatch.rs::parse_chain_tail`).
    Mirrors `dispatch.rs::parse_request`'s ordering: the raw-empty check
    (`Empty`), then the raw byte-length cap (`InputTooLarge`), both before
    any routing or parsing; the chain/batch operation-count cap
    (`TooManyOps`) is then checked once the operation list for that mode is
    known."""
    text = text.strip()
    if not text:
        raise DslParseError("empty ops string", variant="Empty")
    raw_len = len(text.encode("utf-8"))
    if raw_len > MAX_OPS_INPUT_LEN:
        raise DslParseError(
            f"ops input is {raw_len} bytes; max is {MAX_OPS_INPUT_LEN} bytes",
            variant="InputTooLarge",
        )
    if text[0] == "[":
        if text[-1] != "]":
            raise DslParseError(f"unterminated batch: {text!r}")
        inner = text[1:-1].strip()
        if not inner:
            return [], "parallel"
        if len(_split_top_level(inner, "|")) > 1:
            raise DslParseError(
                "mixed separators: '|' is not allowed inside '[...]'", variant="MixedSeparators"
            )
        parts = _split_top_level(inner, ",")
        if not parts[-1].strip():
            # Mirrors `dispatch.rs::parse_fn_batch`: a `,` followed by `]`.
            raise DslParseError(f"trailing comma in batch: {text!r}", variant="TrailingComma")
        if len(parts) > MAX_OPS:
            raise DslParseError(
                f"batch has {len(parts)} ops; max is {MAX_OPS}", variant="TooManyOps"
            )
        return [_parse_op(p, in_chain=False) for p in parts], "parallel"
    chain_parts = _split_top_level(text, "|")
    if len(chain_parts) > 1:
        if len(chain_parts) > MAX_OPS:
            raise DslParseError(
                f"chain has {len(chain_parts)} ops; max is {MAX_OPS}", variant="TooManyOps"
            )
        return [_parse_op(p, in_chain=True) for p in chain_parts], "chain"
    return [_parse_op(text, in_chain=False)], "single"


def parse_dsl(text: str) -> list[tuple[str, dict[str, Any]]]:
    """Parses a request DSL string into `[(verb, args), ...]`."""
    ops, _mode = parse_dsl_with_mode(text)
    return ops
