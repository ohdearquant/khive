"""Renders the client's internal ops form as request DSL text.

`render_dsl` takes the decoded `[{"tool", "args"}, ...]` ops-array form this
client builds internally and renders it as one request DSL string — the
wire shape the request parser (`crates/khive-request`) accepts. DSL text a
caller already holds passes through untouched, whether it is the whole
`ops` value or one element beside `{"tool", "args"}` dicts. This module is a
self-contained renderer and envelope-shape-agnostic: it does not call, wire
into, or assume the existence of any particular transport — a transport
that wants to send DSL text over the wire calls `render_dsl` explicitly.

This module's own emitted subset of the value grammar: a double-quoted
string using only the escapes `\\"` `\\n` `\\t` `\\r` `\\\\` (this renderer
never emits `\\uXXXX`; non-ASCII text is emitted raw), an integer within
`[-2**63, 2**64 - 1]` (see `_MIN_SIGNED_64`/`_MAX_UNSIGNED_64` below — an
integer outside that range has no exact representation in the parser's
decoded value and is refused rather than silently reinterpreted), a finite
float (`NaN`/`Infinity` have no representation in the grammar),
`true`/`false`/`null`, an array of values in this same grammar, or an
object handed to a JSON parser verbatim. A control character other than
newline, tab, or carriage return cannot be carried in a DSL string this
module renders.

The parser's accepted grammar is broader than what this module emits: it
decodes any JSON string escape, including `\\uXXXX`, `\\/`, `\\b`, and `\\f`;
it has no `\\'` escape, so that sequence — and any other backslash sequence
this list does not name — is rejected, not decoded literally. This module
never needs to emit those additional forms itself, but raw DSL text a
caller passes through verbatim (see below) may use any of them; the parser
is what validates that text, not this module.

A string value equal to `$prev`, or starting with `$prev.` or `$prev[`, is a
chain back-reference in this grammar (at any depth — a scalar, an array
element, or an object value). A caller's own string that merely starts with
that text is rendered with one extra leading backslash so it decodes back to
the literal text instead of being resolved as a reference; the same rule
applies wherever such a string appears, nested or not.

A caller's own string with exactly one leading backslash followed by that
same `$prev`-shaped text has no representation in this grammar: rendered
plainly it decodes back to the bare `$prev`-shaped text (the parser's escape
rule strips exactly one leading backslash before matching), and rendered
with this module's own escape it decodes to two backslashes instead of one.
Such a value raises `TransportError` rather than being silently corrupted.
Two or more leading backslashes are unaffected — they render unchanged and
round-trip exactly.

Raw DSL text a caller passes through verbatim — the whole `ops` value as a
`str`, or a `str` element beside `{"tool", "args"}` dicts inside a list — is
returned unparsed. This module rejects it only for being empty or
whitespace-only, and for exceeding the raw byte cap (see `MAX_OPS_INPUT_LEN`
below); it does not otherwise validate that text, because it did not
construct it and has no way to check it against the parser's full grammar.
Everything else about that text — a malformed `$prev` reference, a
DuplicateArg, a syntax error — is the request parser's to reject when the
request actually reaches it; the client does not parse raw DSL text.
"""

from __future__ import annotations

import json
import math
import re
from typing import Any

from .errors import TransportError

_STRING_ESCAPES = {"\\": "\\\\", '"': '\\"', "\n": "\\n", "\t": "\\t", "\r": "\\r"}

# Mirrors `parser_impl.rs::Parser::parse_identifier`: an ASCII letter or
# underscore, then any number of ASCII alphanumerics/underscores.
_IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")

# `khive_types::pack::RESERVED_ENVELOPE_ARGS` — argument names `dispatch.rs`'s
# `reject_reserved_args` refuses on every op because they belong to the
# envelope, not a verb's own arguments.
_RESERVED_ENVELOPE_ARGS = ("presentation", "presentation_per_op")

# Mirrors `crates/khive-request/src/types.rs::MAX_OPS` — the parser rejects
# a 101st chained, batched, or JSON-form operation.
MAX_OPS = 100

# Mirrors `crates/khive-request/src/types.rs::MAX_OPS_INPUT_LEN` — the
# parser rejects a trimmed raw request longer than this many UTF-8 bytes,
# checked before any parsing begins.
MAX_OPS_INPUT_LEN = 1024 * 1024

# Mirrors `crates/khive-request/src/types.rs::NESTING_DEPTH_LIMIT` — the
# function-form parser's `Parser::enter_container` rejects array/object
# argument nesting past this depth (depth 64 is accepted, 65 is refused).
NESTING_DEPTH_LIMIT = 64

# The exact range `serde_json::Value::Number` (default features, no
# `arbitrary_precision`; `crates/Cargo.toml`) can hold as an integer: i64 on
# the negative side, u64 on the positive side. A JSON integer literal
# outside this range still decodes — as `f64`, not as the integer the
# caller wrote — so this module refuses it rather than silently changing
# its type on the wire (see `DSL_WIRE_CONTRACT.md`'s integer-range row).
_MIN_SIGNED_64 = -(2**63)
_MAX_UNSIGNED_64 = 2**64 - 1


def _validate_tool_name(tool: str) -> None:
    """Mirrors `parser_impl.rs::Parser::parse_op`: a tool is one identifier,
    or two identifiers joined by a single `.` (`pack.verb`) — a third
    segment is unsupported verb nesting."""
    segments = tool.split(".")
    if len(segments) > 2 or not all(_IDENT_RE.fullmatch(seg) for seg in segments):
        raise TransportError(
            f"invalid tool name {tool!r}: must be one or two identifier segments joined by '.'"
        )


def _validate_arg_name(name: str, tool: str) -> None:
    if not _IDENT_RE.fullmatch(name):
        raise TransportError(f"op {tool!r}: invalid argument name {name!r}")
    if name in _RESERVED_ENVELOPE_ARGS:
        raise TransportError(
            f"op {tool!r}: argument name {name!r} is reserved for the request envelope"
        )


def _needs_prev_escape(value: str) -> bool:
    """Whether `value` is shaped like a `$prev` chain reference and must be
    escaped to survive as a literal (see `parser_impl.rs::string_as_prev_ref`
    — matched here on the same three prefixes, at any nesting depth)."""
    return value == "$prev" or value.startswith(("$prev.", "$prev["))


def _is_unrepresentable_prev_literal(value: str) -> bool:
    """Whether `value` has exactly one leading backslash followed by a
    `$prev`-shaped string — the one literal this grammar cannot carry (see
    the module docstring): the parser's escape rule strips exactly one
    leading backslash before matching `$prev`/`$prev.`/`$prev[`, so rendering
    it plainly loses the backslash and rendering it with this module's own
    escape produces two backslashes instead of one. Two or more leading
    backslashes are unaffected."""
    return value.startswith("\\") and not value.startswith("\\\\") and _needs_prev_escape(value[1:])


def _reject_unrepresentable_prev_literal(value: str, arg_name: str) -> None:
    if _is_unrepresentable_prev_literal(value):
        raise TransportError(
            f"argument {arg_name!r}: a string with exactly one leading backslash "
            "followed by a '$prev'-shaped reference has no representation in the "
            "request DSL"
        )


def _check_container_depth(depth: int, arg_name: str) -> None:
    """Mirrors `parser_impl.rs::Parser::enter_container`: `depth` counts one
    increment per array/object container entered (the top-level array or
    object bound to an argument is depth 1); depth 64 is accepted, 65 is
    refused with `NestingTooDeep`."""
    if depth > NESTING_DEPTH_LIMIT:
        raise TransportError(
            f"argument {arg_name!r}: container nesting depth {depth} exceeds the "
            f"request parser's max of {NESTING_DEPTH_LIMIT}"
        )


def _check_integer_range(value: int, arg_name: str) -> None:
    """Mirrors `serde_json::Value`'s exact integer range (see
    `_MIN_SIGNED_64`/`_MAX_UNSIGNED_64` above): outside it, the parser's
    decoded value is `f64`, not the integer the caller wrote, so this module
    refuses the value rather than changing its type on the wire."""
    if value < _MIN_SIGNED_64 or value > _MAX_UNSIGNED_64:
        raise TransportError(
            f"argument {arg_name!r}: integer {value} is outside "
            f"[{_MIN_SIGNED_64}, {_MAX_UNSIGNED_64}], the exact range the request "
            "parser's JSON decoder can represent as an integer"
        )


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
    _reject_unrepresentable_prev_literal(value, arg_name)
    if _needs_prev_escape(value):
        value = "\\" + value
    return _render_string(value, arg_name)


def _prep_for_json(value: Any, arg_name: str, depth: int = 0) -> Any:
    """Walks an object-argument value tree before handing it to `json.dumps`,
    applying the same `$prev`-literal escape `_render_string_value` applies
    outside objects — the cloud parser resolves `$prev` references at any
    depth of an object argument, not just at its top level. `depth` is the
    container depth already entered before `value` itself (see
    `_check_container_depth`); entering `value`, when it is itself a list or
    dict, is one more increment, checked here before recursing."""
    if isinstance(value, str):
        _reject_unrepresentable_prev_literal(value, arg_name)
        return "\\" + value if _needs_prev_escape(value) else value
    if isinstance(value, list):
        child_depth = depth + 1
        _check_container_depth(child_depth, arg_name)
        return [_prep_for_json(v, arg_name, child_depth) for v in value]
    if isinstance(value, dict):
        child_depth = depth + 1
        _check_container_depth(child_depth, arg_name)
        prepped: dict[str, Any] = {}
        for k, v in value.items():
            if not isinstance(k, str):
                raise TransportError(f"argument {arg_name!r}: object key {k!r} must be a string")
            prepped[k] = _prep_for_json(v, arg_name, child_depth)
        return prepped
    if value is None or isinstance(value, bool):
        return value
    if isinstance(value, int):
        _check_integer_range(value, arg_name)
        return value
    if isinstance(value, float):
        return value
    raise TransportError(
        f"argument {arg_name!r}: cannot render {type(value).__name__} in the request DSL"
    )


def _render_value(value: Any, arg_name: str, depth: int = 0) -> str:
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
    if isinstance(value, int):
        _check_integer_range(value, arg_name)
        try:
            return json.dumps(value)
        except (TypeError, ValueError, OverflowError) as exc:
            raise TransportError(f"argument {arg_name!r}: {exc}") from exc
    if isinstance(value, float):
        try:
            return json.dumps(value)
        except (TypeError, ValueError, OverflowError) as exc:
            raise TransportError(f"argument {arg_name!r}: {exc}") from exc
    if isinstance(value, list):
        child_depth = depth + 1
        _check_container_depth(child_depth, arg_name)
        return "[" + ", ".join(_render_value(v, arg_name, child_depth) for v in value) + "]"
    if isinstance(value, dict):
        prepped = _prep_for_json(value, arg_name, depth)
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
    _validate_tool_name(tool)
    if "args" in entry:
        args = entry["args"]
        if not isinstance(args, dict):
            raise TransportError(f"op {tool!r} args must be an object, got {type(args).__name__}")
    else:
        args = {}
    for key in args:
        if not isinstance(key, str):
            raise TransportError(f"op {tool!r}: argument name {key!r} must be a string")
        _validate_arg_name(key, tool)
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
        _check_raw_text(ops)
        return ops
    if not ops:
        raise TransportError("cannot render an empty operation list")
    if len(ops) > MAX_OPS:
        raise TransportError(
            f"cannot render {len(ops)} operations: the request parser accepts at most "
            f"{MAX_OPS} in a chain, batch, or JSON-form request"
        )
    rendered = [_render_entry(entry) for entry in ops]
    if len(rendered) == 1:
        text = rendered[0]
    elif chained:
        text = " | ".join(rendered)
    else:
        text = "[" + ", ".join(rendered) + "]"
    _check_raw_byte_cap(text)
    return text


def _check_raw_byte_cap(text: str) -> None:
    """Mirrors `dispatch.rs::parse_request`'s `MAX_OPS_INPUT_LEN` check: the
    parser trims the request text with `str::trim()` before measuring its
    UTF-8 byte length, so this checks the trimmed length too — without
    altering the text `render_dsl` actually returns."""
    trimmed_len = len(text.strip().encode("utf-8"))
    if trimmed_len > MAX_OPS_INPUT_LEN:
        raise TransportError(
            f"rendered request is {trimmed_len} bytes; the request parser accepts at "
            f"most {MAX_OPS_INPUT_LEN} bytes"
        )


def _check_raw_text(text: str) -> None:
    """Mirrors `dispatch.rs::parse_request`'s `Empty` check (P1) and the
    `MAX_OPS_INPUT_LEN` check (P2), applied to DSL text a caller passes
    through verbatim — the only two properties this module can check
    without parsing text it did not itself construct (see the module
    docstring)."""
    if not text.strip():
        raise TransportError("cannot render an empty (or whitespace-only) request")
    _check_raw_byte_cap(text)
