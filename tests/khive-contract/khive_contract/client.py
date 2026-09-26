"""MCP stdio wrapper for khive-mcp integration tests.

Spawns the khive-mcp binary as a subprocess and frames JSON-RPC 2.0 messages
over stdin/stdout.  Tests must use KhiveMcpSession as a context manager and
never open subprocesses directly.
"""

from __future__ import annotations

import json
import importlib.util
import os
import subprocess
from pathlib import Path
from types import TracebackType
from typing import Any, Literal, Mapping, Sequence


class KhiveMcpError(RuntimeError):
    """Base class for khive contract client failures."""


class KhiveRpcError(KhiveMcpError):
    """JSON-RPC or MCP boundary error.

    Raised on a top-level JSON-RPC ``error``, an ``isError`` tool result
    without a request envelope, unexpected stdout closure, or invalid JSON.
    A valid all-failed request envelope remains readable with ``isError`` set.
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
        detail: Mapping[str, Any] | None = None,
    ) -> None:
        super().__init__(f"verb '{tool}' (index {index}) failed: {message}")
        self.tool = tool
        self.message = message
        self.index = index
        self.envelope = envelope
        # The structured fields beside the message (kind, domain_disposition,
        # domain_result) when the server sent an object rather than a string.
        self.detail: Mapping[str, Any] = detail or {}


def error_text(op_result: Mapping[str, Any]) -> str:
    """Return the human-readable text of a per-op error.

    A per-op error is an object carrying `message` alongside its disposition
    fields; the daemon text protocol still sends a bare string. Callers that
    want to assert on the wording read it through here so both shapes work,
    and so an assertion failure quotes the text rather than a dict repr.
    """
    err = op_result.get("error")
    if isinstance(err, Mapping):
        return str(err.get("message", ""))
    return str(err or "")


def error_detail(op_result: Mapping[str, Any]) -> Mapping[str, Any]:
    """Return the structured fields of a per-op error, empty for the text form."""
    err = op_result.get("error")
    return err if isinstance(err, Mapping) else {}


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


def _load_shared_resolver(repo_root: Path):
    """Import tests/kkernel_binary.py by path; this package has no import path to it."""
    module_path = repo_root / "tests" / "kkernel_binary.py"
    spec = importlib.util.spec_from_file_location("kkernel_binary", module_path)
    if spec is None or spec.loader is None:
        raise FileNotFoundError(f"shared kkernel resolver not found at {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _resolve_binary(binary: str | Path | None) -> Path:
    if binary is not None:
        return Path(binary)
    # KKERNEL_BINARY is canonical after the binary unification; KHIVE_MCP_BINARY
    # is retained as a deprecated alias.
    env_val = os.environ.get("KKERNEL_BINARY") or os.environ.get("KHIVE_MCP_BINARY")
    if env_val:
        return Path(env_val)
    repo_root = _find_repo_root(Path(__file__).parent)
    if repo_root is not None:
        # One resolution rule for every harness: tests/kkernel_binary.py mirrors
        # scripts/ci.sh, so a custom CARGO_TARGET_DIR selects the same binary
        # here as in the smoke harnesses.
        release = Path(_load_shared_resolver(repo_root).resolve_binary_path())
        if release.exists():
            return release
        debug = release.parent.parent / "debug" / "kkernel"
        if debug.exists():
            return debug
    raise FileNotFoundError(
        "kkernel binary not found. "
        "Set KKERNEL_BINARY or build with: cd crates && cargo build --release -p kkernel"
    )


def _load_shared_harness():
    repo_root = _find_repo_root(Path(__file__).parent)
    if repo_root is None:
        raise FileNotFoundError("contract harness repository root not found")
    path = repo_root / "tests/contract_harness.py"
    spec = importlib.util.spec_from_file_location("contract_harness", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


_harness = _load_shared_harness()
OwnedContractStore = _harness.OwnedContractStore


class KhiveMcpSession:
    """Context-manager wrapper around a khive-mcp stdio subprocess.

    Usage::

        with KhiveMcpSession(packs=("kg",)) as session:
            result = session.verb("create", {"kind": "entity", "entity_kind": "concept",
                                              "name": "Test"})
    """

    def __init__(
        self,
        binary: str | Path | None = None,
        *,
        db: str | Path | None = None,
        config: str | Path | None = None,
        store: OwnedContractStore | None = None,
        packs: Sequence[str] = ("kg",),
        namespace: str | None = None,
        no_embed: bool = True,
        log: str = "error",
        env: Mapping[str, str] | None = None,
        timeout: float = 10.0,
        reap_timeout: float | None = None,
        presentation: Literal["agent", "verbose", "human"] = "verbose",
    ) -> None:
        if timeout <= 0:
            raise ValueError("timeout must be positive")
        if reap_timeout is not None and reap_timeout <= 0:
            raise ValueError("reap_timeout must be positive")
        self._binary = _resolve_binary(binary).resolve()
        self._db = db if db in (None, ":memory:") else Path(db).resolve()
        self._config = Path(config).resolve() if config is not None else None
        self._store = store
        self._owns_store = store is None
        self._transport = None
        self._packs = list(packs)
        self._namespace = namespace
        self._no_embed = no_embed
        self._log = log
        self._env = env
        self._timeout = timeout
        # Defaults to the exchange budget, so a caller that does not care keeps
        # the behaviour it had. See JsonRpcTransport for why they are separate.
        self._reap_timeout = reap_timeout
        self._default_presentation = presentation
        self._id_counter = 0
        self.proc: subprocess.Popen | None = None

    # ------------------------------------------------------------------
    # Context manager
    # ------------------------------------------------------------------

    def __enter__(self) -> "KhiveMcpSession":
        binary = self._binary
        if not binary.exists():
            raise FileNotFoundError(
                f"kkernel binary not found at {binary}. "
                "Build with: cd crates && cargo build --release -p kkernel"
            )
        if self._store is None:
            self._store = OwnedContractStore()
        cmd = [str(binary), "mcp"]
        db = self._db if self._db is not None else (None if self._config else self._store.db)
        if db is not None:
            cmd += ["--db", str(db)]
        config = self._config or self._store.config
        cmd += ["--config", str(config)]
        if self._no_embed:
            cmd.append("--no-embed")
        cmd += ["--log", self._log]
        for pack in self._packs:
            cmd += ["--pack", pack]
        if self._namespace is not None:
            cmd += ["--namespace", self._namespace]
        source_env = dict(os.environ)
        source_env.update(self._env or {})
        child_env = self._store.child_env(source_env)
        child_env["KHIVE_CONFIG"] = str(config)
        try:
            self.proc = subprocess.Popen(
                cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                bufsize=0, env=child_env, cwd=self._store.root,
            )
            self._transport = _harness.attach_transport(
                self.proc, self._timeout, self._reap_timeout
            )
            self._do_initialize()
            return self
        except BaseException:
            self.close(force=True)
            raise

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.close()

    def close(self, *, force: bool = False) -> None:
        if self.proc is not None:
            if self._transport is not None:
                self._transport.close(force=force)
            else:
                _harness.reap_child(
                    self.proc,
                    timeout=self._timeout if self._reap_timeout is None else self._reap_timeout,
                    force=force,
                )
            self.proc = None
            self._transport = None
        if self._owns_store and self._store is not None:
            self._store.close()
            self._store = None

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
        assert self._transport is not None
        self._transport.send(msg)
        return rpc_id

    def _send_notification(self, method: str, params: Any = None) -> None:
        msg: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        assert self._transport is not None
        self._transport.send(msg)

    def _read_response(self, expected_id: int) -> dict[str, Any]:
        assert self._transport is not None
        try:
            return self._transport.response(expected_id)
        except (TimeoutError, EOFError, OSError, ValueError) as exc:
            stderr_tail = self._read_stderr()
            self.close(force=True)
            raise KhiveRpcError(str(exc), rpc_id=expected_id, stderr_tail=stderr_tail) from exc

    def _read_stderr(self) -> str:
        if self._transport is None:
            return ""
        return self._transport.stderr.decode(errors="replace").strip()

    # ------------------------------------------------------------------
    # MCP handshake
    # ------------------------------------------------------------------

    def _do_initialize(self) -> None:
        assert self.proc is not None
        if self.proc.poll() is not None:
            stderr_tail = self._read_stderr()
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
        content = result.get("content", [])
        text = content[0]["text"] if content else ""
        if not text:
            raise KhiveRpcError("Empty content in tools/call response", rpc_id=rpc_id)
        try:
            envelope = json.loads(text)
        except json.JSONDecodeError as exc:
            raise KhiveRpcError(
                f"Could not parse tools/call response as JSON: {text!r}",
                rpc_id=rpc_id,
            ) from exc
        if result.get("isError") and not (
            isinstance(envelope, dict)
            and isinstance(envelope.get("results"), list)
            and isinstance(envelope.get("summary"), dict)
        ):
            raise KhiveRpcError(text, code=-32603, rpc_id=rpc_id)
        return envelope

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
                message=error_text(first) or "<no error string>",
                index=0,
                envelope=envelope,
                detail=error_detail(first),
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
