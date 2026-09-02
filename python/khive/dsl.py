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
`\\uXXXX`), an integer, a finite float (`NaN`/`Infinity` have no
representation in the grammar), `true`/`false`/`null`, an array of values in
this same grammar, or an object handed to a JSON parser verbatim. A control
character other than newline, tab, or carriage return cannot be carried in a
DSL string.

A string value equal to `$prev`, or starting with `$prev.` or `$prev[`, is a
chain back-reference in this grammar (at any depth — a scalar, an array
element, or an object value). A caller's own string that merely starts with
that text is rendered with one extra leading backslash so it decodes back to
the literal text instead of being resolved as a reference; the same rule
applies wherever such a string appears, nested or not.
"""

from __future__ import annotations

import json
import math
from typing import Any

from .errors import TransportError

_STRING_ESCAPES = {"\\": "\\\\", '"': '\\"', "\n": "\\n", "\t": "\\t", "\r": "\\r"}


def _needs_prev_escape(value: str) -> bool:
    """Whether `value` is shaped like a `$prev` chain reference and must be
    escaped to survive as a literal (see `parser_impl.rs::string_as_prev_ref`
    — matched here on the same three prefixes, at any nesting depth)."""
    return value == "$prev" or value.startswith(("$prev.", "$prev["))


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


def _render_string_value(value: str, arg_name: str) -> str:
    """Renders a string in argument-VALUE position, applying the `$prev`
    literal escape (never applied to object keys, which are never resolved
    as references)."""
    if _needs_prev_escape(value):
        value = "\\" + value
    return _render_string(value, arg_name)


def _prep_for_json(value: Any, arg_name: str) -> Any:
    """Walks an object-argument value tree before handing it to `json.dumps`,
    applying the same `$prev`-literal escape `_render_string_value` applies
    outside objects — the cloud parser resolves `$prev` references at any
    depth of an object argument, not just at its top level."""
    if isinstance(value, str):
        return "\\" + value if _needs_prev_escape(value) else value
    if isinstance(value, list):
        return [_prep_for_json(v, arg_name) for v in value]
    if isinstance(value, dict):
        prepped: dict[str, Any] = {}
        for k, v in value.items():
            if not isinstance(k, str):
                raise TransportError(f"argument {arg_name!r}: object key {k!r} must be a string")
            prepped[k] = _prep_for_json(v, arg_name)
        return prepped
    if value is None or isinstance(value, (bool, int, float)):
        return value
    raise TransportError(
        f"argument {arg_name!r}: cannot render {type(value).__name__} in the request DSL"
    )


def _render_value(value: Any, arg_name: str) -> str:
    if isinstance(value, str):
        return _render_string_value(value, arg_name)
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return "null"
    if isinstance(value, float) and not math.isfinite(value):
        raise TransportError(
            f"argument {arg_name!r}: non-finite float {value!r} has no representation in "
            "the request DSL"
        )
    if isinstance(value, (int, float)):
        try:
            return json.dumps(value)
        except (TypeError, ValueError, OverflowError) as exc:
            raise TransportError(f"argument {arg_name!r}: {exc}") from exc
    if isinstance(value, list):
        return "[" + ", ".join(_render_value(v, arg_name) for v in value) + "]"
    if isinstance(value, dict):
        prepped = _prep_for_json(value, arg_name)
        try:
            return json.dumps(prepped, ensure_ascii=False, separators=(",", ":"), allow_nan=False)
        except (TypeError, ValueError, OverflowError) as exc:
            raise TransportError(f"argument {arg_name!r}: {exc}") from exc
    raise TransportError(
        f"argument {arg_name!r}: cannot render {type(value).__name__} in the request DSL"
    )


def _render_op(entry: dict[str, Any]) -> str:
    tool = entry.get("tool")
    if not isinstance(tool, str) or not tool:
        raise TransportError(f"op entry has no 'tool' name: {str(entry)[:120]}")
    args = entry.get("args") or {}
    if not isinstance(args, dict):
        raise TransportError(f"op {tool!r} args must be an object, got {type(args).__name__}")
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

    A single op renders bare (`whoami()`), whether or not `chained` is set.
    More than one renders as a parallel batch (`[a(), b()]`), or — when
    `chained=True` — as a chain (`a() | b()`) referencing `$prev`; a chain is
    top-level syntax with no enclosing brackets (`[a() | b()]` is a mixed-
    separator batch and the cloud parser rejects it). The facade never emits
    chains, so `chained` exists for callers that build one directly.

    A `str` is DSL text already (`whoami()`, `[a(), b()]`, `a() | b()`) and
    is returned untouched. Inside a list, a `str` element is one op in DSL
    text and is used verbatim beside the rendered dict entries.
    """
    if isinstance(ops, str):
        return ops
    rendered = [_render_entry(entry) for entry in ops]
    if not rendered:
        return ""
    if len(rendered) == 1:
        return rendered[0]
    if chained:
        return " | ".join(rendered)
    return "[" + ", ".join(rendered) + "]"
