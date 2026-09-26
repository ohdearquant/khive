"""Command-line access to khive-cloud's request endpoint."""

from __future__ import annotations

import argparse
import json
import os
import sys
from typing import Any

from .cloud import _cloud_configuration
from .errors import AuthError, RateLimited, TransportError
from .transport import HttpTransport


class _Parser(argparse.ArgumentParser):
    def error(self, message: str) -> None:
        raise ValueError(message)


def _redact(text: str, secrets: list[str]) -> str:
    for secret in sorted(set(secrets), key=len, reverse=True):
        if secret:
            text = text.replace(secret, "[REDACTED]")
    return text


def _redact_result(value: Any, secrets: list[str]) -> Any:
    if isinstance(value, str):
        return _redact(value, secrets)
    if isinstance(value, list):
        return [_redact_result(item, secrets) for item in value]
    if isinstance(value, dict):
        return {key: _redact_result(item, secrets) for key, item in value.items()}
    return value


def _contains_secret_key(value: Any, secrets: list[str]) -> bool:
    if isinstance(value, list):
        return any(_contains_secret_key(item, secrets) for item in value)
    if isinstance(value, dict):
        for key, item in value.items():
            if isinstance(key, str) and any(secret and secret in key for secret in secrets):
                return True
            if _contains_secret_key(item, secrets):
                return True
    return False


def _error(exc: Exception, code: int, secrets: list[str]) -> int:
    escaped = [
        json.dumps(secret, ensure_ascii=ascii_only)[1:-1]
        for secret in secrets
        for ascii_only in (False, True)
    ]
    message = _redact(str(exc), secrets + escaped)
    message = " ".join(message.splitlines())
    print(f"{type(exc).__name__}: {message}", file=sys.stderr)
    return code


def main(argv: list[str] | None = None) -> int:
    """Send one request and return a documented process exit code."""
    args = list(sys.argv[1:] if argv is None else argv)
    secrets = [os.environ.get("KHIVE_CLOUD_API_KEY", "")]
    # Collect credentials before parsing, so usage errors cannot echo their value.
    for index, arg in enumerate(args):
        if arg == "--api-key" and index + 1 < len(args):
            secrets.append(args[index + 1])
        elif arg.startswith("--api-key="):
            secrets.append(arg.partition("=")[2])
    parser = _Parser(prog="khive-cloud", allow_abbrev=False)
    parser.add_argument("--url", help="base URL (otherwise KHIVE_CLOUD_URL)")
    parser.add_argument("--allow-insecure", action="store_true")
    parser.add_argument("command", choices=("whoami", "exec"))
    parser.add_argument("ops", nargs="?", help="request DSL for exec, sent verbatim")
    try:
        options = parser.parse_args(args)
        if options.command == "exec" and options.ops is None:
            raise ValueError("exec requires an ops string")
        if options.command == "whoami" and options.ops is not None:
            raise ValueError("whoami takes no ops argument")
        try:
            url, api_key = _cloud_configuration(options.url, None)
        except ValueError as exc:
            raise ValueError(
                str(exc)
                .replace("base_url", "--url/base_url")
                .replace("api_key or KHIVE_CLOUD_API_KEY", "KHIVE_CLOUD_API_KEY")
            ) from None
        with HttpTransport(url, api_key, allow_insecure=options.allow_insecure) as transport:
            ops = "whoami()" if options.command == "whoami" else options.ops
            result = transport.send_dsl(ops, timeout=30.0)["result"]
        redacted_result = _redact_result(result, secrets)
        if _contains_secret_key(redacted_result, secrets):
            print(
                "Response carried a credential in a field name; result withheld.",
                file=sys.stderr,
            )
            return 6
        # ASCII escapes survive non-UTF-8 stdout encodings after the request
        # has already been dispatched; a print failure must not look like a
        # pre-dispatch usage/configuration error that invites a write retry.
        rendered = json.dumps(redacted_result, ensure_ascii=True)
        print(rendered)
        return 0
    except SystemExit as exc:
        return int(exc.code or 0)
    except AuthError as exc:
        return _error(exc, 3, secrets)
    except RateLimited as exc:
        return _error(exc, 4, secrets)
    except TransportError as exc:
        return _error(exc, 5, secrets)
    except (ValueError, ImportError) as exc:
        return _error(exc, 2, secrets)


if __name__ == "__main__":
    raise SystemExit(main())
