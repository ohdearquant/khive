"""MCP stdio wrapper for khive-mcp integration tests.

Spawns the khive-mcp binary as a subprocess and frames JSON-RPC 2.0 messages
over stdin/stdout.  Tests must use KhiveMcpSession as a context manager and
never open subprocesses directly.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from types import TracebackType
from typing import Any, Literal, Mapping, Sequence


class KhiveMcpError(RuntimeError):
    """Base class for khive contract client failures."""


class KhiveRpcError(KhiveMcpError):
    """JSON-RPC or MCP boundary error.

    Raised when the server returns a top-level JSON-RPC ``error``, when
    ``tools/call`` returns ``result.isError``, when stdout closes unexpectedly,
    or when a response cannot be parsed as JSON.
    """

    def __init__(
        self,
        message: str,
        *,
        code: int | None = None,
        data: Any | None = None,
        rpc_id: int | None = None,
        stderr_tail: str = "",
    ) -> None:
        parts = [message]
        if rpc_id is not None:
            parts.append(f"(id={rpc_id})")
        if stderr_tail:
            parts.append(f"stderr: {stderr_tail}")
        super().__init__(" ".join(parts))
        self.code = code
        self.message = message
        self.data = data


class KhiveOperationError(KhiveMcpError):
    """Per-operation failure inside a successful request envelope."""

    def __init__(
        self,
        *,
        tool: str,
        message: str,
        index: int,
        envelope: Mapping[str, Any],
    ) -> None:
        super().__init__(f"verb '{tool}' (index {index}) failed: {message}")
        self.tool = tool
        self.message = message
        self.index = index
        self.envelope = envelope


def _find_repo_root(start: Path) -> Path | None:
    """Walk up from *start* looking for .git."""
    current = start.resolve()
    for _ in range(20):
        if (current / ".git").exists():
            return current
        parent = current.parent
        if parent == current:
            return None
        current = parent
    return None


def _resolve_binary(binary: str | Path | None) -> Path:
    if binary is not None:
        return Path(binary)
    env_val = os.environ.get("KHIVE_MCP_BINARY")
    if env_val:
        return Path(env_val)
    repo_root = _find_repo_root(Path(__file__).parent)
    if repo_root is not None:
        release = repo_root / "crates" / "target" / "release" / "khive-mcp"
        if release.exists():
            return release
        debug = repo_root / "crates" / "target" / "debug" / "khive-mcp"
        if debug.exists():
            return debug
    raise FileNotFoundError(
        "khive-mcp binary not found. "
        "Set KHIVE_MCP_BINARY or build with: cd crates && cargo build --release -p khive-mcp"
    )


class KhiveMcpSession:
    """Context-manager wrapper around a khive-mcp stdio subprocess.

    Usage::

        with KhiveMcpSession(packs=("kg",)) as session:
            result = session.verb("create", {"kind": "entity", "entity_kind": "concept",
                                              "name": "Test", "namespace": "ns"})
    """

    def __init__(
        self,
        binary: str | Path | None = None,
        *,
        db: str | Path = ":memory:",
        packs: Sequence[str] = ("kg",),
        namespace: str | None = None,
        no_embed: bool = True,
        log: str = "error",
        env: Mapping[str, str] | None = None,
        timeout: float = 10.0,
        presentation: Literal["agent", "verbose", "human"] = "verbose",
    ) -> None:
        self._binary = _resolve_binary(binary)
        self._db = db
        self._packs = list(packs)
        self._namespace = namespace
        self._no_embed = no_embed
        self._log = log
        self._env = env
        self._timeout = timeout
        self._default_presentation = presentation
        self._id_counter = 0
        self.proc: subprocess.Popen[str] | None = None

    # ------------------------------------------------------------------
    # Context manager
    # ------------------------------------------------------------------

    def __enter__(self) -> "KhiveMcpSession":
        binary = self._binary
        if not binary.exists():
            raise FileNotFoundError(
                f"khive-mcp binary not found at {binary}. "
                "Build with: cd crates && cargo build --release -p khive-mcp"
            )
        cmd = [str(binary), "--db", str(self._db)]
        if self._no_embed:
            cmd.append("--no-embed")
        cmd += ["--log", self._log]
        for pack in self._packs:
            cmd += ["--pack", pack]
        if self._namespace is not None:
            cmd += ["--namespace", self._namespace]

        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env={**os.environ, **(self._env or {})},
        )
        self._do_initialize()
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.close()

    def close(self) -> None:
        if self.proc is None:
            return
        try:
            if self.proc.stdin and not self.proc.stdin.closed:
                self.proc.stdin.close()
            self.proc.wait(timeout=self._timeout)
        except Exception:
            self.proc.kill()
        finally:
            self.proc = None

    # ------------------------------------------------------------------
    # JSON-RPC framing
    # ------------------------------------------------------------------

    def _next_id(self) -> int:
        self._id_counter += 1
        return self._id_counter

    def _send_request(self, method: str, params: Any = None) -> int:
        rpc_id = self._next_id()
        msg: dict[str, Any] = {"jsonrpc": "2.0", "id": rpc_id, "method": method}
        if params is not None:
            msg["params"] = params
        assert self.proc and self.proc.stdin
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        return rpc_id

    def _send_notification(self, method: str, params: Any = None) -> None:
        msg: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        assert self.proc and self.proc.stdin
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def _read_response(self, expected_id: int) -> dict[str, Any]:
        assert self.proc and self.proc.stdout
        while True:
            line = self.proc.stdout.readline()
            if not line:
                stderr_tail = self._read_stderr()
                raise KhiveRpcError(
                    "MCP server closed stdout unexpectedly",
                    rpc_id=expected_id,
                    stderr_tail=stderr_tail,
                )
            try:
                msg = json.loads(line)
            except json.JSONDecodeError as exc:
                raise KhiveRpcError(
                    f"Malformed JSON from server: {line!r}",
                    rpc_id=expected_id,
                ) from exc
            # Skip notifications (no "id" field)
            if "id" not in msg:
                continue
            if msg["id"] == expected_id:
                return msg
            # Unexpected id — skip (shouldn't happen in single-threaded flow)

    def _read_stderr(self) -> str:
        if self.proc is None or self.proc.stderr is None:
            return ""
        try:
            # Non-blocking read of available stderr
            import select as _select

            ready, _, _ = _select.select([self.proc.stderr], [], [], 0.1)
            if ready:
                return self.proc.stderr.read(4096)
        except Exception:
            pass
        return ""

    # ------------------------------------------------------------------
    # MCP handshake
    # ------------------------------------------------------------------

    def _do_initialize(self) -> None:
        assert self.proc is not None
        if self.proc.poll() is not None:
            stderr_tail = ""
            if self.proc.stderr:
                stderr_tail = self.proc.stderr.read()
            raise KhiveRpcError(
                "khive-mcp process exited before initialize",
                stderr_tail=stderr_tail,
            )
        rpc_id = self._send_request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "khive-contract", "version": "0.1.0"},
            },
        )
        resp = self._read_response(rpc_id)
        if "error" in resp:
            raise KhiveRpcError(
                resp["error"].get("message", "initialize failed"),
                code=resp["error"].get("code"),
                data=resp["error"].get("data"),
                rpc_id=rpc_id,
            )
        server_name = resp.get("result", {}).get("serverInfo", {}).get("name", "")
        if server_name != "khive-mcp":
            raise KhiveRpcError(
                f"Unexpected serverInfo.name: {server_name!r}",
                rpc_id=rpc_id,
            )
        self._send_notification("notifications/initialized")

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def request(
        self,
        ops: str,
        *,
        presentation: Literal["agent", "verbose", "human"] | None = None,
    ) -> dict[str, Any]:
        """Send a raw ops string to the `request` tool and return the parsed envelope."""
        pres = presentation or self._default_presentation
        rpc_id = self._send_request(
            "tools/call",
            {
                "name": "request",
                "arguments": {"ops": ops, "presentation": pres},
            },
        )
        resp = self._read_response(rpc_id)
        if "error" in resp:
            err = resp["error"]
            raise KhiveRpcError(
                err.get("message", "JSON-RPC error"),
                code=err.get("code"),
                data=err.get("data"),
                rpc_id=rpc_id,
            )
        result = resp.get("result", {})
        if result.get("isError"):
            content = result.get("content", [])
            text = content[0]["text"] if content else ""
            raise KhiveRpcError(
                text or "tools/call returned isError",
                code=-32603,
                rpc_id=rpc_id,
            )
        content = result.get("content", [])
        text = content[0]["text"] if content else ""
        if not text:
            raise KhiveRpcError("Empty content in tools/call response", rpc_id=rpc_id)
        try:
            return json.loads(text)
        except json.JSONDecodeError as exc:
            raise KhiveRpcError(
                f"Could not parse tools/call response as JSON: {text!r}",
                rpc_id=rpc_id,
            ) from exc

    def request_batch(
        self,
        ops_list: Sequence[Mapping[str, Any]],
        *,
        presentation: Literal["agent", "verbose", "human"] | None = None,
    ) -> dict[str, Any]:
        """Send a list of op dicts as a JSON-form batch and return the raw envelope."""
        for i, op in enumerate(ops_list):
            if not isinstance(op.get("tool"), str):
                raise ValueError(f"ops_list[{i}] missing 'tool' string: {op!r}")
            if not isinstance(op.get("args"), Mapping):
                raise ValueError(f"ops_list[{i}] missing 'args' mapping: {op!r}")
        serialized = json.dumps(list(ops_list))
        return self.request(serialized, presentation=presentation)

    def verb(
        self,
        name: str,
        args: Mapping[str, Any] | None = None,
        *,
        presentation: Literal["agent", "verbose", "human"] | None = None,
    ) -> Any:
        """Call a single verb and return its result, raising on per-op failure."""
        envelope = self.request_batch(
            [{"tool": name, "args": dict(args or {})}],
            presentation=presentation,
        )
        results = envelope.get("results") or []
        if not results:
            raise KhiveRpcError(f"empty results from verb '{name}'")
        first = results[0]
        if not first.get("ok", False):
            raise KhiveOperationError(
                tool=first.get("tool", name),
                message=first.get("error", "<no error string>"),
                index=0,
                envelope=envelope,
            )
        return first.get("result")

    def tools_list(self) -> list[dict[str, Any]]:
        """Call tools/list and return the list of tool descriptors."""
        rpc_id = self._send_request("tools/list", {})
        resp = self._read_response(rpc_id)
        if "error" in resp:
            err = resp["error"]
            raise KhiveRpcError(
                err.get("message", "tools/list failed"),
                code=err.get("code"),
                rpc_id=rpc_id,
            )
        return resp.get("result", {}).get("tools", [])
