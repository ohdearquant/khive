#!/usr/bin/env python3
"""Question-bank runner: score retrieval arms on a frozen bank against a staged copy of a khive store.

A bank is a directory holding bank.json and cases.jsonl. bank.json names the bank (bank_id, version) and carries the
sha256 of cases.jsonl (cases_sha256); the runner refuses a bank whose cases file no longer matches, so a version is
frozen once written and a fix mints the next version. One case per line:

    {"case_id": "c001", "query": "...", "T": "2026-10-20T14:03:11Z",
     "needed_info": [{"statement": "...", "ids": ["<full uuid>", "<full uuid>"]}],
     "pass_predicate": "...", "tags": ["..."], "provenance": "..."}

`query` is the context the task had at instant T. `needed_info` lists the facts the task required; each member's ids
are one equivalence set (the same fact kept as a memory, a note and an entity counts once, whichever comes back).

Measures, per case and arm, over the ranked ids the arm returns:
    hit@k       1 when some member has an id in the top k, else 0
    coverage@k  the share of members with an id in the top k
    MRR@10      the reciprocal rank of the first id belonging to any member, 0 when none is in the top 10
for k = 5 and 10. One JSONL file per arm, keyed by query_id = case_id, so `bootstrap.py --metrics` pairs two arms.

The store is never the caller's live one. The runner copies the named store files into a fresh scratch home (the
events store beside the main one), refuses a source under the caller's ~/.khive, and refuses one with a non-empty
WAL unless --copy-wal says no process writes it (the WAL is then staged beside its store). A source whose inode, size
or modification time changes during its copy stops the run, and the gap between the two sources' modification times
is recorded. It writes its own config for an actor that has no binding row, and serves every arm from one
`kkernel mcp` stdio process on the copy, run with KHIVE_NO_DAEMON=1 so no warm daemon starts or outlives the run.
Writes made during the run (serve records, the profiles the feedback arms create) land in the copy. The scratch home
is kept, and its path and the staged files' sha256 before and after the run are recorded; --remove-scratch removes it.

Memory pressure is read before the copy and again before the server starts, and the run starts only when it is
normal: on macOS kern.memorystatus_vm_pressure_level is 1; on Linux /proc/pressure/memory shows no full-stall time
over the last 10 s (full avg10=0.00). An unreadable pressure counts as not normal. Both reads go in the run record.
Every row carries the bank id and version, the population, the arm, the store snapshot id and the kkernel version.

Arms. Every recall passes created_before = T, an exact store slice; a returned hit created at or after T stops the run.
    recall_rrf, recall_keyword_only, recall_vector_only
                    memory.recall at that fusion strategy, served with configured defaults (no binding row)
    profile_on      memory.recall with profile_id = --profile
    profile_off     memory.recall at the default fusion strategy with configured defaults
    feedback_fresh  memory.recall served by a profile created in the copy that has no feedback events
    feedback_asof   memory.recall served by a profile created in the copy and fed, before each case, the --profile
                    profile's explicit feedback events created before that case's T (cases run in T order). A replayed
                    event is a new event, so its effect on profile state matches the original only as far as
                    `calibrate-replay` measures.
    composed        reciprocal-rank fusion (k = 60) of recall_rrf with search(kind=note) and search(kind=entity).
                    search takes no time filter, so hits created at or after T are dropped before fusion and counted.
The served profile is checked on every hit: the default arms must read serve_attribution "unspecified", the profile
arms the profile they name. A mismatch stops the run, because the arm would not be the arm its name says.

Before any case runs, a control memory (--control-id) must come back in the top 5 of its own content under both the
vector-only and the keyword-only strategy: that proves the copy is visible to the bench actor and that the configured
embedding model matches the store's vectors. The run waits (bounded by --ready-max-s) until the vector index answers
without a degraded flag.

Subcommands:
    check-bank        validate a bank directory and print its population
    run               score the chosen arms and write <out>/<arm>.jsonl plus <out>/run.json
    calibrate-replay  replay the --profile profile's whole explicit feedback history into a fresh profile and compare
                      the two on the bank's queries (top-10 overlap, rank-score differences on shared ids). Each query
                      is recalled live, then by the replayed profile, then live again, and the two live answers are
                      compared the same way: that A/A floor bounds how much of the live/replay difference is the
                      recall's own run-to-run variation rather than the replay
    self-test         the measure, fusion, bank-check and response-parsing arms, no kkernel needed

Standard library only.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
import types
from pathlib import Path

UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
KS = (5, 10)
RRF_K = 60
POOL = 50
BENCH_ACTOR = "bench:unbound"
RECALL_ARMS = {
    "recall_rrf": {"fusion_strategy": "rrf"},
    "recall_keyword_only": {"fusion_strategy": "keyword_only"},
    "recall_vector_only": {"fusion_strategy": "vector_only"},
    "profile_off": {},
}
ARMS = ("recall_rrf", "recall_keyword_only", "recall_vector_only", "profile_on", "profile_off", "feedback_fresh",
        "feedback_asof", "composed")


class BankError(SystemExit):
    def __init__(self, msg: str):
        super().__init__(f"bank: {msg}")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def parse_instant(value) -> datetime.datetime:
    """An RFC 3339 instant with an offset, or an integer of microseconds since the epoch (the store's own unit)."""
    if isinstance(value, int) and not isinstance(value, bool):
        return datetime.datetime.fromtimestamp(value / 1_000_000, datetime.timezone.utc)
    if not isinstance(value, str):
        raise BankError(f"not an instant: {value!r}")
    text = value.strip().replace("Z", "+00:00")
    m = re.fullmatch(r"(.*T\d\d:\d\d:\d\d)(\.\d+)?([+-]\d\d:\d\d)", text)
    if not m:
        raise BankError(f"not an RFC 3339 instant with an offset: {value!r}")
    frac = (m.group(2) or "")[:7].ljust(7, "0") if m.group(2) else ""
    try:
        return datetime.datetime.fromisoformat(m.group(1) + frac + m.group(3))
    except ValueError:
        raise BankError(f"not an RFC 3339 instant with an offset: {value!r}") from None


def load_bank(bank_dir: Path) -> tuple[dict, list[dict]]:
    try:
        meta = json.loads((bank_dir / "bank.json").read_text())
    except (OSError, ValueError) as e:
        raise BankError(f"cannot read {bank_dir / 'bank.json'}: {e!r}") from None
    if not isinstance(meta, dict) or not isinstance(meta.get("bank_id"), str) or not meta["bank_id"]:
        raise BankError("bank.json lacks bank_id")
    version = meta.get("version")
    if not isinstance(version, int) or isinstance(version, bool) or version < 1:
        raise BankError(f"bank.json version must be an integer from 1, got {version!r}")
    if not isinstance(meta.get("cases_sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", meta["cases_sha256"]):
        raise BankError("bank.json cases_sha256 must be 64 lowercase hex digits")
    cases_path = bank_dir / "cases.jsonl"
    got = sha256_file(cases_path)
    if got != meta["cases_sha256"]:
        raise BankError(f"cases.jsonl sha256 {got} differs from bank.json cases_sha256 {meta['cases_sha256']}: a "
                        "frozen bank version was edited; mint a new version instead")
    cases, seen = [], set()
    for n, line in enumerate(cases_path.read_text().splitlines(), 1):
        if not line.strip():
            continue
        try:
            c = json.loads(line)
        except ValueError as e:
            raise BankError(f"cases.jsonl line {n}: {e!r}") from None
        cid = c.get("case_id") if isinstance(c, dict) else None
        if not isinstance(cid, str) or not cid or cid in seen:
            raise BankError(f"cases.jsonl line {n}: case_id missing or repeated")
        seen.add(cid)
        if not isinstance(c.get("query"), str) or not c["query"].strip():
            raise BankError(f"case {cid}: empty query")
        parse_instant(c.get("T"))
        members = c.get("needed_info")
        if not isinstance(members, list) or not members:
            raise BankError(f"case {cid}: needed_info must be a non-empty list")
        for m in members:
            ids = m.get("ids") if isinstance(m, dict) else None
            if not isinstance(ids, list) or not ids or not all(isinstance(i, str) and UUID_RE.fullmatch(i) for i in ids):
                raise BankError(f"case {cid}: every needed_info member needs a non-empty ids list of full lowercase UUIDs")
        cases.append(c)
    if not cases:
        raise BankError("cases.jsonl holds no case")
    return meta, cases


def measures(ranked: list[str], needed_info: list[dict]) -> dict:
    ranked = list(dict.fromkeys(ranked))
    members = [set(m["ids"]) for m in needed_info]
    gold = set().union(*members)
    out = {}
    for k in KS:
        top = set(ranked[:k])
        covered = sum(1 for m in members if m & top)
        out[f"hit@{k}"] = 1.0 if covered else 0.0
        out[f"coverage@{k}"] = covered / len(members)
    out["MRR@10"] = next((1.0 / i for i, x in enumerate(ranked[:10], 1) if x in gold), 0.0)
    return out


def rrf(lists: list[list[str]], k: int = RRF_K) -> list[str]:
    score: dict[str, float] = {}
    for lst in lists:
        for rank, x in enumerate(dict.fromkeys(lst), 1):
            score[x] = score.get(x, 0.0) + 1.0 / (k + rank)
    return sorted(score, key=lambda x: (-score[x], x))


def op(verb: str, **args) -> str:
    return f"{verb}(" + ", ".join(f"{k}={json.dumps(v)}" for k, v in args.items() if v is not None) + ")"


def hit_list(result, what: str) -> list[dict]:
    """The hit list of a recall or search result: a bare array, or an envelope with a results array. A degraded or
    budget-truncated response is refused, never scored."""
    if isinstance(result, list):
        hits = result
    elif isinstance(result, dict) and isinstance(result.get("results"), list):
        for flag in ("degraded", "truncated", "budget_capped", "truncated_for_budget"):
            if result.get(flag):
                raise BankError(f"{what}: response carries {flag}={result.get(flag)!r}; not scored")
        hits = result["results"]
    else:
        raise BankError(f"{what}: unexpected response shape {type(result).__name__}")
    for h in hits:
        if not isinstance(h, dict):
            raise BankError(f"{what}: a hit is not an object")
        if h.get("degraded") or h.get("truncated"):
            raise BankError(f"{what}: a hit carries degraded/truncated ({h.get('degraded')!r}); not scored")
    return hits


def hit_id(h: dict, what: str) -> str:
    for field in ("full_id", "id"):
        v = h.get(field)
        if isinstance(v, str) and UUID_RE.fullmatch(v):
            return v
    raise BankError(f"{what}: a hit carries no full UUID (fields {sorted(h)})")


class Bridge:
    """One `kkernel mcp` stdio process. Each call sends one `request` with presentation=verbose (the full shape, full
    ids) and returns the per-op results in op order."""

    def __init__(self, kkernel: str, db: Path, env: dict, err_path: Path):
        self.err = open(err_path, "ab")
        self.proc = subprocess.Popen([kkernel, "mcp", "--db", str(db), "--log", "warn"], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=self.err, env=env, bufsize=0)
        self.next_id = 1
        self.buf = b""

    def _readline(self, deadline: float) -> bytes:
        while b"\n" not in self.buf:
            left = deadline - time.monotonic()
            if left <= 0:
                raise BankError("kkernel mcp: no response before the deadline")
            ready, _, _ = select.select([self.proc.stdout], [], [], left)
            if ready:
                chunk = os.read(self.proc.stdout.fileno(), 1 << 16)
                if not chunk:
                    raise BankError(f"kkernel mcp closed its output (rc {self.proc.poll()}); see the stderr log")
                self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line

    def _rpc(self, method: str, params: dict, timeout: float) -> dict:
        i = self.next_id
        self.next_id += 1
        self.proc.stdin.write((json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n").encode())
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while True:
            try:
                msg = json.loads(self._readline(deadline))
            except ValueError:
                continue
            if msg.get("id") == i:
                if "error" in msg:
                    raise BankError(f"kkernel mcp refused {method}: {msg['error']!r}")
                return msg.get("result", {})

    def handshake(self) -> dict:
        r = self._rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                     "clientInfo": {"name": "question-bank", "version": "1"}}, 600)
        self.proc.stdin.write(b'{"jsonrpc": "2.0", "method": "notifications/initialized"}\n')
        self.proc.stdin.flush()
        return r

    def ops(self, ops: list[str], timeout: float = 900) -> list[dict]:
        text = ops[0] if len(ops) == 1 else "[" + ", ".join(ops) + "]"
        r = self._rpc("tools/call", {"name": "request", "arguments": {"ops": text, "presentation": "verbose"}}, timeout)
        try:
            env = json.loads(r["content"][0]["text"])
            results = env["results"]
        except (KeyError, IndexError, TypeError, ValueError):
            raise BankError(f"unexpected request envelope: {json.dumps(r)[:300]}") from None
        if len(results) != len(ops):
            raise BankError(f"sent {len(ops)} ops, got {len(results)} results")
        return results

    def one(self, op_text: str, timeout: float = 900):
        r = self.ops([op_text], timeout)[0]
        if not r.get("ok"):
            raise BankError(f"{op_text[:80]}: {r.get('error')!r}")
        return r.get("result")

    def close(self) -> None:
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=60)
        except (OSError, subprocess.TimeoutExpired):
            self.proc.kill()
        self.err.close()


def under(path: Path, root: Path) -> bool:
    return path == root or root in path.parents


def fingerprint(path: Path) -> tuple[int, int, int]:
    st = path.stat()
    return st.st_ino, st.st_size, st.st_mtime_ns


def copy_at_rest(src: Path, dest: Path) -> int:
    """Copy src to dest; refuse when src's inode, size or modification time moved during the copy. Returns mtime_ns."""
    before = fingerprint(src)
    shutil.copyfile(src, dest)
    after = fingerprint(src)
    if before != after:
        raise BankError(f"{src} changed during its copy (inode, size, mtime_ns {before} -> {after}); the copy at "
                        f"{dest} is not used")
    return before[2]


def memory_pressure() -> dict:
    """The kernel's memory pressure now, as {"source", "reading", "normal"}; normal is False when unreadable."""
    if sys.platform == "darwin":
        try:
            r = subprocess.run(["/usr/sbin/sysctl", "-n", "kern.memorystatus_vm_pressure_level"], capture_output=True,
                               text=True, timeout=10)
            level = r.stdout.strip()
        except (OSError, subprocess.SubprocessError) as e:
            return {"source": "kern.memorystatus_vm_pressure_level", "reading": repr(e), "normal": False}
        return {"source": "kern.memorystatus_vm_pressure_level", "reading": level, "normal": level == "1"}
    try:
        lines = Path("/proc/pressure/memory").read_text().splitlines()
    except OSError as e:
        return {"source": "/proc/pressure/memory", "reading": repr(e), "normal": False}
    return {"source": "/proc/pressure/memory", "reading": " | ".join(lines), "normal": psi_full_avg10(lines) == 0.0}


def psi_full_avg10(lines: list[str]) -> float | None:
    """The avg10 field of a PSI file's "full" line, None when absent or unparsable."""
    for line in lines:
        kind, _, rest = line.partition(" ")
        if kind == "full":
            fields = dict(f.split("=", 1) for f in rest.split() if "=" in f)
            try:
                return float(fields["avg10"])
            except (KeyError, ValueError):
                return None
    return None


def require_normal_pressure(when: str) -> dict:
    p = memory_pressure()
    if not p["normal"]:
        raise BankError(f"memory pressure is not normal {when} ({p['source']}: {p['reading']}); start when it is")
    return p


def adopt(args, scratch: Path, live: Path) -> list[dict]:
    """Verify a store copy a guarded transfer already placed at scratch/home/.khive, in place; return manifest rows."""
    kdir = scratch / "home" / ".khive"
    if under(kdir.resolve(), live):
        raise BankError(f"refusing {kdir}: it is under the caller's live store directory {live}")
    try:
        rec = json.loads((scratch / "transfer.json").read_text())
    except (OSError, ValueError) as e:
        raise BankError(f"--staged-home needs its transfer record {scratch / 'transfer.json'}: {e!r}")
    if rec.get("verdict") != "ok":
        raise BankError(f"the transfer record's verdict is {rec.get('verdict')!r}, not 'ok'")
    if rec.get("snapshot_id") != args.snapshot_id:
        raise BankError(f"--snapshot-id {args.snapshot_id} is not the transfer's {rec.get('snapshot_id')}")
    rows = {f.get("staged_as"): f for f in rec.get("files", [])}
    manifest = []
    for name in ("khive.db", "khive.db.events.db"):
        f, row = kdir / name, rows.get(name)
        if row is None:
            raise BankError(f"the transfer record has no {name}")
        for side in ("-wal", "-shm"):
            if Path(str(f) + side).exists():
                raise BankError(f"refusing {f}: {name}{side} exists, so a server has opened this copy and it is no "
                                "longer the transferred snapshot")
        if not f.is_file() or f.stat().st_size != row["bytes"] or sha256_file(f) != row["sha256"]:
            raise BankError(f"refusing {f}: it is not the {row['bytes']}-byte file with sha256 {row['sha256']} the "
                            "transfer recorded")
        manifest.append({**row, "adopted_in_place": str(f)})
    return manifest


def link_models(home: Path, models: Path) -> None:
    """Link home/.lattice/models to models; a staged home reused by a later run keeps a link that already points there."""
    link = home / ".lattice" / "models"
    link.parent.mkdir(exist_ok=True)
    if link.is_symlink() and Path(os.readlink(link)) == models:
        return
    if link.is_symlink() or link.exists():
        raise BankError(f"{link} exists and is not a link to {models}")
    os.symlink(models, link)


def stage(args, scratch: Path) -> tuple[Path, dict, list[dict]]:
    """Copy the store into scratch/home/.khive (or adopt a transferred copy there), write the bench config, return
    (main db path, env, manifest rows)."""
    live = Path(os.path.expanduser("~/.khive")).resolve()
    home = scratch / "home"
    kdir = home / ".khive"
    if args.staged_home:
        manifest, sources = adopt(args, scratch, live), ()
        (kdir / "vol-locks").mkdir(exist_ok=True)
    else:
        (kdir / "vol-locks").mkdir(parents=True)
        manifest, sources = [], ((args.store_main, "khive.db"), (args.store_events, "khive.db.events.db"))
    for src, name in sources:
        src = Path(src).resolve()
        if under(src, live):
            raise BankError(f"refusing {src}: it is under the caller's live store directory {live}")
        wal = Path(str(src) + "-wal")
        dest = kdir / name
        if wal.exists() and wal.stat().st_size > 0:
            if not args.copy_wal:
                raise BankError(f"refusing {src}: its WAL is not empty; stage a checkpointed copy, or pass --copy-wal "
                                "when no process writes the source")
            mtime = copy_at_rest(wal, Path(str(dest) + "-wal"))
            manifest.append({"source": str(wal), "staged_as": name + "-wal", "bytes": wal.stat().st_size,
                             "source_mtime_ns": mtime, "sha256": sha256_file(Path(str(dest) + "-wal"))})
        mtime = copy_at_rest(src, dest)
        manifest.append({"source": str(src), "staged_as": name, "bytes": dest.stat().st_size,
                         "source_mtime_ns": mtime, "sha256": sha256_file(dest)})
    if args.models_dir:
        link_models(home, Path(args.models_dir).resolve())
    engines = Path(args.engines_file).read_text()
    for line in engines.splitlines():
        s = line.strip()
        if s.startswith("[") and s != "[[engines]]":
            raise BankError(f"--engines-file holds only [[engines]] tables; found {s}")
    visible = [n for n in args.namespaces.split(",") if n]
    (kdir / "config.toml").write_text(
        f'[actor]\nid = "{BENCH_ACTOR}"\ndisplay_name = "{BENCH_ACTOR}"\nvisible_namespaces = {json.dumps(visible)}\n\n'
        f'{engines.strip()}\n\n[[backends]]\nname = "main"\nkind = "sqlite"\npath = "{kdir / "khive.db"}"\n')
    h = str(home)
    env = {"HOME": h, "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8",
           "KHIVE_CONFIG": h + "/.khive/config.toml", "KHIVE_SOCKET": h + "/.khive/k.sock",
           "KHIVE_PID": h + "/.khive/k.pid", "KHIVE_LOCK": h + "/.khive/k.lock",
           "KHIVE_RECOVERER_LOCK": h + "/.khive/recoverer.lock", "KHIVE_VOLUME_LOCK_DIR": h + "/.khive/vol-locks",
           "KHIVE_DB": str(kdir / "khive.db"), "KHIVE_NO_DAEMON": "1"}
    return kdir / "khive.db", env, manifest


def staged_state(scratch: Path) -> list[dict]:
    """sha256 of every non-empty store file in the scratch home, read after the server has closed."""
    kdir = scratch / "home" / ".khive"
    rows = []
    for name in ("khive.db", "khive.db-wal", "khive.db.events.db", "khive.db.events.db-wal"):
        f = kdir / name
        if f.exists() and f.stat().st_size > 0:
            rows.append({"staged_as": name, "bytes": f.stat().st_size, "sha256": sha256_file(f)})
    return rows


def finish_scratch(args, scratch: Path, record: dict) -> None:
    """Record where the scratch home is and what it holds now; remove it only under --remove-scratch."""
    kdir = scratch / "home" / ".khive"
    holds_store = any((kdir / n).exists() for n in ("khive.db", "khive.db.events.db"))
    record["scratch"] = {"path": str(scratch), "kept": bool(args.staged_home) or (holds_store and not args.remove_scratch)}
    if record["scratch"]["kept"]:
        record["scratch"]["after_run"] = staged_state(scratch)
    else:
        shutil.rmtree(scratch)


def recall(br: Bridge, query: str, before: str | None, what: str, **extra) -> list[dict]:
    hits = hit_list(br.one(op("memory.recall", query=query, top_k=POOL, created_before=before, **extra)), what)
    if before:
        t = parse_instant(before)
        late = [h for h in hits if parse_instant(h.get("created_at")) >= t]
        if late:
            raise BankError(f"{what}: {len(late)} hit(s) created at or after {before} despite created_before")
    return hits


def check_served(hits: list[dict], arm: str, want_profile: str | None, what: str) -> None:
    for h in hits:
        att, by = h.get("serve_attribution"), h.get("served_by_profile_id")
        ok = (att == "unspecified") if want_profile is None else (att == "profile" and by == want_profile)
        if not ok:
            raise BankError(f"{what}: arm {arm} expects {'configured defaults' if want_profile is None else want_profile}"
                            f", hit served as {att!r} by {by!r}")


def wait_ready(br: Bridge, query: str, max_s: int) -> float:
    t0 = time.monotonic()
    while True:
        r = br.ops([op("memory.recall", query=query, top_k=5, fusion_strategy="vector_only")])[0]
        if r.get("ok"):
            try:
                hit_list(r.get("result"), "readiness")
                return time.monotonic() - t0
            except BankError:
                pass
        if time.monotonic() - t0 > max_s:
            raise BankError(f"the vector index did not answer undegraded within {max_s} s: {json.dumps(r)[:300]}")
        time.sleep(15)


def control(br: Bridge, control_id: str) -> dict:
    rec = br.one(op("get", id=control_id))
    content = rec.get("content") if isinstance(rec, dict) else None
    if not isinstance(content, str) or not content.strip():
        raise BankError(f"control {control_id}: no content to query with")
    out = {"control_id": control_id}
    for strategy in ("vector_only", "keyword_only"):
        ids = [hit_id(h, "control") for h in hit_list(br.one(op("memory.recall", query=content[:400], top_k=5,
                                                                   fusion_strategy=strategy)), "control")]
        if control_id not in ids:
            raise BankError(f"control {control_id} is not in the top 5 of its own content under {strategy} "
                            f"(got {ids}): the copy is not visible to the bench actor or the embedding model differs")
        out[strategy] = ids.index(control_id) + 1
    return out


def dig(obj, names: tuple[str, ...], depth: int = 3):
    if not isinstance(obj, dict) or depth < 0:
        return None
    for n in names:
        if n in obj and not isinstance(obj[n], dict):
            return obj[n]
    for v in obj.values():
        found = dig(v, names, depth - 1)
        if found is not None:
            return found
    return None


def feedback_events(br: Bridge, actors: list[str], profile: str, since: str, until: str) -> list[dict]:
    """The profile's explicit feedback events in [since, until), oldest first, as {target_id, signal, created_at}."""
    rows = []
    for actor in actors:
        after = None
        while True:
            page = br.one(op("brain.event_page", since=since, until=until, kind="feedback_explicit", actor=actor,
                             limit=1000, after=after))
            for e in page.get("events", []):
                if dig(e, ("served_by_profile_id",)) != profile:
                    continue
                target = dig(e, ("target_id",))
                signal = dig(e, ("signal",))
                if not (isinstance(target, str) and UUID_RE.fullmatch(target) and isinstance(signal, str)):
                    raise BankError(f"a feedback event without a readable target and signal (fields {sorted(e)})")
                rows.append({"target_id": target, "signal": signal, "created_at": e.get("created_at")})
            if not page.get("has_more"):
                break
            after = page.get("next_after")
    rows.sort(key=lambda r: parse_instant(r["created_at"]))
    return rows


def spread(items: list, n: int) -> list:
    """n items evenly spaced over the list, first and last included; all of them when n >= len(items)."""
    if n >= len(items):
        return list(items)
    if n <= 1:
        return items[-1:] if n == 1 else []
    return [items[round(i * (len(items) - 1) / (n - 1))] for i in range(n)]


def served_queries(br: Bridge, actors: list[str], profile: str, since: str, until: str, n: int) -> tuple[list, int]:
    """n distinct queries the profile served in [since, until), spread over time, as calibration cases."""
    seen = {}
    for actor in actors:
        after = None
        while True:
            page = br.one(op("brain.event_page", since=since, until=until, kind="recall_executed", actor=actor,
                             limit=1000, after=after))
            for e in page.get("events", []):
                if e.get("outcome") != "success" or dig(e, ("served_by_profile_id",)) != profile:
                    continue
                q = dig(e, ("query",))
                if isinstance(q, str) and q.strip() and q not in seen:
                    seen[q] = {"case_id": f"recall-{e.get('id')}", "query": q, "created_at": e.get("created_at")}
            if not page.get("has_more"):
                break
            after = page.get("next_after")
    ordered = sorted(seen.values(), key=lambda c: parse_instant(c["created_at"]))
    return spread(ordered, n), len(ordered)


def replay(br: Bridge, events: list[dict], into: str) -> dict:
    counts = {"replayed": 0, "refused": 0, "refused_by_signal": {}}
    for i in range(0, len(events), 8):
        batch = events[i:i + 8]
        res = br.ops([op("brain.feedback", target_id=e["target_id"], signal=e["signal"], served_by_profile_id=into)
                      for e in batch])
        for e, r in zip(batch, res):
            if r.get("ok"):
                counts["replayed"] += 1
            else:
                counts["refused"] += 1
                counts["refused_by_signal"][e["signal"]] = counts["refused_by_signal"].get(e["signal"], 0) + 1
    return counts


def kkernel_version(kkernel: str) -> str:
    r = subprocess.run([kkernel, "--version"], capture_output=True, text=True, check=False)
    return (r.stdout or r.stderr).strip()


def cmd_check_bank(args) -> int:
    meta, cases = load_bank(Path(args.bank))
    print(json.dumps({"bank_id": meta["bank_id"], "version": meta["version"], "population": len(cases),
                      "needed_info_members": sum(len(c["needed_info"]) for c in cases)}))
    return 0


def start(args, run_id: str, scratch: Path):
    meta, cases = load_bank(Path(args.bank)) if args.bank else ({"bank_id": None, "version": None}, [])
    pressure = {"before_copy": require_normal_pressure("before the copy")}
    db, env, manifest = stage(args, scratch)
    pressure["before_server"] = require_normal_pressure("before the server starts")
    stores = [m for m in manifest if m["staged_as"] in ("khive.db", "khive.db.events.db")]
    skew_s = abs(stores[0]["source_mtime_ns"] - stores[1]["source_mtime_ns"]) / 1e9
    br = Bridge(args.kkernel, db, env, scratch / "kkernel-mcp.err")
    try:
        br.handshake()
        ready_s = wait_ready(br, cases[0]["query"] if cases else "store ready probe", args.ready_max_s)
        ctl = control(br, args.control_id)
        br.one(op("brain.profile", profile_id=args.profile))
    except BaseException:
        br.close()
        raise
    base = {"bank_id": meta["bank_id"], "bank_version": meta["version"], "population": len(cases),
            "snapshot_id": args.snapshot_id, "kkernel_version": kkernel_version(args.kkernel), "run_id": run_id}
    return meta, cases, br, base, {"ready_s": round(ready_s, 1), "control": ctl, "staged": manifest,
                                   "source_mtime_skew_s": round(skew_s, 3), "memory_pressure": pressure}


def scratch_dir(args, run_id: str) -> Path:
    if os.environ.get("KHIVE_DB"):
        raise BankError("refusing to run with KHIVE_DB set in the caller's environment; unset it")
    if args.bank:
        load_bank(Path(args.bank))
    if args.staged_home:
        if args.remove_scratch:
            raise BankError("--remove-scratch does not apply to --staged-home: the runner did not make that copy")
        return Path(args.staged_home).resolve()
    return Path(tempfile.mkdtemp(prefix=f"khive-question-bank-{run_id}-", dir=args.scratch_parent))


def cmd_run(args) -> int:
    arms = [a for a in args.arms.split(",") if a]
    unknown = sorted(set(arms) - set(ARMS))
    if unknown or not arms:
        raise BankError(f"unknown arms {unknown}; choose from {', '.join(ARMS)}")
    run_id = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dt%H%M%Sz")
    out = Path(args.out)
    scratch = scratch_dir(args, run_id)
    out.mkdir(parents=True, exist_ok=False)
    summary = {"status": "incomplete", "arms": {}}
    try:
        meta, cases, br, base, setup = start(args, run_id, scratch)
        summary.update(base)
        summary.update(setup)
        try:
            run_arms(args, arms, run_id, cases, br, base, out, summary)
            summary["status"] = "complete"
        finally:
            br.close()
    finally:
        try:
            finish_scratch(args, scratch, summary)
        finally:
            (out / "run.json").write_text(json.dumps(summary, indent=1) + "\n")
    return 0


def run_arms(args, arms, run_id, cases, br, base, out, summary) -> None:
    fresh = asof = None
    if "feedback_fresh" in arms:
        fresh = f"bench-fresh-{run_id}"
        br.one(op("brain.create_profile", name=fresh, description="question-bank arm: no feedback events"))
    if "feedback_asof" in arms:
        asof = f"bench-asof-{run_id}"
        br.one(op("brain.create_profile", name=asof, description="question-bank arm: feedback replayed as of T"))
    actors = [a for a in args.feedback_actors.split(",") if a]
    for arm in arms:
        rows, extra = [], {}
        ordered = sorted(cases, key=lambda c: parse_instant(c["T"])) if arm == "feedback_asof" else cases
        fed_until, replay_counts = args.feedback_since, {"replayed": 0, "refused": 0, "refused_by_signal": {}}
        for c in ordered:
            what = f"{arm} {c['case_id']}"
            if arm in RECALL_ARMS:
                hits = recall(br, c["query"], c["T"], what, **RECALL_ARMS[arm])
                check_served(hits, arm, None, what)
                ranked = [hit_id(h, what) for h in hits]
            elif arm == "profile_on":
                hits = recall(br, c["query"], c["T"], what, profile_id=args.profile)
                check_served(hits, arm, args.profile, what)
                ranked = [hit_id(h, what) for h in hits]
            elif arm == "feedback_fresh":
                hits = recall(br, c["query"], c["T"], what, profile_id=fresh)
                check_served(hits, arm, fresh, what)
                ranked = [hit_id(h, what) for h in hits]
            elif arm == "feedback_asof":
                if parse_instant(c["T"]) > parse_instant(fed_until):
                    got = replay(br, feedback_events(br, actors, args.profile, fed_until, c["T"]), asof)
                    for k in ("replayed", "refused"):
                        replay_counts[k] += got[k]
                    for s, n in got["refused_by_signal"].items():
                        replay_counts["refused_by_signal"][s] = replay_counts["refused_by_signal"].get(s, 0) + n
                    fed_until = c["T"]
                hits = recall(br, c["query"], c["T"], what, profile_id=asof)
                check_served(hits, arm, asof, what)
                ranked = [hit_id(h, what) for h in hits]
                extra = {"feedback_replayed_so_far": replay_counts["replayed"]}
            else:  # composed
                hits = recall(br, c["query"], c["T"], what, fusion_strategy="rrf")
                check_served(hits, arm, None, what)
                mem = [hit_id(h, what) for h in hits]
                t, dropped, lists = parse_instant(c["T"]), 0, [mem]
                for kind in ("note", "entity"):
                    kept = []
                    for h in hit_list(br.one(op("search", kind=kind, query=c["query"], limit=POOL)), what):
                        if parse_instant(h.get("created_at")) >= t:
                            dropped += 1
                        else:
                            kept.append(hit_id(h, what))
                    lists.append(kept)
                ranked = rrf(lists)
                extra = {"dropped_created_at_or_after_T": dropped}
            rows.append({"query_id": c["case_id"], "arm": arm, **base, **measures(ranked, c["needed_info"]),
                         "returned": len(ranked), "top10": ranked[:10], **extra})
        with open(out / f"{arm}.jsonl", "w") as f:
            for r in rows:
                f.write(json.dumps(r) + "\n")
        n = len(rows)
        agg = {m: round(sum(r[m] for r in rows) / n, 4) for m in ("hit@5", "hit@10", "coverage@5", "coverage@10",
                                                                "MRR@10")}
        summary["arms"][arm] = {"n": n, **agg, **({"replay": replay_counts} if arm == "feedback_asof" else {})}
        print(json.dumps({"arm": arm, "n": n, **agg}), flush=True)


def compare(x: list[dict], y: list[dict]) -> dict:
    """Top-10 overlap relative to x, whether the two lists are the same ids in the same order, and the largest
    rank-score gap on a shared id."""
    rx = {hit_id(h, "calibrate"): h.get("rank_score") for h in x}
    ry = {hit_id(h, "calibrate"): h.get("rank_score") for h in y}
    shared = [i for i in rx if i in ry]
    diffs = [abs(rx[i] - ry[i]) for i in shared if isinstance(rx[i], (int, float)) and isinstance(ry[i], (int, float))]
    return {"overlap@10": len(shared) / max(len(rx), 1), "same_order": list(rx) == list(ry),
            "max_abs_rank_score_diff": max(diffs, default=0.0)}


def aggregate(rows: list[dict]) -> dict:
    n = len(rows)
    return {"mean_overlap@10": round(sum(r["overlap@10"] for r in rows) / n, 4),
            "same_order_share": round(sum(r["same_order"] for r in rows) / n, 4),
            "max_abs_rank_score_diff": max(r["max_abs_rank_score_diff"] for r in rows)}


def calibrate_case(br, case: dict, live: str, replayed: str) -> dict:
    """One query recalled live, then by the replayed profile, then live again (A B A). The first live answer is the
    reference for both comparisons, so the A/A row and the replay row share it."""
    def recall_top10(profile):
        return hit_list(br.one(op("memory.recall", query=case["query"], top_k=10, profile_id=profile)), "calibrate")
    a = recall_top10(live)
    b = recall_top10(replayed)
    a2 = recall_top10(live)
    return {"query_id": case["case_id"], **compare(a, b), "aa": compare(a, a2),
            "replayed_vs_second_live": compare(a2, b)}


def cmd_calibrate_replay(args) -> int:
    run_id = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dt%H%M%Sz")
    out = Path(args.out)
    if out.exists():
        raise BankError(f"refusing to overwrite {out}")
    scratch = scratch_dir(args, run_id)
    report = {"status": "incomplete"}
    try:
        meta, cases, br, base, setup = start(args, run_id, scratch)
        report.update(base)
        report.update(setup)
        try:
            twin = f"bench-replay-{run_id}"
            br.one(op("brain.create_profile", name=twin, description="question-bank calibration: full feedback replay"))
            now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
            actors = [a for a in args.feedback_actors.split(",") if a]
            if args.queries_from_events:
                cases, distinct = served_queries(br, actors, args.profile, args.feedback_since, now, args.n)
                if not cases:
                    raise BankError(f"no recall_executed event served by {args.profile} for {actors}: nothing to compare")
                report["query_source"] = {"kind": "recall_executed", "profile": args.profile, "actors": actors,
                                          "distinct_queries": distinct, "sampled": len(cases)}
                report["population"] = len(cases)
            events = feedback_events(br, actors, args.profile, args.feedback_since, now)
            report["events_found"] = len(events)
            report.update(replay(br, events, twin))
            rows = [calibrate_case(br, c, args.profile, twin) for c in cases[: args.n]]
            report.update({"queries": len(rows), "recall_order": "live, replayed, live", **aggregate(rows),
                           "aa": aggregate([r["aa"] for r in rows]),
                           "replayed_vs_second_live": aggregate([r["replayed_vs_second_live"] for r in rows]),
                           "rows": rows})
            report["status"] = "complete"
        finally:
            br.close()
    finally:
        try:
            finish_scratch(args, scratch, report)
        finally:
            out.write_text(json.dumps(report, indent=1) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k not in ("rows", "staged")}), flush=True)
    return 0


def self_test() -> int:
    fails = []

    def check(name, cond):
        print(("ok   " if cond else "FAIL ") + name)
        if not cond:
            fails.append(name)

    def refused(fn, *a):
        try:
            fn(*a)
        except BankError:
            return True
        return False

    u = [f"{i:08x}-0000-4000-8000-000000000000" for i in range(12)]
    ni = [{"statement": "a", "ids": [u[0], u[1]]}, {"statement": "b", "ids": [u[2]]}]
    m = measures([u[5], u[1], u[6], u[7], u[8], u[2]], ni)
    check("measures: equivalence set counts once, second member at rank 6",
          m == {"hit@5": 1.0, "coverage@5": 0.5, "hit@10": 1.0, "coverage@10": 1.0, "MRR@10": 0.5})
    check("measures: nothing returned scores zero", measures([], ni)["MRR@10"] == 0.0 and measures([], ni)["hit@10"] == 0.0)
    check("measures: a repeated id does not move later ranks",
          measures([u[5], u[5], u[0]], ni)["MRR@10"] == 0.5)
    check("rrf: an id in two lists outranks ids in one", rrf([[u[0], u[1]], [u[2], u[1]]])[0] == u[1])
    check("instant: Z, offset and microseconds agree",
          parse_instant("2026-10-28T12:00:00Z") == parse_instant("2026-10-28T08:00:00-04:00")
          == parse_instant(1_793_188_800_000_000))
    check("instant: a naive time is refused", refused(parse_instant, "2026-10-28T12:00:00"))
    check("hits: a degraded envelope is refused", refused(hit_list, {"results": [], "degraded": True}, "t"))
    check("hits: a degraded hit is refused", refused(hit_list, [{"id": u[0], "degraded": "ann_unavailable"}], "t"))
    check("hits: a hit without a full id is refused", refused(hit_id, {"id": "0000abcd"}, "t"))
    check("served: an unbound arm refuses a profile-served hit",
          refused(check_served, [{"serve_attribution": "profile", "served_by_profile_id": "p"}], "x", None, "t"))
    check("served: a profile arm refuses another profile",
          refused(check_served, [{"serve_attribution": "profile", "served_by_profile_id": "q"}], "x", "p", "t"))
    check("live store: a file under the live directory is refused, a sibling directory is not",
          under(Path("/h/.khive/khive.db"), Path("/h/.khive")) and not under(Path("/h/.khive-stage/khive.db"), Path("/h/.khive")))
    psi = "some avg10=0.87 avg60=0.16 avg300=0.03 total=812622984"
    check("pressure: PSI full avg10 parses, zero only for no stall",
          psi_full_avg10([psi, "full avg10=0.00 avg60=0.00 avg300=0.00 total=1"]) == 0.0
          and psi_full_avg10([psi, "full avg10=0.12 avg60=0.01 avg300=0.00 total=9"]) == 0.12
          and psi_full_avg10([psi, "full avg10=10.00 avg60=0.01 avg300=0.00 total=9"]) == 10.0
          and psi_full_avg10([psi]) is None)
    global memory_pressure
    real_pressure = memory_pressure
    try:
        memory_pressure = lambda: {"source": "test", "reading": "2", "normal": False}  # noqa: E731
        check("pressure: a run does not start at a pressure that is not normal",
              refused(require_normal_pressure, "in the test"))
        memory_pressure = lambda: {"source": "test", "reading": "1", "normal": True}  # noqa: E731
        check("pressure: a normal reading is returned and recorded", require_normal_pressure("in the test")["reading"] == "1")
    finally:
        memory_pressure = real_pressure
    with tempfile.TemporaryDirectory() as d:
        src, dest = Path(d) / "src.db", Path(d) / "dest.db"
        src.write_bytes(b"x" * 4096)
        check("copy: a source at rest copies", copy_at_rest(src, dest) == src.stat().st_mtime_ns)
        real_copy = shutil.copyfile

        def growing(a, b):
            real_copy(a, b)
            with open(a, "ab") as f:
                f.write(b"y")

        shutil.copyfile = growing
        try:
            check("copy: a source that grows during its copy is refused", refused(copy_at_rest, src, Path(d) / "d2.db"))
        finally:
            shutil.copyfile = real_copy
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        kdir = d / "home" / ".khive"
        kdir.mkdir(parents=True)
        files = []
        for name, body in (("khive.db", b"m" * 4096), ("khive.db.events.db", b"e" * 8192)):
            (kdir / name).write_bytes(body)
            files.append({"source": f"/src/{name}", "staged_as": name, "bytes": len(body), "source_mtime_ns": 1,
                          "sha256": sha256_file(kdir / name)})
        record = {"verdict": "ok", "snapshot_id": "snap", "files": files}
        (d / "transfer.json").write_text(json.dumps(record))
        a = types.SimpleNamespace(snapshot_id="snap", staged_home=str(d), remove_scratch=False)
        live = d / "elsewhere" / ".khive"
        rows = adopt(a, d, live)
        check("adopt: a copy matching its transfer record is used in place",
              [r["adopted_in_place"] for r in rows] == [str(kdir / "khive.db"), str(kdir / "khive.db.events.db")])
        check("adopt: a snapshot id other than the transfer's is refused",
              refused(adopt, types.SimpleNamespace(snapshot_id="other", staged_home=str(d)), d, live))
        (kdir / "khive.db.events.db").write_bytes(b"E" * 8192)
        check("adopt: a same-size copy whose bytes differ is refused", refused(adopt, a, d, live))
        (kdir / "khive.db.events.db").write_bytes(b"e" * 8192)
        (kdir / "khive.db-wal").write_bytes(b"")
        check("adopt: a copy a server has opened (a sidecar exists) is refused", refused(adopt, a, d, live))
        (kdir / "khive.db-wal").unlink()
        (d / "transfer.json").write_text(json.dumps({**record, "verdict": "refused"}))
        check("adopt: a transfer whose own verdict is not ok is refused", refused(adopt, a, d, live))
        (d / "transfer.json").write_text(json.dumps(record))
        check("adopt: a staged home under the live store directory is refused", refused(adopt, a, d, (d / "home").resolve()))
        check("adopt: --remove-scratch is refused for a staged home", refused(
            scratch_dir, types.SimpleNamespace(staged_home=str(d), remove_scratch=True, bank=None), "t"))
        models, other = d / "models", d / "other-models"
        models.mkdir()
        other.mkdir()
        link_models(d / "home", models)
        check("models: a reused home keeps a link to the same models directory",
              not refused(link_models, d / "home", models)
              and os.readlink(d / "home" / ".lattice" / "models") == str(models))
        check("models: a reused home whose link points elsewhere is refused", refused(link_models, d / "home", other))
    check("spread: first and last kept, evenly spaced", spread(list(range(10)), 4) == [0, 3, 6, 9])
    check("spread: short lists come back whole", spread([1, 2], 5) == [1, 2] and spread([1, 2, 3], 0) == [])
    check("dig: finds a nested field", dig({"payload": {"data": {"signal": "useful"}}}, ("signal",)) == "useful")
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        (d / "cases.jsonl").write_text(json.dumps({"case_id": "c1", "query": "q", "T": "2026-10-28T12:00:00Z",
                                                   "needed_info": ni}) + "\n")
        (d / "bank.json").write_text(json.dumps({"bank_id": "b", "version": 1,
                                                 "cases_sha256": sha256_file(d / "cases.jsonl")}))
        check("bank: a frozen bank loads", len(load_bank(d)[1]) == 1)
        (d / "bank.json").write_text(json.dumps({"bank_id": "b", "version": 0,
                                                 "cases_sha256": sha256_file(d / "cases.jsonl")}))
        check("bank: version 0 is refused", refused(load_bank, d))
        (d / "bank.json").write_text(json.dumps({"bank_id": "b", "version": 1,
                                                 "cases_sha256": sha256_file(d / "cases.jsonl")}))
        with open(d / "cases.jsonl", "a") as f:
            f.write(json.dumps({"case_id": "c2", "query": "q", "T": "2026-10-28T12:00:00Z", "needed_info": ni}) + "\n")
        check("bank: an edited cases file is refused", refused(load_bank, d))
        (d / "cases.jsonl").write_text(json.dumps({"case_id": "c1", "query": "q", "T": "2026-10-28T12:00:00Z",
                                                   "needed_info": [{"ids": ["0000abcd"]}]}) + "\n")
        (d / "bank.json").write_text(json.dumps({"bank_id": "b", "version": 2,
                                                 "cases_sha256": sha256_file(d / "cases.jsonl")}))
        check("bank: a short id in needed_info is refused", refused(load_bank, d))

    def hits(*pairs):
        return [{"id": u[i], "rank_score": s} for i, s in pairs]

    check("compare: identical lists overlap 1.0 in the same order with no rank gap",
          compare(hits((0, .9), (1, .8)), hits((0, .9), (1, .8)))
          == {"overlap@10": 1.0, "same_order": True, "max_abs_rank_score_diff": 0.0})
    sw = compare(hits((0, .9), (1, .8)), hits((1, .85), (0, .7)))
    check("compare: the same set reordered keeps overlap 1.0, loses order, gap 0.2 on a shared id",
          sw["overlap@10"] == 1.0 and not sw["same_order"] and abs(sw["max_abs_rank_score_diff"] - 0.2) < 1e-9)
    check("compare: an unshared hit halves overlap and adds no rank gap",
          compare(hits((0, .9), (1, .8)), hits((0, .9), (2, .1)))
          == {"overlap@10": 0.5, "same_order": False, "max_abs_rank_score_diff": 0.0})
    ag = aggregate([{"overlap@10": 1.0, "same_order": True, "max_abs_rank_score_diff": 0.1},
                    {"overlap@10": 0.5, "same_order": False, "max_abs_rank_score_diff": 0.3}])
    check("aggregate: mean overlap, share in the same order, largest gap",
          ag == {"mean_overlap@10": 0.75, "same_order_share": 0.5, "max_abs_rank_score_diff": 0.3})

    class ScriptedBridge:
        def __init__(self, answers):
            self.answers, self.calls = list(answers), []

        def one(self, text):
            self.calls.append(text)
            return self.answers.pop(0)

    sb = ScriptedBridge([hits((0, .9), (1, .8)), hits((0, .9), (1, .8)), hits((1, .9), (0, .8))])
    row = calibrate_case(sb, {"case_id": "q1", "query": "q"}, "live-p", "twin-p")
    check("calibrate: live, replayed, live; the replay row reads the first live answer, the A/A row both live answers",
          ['profile_id="live-p"' in c for c in sb.calls] == [True, False, True] and 'profile_id="twin-p"' in sb.calls[1]
          and row["same_order"] and not row["aa"]["same_order"] and not row["replayed_vs_second_live"]["same_order"])
    print("SELF-TEST " + ("PASS" if not fails else f"FAIL ({len(fails)})"))
    return 1 if fails else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    cb = sub.add_parser("check-bank")
    cb.add_argument("bank")
    for name in ("run", "calibrate-replay"):
        p = sub.add_parser(name)
        p.add_argument("--kkernel", default="kkernel")
        p.add_argument("--bank", required=(name == "run"))
        p.add_argument("--store-main", default=None, help="the staged copy's main store file")
        p.add_argument("--store-events", default=None, help="the staged copy's events store file")
        p.add_argument("--staged-home", default=None,
                       help="a directory holding home/.khive/{khive.db,khive.db.events.db} and transfer.json from a "
                            "guarded transfer; verified and used in place instead of copied, and never removed")
        p.add_argument("--snapshot-id", required=True, help="written into every row")
        p.add_argument("--engines-file", required=True, help="the [[engines]] tables the store was embedded with")
        p.add_argument("--models-dir", default=None, help="linked as the scratch home's ~/.lattice/models")
        p.add_argument("--namespaces", required=True, help="comma-separated namespaces the bench actor reads")
        p.add_argument("--profile", required=True, help="the store's live profile id")
        p.add_argument("--control-id", required=True, help="a memory that must come back for its own content")
        p.add_argument("--feedback-actors", default="", help="comma-separated actors whose feedback events replay")
        p.add_argument("--feedback-since", default="2025-01-01T00:00:00Z")
        p.add_argument("--ready-max-s", type=int, default=3600)
        p.add_argument("--scratch-parent", default=None)
        p.add_argument("--remove-scratch", action="store_true",
                       help="remove the scratch home and its store copy at the end (kept by default)")
        p.add_argument("--copy-wal", action="store_true", help="stage a non-empty WAL with its store (no writer active)")
        if name == "run":
            p.add_argument("--arms", default=",".join(ARMS))
            p.add_argument("--out", required=True, help="a new directory: <arm>.jsonl per arm and run.json")
        else:
            p.add_argument("--n", type=int, default=50)
            p.add_argument("--queries-from-events", action="store_true",
                           help="compare on n distinct queries the profile served (recall_executed), not a bank")
            p.add_argument("--out", required=True, help="the calibration report (JSON)")
    sub.add_parser("self-test")
    args = ap.parse_args()
    if args.cmd == "check-bank":
        return cmd_check_bank(args)
    if args.cmd == "self-test":
        return self_test()
    if bool(args.staged_home) == bool(args.store_main or args.store_events) or (
            not args.staged_home and not (args.store_main and args.store_events)):
        raise BankError("take either --staged-home, or both --store-main and --store-events")
    if args.cmd == "calibrate-replay" and bool(args.bank) == args.queries_from_events:
        raise BankError("calibrate-replay takes exactly one of --bank and --queries-from-events")
    if (args.cmd == "calibrate-replay" or "feedback_asof" in args.arms.split(",")) and not args.feedback_actors:
        raise BankError("--feedback-actors is required to replay feedback events; an empty list would replay none")
    return cmd_run(args) if args.cmd == "run" else cmd_calibrate_replay(args)


if __name__ == "__main__":
    sys.exit(main())
