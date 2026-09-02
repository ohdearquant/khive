"""A small parser mirroring the khive-cloud request DSL grammar.

Used only to make the offline fake REST/MCP servers in `conftest.py` enforce
the real wire contract (`ops` is one DSL string, not the client's internal
JSON ops-array form) instead of silently accepting whatever the client under
test happens to send.
"""

from __future__ import annotations

import json
import re
from typing import Any

_OP_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_.]*)\((.*)\)$", re.DOTALL)
_FINITE_NUMBER_RE = re.compile(r"^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$")


class DslParseError(ValueError):
    pass


def _reject_non_finite_constant(name: str) -> float:
    raise DslParseError(f"non-finite constant {name!r} has no representation in the request DSL")


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


def _parse_value(text: str) -> Any:
    text = text.strip()
    if not text:
        raise DslParseError("empty value")
    if text[0] == '"':
        return _parse_string(text)
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
        return [_parse_value(p) for p in _split_top_level(inner, ",")]
    if text[0] == "{":
        try:
            return json.loads(text, parse_constant=_reject_non_finite_constant)
        except ValueError as exc:
            raise DslParseError(f"malformed object literal: {text!r}") from exc
    try:
        return int(text)
    except ValueError:
        pass
    if _FINITE_NUMBER_RE.match(text):
        return float(text)
    raise DslParseError(f"unparseable value: {text!r}")


def _parse_op(text: str) -> tuple[str, dict[str, Any]]:
    text = text.strip()
    match = _OP_RE.match(text)
    if not match:
        raise DslParseError(f"not a call: {text!r}")
    verb, argtext = match.group(1), match.group(2).strip()
    args: dict[str, Any] = {}
    if argtext:
        for piece in _split_top_level(argtext, ","):
            piece = piece.strip()
            if not piece:
                continue
            if "=" not in piece:
                raise DslParseError(f"malformed arg: {piece!r}")
            key, _, val = piece.partition("=")
            args[key.strip()] = _parse_value(val.strip())
    return verb, args


def parse_dsl(text: str) -> list[tuple[str, dict[str, Any]]]:
    """Parse a request DSL string into `[(verb, args), ...]`."""
    text = text.strip()
    if not text:
        raise DslParseError("empty ops string")
    if text[0] == "[":
        if text[-1] != "]":
            raise DslParseError(f"unterminated batch: {text!r}")
        inner = text[1:-1].strip()
        if not inner:
            return []
        chain_parts = _split_top_level(inner, "|")
        if len(chain_parts) > 1:
            return [_parse_op(p) for p in chain_parts]
        return [_parse_op(p) for p in _split_top_level(inner, ",")]
    return [_parse_op(text)]
