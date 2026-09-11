#!/usr/bin/env python3
"""Measure isolated daemon restarts through actual stdio MCP, without building.

Example: uv run python scripts/perf/socket_handover_bench.py --binary /path/to/kkernel \
    --output /path/to/handover.json

Requires Unix, ps and lsof. Ambiguous process ownership aborts the run and
preserves its directory. No raw daemon protocol or local-dispatch fallback is used.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import stat
import subprocess
import tempfile
import time


INTERVAL = 0.05
CYCLES = 10
REQUEST_TIMEOUT = 30.0
START_TIMEOUT = 60.0
STOP_TIMEOUT = 15.0
SETTLE = 2.0


class OwnershipError(RuntimeError):
    pass


def process_table():
    result = subprocess.run(
        ["ps", "-ww", "-axo", "pid=,ppid=,lstart=,stat=,command="],
        check=True, capture_output=True, text=True, timeout=3,
    )
    rows = {}
    for line in result.stdout.splitlines():
        parts = line.split(None, 8)
        if len(parts) == 9:
            rows[int(parts[0])] = {
                "pid": int(parts[0]), "ppid": int(parts[1]),
                "started": " ".join(parts[2:7]), "state": parts[7],
                "command": parts[8],
            }
    return rows


def process_cwd(pid):
    result = subprocess.run(
        ["lsof", "-a", "-p", str(pid), "-d", "cwd", "-Fpn"],
        check=True, capture_output=True, text=True, timeout=3,
    )
    lines = result.stdout.splitlines()
    if f"p{pid}" not in lines:
        raise OwnershipError(f"lsof did not identify PID {pid}")
    paths = [line[1:] for line in lines if line.startswith("n")]
    if len(paths) != 1:
        raise OwnershipError(f"ambiguous cwd for PID {pid}: {paths!r}")
    return Path(paths[0]).resolve()


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class IsolatedProcesses:
    def __init__(self, root, binary, env):
        self.root, self.binary, self.env = root, binary, env
        self.children = {}
        self.owned = {}
        self.logs = []
        self.bridge_pid = None
        self.pid_file = root / "p"

    async def launch(self, daemon):
        log = (self.root / f"child-{len(self.logs)}.stderr").open("wb")
        self.logs.append(log)
        command = [str(self.binary), "mcp"] + (["--daemon"] if daemon else [])
        child = await asyncio.create_subprocess_exec(
            *command, cwd=self.root, env=self.env, stderr=log,
            stdin=asyncio.subprocess.DEVNULL if daemon else asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.DEVNULL if daemon else asyncio.subprocess.PIPE,
        )
        self.children[child.pid] = child
        if not daemon:
            self.bridge_pid = child.pid
        await self.discover()
        return child

    def _identity(self, row):
        argv = shlex.split(row["command"])
        if len(argv) < 2 or argv[:2] != [str(self.binary), "mcp"]:
            raise OwnershipError(f"unexpected executable/argv for PID {row['pid']}")
        daemon = "--daemon" in argv[2:]
        if not daemon and row["pid"] != self.bridge_pid:
            raise OwnershipError(f"unrecognized bridge PID {row['pid']}")
        if process_cwd(row["pid"]) != self.root:
            raise OwnershipError(f"PID {row['pid']} is outside the isolated cwd")
        return (row["started"], row["command"], daemon)

    def _discover(self):
        rows = process_table()
        candidates = {
            pid: row for pid, row in rows.items()
            if row["command"].startswith(str(self.binary) + " ")
        }
        for pid, row in candidates.items():
            if "Z" in row["state"]:
                continue
            try:
                identity = self._identity(row)
            except (subprocess.CalledProcessError, OwnershipError) as error:
                current = process_table().get(pid)
                if current is None or "Z" in current["state"]:
                    continue
                raise OwnershipError(f"cannot verify live PID {pid}: {error}") from error
            if pid in self.owned:
                if identity != self.owned[pid]:
                    raise OwnershipError(f"PID {pid} changed identity")
                continue
            ancestor = pid
            seen = set()
            while ancestor not in self.children and ancestor not in self.owned:
                if ancestor in seen or ancestor not in rows:
                    raise OwnershipError(f"cannot establish owned ancestry for PID {pid}")
                seen.add(ancestor)
                ancestor = rows[ancestor]["ppid"]
            self.owned[pid] = identity
        return rows

    async def discover(self):
        return await asyncio.to_thread(self._discover)

    async def published(self):
        try:
            raw = self.pid_file.read_text().strip()
        except FileNotFoundError:
            return None
        if not raw.isdecimal() or int(raw) <= 1:
            raise OwnershipError(f"invalid isolated PID file: {raw!r}")
        pid = int(raw)
        rows = await self.discover()
        row = rows.get(pid)
        if row is None or "Z" in row["state"]:
            return None
        if pid not in self.owned or not self.owned[pid][2]:
            raise OwnershipError(f"published PID {pid} is not a verified owned daemon")
        return pid

    async def wait_published(self, previous=None):
        deadline = time.monotonic() + START_TIMEOUT
        while time.monotonic() < deadline:
            pid = await self.published()
            if pid is not None and pid != previous:
                try:
                    if stat.S_ISSOCK((self.root / "s").lstat().st_mode):
                        return pid
                except FileNotFoundError:
                    pass
            await asyncio.sleep(0.05)
        raise TimeoutError("isolated successor did not publish a verified PID/socket")

    async def signal_owned(self, pid, sig, published=False):
        rows = await asyncio.to_thread(process_table)
        row = rows.get(pid)
        if row is None or "Z" in row["state"]:
            return
        if pid not in self.owned:
            raise OwnershipError(f"refusing to signal unverified PID {pid}")
        identity = await asyncio.to_thread(self._identity, row)
        if identity != self.owned[pid]:
            raise OwnershipError(f"refusing to signal changed PID {pid}")
        if published and self.pid_file.read_text().strip() != str(pid):
            raise OwnershipError("published PID changed before stop; no signal sent")
        try:
            sent_at = time.monotonic()
            os.kill(pid, sig)
            return sent_at
        except ProcessLookupError:
            return None

    async def wait_gone(self, pid, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            child = self.children.get(pid)
            if child is not None and child.returncode is not None:
                await child.wait()
                return True
            rows = await asyncio.to_thread(process_table)
            row = rows.get(pid)
            if row is None or "Z" in row["state"]:
                return True
            if (row["started"], row["command"]) != self.owned.get(pid, ())[:2]:
                raise OwnershipError(f"PID {pid} changed while awaiting exit")
            await asyncio.sleep(0.05)
        return False

    async def cleanup(self):
        errors = []
        try:
            await self.discover()
        except Exception as error:
            errors.append(f"cleanup discovery refused: {error}")
        try:
            bridge = self.children.get(self.bridge_pid)
            if bridge is not None and bridge.stdin is not None:
                bridge.stdin.close()
                deadline = time.monotonic() + STOP_TIMEOUT
                while bridge.returncode is None and time.monotonic() < deadline:
                    try:
                        await self.discover()
                    except Exception as error:
                        errors.append(f"bridge-drain discovery refused: {error}")
                        break
                    await asyncio.sleep(0.05)
                if bridge.returncode is None:
                    await self.signal_owned(bridge.pid, signal.SIGTERM)
                    try:
                        await asyncio.wait_for(bridge.wait(), STOP_TIMEOUT)
                    except asyncio.TimeoutError:
                        await self.signal_owned(bridge.pid, signal.SIGKILL)
                        await asyncio.wait_for(bridge.wait(), 5)
                else:
                    await bridge.wait()
            try:
                await self.discover()
            except Exception as error:
                errors.append(f"post-bridge discovery refused: {error}")
            for pid in list(self.owned):
                if not await self.wait_gone(pid, 0.1):
                    await self.signal_owned(pid, signal.SIGTERM)
            for pid in list(self.owned):
                if not await self.wait_gone(pid, STOP_TIMEOUT):
                    await self.signal_owned(pid, signal.SIGKILL)
                    if not await self.wait_gone(pid, 5):
                        errors.append(f"owned PID {pid} did not exit")
            for child in self.children.values():
                await asyncio.wait_for(child.wait(), 5)
            rows = await self.discover()
            for pid, row in rows.items():
                if row["command"].startswith(str(self.binary) + " "):
                    errors.append(f"isolated process remains: {pid} ({row['state']})")
        except Exception as error:
            errors.append(f"cleanup refused or incomplete: {error}")
        for log in self.logs:
            log.close()
        return errors


def stats_error(response):
    if "error" in response:
        return response["error"]
    result = response.get("result")
    if not isinstance(result, dict):
        return "missing MCP result"
    if result.get("isError"):
        return result
    texts = [item.get("text", "") for item in result.get("content", [])
             if item.get("type") == "text"]
    try:
        body = json.loads("\n".join(texts))
    except (ValueError, TypeError):
        return {"invalid_result_content": texts}
    entries = body.get("results") if isinstance(body, dict) else None
    if not isinstance(entries, list) or len(entries) != 1:
        return {"unexpected_stats_envelope": body}
    if not isinstance(entries[0], dict):
        return {"invalid_stats_entry": entries[0]}
    if entries[0].get("ok") is not True or entries[0].get("tool") != "stats":
        return {"stats_error": entries[0]}
    if not isinstance(entries[0].get("result"), dict):
        return {"missing_stats_result": entries[0]}
    return None


class McpProbe:
    def __init__(self, bridge):
        self.bridge = bridge
        self.started = time.monotonic()
        self.requests = []
        self.pending = {}
        self.queue = asyncio.Queue(maxsize=128)
        self.stop = asyncio.Event()
        self.protocol_errors = []
        self.tasks = []

    def ms(self):
        return (time.monotonic() - self.started) * 1000

    async def initialize(self):
        message = {"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "socket-handover-bench", "version": "1.0"},
        }}
        self.bridge.stdin.write((json.dumps(message) + "\n").encode())
        await asyncio.wait_for(self.bridge.stdin.drain(), 5)
        response = json.loads(await asyncio.wait_for(self.bridge.stdout.readline(), START_TIMEOUT))
        if response.get("id") != 0 or "error" in response or "result" not in response:
            raise RuntimeError(f"MCP initialize failed: {response!r}")
        self.bridge.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        await asyncio.wait_for(self.bridge.stdin.drain(), 5)

    def finish(self, record, outcome, error=None):
        if record["outcome"] != "pending":
            return
        record.update(outcome=outcome, completed_ms=self.ms(), error=error)
        if record["sent_ms"] is not None:
            record["latency_ms"] = record["completed_ms"] - record["sent_ms"]
        self.pending.pop(record["id"], None)

    async def offer(self):
        deadline = time.monotonic()
        while not self.stop.is_set():
            await asyncio.sleep(max(0, deadline - time.monotonic()))
            if self.stop.is_set():
                break
            record = {"id": len(self.requests) + 1,
                      "scheduled_ms": (deadline - self.started) * 1000,
                      "offered_ms": self.ms(), "sent_ms": None,
                      "send_lateness_ms": None, "latency_ms": None,
                      "completed_ms": None, "outcome": "pending", "error": None}
            record["offer_lateness_ms"] = record["offered_ms"] - record["scheduled_ms"]
            self.requests.append(record)
            self.pending[record["id"]] = record
            try:
                self.queue.put_nowait(record)
            except asyncio.QueueFull:
                self.finish(record, "send_queue_full", "offered cadence exceeded stdin capacity")
            deadline += INTERVAL

    async def writer(self):
        while True:
            record = await self.queue.get()
            if record["outcome"] != "pending":
                continue
            message = {"jsonrpc": "2.0", "id": record["id"], "method": "tools/call",
                       "params": {"name": "request", "arguments": {"ops": "stats()"}}}
            try:
                attempted = self.ms()
                self.bridge.stdin.write((json.dumps(message) + "\n").encode())
                record["sent_ms"] = attempted
                record["send_lateness_ms"] = attempted - record["scheduled_ms"]
                await asyncio.wait_for(self.bridge.stdin.drain(), 5)
                record["stdin_drained_ms"] = self.ms()
            except Exception as error:
                self.finish(record, "send_error", str(error))

    async def reader(self):
        try:
            while True:
                line = await self.bridge.stdout.readline()
                if not line:
                    raise EOFError("MCP stdout closed")
                response = json.loads(line)
                if not isinstance(response, dict):
                    raise ValueError("MCP response is not an object")
                record = self.pending.get(response.get("id"))
                if record is None:
                    request_id = response.get("id")
                    if isinstance(request_id, int) and 1 <= request_id <= len(self.requests):
                        self.requests[request_id - 1]["late_response"] = {
                            "received_ms": self.ms(), "response": response}
                        continue
                    self.protocol_errors.append({"received_ms": self.ms(), "response": response})
                    continue
                error = stats_error(response)
                self.finish(record, "success" if error is None else "response_error", error)
        except Exception as error:
            self.protocol_errors.append({"reader_error": str(error), "received_ms": self.ms()})
            for record in list(self.pending.values()):
                self.finish(record, "transport_error", str(error))

    async def deadlines(self):
        while True:
            now = self.ms()
            for record in list(self.pending.values()):
                if now - record["offered_ms"] >= REQUEST_TIMEOUT * 1000:
                    self.finish(record, "timeout", "request deadline exceeded; result unavailable")
            await asyncio.sleep(INTERVAL)

    def start(self):
        self.tasks = [asyncio.create_task(task()) for task in
                      (self.offer, self.writer, self.reader, self.deadlines)]

    async def wait_success(self, scheduled_after_ms):
        deadline = time.monotonic() + START_TIMEOUT
        while time.monotonic() < deadline:
            for record in self.requests:
                if record["scheduled_ms"] >= scheduled_after_ms and record["outcome"] == "success":
                    return {"id": record["id"], "scheduled_ms": record["scheduled_ms"],
                            "completed_ms": record["completed_ms"]}
            if self.bridge.returncode is not None:
                raise RuntimeError("MCP bridge exited before a successful stats response")
            await asyncio.sleep(INTERVAL)
        raise TimeoutError("no successful post-publication MCP stats response within budget")

    async def settle(self):
        self.stop.set()
        deadline = time.monotonic() + REQUEST_TIMEOUT + 1
        while self.pending and time.monotonic() < deadline:
            await asyncio.sleep(INTERVAL)
        for record in list(self.pending.values()):
            self.finish(record, "unavailable", "benchmark ended before a response")
        for task in self.tasks:
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)


def summarize(records):
    if not records:
        return {"calls": 0, "failed_calls": None, "longest_failure_gap_ms": None,
                "failure_gap_censored": True}
    failed = [record for record in records if record["outcome"] != "success"]
    longest = 0.0
    beginning = None
    for record in records:
        if record["outcome"] != "success":
            if beginning is None:
                beginning = record["scheduled_ms"]
        elif beginning is not None:
            longest = max(longest, record["scheduled_ms"] - beginning)
            beginning = None
    censored = beginning is not None
    lower_bound = max(longest, records[-1]["scheduled_ms"] + INTERVAL * 1000 - beginning) if censored else longest
    return {"calls": len(records), "failed_calls": len(failed),
            "longest_failure_gap_ms": None if censored else longest,
            "failure_gap_lower_bound_ms": lower_bound, "failure_gap_censored": censored,
            "max_latency_ms": max((row["latency_ms"] for row in records
                                   if row["latency_ms"] is not None), default=None),
            "max_send_lateness_ms": max((row["send_lateness_ms"] for row in records
                                         if row["send_lateness_ms"] is not None), default=None)}


async def benchmark(args):
    root = Path(tempfile.mkdtemp(prefix="khb-", dir="/tmp")).resolve()
    source = args.binary.absolute()
    binary = root / "kkernel-bench"
    report = {"status": "unavailable", "binary_source": str(source),
              "binary_copy": str(binary),
              "isolated_root": str(root), "cycles_requested": CYCLES,
              "interval_ms": INTERVAL * 1000, "cycles": [], "requests": [],
              "send_time_definition": "monotonic time when bytes are submitted to asyncio stdin; drain completion is recorded separately",
              "failure_gap_definition": "consecutive failed offered slots through the next successful offered slot; trailing runs are censored",
              "restart_attribution": "scheduled offer time from each stop through the next stop (last through end of offering)",
              "summary_population": "offered slots from the first controlled stop through the end of offering; warmup reported separately",
              "errors": []}
    processes = None
    probe = None
    ownership_ambiguous = False
    try:
        source = source.resolve(strict=True)
        report["binary_source"] = str(source)
        if not source.is_file() or not os.access(source, os.X_OK):
            raise ValueError("--binary must name an existing executable file")
        if not shutil.which("ps") or not shutil.which("lsof"):
            raise RuntimeError("ps and lsof are required for positive process ownership checks")
        await asyncio.to_thread(process_table)
        await asyncio.to_thread(process_cwd, os.getpid())
        shutil.copy2(source, binary)
        report["binary_source_sha256"] = sha256(source)
        report["binary_copy_sha256"] = sha256(binary)
        if report["binary_source_sha256"] != report["binary_copy_sha256"]:
            raise RuntimeError("binary changed while copying")
        config = root / "c.toml"
        config.write_text("")
        env = {key: value for key, value in os.environ.items() if not key.startswith("KHIVE_")}
        env.update(HOME=str(root), TMPDIR=str(root), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                   XDG_RUNTIME_DIR=str(root), KHIVE_CONFIG=str(config),
                   KHIVE_DB=str(root / "d.db"), KHIVE_SOCKET=str(root / "s"),
                   KHIVE_PID=str(root / "p"), KHIVE_LOCK=str(root / "l"),
                   KHIVE_RECOVERER_LOCK=str(root / "r"), KHIVE_PACKS="kg",
                   KHIVE_NO_EMBED="true", KHIVE_DAEMON_STRICT="1",
                   KHIVE_OUTPUT_FORMAT="json", KHIVE_EVENTS_SPLIT="0")
        processes = IsolatedProcesses(root, binary, env)
        await processes.launch(daemon=True)
        await processes.wait_published()
        bridge = await processes.launch(daemon=False)
        probe = McpProbe(bridge)
        await probe.initialize()
        probe.start()
        report["warmup_success"] = await probe.wait_success(0)
        await asyncio.sleep(SETTLE)
        for number in range(1, CYCLES + 1):
            previous = await processes.published()
            if previous is None:
                raise RuntimeError("no verified published daemon before restart")
            requested_ms = probe.ms()
            sent_at = await processes.signal_owned(previous, signal.SIGTERM, published=True)
            if sent_at is None:
                raise RuntimeError("published daemon exited before the controlled stop")
            cycle = {"restart": number, "stop_ms": (sent_at - probe.started) * 1000,
                     "stop_requested_ms": requested_ms, "previous_pid": previous,
                     "status": "unavailable", "predecessor_exit_ms": None,
                     "successor_launch_ms": None, "successor_pid": None,
                     "successor_published_ms": None, "first_success": None}
            report["cycles"].append(cycle)
            if not await processes.wait_gone(previous, STOP_TIMEOUT):
                raise TimeoutError("predecessor did not stop within the restart budget")
            cycle["predecessor_exit_ms"] = probe.ms()
            cycle["successor_launch_ms"] = probe.ms()
            successor = await processes.launch(daemon=True)
            cycle["explicit_successor_pid"] = successor.pid
            cycle["successor_pid"] = await processes.wait_published(previous)
            cycle["successor_published_ms"] = probe.ms()
            cycle["first_success"] = await probe.wait_success(cycle["successor_published_ms"])
            await asyncio.sleep(SETTLE)
            cycle["status"] = "complete"
        report["offering_ended_ms"] = probe.ms()
        report["status"] = "complete"
    except (Exception, asyncio.CancelledError) as error:
        ownership_ambiguous = isinstance(error, OwnershipError)
        report["errors"].append(f"{type(error).__name__}: {error}")
    finally:
        if probe is not None:
            report.setdefault("offering_ended_ms", probe.ms())
            await probe.settle()
            report["requests"] = probe.requests
            report["protocol_errors"] = probe.protocol_errors
        cleanup_errors = await processes.cleanup() if processes is not None else []
        report["errors"].extend(cleanup_errors)
        if cleanup_errors:
            report["status"] = "unavailable"
        first_stop = report["cycles"][0]["stop_ms"] if report["cycles"] else None
        warmup = [row for row in report["requests"]
                  if first_stop is None or row["scheduled_ms"] < first_stop]
        measured = [row for row in report["requests"]
                    if first_stop is not None and row["scheduled_ms"] >= first_stop]
        report["warmup_summary"] = summarize(warmup)
        report["summary"] = summarize(measured)
        if report["status"] != "complete":
            report["observed_summary"] = report["summary"]
            report["summary"] = {"calls": len(measured), "failed_calls": None,
                                 "longest_failure_gap_ms": None,
                                 "reason": "measurement incomplete; see observed_summary"}
        for index, cycle in enumerate(report["cycles"]):
            end = (report["cycles"][index + 1]["stop_ms"]
                   if index + 1 < len(report["cycles"]) else report.get("offering_ended_ms"))
            records = [record for record in report["requests"]
                       if end is not None and cycle["stop_ms"] <= record["scheduled_ms"] < end]
            summary = summarize(records)
            if cycle["status"] == "complete":
                cycle.update(summary)
            else:
                cycle.update(observed_summary=summary, failed_calls=None,
                             longest_failure_gap_ms=None)
        report["cycles_completed"] = sum(cycle["status"] == "complete" for cycle in report["cycles"])
        report["stderr"] = {}
        for path in sorted(root.rglob("*")):
            if path.is_file() and (path.suffix in (".stderr", ".log")):
                with path.open("rb") as log:
                    size = path.stat().st_size
                    log.seek(max(0, size - 65536))
                    report["stderr"][str(path.relative_to(root))] = {
                        "bytes": size, "tail": log.read().decode(errors="replace")}
        preserve = bool(cleanup_errors) or ownership_ambiguous
        report["isolated_directory_preserved"] = preserve
        if not preserve:
            shutil.rmtree(root)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("--output must not already exist")
    report = asyncio.run(benchmark(args))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output:
        output.write(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "output": str(args.output),
                      "summary": report["summary"]}))
    return 0 if report["status"] == "complete" else 2


if __name__ == "__main__":
    raise SystemExit(main())
