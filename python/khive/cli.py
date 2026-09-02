"""`khive-cloud` — command-line client for a khive-cloud deployment.

Talks over `HttpTransport` (REST) for `whoami`/`exec`/`health`, and the MCP
helpers for `tools`. Exit codes: 0 ok, 1 server/op error, 2 usage.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections.abc import Sequence
from typing import Any

from . import mcp as _mcp
from .errors import HttpError, KhiveError, OperationError
from .ops import encode, op
from .transport import HttpTransport

URL_ENV = "KHIVE_CLOUD_URL"
KEY_ENV = "KHIVE_CLOUD_API_KEY"


def _require_url(args: argparse.Namespace) -> str:
    base_url = args.url or os.environ.get(URL_ENV)
    if not base_url:
        print(f"error: no base URL given — pass --url or set {URL_ENV}", file=sys.stderr)
        raise SystemExit(2)
    return base_url


def _require_api_key(args: argparse.Namespace) -> str:
    api_key = args.api_key or os.environ.get(KEY_ENV)
    if not api_key:
        print(f"error: no API key given — pass --api-key or set {KEY_ENV}", file=sys.stderr)
        raise SystemExit(2)
    return api_key


def _print_json(value: Any) -> None:
    print(json.dumps(value, indent=2, default=str))


def _first_result(envelope: dict[str, Any]) -> Any:
    entry = (envelope.get("results") or [{}])[0]
    if entry.get("aborted"):
        raise OperationError(entry.get("tool", "?"), "aborted")
    if not entry.get("ok"):
        raise OperationError(entry.get("tool", "?"), entry.get("error") or "unknown error")
    return entry.get("result")


def _cmd_whoami(args: argparse.Namespace) -> int:
    base_url = _require_url(args)
    api_key = _require_api_key(args)
    transport = HttpTransport(base_url, api_key)
    try:
        response = transport.round_trip({"ops": encode([op("whoami")])}, timeout=30.0)
        _print_json(_first_result(response["result"]))
    finally:
        transport.close()
    return 0


def _cmd_exec(args: argparse.Namespace) -> int:
    base_url = _require_url(args)
    api_key = _require_api_key(args)
    transport = HttpTransport(base_url, api_key)
    try:
        response = transport.send_dsl(args.ops, timeout=30.0)
        envelope = response["result"]
        _print_json(envelope)
        summary = envelope.get("summary", {})
        if summary.get("failed") or summary.get("aborted"):
            return 1
        return 0
    finally:
        transport.close()


def _cmd_tools(args: argparse.Namespace) -> int:
    base_url = _require_url(args)
    api_key = _require_api_key(args)
    for name in _mcp.mcp_list_tools(base_url, api_key):
        print(name)
    return 0


def _cmd_health(args: argparse.Namespace) -> int:
    base_url = _require_url(args)
    transport = HttpTransport(base_url, "unused")
    try:
        response = transport.round_trip({"ops": "", "metrics_only": True}, timeout=10.0)
        _print_json(response["metrics"])
    finally:
        transport.close()
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="khive-cloud", description="khive-cloud CLI client")
    parser.add_argument("--url", default=None, help=f"khive-cloud base URL (env {URL_ENV})")
    parser.add_argument("--api-key", default=None, help=f"API key (env {KEY_ENV})")
    sub = parser.add_subparsers(dest="command", required=True)

    p_whoami = sub.add_parser("whoami", help="print the caller's resolved identity")
    p_whoami.set_defaults(func=_cmd_whoami)

    p_exec = sub.add_parser("exec", help="run a request DSL string")
    p_exec.add_argument(
        "ops", help="a request DSL string, e.g. 'stats()' or '[whoami(), stats()]'"
    )
    p_exec.set_defaults(func=_cmd_exec)

    p_tools = sub.add_parser("tools", help="list MCP tool names")
    p_tools.set_defaults(func=_cmd_tools)

    p_health = sub.add_parser("health", help="check server health (no auth required)")
    p_health.set_defaults(func=_cmd_health)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except HttpError as exc:
        print(f"error: {type(exc).__name__}: HTTP {exc.status} {exc.body}", file=sys.stderr)
        return 1
    except KhiveError as exc:
        print(f"error: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
