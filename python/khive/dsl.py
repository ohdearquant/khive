"""Rendering the khive-cloud request DSL from the client's internal ops form.

`HttpTransport`/`AsyncHttpTransport` talk to khive-cloud's `POST /v1/request`,
whose `ops` field is one string in the request DSL — not the JSON array of
`{"tool", "args"}` dicts `khive.ops.encode` produces for the daemon's native
socket wire (`{"ops":"[{\\"tool\\":\\"whoami\\",\\"args\\":{}}]"}` is refused
with `unknown verb: Missing 'verb' field in JSON`). `render_dsl` bridges the
two: it takes the decoded ops-array form and renders it as DSL text. DSL
text a caller already holds passes through untouched, whether it is the
whole `ops` value or one element beside `{"tool", "args"}` dicts.

Value grammar, as the cloud parser implements it: a double-quoted string
(decoded escapes are `\\"` `\\'` `\\n` `\\t` `\\r` `\\\\`; any other backslash
sequence is kept literally, so non-ASCII text must be emitted raw — never as
`\\uXXXX`), an integer, a float, `true`/`false`/`null`, an array of values in
this same grammar, or an object handed to a JSON parser verbatim. A control
character other than newline, tab, or carriage return cannot be carried in a
DSL string.
"""

from __future__ import annotations

import json
from typing import Any

from .errors import TransportError

_STRING_ESCAPES = {"\\": "\\\\", '"': '\\"', "\n": "\\n", "\t": "\\t", "\r": "\\r"}


def _render_string(value: str, arg_name: str) -> str:
    out = ['"']
    for ch in value:
        escaped = _STRING_ESCAPES.get(ch)
        if escaped is not None:
            out.append(escaped)
        elif ord(ch) < 0x20:
            raise TransportError(
                f"argument {arg_name!r}: control character {ch!r} cannot be carried in the "
                "request DSL"
            )
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def _render_value(value: Any, arg_name: str) -> str:
    if isinstance(value, str):
        return _render_string(value, arg_name)
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return "null"
    if isinstance(value, (int, float)):
        return json.dumps(value)
    if isinstance(value, list):
        return "[" + ", ".join(_render_value(v, arg_name) for v in value) + "]"
    if isinstance(value, dict):
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    raise TransportError(
        f"argument {arg_name!r}: cannot render {type(value).__name__} in the request DSL"
    )


def _render_op(entry: dict[str, Any]) -> str:
    tool = entry.get("tool")
    if not isinstance(tool, str) or not tool:
        raise TransportError(f"op entry has no 'tool' name: {str(entry)[:120]}")
    args = entry.get("args") or {}
    rendered_args = ", ".join(f"{k}={_render_value(v, k)}" for k, v in args.items())
    return f"{tool}({rendered_args})"


def _render_entry(entry: Any) -> str:
    if isinstance(entry, str):
        # Already one op in DSL text; used verbatim.
        return entry
    if isinstance(entry, dict):
        return _render_op(entry)
    raise TransportError(f"cannot render {type(entry).__name__} as a request op")


def render_dsl(ops: str | list[str | dict[str, Any]], *, chained: bool = False) -> str:
    """Render the client's internal `[{"tool", "args"}]` ops form as DSL text.

    A single op renders bare (`whoami()`). More than one renders as a
    parallel batch (`[a(), b()]`), or — when `chained=True` — as a chain
    (`[a() | b()]`) referencing `$prev`; the facade never emits chains, so
    `chained` exists for callers that build one directly.

    A `str` is DSL text already (`whoami()`, `[a(), b()]`, `[a() | b()]`)
    and is returned untouched. Inside a list, a `str` element is one op in
    DSL text and is used verbatim beside the rendered dict entries.
    """
    if isinstance(ops, str):
        return ops
    rendered = [_render_entry(entry) for entry in ops]
    if not rendered:
        return ""
    if len(rendered) == 1 and not chained:
        return rendered[0]
    separator = " | " if chained else ", "
    return "[" + separator.join(rendered) + "]"
