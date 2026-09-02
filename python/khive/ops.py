"""Op construction: every request is sent in the parser's JSON form.

The daemon's request DSL accepts two equivalent encodings; this client only
ever emits the JSON array form `[{"tool": ..., "args": {...}}, ...]`. The
string form exists for humans; generating it from arbitrary user content
would mean escaping quotes and newlines correctly forever, and one missed
case silently changes the op. JSON has no such seam.

Args are pruned of `None` values so optional parameters genuinely stay
absent instead of arriving as JSON nulls.
"""

from __future__ import annotations

import json
from typing import Any


def op(tool: str, **args: Any) -> dict[str, Any]:
    return {"tool": tool, "args": {k: v for k, v in args.items() if v is not None}}


def encode(ops: list[dict[str, Any]]) -> str:
    return json.dumps(ops)
