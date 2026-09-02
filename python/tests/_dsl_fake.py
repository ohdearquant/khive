"""A small parser mirroring the khive-cloud request DSL grammar.

Used only to make the offline fake REST/MCP servers in `conftest.py` enforce
the real wire contract (`ops` is one request DSL string, not the client's
internal JSON ops-array form) instead of silently accepting whatever the
submitted renderer happens to send.
"""

from __future__ import annotations

import json
import re
from typing import Any

_IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
_JSON_NUMBER_RE = re.compile(r"^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$")


class DslParseError(ValueError):
    pass


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
    raise DslParseError(f"non-finite constant {name!r} has no representation in the request DSL")


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
        raise DslParseError(f"invalid identifier at {pos}: {text!r}")
    tool = m.group(0)
    pos = m.end()
    if pos < len(text) and text[pos] == ".":
        pos += 1
        m2 = _IDENT_RE.match(text, pos)
        if not m2:
            raise DslParseError(f"invalid identifier at {pos}: {text!r}")
        tool = f"{tool}.{m2.group(0)}"
        pos = m2.end()
        if pos < len(text) and text[pos] == ".":
            raise DslParseError(f"unsupported verb nesting: {tool}{text[pos:]!r}")
    return tool, pos


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
            while i < n and path[i].isdigit():
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
            raise DslParseError("$prev reference used outside a chain")
        return PrevRef("")
    if s.startswith("$prev."):
        rest = s[len("$prev.") :]
        if rest and _quoted_prev_path_is_valid(rest):
            if not in_chain:
                raise DslParseError("$prev reference used outside a chain")
            return PrevRef(rest)
        return s
    if s.startswith("$prev["):
        after = s[len("$prev[") :]
        close = after.find("]")
        if close != -1:
            index_str = after[:close]
            if index_str and index_str.isdigit():
                tail = after[close + 1 :]
                if _quoted_prev_path_is_valid(tail):
                    if not in_chain:
                        raise DslParseError("$prev reference used outside a chain")
                    return PrevRef(f"[{index_str}]{tail}")
        return s
    return s


def _parse_bare_prev_ref(text: str, in_chain: bool) -> PrevRef:
    """Parses the primary (unquoted) `$prev` reference syntax — mirrors
    `parser_impl.rs::parse_prev_ref`, triggered when a value starts with the
    `$` sigil directly, as opposed to a quoted string that merely looks like
    one (`_string_as_prev_ref`'s job). `text` is the whole isolated value
    token, so a full parse must consume it exactly."""
    if not (text == "$prev" or text.startswith(("$prev.", "$prev["))):
        raise DslParseError(f"expected '$prev', found {text!r}")
    pos = len("$prev")
    n = len(text)
    path = ""
    while pos < n:
        c = text[pos]
        if c == ".":
            pos += 1
            m = _IDENT_RE.match(text, pos)
            if not m:
                raise DslParseError(f"expected identifier after '.' in {text!r}")
            if path:
                path += "."
            path += m.group(0)
            pos = m.end()
        elif c == "[":
            pos += 1
            idx_start = pos
            while pos < n and text[pos].isdigit():
                pos += 1
            if pos == idx_start or pos >= n or text[pos] != "]":
                raise DslParseError(f"malformed index in $prev path: {text!r}")
            index_str = text[idx_start:pos]
            pos += 1
            if path:
                path += "."
            path += f"[{index_str}]"
        else:
            raise DslParseError(f"unexpected {c!r} in $prev path: {text!r}")
    if not in_chain:
        raise DslParseError("$prev reference used outside a chain")
    return PrevRef(path)


def _apply_prev_ref_rules(value: Any, in_chain: bool) -> Any:
    """Recursively applies `_string_as_prev_ref` to a decoded JSON tree — the
    cloud parser applies `string_as_prev_ref` to every string it parses
    inside an object/array argument, not just at the argument's own top
    level."""
    if isinstance(value, str):
        return _string_as_prev_ref(value, in_chain=in_chain)
    if isinstance(value, list):
        return [_apply_prev_ref_rules(v, in_chain) for v in value]
    if isinstance(value, dict):
        return {k: _apply_prev_ref_rules(v, in_chain) for k, v in value.items()}
    return value


def _parse_string(text: str) -> str:
    if len(text) < 2 or text[-1] != '"':
        raise DslParseError(f"unterminated string: {text!r}")
    body = text[1:-1]
    out: list[str] = []
    i = 0
    n = len(body)
    while i < n:
        ch = body[i]
        if ch == "\\" and i + 1 < n:
            nxt = body[i + 1]
            decoded = {"\\": "\\", '"': '"', "'": "'", "n": "\n", "t": "\t", "r": "\r"}.get(nxt)
            if decoded is not None:
                out.append(decoded)
                i += 2
                continue
            # any other backslash sequence is kept literally
            out.append(ch)
            i += 1
            continue
        if ord(ch) < 0x20:
            raise DslParseError(f"raw control character in string literal: {ch!r}")
        out.append(ch)
        i += 1
    return "".join(out)


def _parse_value(text: str, in_chain: bool = False) -> Any:
    text = text.strip()
    if not text:
        raise DslParseError("empty value")
    if text[0] == "$":
        return _parse_bare_prev_ref(text, in_chain)
    if text[0] == '"':
        return _string_as_prev_ref(_parse_string(text), in_chain=in_chain)
    if text == "true":
        return True
    if text == "false":
        return False
    if text == "null":
        return None
    if text[0] == "[":
        if text[-1] != "]":
            raise DslParseError(f"unterminated array: {text!r}")
        inner = text[1:-1].strip()
        if not inner:
            return []
        return [_parse_value(p, in_chain) for p in _split_top_level(inner, ",")]
    if text[0] == "{":
        try:
            raw = json.loads(text, parse_constant=_reject_non_finite_constant)
        except ValueError as exc:
            raise DslParseError(f"malformed object literal: {text!r}") from exc
        return _apply_prev_ref_rules(raw, in_chain)
    if _JSON_NUMBER_RE.match(text):
        return json.loads(text, parse_constant=_reject_non_finite_constant)
    raise DslParseError(f"unparseable value: {text!r}")


def _parse_op(text: str, in_chain: bool = False) -> tuple[str, dict[str, Any]]:
    text = text.strip()
    tool, pos = _parse_tool_name(text, 0)
    pos = _skip_ws(text, pos)
    if pos >= len(text) or text[pos] != "(" or text[-1] != ")":
        raise DslParseError(f"not a call: {text!r}")
    argtext = text[pos + 1 : -1].strip()
    args: dict[str, Any] = {}
    if argtext:
        for piece in _split_top_level(argtext, ","):
            piece = piece.strip()
            if not piece:
                continue
            if "=" not in piece:
                raise DslParseError(f"malformed arg: {piece!r}")
            key, _, val = piece.partition("=")
            args[key.strip()] = _parse_value(val.strip(), in_chain)
    return tool, args


def parse_dsl_with_mode(text: str) -> tuple[list[tuple[str, dict[str, Any]]], str]:
    """Parses a request DSL string into `([(verb, args), ...], mode)`, where
    `mode` is `"single"`, `"parallel"`, or `"chain"` — a `$prev` reference is
    accepted only in the last of these (`dispatch.rs::parse_chain_tail`)."""
    text = text.strip()
    if not text:
        raise DslParseError("empty ops string")
    if text[0] == "[":
        if text[-1] != "]":
            raise DslParseError(f"unterminated batch: {text!r}")
        inner = text[1:-1].strip()
        if not inner:
            return [], "parallel"
        chain_parts = _split_top_level(inner, "|")
        if len(chain_parts) > 1:
            return [_parse_op(p, in_chain=True) for p in chain_parts], "chain"
        return [_parse_op(p, in_chain=False) for p in _split_top_level(inner, ",")], "parallel"
    chain_parts = _split_top_level(text, "|")
    if len(chain_parts) > 1:
        return [_parse_op(p, in_chain=True) for p in chain_parts], "chain"
    return [_parse_op(text, in_chain=False)], "single"


def parse_dsl(text: str) -> list[tuple[str, dict[str, Any]]]:
    """Parses a request DSL string into `[(verb, args), ...]`."""
    ops, _mode = parse_dsl_with_mode(text)
    return ops
