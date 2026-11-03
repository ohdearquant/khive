#!/usr/bin/env python3
"""Question-bank runner: score retrieval arms across frozen banks and profiles against one staged store copy.

A bank is a directory holding bank.json and cases.jsonl. bank.json names the bank (bank_id, version) and carries the
sha256 of cases.jsonl (cases_sha256); the runner refuses a bank whose cases file no longer matches, so a version is
frozen once written and a fix mints the next version. One case per line:

    {"case_id": "c001", "query": "...", "T": "2026-10-20T14:03:11Z",
     "needed_info": [{"statement": "...", "ids": ["<full uuid>", "<full uuid>"]}],
     "pass_predicate": "...", "tags": ["..."], "provenance": "..."}

`query` is the context the task had at instant T. An optional `namespace:<ns>` tag selects the recall namespace for
that case; only the first colon is a separator. `needed_info` lists the facts the task required; each member's ids
are one equivalence set (the same fact kept as a memory, a note and an entity counts once, whichever comes back).

Measures, per case and arm, over the ranked ids the arm returns:
    hit@k       1 when some member has an id in the top k, else 0
    coverage@k  the share of members with an id in the top k
    MRR@10      the reciprocal rank of the first id belonging to any member, 0 when none is in the top 10
for k = 5 and 10. Each bank writes one JSONL file per arm, keyed by query_id = case_id, so
`bootstrap.py --metrics` pairs two arms from the same bank.

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

`run` accepts repeated `--bank` and `--profile` flags. Bank ids and profiles must be distinct. The visible namespace
set is the sorted union of `--namespaces` and each case's optional `namespace:<ns>` tag; the union cannot be empty.
The tag value includes every character after its first colon. One invocation stages one copy and serves every bank
and profile. Output files are `<out>/<bank_id>/<arm>.jsonl`, except profile-on files, which are
`<out>/<bank_id>/profile_on.<profile>.jsonl`. `run.json` records bank metadata and namespace counts, profiles, the
visible namespace set, and metrics grouped by bank and file stem. `calibrate-replay` accepts one bank or
`--queries-from-events` and exactly one profile.

Before any case runs, a control memory (--control-id) must come back in the top 5 of its own content under both the
vector-only and the keyword-only strategy: that proves the copy is visible to the bench actor and that the configured
embedding model matches the store's vectors. The run waits (bounded by --ready-max-s) until the vector index answers
without a degraded flag.

`rebind-gate` decides whether one candidate profile may serve recall again. It takes two banks, one whose answers are in
the reader's own namespace and one whose answers are in the shared pool (`local`), and asks each question twice,
adjacent in time: with no profile, then with the candidate (`profile_id`). Every recall asks for 50 hits with score
breakdowns and timings, at the token budget the recall help in the copy gives as its cap (`--token-budget-cap` supplies
it when the help is read and states none; with neither, or when the help cannot be read, the gate refuses). Two rounds
run `--round-gap-s` apart, and every round serves each bank from a fresh `kkernel mcp` process. The pass is read in
each round separately and must hold in both:
    own-namespace bank   MRR@10 with the candidate is at least MRR@10 with no profile
    shared-pool bank     MRR@10 falls by at most 0.04 (exact arithmetic, so exactly 0.04 passes) and at most one
                         question is lost: in the no-profile top ten and outside the candidate's top ten
Nothing runs when the runtime has a brain_profile (config, environment or command line), when a recall binding resolves
for the caller, or when the two arms' caller, primary namespace or readable namespaces differ. A hit whose attribution is
not its arm's (`unspecified` with no served_by_profile_id for no profile; `profile` and the candidate's id for the
candidate) stops the run. A degraded recall, or a no-profile recall with no hits, voids its round, which runs again in
fresh processes; a question that voids three runs in a row stops the gate and is named. Exit status: 0 pass, 10 fail,
11 the run cannot be read (wrong attribution, repeated voids), 12 a required condition does not hold. The report adds, per
arm and round, hit@10, the lost and gained questions, widened questions, recalls under 50 hits, the top-10 hits whose
composite sits at its clamp (pre_entity_score 1.0; not available when a weighted reranker is configured), questions whose
top ten differs between the rounds, and whether the candidate's entity term is 1 for every hit.

The candidate must be on the staged copy: every round's server process reads it before its first recall and records it.
Without `--create-candidate-local-factor` the gate creates nothing, so the candidate must already be there (for example
a profile the frozen store holds). A profile's `local_factor` can only be set when it is created, so a different value
is a new profile. `--create-candidate-local-factor F` (a finite number in [0.5, 1], checked before anything is staged)
creates the candidate on the copy with that `local_factor` before round 1. One extra `kkernel mcp` process does it, as
the same bench actor and with the own-namespace bank's read set as the round processes: it reads the candidate and
refuses the run (exit 12) if the profile is already on the copy, calls `brain.create_profile` (consumer_kind recall,
left inactive, since a recall that names a profile by id serves it unless it is archived), and reads it back, where
`local_factor` must equal F, `total_events` must be 0 and the lifecycle must not be archived. It runs no recall before
the create: the create commits the server's in-memory brain state to the copy, and a recall's credits would be
committed with it. The responses, the pressure reading, the brain_profile check and the stderr path go in
rebind-gate.json under `candidate_created`. The create writes to the copy, so a copy adopted with `--staged-home` no
longer matches its transfer record afterwards and a later run on it is refused.

Subcommands:
    check-bank        validate a bank directory and print its population
    run               score repeated banks and profiles, writing <out>/<bank_id>/<arm>.jsonl plus run.json
    calibrate-replay  replay the --profile profile's whole explicit feedback history into a fresh profile and compare
                      the two on the bank's queries (top-10 overlap, rank-score differences on shared ids). Each query
                      is recalled live, then by the replayed profile, then live again, and the two live answers are
                      compared the same way: that A/A floor bounds how much of the live/replay difference is the
                      recall's own run-to-run variation rather than the replay
    rebind-gate       decide whether one candidate profile may serve recall again (above), writing
                      <out>/rebind-gate.json and <out>/rebind-gate.txt
    self-test         the measure, fusion, bank-check, response-parsing and rebind-gate decision arms, no kkernel
                      needed

Standard library only.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import math
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
import types
from fractions import Fraction
from pathlib import Path

UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
KS = (5, 10)
RRF_K = 60
POOL = 50
# Hits a budget-capped recall response must still carry for the top-k measures to be exact: the cap keeps a
# rank-order prefix of the ranked list, so the first max(KS) ranks are unchanged when at least that many survive.
SCORED_DEPTH = max(KS)
# Budget config sent with every memory.recall op this runner builds (see recall_op). memory.recall ranks, cuts the
# list to top_k, then keeps the rank-order prefix whose content, counted in bytes, fits default_token_budget x
# chars_per_token (defaults 4000 x 4) and stamps every kept hit truncated. The pack clamps a requested budget to its cap of 16000 tokens (apply_dos_caps in
# crates/khive-pack-memory/src/scoring.rs, enforced in handlers/recall.rs), so this asks for the largest budget the
# pack allows. A request config replaces the served recall config wholesale and serde defaults fill every field it
# omits; those defaults equal what this runner serves because the config it writes has no memory section.
RECALL_CONFIG = {"scoring": {"default_token_budget": 16000}}
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


def case_namespace(case: dict) -> str | None:
    values = [tag.split(":", 1)[1] for tag in case.get("tags", [])
              if isinstance(tag, str) and tag.startswith("namespace:")]
    if len(values) > 1:
        raise BankError(f"case {case.get('case_id')}: at most one namespace:<ns> tag is allowed")
    if values and not values[0]:
        raise BankError(f"case {case.get('case_id')}: namespace tag value must not be empty")
    return values[0] if values else None


def namespace_counts(cases: list[dict]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for case in cases:
        ns = case_namespace(case)
        if ns is not None:
            counts[ns] = counts.get(ns, 0) + 1
    return counts


def visible_namespace_union(configured: str, banks: list[list[dict]]) -> list[str]:
    values = {n for n in configured.split(",") if n}
    for cases in banks:
        values.update(namespace_counts(cases))
    if not values:
        raise BankError("the visible namespace union is empty")
    return sorted(values)


def safe_name(value: str, what: str) -> None:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", value):
        raise BankError(f"{what} {value!r} is not safe for a file name")


def require_distinct(values: list[str], what: str) -> None:
    if len(set(values)) != len(values):
        raise BankError(f"{what} must be distinct")


def require_profile_count(profiles: list[str], arm: str) -> None:
    if arm == "feedback_asof" and len(profiles) != 1:
        raise BankError("feedback_asof requires exactly one --profile")


def require_calibration_shape(banks: list[str], from_events: bool) -> None:
    if (bool(banks) == from_events) or (banks and len(banks) != 1):
        raise BankError("calibrate-replay takes exactly one bank or --queries-from-events")


def require_calibration_profiles(profiles: list[str]) -> None:
    if len(profiles) != 1:
        raise BankError("calibrate-replay takes exactly one --profile")


def require_composed_namespace(namespace: str | None) -> None:
    if namespace:
        raise BankError("composed refuses namespaced cases because search has no read-namespace filter")


def load_bank(bank_dir: Path) -> tuple[dict, list[dict]]:
    try:
        meta = json.loads((bank_dir / "bank.json").read_text())
    except (OSError, ValueError) as e:
        raise BankError(f"cannot read {bank_dir / 'bank.json'}: {e!r}") from None
    if not isinstance(meta, dict) or not isinstance(meta.get("bank_id"), str) or not meta["bank_id"]:
        raise BankError("bank.json lacks bank_id")
    safe_name(meta["bank_id"], "bank_id")
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
        case_namespace(c)
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


def recall_op(config: dict | None = None, **args) -> str:
    """A memory.recall op carrying a budget config: the runner's, or the one passed. Every recall the runner sends is
    built here."""
    return op("memory.recall", config=RECALL_CONFIG if config is None else config, **args)


def hit_list(result, what: str, depth: int | None = None) -> list[dict]:
    """The hit list of a recall or search result: a bare array, or an envelope with a results array. A degraded
    response is always refused, and so is an envelope carrying any flag (that includes the capped-empty envelope,
    which has no hit to score). A hit stamped truncated comes from a budget-capped response, which keeps a
    rank-order prefix of the ranked list: it is accepted only when the caller passes a scored depth and at least that
    many hits came back, so ranks 1..depth are exact. Without a depth any capped hit is refused."""
    def refuse(flag: str, value, returned: int):
        needed = "none given, a capped hit is not accepted" if depth is None else str(depth)
        raise BankError(f"{what}: {flag}={value!r}; {returned} hit(s) returned, scored depth required: {needed}; "
                        "not scored")

    if isinstance(result, list):
        hits = result
    elif isinstance(result, dict) and isinstance(result.get("results"), list):
        for flag in ("degraded", "truncated", "budget_capped", "truncated_for_budget"):
            if result.get(flag):
                refuse(flag, result.get(flag), len(result["results"]))
        hits = result["results"]
    else:
        raise BankError(f"{what}: unexpected response shape {type(result).__name__}")
    for h in hits:
        if not isinstance(h, dict):
            raise BankError(f"{what}: a hit is not an object")
        if h.get("degraded"):
            refuse("degraded", h.get("degraded"), len(hits))
        if h.get("truncated") and (depth is None or len(hits) < depth):
            refuse("truncated", h.get("truncated"), len(hits))
    return hits


def hit_id(h: dict, what: str) -> str:
    for field in ("full_id", "id"):
        v = h.get(field)
        if isinstance(v, str) and UUID_RE.fullmatch(v):
            return v
    raise BankError(f"{what}: a hit carries no full UUID (fields {sorted(h)})")


def mcp_argv(kkernel: str, db: Path) -> list[str]:
    """The command line of every `kkernel mcp` process this runner starts."""
    return [kkernel, "mcp", "--db", str(db), "--log", "warn"]


class Bridge:
    """One `kkernel mcp` stdio process. Each call sends one `request` with presentation=verbose (the full shape, full
    ids) and returns the per-op results in op order."""

    def __init__(self, kkernel: str, db: Path, env: dict, err_path: Path):
        self.err = open(err_path, "ab")
        self.proc = subprocess.Popen(mcp_argv(kkernel, db), stdin=subprocess.PIPE,
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
    """Copy the store into scratch/home/.khive (or adopt a transferred copy there) and return
    (main db path, env, manifest rows). The config is written per bank by write_config."""
    live = Path(os.path.expanduser("~/.khive")).resolve()
    home = scratch / "home"
    kdir = home / ".khive"
    if args.staged_home:
        manifest, sources = adopt(args, scratch, live), ()
        (kdir / "vol-locks").mkdir(exist_ok=True)
    else:
        (kdir / "vol-locks").mkdir(parents=True)
        manifest, sources = [], ((args.store_main, "khive.db"), (args.store_events, "khive.db.events.db"))
    # Every source is checked before any is copied, so a refusal on a later source leaves no copy of an earlier one.
    plan = []
    for src, name in sources:
        src = Path(src).resolve()
        if under(src, live):
            raise BankError(f"refusing {src}: it is under the caller's live store directory {live}")
        wal = Path(str(src) + "-wal")
        has_wal = wal.exists() and wal.stat().st_size > 0
        if has_wal and not args.copy_wal:
            raise BankError(f"refusing {src}: its WAL is not empty; stage a checkpointed copy, or pass --copy-wal "
                            "when no process writes the source")
        plan.append((src, wal, has_wal, name, kdir / name))
    for src, wal, has_wal, name, dest in plan:
        if has_wal:
            mtime = copy_at_rest(wal, Path(str(dest) + "-wal"))
            manifest.append({"source": str(wal), "staged_as": name + "-wal", "bytes": wal.stat().st_size,
                             "source_mtime_ns": mtime, "sha256": sha256_file(Path(str(dest) + "-wal"))})
        mtime = copy_at_rest(src, dest)
        manifest.append({"source": str(src), "staged_as": name, "bytes": dest.stat().st_size,
                         "source_mtime_ns": mtime, "sha256": sha256_file(dest)})
    if args.models_dir:
        link_models(home, Path(args.models_dir).resolve())
    h = str(home)
    env = {"HOME": h, "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8",
           "KHIVE_CONFIG": h + "/.khive/config.toml", "KHIVE_SOCKET": h + "/.khive/k.sock",
           "KHIVE_PID": h + "/.khive/k.pid", "KHIVE_LOCK": h + "/.khive/k.lock",
           "KHIVE_RECOVERER_LOCK": h + "/.khive/recoverer.lock", "KHIVE_VOLUME_LOCK_DIR": h + "/.khive/vol-locks",
           "KHIVE_DB": str(kdir / "khive.db"), "KHIVE_NO_DAEMON": "1"}
    return kdir / "khive.db", env, manifest


def read_engines(args) -> str:
    """The `--engines-file` text, checked to hold only [[engines]] tables."""
    engines = Path(args.engines_file).read_text()
    for line in engines.splitlines():
        s = line.strip()
        if s.startswith("[") and s != "[[engines]]":
            raise BankError(f"--engines-file holds only [[engines]] tables; found {s}")
    return engines


def write_config(scratch: Path, args, visible: list[str]) -> None:
    """Write the bench config of the staged copy, with `visible` as the actor's visible namespaces. It runs once per
    bank, before that bank's server starts; the staged store files are not touched."""
    kdir = scratch / "home" / ".khive"
    engines = read_engines(args)
    (kdir / "config.toml").write_text(
        f'[actor]\nid = "{BENCH_ACTOR}"\ndisplay_name = "{BENCH_ACTOR}"\nvisible_namespaces = {json.dumps(visible)}\n\n'
        f'{engines.strip()}\n\n[[backends]]\nname = "main"\nkind = "sqlite"\npath = "{kdir / "khive.db"}"\n')


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


def recall(br: Bridge, query: str, before: str | None, what: str, depth: int | None = None,
           **extra) -> list[dict]:
    """One recall of the pool. It never names a namespace: the server serves the call from its default namespace,
    where the named profiles live, and a case's namespace tag stays the bank's population label. A capped response
    is accepted only with a depth and at least that many hits."""
    if "namespace" in extra:
        raise BankError(f"{what}: a recall carries no namespace argument")
    hits = hit_list(br.one(recall_op(query=query, top_k=POOL, created_before=before, **extra)), what, depth)
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


def parse_bank_visible(specs: list[str] | None) -> dict[str, list[str]]:
    """`--bank-visible <bank_id>=<ns>[,<ns>...]`, repeated, as {bank_id: [namespace, ...]}."""
    out: dict[str, list[str]] = {}
    for spec in specs or []:
        bank_id, sep, names = spec.partition("=")
        if not sep or not bank_id:
            raise BankError(f"--bank-visible {spec!r}: expected <bank_id>=<ns>[,<ns>...]")
        listed = names.split(",")
        if any(not n for n in listed):
            raise BankError(f"--bank-visible {spec!r}: name at least one namespace and no empty one")
        require_distinct(listed, f"--bank-visible namespaces of bank {bank_id}")
        if bank_id in out:
            raise BankError(f"--bank-visible names bank {bank_id} more than once")
        out[bank_id] = listed
    return out


def require_bank_visible(banks: list[tuple[dict, list[dict]]], visible_by_bank: dict[str, list[str]]) -> None:
    """Every bank has a visible set, every set belongs to a given bank, and every case's namespace tag is in its
    bank's set. It runs before anything is staged."""
    given = [m["bank_id"] for m, _ in banks]
    stray = sorted(set(visible_by_bank) - set(given))
    if stray:
        raise BankError(f"--bank-visible names bank(s) {stray} that are not given with --bank")
    for meta, cases in banks:
        bank_id = meta["bank_id"]
        if bank_id not in visible_by_bank:
            raise BankError(f"bank {bank_id} has no --bank-visible set")
        allowed = set(visible_by_bank[bank_id])
        for c in cases:
            ns = case_namespace(c)
            if ns is not None and ns not in allowed:
                raise BankError(f"bank {bank_id} case {c.get('case_id')}: namespace tag {ns!r} is not in the bank's "
                                f"visible set {sorted(allowed)}")


BRAIN_ROW = "__brain__"


def load_expected_events(path: str, namespace: str, profiles: list[str]) -> dict:
    """The expected `total_events` of each profile, from a brain_profile_snapshots export: one JSON array per line,
    [profile_id, namespace, snapshot_json, updated_at]. The row is the one whose profile_id is `__brain__` and whose
    namespace is `namespace`; its snapshot holds profiles.<id>.total_events."""
    p = Path(path)
    try:
        raw = p.read_bytes()
    except OSError as e:
        raise BankError(f"cannot read --expect-profiles {p}: {e!r}") from None
    rows = []
    for n, line in enumerate(raw.decode("utf-8", "replace").splitlines(), 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except ValueError:
            raise BankError(f"{p} line {n}: not JSON") from None
        if not (isinstance(row, list) and len(row) >= 3):
            raise BankError(f"{p} line {n}: expected [profile_id, namespace, snapshot_json, updated_at]")
        if row[0] == BRAIN_ROW and row[1] == namespace:
            rows.append((n, row))
    if not rows:
        raise BankError(f"{p}: no {BRAIN_ROW} row for namespace {namespace!r}")
    if len(rows) > 1:
        raise BankError(f"{p}: {len(rows)} {BRAIN_ROW} rows for namespace {namespace!r} (lines "
                        f"{[n for n, _ in rows]}); expected one")
    n, row = rows[0]
    try:
        snapshot = json.loads(row[2]) if isinstance(row[2], str) else None
    except ValueError:
        snapshot = None
    held = snapshot.get("profiles") if isinstance(snapshot, dict) else None
    if not isinstance(held, dict):
        raise BankError(f"{p} line {n}: snapshot_json is not a JSON string holding a profiles object")
    events: dict[str, int] = {}
    for profile in profiles:
        entry = held.get(profile)
        total = entry.get("total_events") if isinstance(entry, dict) else None
        if not isinstance(total, int) or isinstance(total, bool):
            raise BankError(f"{p} line {n}: no integer total_events for profile {profile}")
        events[profile] = total
    return {"path": str(p), "sha256": hashlib.sha256(raw).hexdigest(), "namespace": namespace, "events": events}


def read_total_events(br: Bridge, profile: str) -> int:
    """The `total_events` this bank's server reports for the profile."""
    value = br.one(op("brain.profile", profile_id=profile))
    total = value.get("total_events") if isinstance(value, dict) else None
    if not isinstance(total, int) or isinstance(total, bool):
        raise BankError(f"profile {profile}: brain.profile returned no integer total_events (got {total!r})")
    return total


def require_served_events(profile: str, expected: int, served: int) -> None:
    """Stop the run when the served profile is not the frozen one."""
    if served != expected:
        raise BankError(f"profile {profile}: the server serves total_events {served}, the frozen read-back has "
                        f"{expected}; the bank would not run against the frozen profile state")


WRITING_ARMS = ("feedback_fresh", "feedback_asof")


def require_read_only_arms(arms: list[str]) -> None:
    """The served-state check needs the staged copy's brain state to stay the frozen one. The arms that call
    brain.create_profile and brain.feedback commit the server's brain state, credits included, to the staged copy, so
    after them the served state is no longer the frozen one: they cannot run with --expect-profiles."""
    written = [a for a in arms if a in WRITING_ARMS]
    if written:
        raise BankError(f"--expect-profiles cannot be combined with {', '.join(written)}: those arms write brain state "
                        f"to the staged copy, so the served state is no longer the frozen one after them")


def check_served_state(br: Bridge, profiles: list[str], expect: dict, record: list) -> None:
    """Each profile's served `total_events` must equal the frozen read-back. It runs in a bank's server process after
    the handshake and before that process's first recall (see open_bridge). Each compared pair is appended to
    `record` before the comparison, so a refused run still shows what was read."""
    for profile in profiles:
        want = expect["events"][profile]
        try:
            served = read_total_events(br, profile)
        except BankError as e:
            record.append({"profile": profile, "expected": want, "served": None, "error": str(e.code)})
            raise BankError(f"profile {profile}: expected total_events {want}, the server did not return the "
                            f"profile ({e.code})") from None
        record.append({"profile": profile, "expected": want, "served": served})
        require_served_events(profile, want, served)


def read_served_after(br: Bridge, profiles: list[str]) -> list[dict]:
    """The same profiles read again in the server's memory after the bank's arms. This is the in-process count: it
    includes the credits for the recalls just served, and the read-only arms do not write it to the staged copy, so
    it is not stored state. Recorded only: it never stops a run."""
    rows = []
    for profile in profiles:
        try:
            rows.append({"profile": profile, "in_process_total_events": read_total_events(br, profile)})
        except BankError as e:
            rows.append({"profile": profile, "error": str(e.code)})
    return rows


def wait_ready(br: Bridge, query: str, max_s: int) -> float:
    t0 = time.monotonic()
    while True:
        r = br.ops([recall_op(query=query, top_k=5, fusion_strategy="vector_only")])[0]
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
        ids = [hit_id(h, "control") for h in hit_list(br.one(recall_op(query=content[:400], top_k=5,
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
                      "needed_info_members": sum(len(c["needed_info"]) for c in cases),
                      "namespaces": namespace_counts(cases),
                      "without_namespace": sum(case_namespace(c) is None for c in cases)}))
    return 0


def stage_once(args, scratch: Path) -> tuple[Path, dict, dict]:
    """Stage the copy, once. Returns the staged store path, the server environment and the copy's record. Memory
    pressure is read here before the copy; every server start reads it again (run_banks before each bank's server,
    start() before its one server)."""
    pressure = {"before_copy": require_normal_pressure("before the copy")}
    db, env, manifest = stage(args, scratch)
    stores = [m for m in manifest if m["staged_as"] in ("khive.db", "khive.db.events.db")]
    skew_s = abs(stores[0]["source_mtime_ns"] - stores[1]["source_mtime_ns"]) / 1e9
    return db, env, {"staged": manifest, "source_mtime_skew_s": round(skew_s, 3), "memory_pressure": pressure}


def open_bridge(args, db: Path, env: dict, scratch: Path, probe_query: str, profiles: list[str],
                served_check=None, err_name: str = "kkernel-mcp.err"):
    """Start one `kkernel mcp` process on the staged copy, with the config now on disk, and wait until it answers
    undegraded. `served_check`, when given, runs right after the handshake and before the first recall of the
    process: a recall that names no profile is credited to the default profile in the server's memory, so a profile
    read made after the readiness and control recalls no longer shows the stored count. `profiles` are read once as
    an existence check, after those recalls. The server's stderr goes to `scratch / err_name`. Returns the bridge
    and its readiness record."""
    br = Bridge(args.kkernel, db, env, scratch / err_name)
    try:
        br.handshake()
        if served_check is not None:
            served_check(br)
        ready_s = wait_ready(br, probe_query, args.ready_max_s)
        ctl = control(br, args.control_id)
        for profile in profiles:
            br.one(op("brain.profile", profile_id=profile))
    except BaseException:
        br.close()
        raise
    return br, {"ready_s": round(ready_s, 1), "control": ctl}


def start(args, run_id: str, scratch: Path):
    """One server over the union of visible namespaces (args.visible_namespaces); calibrate-replay uses this."""
    banks = [load_bank(Path(p)) for p in args.bank] if args.bank else []
    meta, cases = banks[0] if banks else ({"bank_id": None, "version": None}, [])
    db, env, staged = stage_once(args, scratch)
    staged["memory_pressure"]["before_server"] = require_normal_pressure("before the server starts")
    write_config(scratch, args, args.visible_namespaces)
    br, ready = open_bridge(args, db, env, scratch, cases[0]["query"] if cases else "store ready probe", args.profile)
    base = {"snapshot_id": args.snapshot_id, "kkernel_version": kkernel_version(args.kkernel), "run_id": run_id}
    return banks, br, base, {**ready, **staged}


def scratch_dir(args, run_id: str) -> Path:
    if os.environ.get("KHIVE_DB"):
        raise BankError("refusing to run with KHIVE_DB set in the caller's environment; unset it")
    if args.bank:
        for bank in args.bank:
            load_bank(Path(bank))
    if args.staged_home:
        if args.remove_scratch:
            raise BankError("--remove-scratch does not apply to --staged-home: the runner did not make that copy")
        return Path(args.staged_home).resolve()
    return Path(tempfile.mkdtemp(prefix=f"khive-question-bank-{run_id}-", dir=args.scratch_parent))


def run_banks(args, arms, run_id, banks, visible_by_bank, expect, scratch: Path, out: Path, summary: dict) -> None:
    """Stage the copy once, then serve each bank from its own server, in sequence. Between banks only the config and
    the process change. Each bank's visible set is written to the config before its process starts; with an
    expected-profile file, the served profile state is checked in each bank's server process, after the handshake and
    before that process's first recall, and read again after the arms (recorded only). Memory pressure is read before
    each bank's server starts, and each server writes its stderr to its own file."""
    db, env, staged = stage_once(args, scratch)
    base = {"snapshot_id": args.snapshot_id, "kkernel_version": kkernel_version(args.kkernel), "run_id": run_id}
    summary.update(base)
    summary.update(staged)
    for n, (meta, cases) in enumerate(banks, 1):
        bank_id = meta["bank_id"]
        visible = visible_by_bank[bank_id]
        record = {"bank_id": bank_id, "version": meta["version"], "cases_sha256": meta["cases_sha256"],
                  "population": len(cases), "namespaces": namespace_counts(cases), "visible_namespaces": visible}
        summary["banks"].append(record)
        summary["results"][bank_id] = {}
        write_config(scratch, args, visible)
        record["memory_pressure"] = {"before_server": require_normal_pressure("before this bank's server starts")}
        err_name = f"kkernel-mcp-bank{n}.err"
        record["server_stderr"] = str(scratch / err_name)
        check = None
        if expect:
            record["served_state"] = {"pairs": []}
            check = lambda b, pairs=record["served_state"]["pairs"]: check_served_state(  # noqa: E731
                b, args.profile, expect, pairs)
        br, ready = open_bridge(args, db, env, scratch, cases[0]["query"] if cases else "store ready probe",
                                [] if expect else args.profile, served_check=check, err_name=err_name)
        record.update(ready)
        try:
            args._fresh_profile = None
            if "feedback_fresh" in arms:
                args._fresh_profile = f"bench-fresh-{run_id}-{bank_id}"
                br.one(op("brain.create_profile", name=args._fresh_profile,
                          description="question-bank arm: no feedback events"))
            run_arms(args, arms, run_id, cases, br, out / bank_id, summary,
                     {**base, "bank_id": bank_id, "bank_version": meta["version"], "population": len(cases)}, bank_id)
            if expect:
                record["served_state"]["after_in_process"] = read_served_after(br, args.profile)
        finally:
            br.close()
    summary["status"] = "complete"


def cmd_run(args) -> int:
    arms = [a for a in args.arms.split(",") if a]
    unknown = sorted(set(arms) - set(ARMS))
    if unknown or not arms:
        raise BankError(f"unknown arms {unknown}; choose from {', '.join(ARMS)}")
    banks = [load_bank(Path(p)) for p in args.bank]
    ids = [m["bank_id"] for m, _ in banks]
    require_distinct(ids, "bank ids")
    require_distinct(args.profile, "profile values")
    for p in args.profile:
        safe_name(p, "profile")
    require_profile_count(args.profile, "feedback_asof" if "feedback_asof" in arms else "")
    if "composed" in arms:
        for _, cases in banks:
            for c in cases:
                require_composed_namespace(case_namespace(c))
    visible_by_bank = parse_bank_visible(args.bank_visible)
    require_bank_visible(banks, visible_by_bank)
    expect = None
    if "profile_on" in arms and not args.expect_profiles:
        raise BankError("--expect-profiles is required when the profile_on arm runs")
    if args.expect_profiles:
        require_read_only_arms(arms)
        expect = load_expected_events(args.expect_profiles, args.expect_namespace, args.profile)
    read_engines(args)
    run_id = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dt%H%M%Sz")
    out = Path(args.out)
    scratch = scratch_dir(args, run_id)
    out.mkdir(parents=True, exist_ok=False)
    summary = {"status": "incomplete", "recall_config": RECALL_CONFIG, "banks": [], "profiles": args.profile,
               "bank_visible": visible_by_bank, "results": {}}
    if expect:
        summary["expect_profiles"] = {k: expect[k] for k in ("path", "sha256", "namespace")}
    try:
        run_banks(args, arms, run_id, banks, visible_by_bank, expect, scratch, out, summary)
    finally:
        try:
            finish_scratch(args, scratch, summary)
        finally:
            (out / "run.json").write_text(json.dumps(summary, indent=1) + "\n")
    return 0


def run_arms(args, arms, run_id, cases, br, out, summary, base, bank_id) -> None:
    out.mkdir(parents=True, exist_ok=True)
    fresh = getattr(args, "_fresh_profile", None)
    asof = None
    if "feedback_asof" in arms:
        asof = f"bench-asof-{run_id}-{bank_id}"
        br.one(op("brain.create_profile", name=asof, description="question-bank arm: feedback replayed as of T"))
    actors = [a for a in args.feedback_actors.split(",") if a]
    for arm in arms:
        profiles = args.profile if arm == "profile_on" else [None]
        for serving_profile in profiles:
            rows, extra = [], {}
            ordered = sorted(cases, key=lambda c: parse_instant(c["T"])) if arm == "feedback_asof" else cases
            fed_until, replay_counts = args.feedback_since, {"replayed": 0, "refused": 0, "refused_by_signal": {}}
            for c in ordered:
                what = f"{arm} {c['case_id']}"
                if arm in RECALL_ARMS:
                    hits = recall(br, c["query"], c["T"], what, depth=SCORED_DEPTH,
                                  **RECALL_ARMS[arm])
                    check_served(hits, arm, None, what)
                    ranked = [hit_id(h, what) for h in hits]
                elif arm == "profile_on":
                    hits = recall(br, c["query"], c["T"], what, profile_id=serving_profile,
                                  depth=SCORED_DEPTH)
                    check_served(hits, arm, serving_profile, what)
                    ranked = [hit_id(h, what) for h in hits]
                elif arm == "feedback_fresh":
                    hits = recall(br, c["query"], c["T"], what, profile_id=fresh,
                                  depth=SCORED_DEPTH)
                    check_served(hits, arm, fresh, what)
                    ranked = [hit_id(h, what) for h in hits]
                elif arm == "feedback_asof":
                    if parse_instant(c["T"]) > parse_instant(fed_until):
                        got = replay(br, feedback_events(br, actors, args.profile[0], fed_until, c["T"]), asof)
                        for k in ("replayed", "refused"):
                            replay_counts[k] += got[k]
                        for s, n in got["refused_by_signal"].items():
                            replay_counts["refused_by_signal"][s] = replay_counts["refused_by_signal"].get(s, 0) + n
                        fed_until = c["T"]
                    hits = recall(br, c["query"], c["T"], what, profile_id=asof,
                                  depth=SCORED_DEPTH)
                    check_served(hits, arm, asof, what)
                    ranked = [hit_id(h, what) for h in hits]
                    extra = {"feedback_replayed_so_far": replay_counts["replayed"]}
                else:  # composed
                    ns = case_namespace(c)
                    require_composed_namespace(ns)
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
                row_profile = (serving_profile if arm == "profile_on" else
                               fresh if arm == "feedback_fresh" else asof if arm == "feedback_asof" else None)
                rows.append({"query_id": c["case_id"], "arm": arm, "profile": row_profile,
                             "namespace": case_namespace(c), **base, **measures(ranked, c["needed_info"]),
                             "returned": len(ranked), "capped": any(h.get("truncated") for h in hits),
                             "top10": ranked[:10], **extra})
            stem = f"profile_on.{serving_profile}" if arm == "profile_on" else arm
            with open(out / f"{stem}.jsonl", "w") as f:
                for r in rows:
                    f.write(json.dumps(r) + "\n")
            n = len(rows)
            agg = {m: round(sum(r[m] for r in rows) / n, 4) for m in ("hit@5", "hit@10", "coverage@5", "coverage@10",
                                                                    "MRR@10")}
            summary["results"][bank_id][stem] = {"n": n, **agg, **({"profile": asof, "replay": replay_counts}
                                                                if arm == "feedback_asof" else {})}
            print(json.dumps({"bank_id": bank_id, "arm": arm, "profile": serving_profile, "n": n, **agg}), flush=True)


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
        return hit_list(br.one(recall_op(query=case["query"], top_k=10, profile_id=profile,
                                  namespace=case_namespace(case))), "calibrate")
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
    report = {"status": "incomplete", "recall_config": RECALL_CONFIG, "visible_namespaces": args.visible_namespaces}
    try:
        loaded, br, base, setup = start(args, run_id, scratch)
        meta, cases = loaded[0] if loaded else ({"bank_id": None, "version": None}, [])
        report.update(base)
        report.update(setup)
        try:
            twin = f"bench-replay-{run_id}"
            br.one(op("brain.create_profile", name=twin, description="question-bank calibration: full feedback replay"))
            now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
            actors = [a for a in args.feedback_actors.split(",") if a]
            if args.queries_from_events:
                cases, distinct = served_queries(br, actors, args.profile[0], args.feedback_since, now, args.n)
                if not cases:
                    raise BankError(f"no recall_executed event served by {args.profile[0]} for {actors}: nothing to compare")
                report["query_source"] = {"kind": "recall_executed", "profile": args.profile[0], "actors": actors,
                                          "distinct_queries": distinct, "sampled": len(cases)}
                report["population"] = len(cases)
            events = feedback_events(br, actors, args.profile[0], args.feedback_since, now)
            report["events_found"] = len(events)
            report.update(replay(br, events, twin))
            rows = [calibrate_case(br, c, args.profile[0], twin) for c in cases[: args.n]]
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


# ---- rebind-gate: may one profile serve recall again?
#
# Two banks (answers in the reader's own namespace, answers in the shared `local` pool) are each recalled twice per
# question, once with no profile and once with the candidate named by profile_id, at 50 hits with breakdowns and
# timings, in two rounds a few minutes apart. The pass is read in each round on its own and must hold in both.
# Everything that decides a response or a verdict is a plain function of data, so the self-test feeds it synthetic
# responses; only run_rebind_gate starts processes.

GATE_TOP_K = 50
GATE_TOP = 10
GATE_ROUNDS = 2
GATE_MAX_VOIDS_IN_A_ROW = 3
GATE_SHARED_MAX_LOSS = Fraction(4, 100)
GATE_SHARED_MAX_LOST_QUESTIONS = 1
EXIT_PASS, EXIT_FAIL, EXIT_VOID, EXIT_REFUSED = 0, 10, 11, 12
GATE_ARMS = ("none", "candidate")
GATE_ROLES = ("own", "shared")
# The range brain.create_profile accepts for a profile's local_factor.
GATE_LOCAL_FACTOR_MIN, GATE_LOCAL_FACTOR_MAX = 0.5, 1.0
GATE_CANDIDATE_DESCRIPTION = "Candidate profile created by the rebind-gate bench for one local_factor value."
GATE_PROFILE_KEYS = ("profile_id", "lifecycle", "local_factor", "total_events", "posterior_means")
GATE_CREATE_KEYS = ("created", "profile_id", "consumer_kind", "lifecycle", "description")


class GateStop(Exception):
    """The gate ends without a verdict because a run cannot be read: a hit served by the wrong profile, a question
    that voided its round three runs in a row, a response of an unreadable shape. `code` is the exit status."""

    code = EXIT_VOID


class GateRefused(GateStop):
    """The gate does not start, or stops, because a condition it requires does not hold."""

    code = EXIT_REFUSED


def require_store_source(args) -> None:
    if bool(args.staged_home) == bool(args.store_main or args.store_events) or (
            not args.staged_home and not (args.store_main and args.store_events)):
        raise BankError("take either --staged-home, or both --store-main and --store-events")


def parse_read_set(text: str, what: str) -> list[str]:
    listed = text.split(",")
    if any(not n for n in listed):
        raise BankError(f"{what} {text!r}: name at least one namespace and no empty one")
    require_distinct(listed, what)
    return listed


def require_bank_roles(own_cases: list[dict], shared_cases: list[dict]) -> None:
    """The shared-pool bank's tagged answers are all in `local`, and none of the own-namespace bank's is."""
    for c in shared_cases:
        ns = case_namespace(c)
        if ns is not None and ns != "local":
            raise GateRefused(f"shared-pool bank case {c['case_id']} is tagged namespace:{ns}; its answers must be in "
                              "local (are --own-bank and --shared-bank swapped?)")
    for c in own_cases:
        if case_namespace(c) == "local":
            raise GateRefused(f"own-namespace bank case {c['case_id']} is tagged namespace:local; its answers must be "
                              "in the reader's own namespace (are --own-bank and --shared-bank swapped?)")


def require_brain_profile_unset(config_text: str, env: dict, argv: list[str]) -> dict:
    """The serving process must start with no brain_profile: a recall without profile_id serves a configured one ahead
    of any binding, so the no-profile arm would be a profile arm. Checks the config the process reads, its environment
    and its command line; returns the reading to record."""
    set_in = []
    if any(re.search(r"\bbrain_profile\b\s*=", line.split("#", 1)[0]) for line in config_text.splitlines()):
        set_in.append("config.toml")
    if (env.get("KHIVE_BRAIN_PROFILE") or "").strip():
        set_in.append("KHIVE_BRAIN_PROFILE")
    if any(a == "--brain-profile" or a.startswith("--brain-profile=") for a in argv):
        set_in.append("--brain-profile")
    if set_in:
        raise GateRefused(f"the runtime has a brain_profile set ({', '.join(set_in)}): a recall without profile_id "
                          "would serve it, so the no-profile arm would not be the arm its name says")
    return {"config_toml": "unset", "env_KHIVE_BRAIN_PROFILE": "unset", "cli_flag": "unset"}


def binding_resolves(resolved) -> bool:
    """Whether a `brain.resolve(consumer_kind=recall)` answer matched a binding for the caller. Anything but an explicit
    false counts as matched; an answer without `matched_binding` cannot be read and is refused. The whole answer is
    recorded, and a profile that served the no-profile arm anyway shows in the hits' attribution."""
    if not isinstance(resolved, dict) or "matched_binding" not in resolved:
        raise GateRefused(f"brain.resolve returned no matched_binding field ({json.dumps(resolved)[:200]}): the "
                          "caller's recall bindings cannot be read")
    return resolved["matched_binding"] is not False


def require_no_binding(reads: dict) -> None:
    for where, resolved in reads.items():
        if binding_resolves(resolved):
            raise GateRefused(f"the caller has a recall binding ({where}: {json.dumps(resolved)[:300]}): a recall "
                              "without profile_id would be served by it, so the no-profile arm would not be the arm "
                              "its name says")


def scope_of(who) -> dict:
    """The caller, primary namespace and readable namespaces a `whoami` answer reports."""
    if (not isinstance(who, dict) or not isinstance(who.get("namespace"), str)
            or not isinstance(who.get("visible_namespaces"), list)):
        raise GateRefused(f"whoami returned no namespace scope ({json.dumps(who)[:200]})")
    return {"actor_id": who.get("actor_id"), "namespace": who["namespace"],
            "visible_namespaces": sorted(set(who["visible_namespaces"]))}


def require_same_scope(scopes: dict, read_set: list[str]) -> None:
    """Both arms run as the same caller with the same primary and readable namespaces, and the readable set is the
    bank's read set (the primary namespace is always readable)."""
    none, cand = scopes["none"], scopes["candidate"]
    if none != cand:
        raise GateRefused(f"the arms' caller or namespace scope differ: no profile {json.dumps(none)}, candidate "
                          f"{json.dumps(cand)}; the arms are not compared")
    expected = sorted(set(read_set) | {none["namespace"]})
    if none["visible_namespaces"] != expected:
        raise GateRefused(f"the readable namespaces are {none['visible_namespaces']}, the bank's read set (with the "
                          f"primary namespace {none['namespace']!r}) is {expected}")


def token_budget_cap_from_help(help_result) -> int | None:
    """The token-budget cap a verb's help states: the number after "cap" in the text that follows "token budget".
    None when it states none; two different numbers are refused."""
    texts, stack = [], [help_result]
    while stack:
        x = stack.pop()
        if isinstance(x, str):
            texts.append(x)
        elif isinstance(x, dict):
            stack.extend(x.values())
        elif isinstance(x, list):
            stack.extend(x)
    found = {int(m.group(1).replace("_", "").replace(",", ""))
             for t in texts for m in re.finditer(r"(?is)token[ _]budget[^\n]{0,160}?\bcap\b\W{0,6}(\d[\d_,]*)", t)}
    if len(found) > 1:
        raise GateRefused(f"the recall help states more than one token-budget cap: {sorted(found)}")
    return next(iter(found), None)


def resolve_token_budget_cap(help_result, operator_cap: int | None) -> tuple[int, str]:
    """The budget the gate asks for, and where it was read. The help in the copy decides; an operator-given cap is used
    only when the help states none, and must equal one it does state."""
    stated = token_budget_cap_from_help(help_result)
    if operator_cap is not None and operator_cap <= 0:
        raise GateRefused(f"--token-budget-cap must be positive, got {operator_cap}")
    if stated is not None and operator_cap is not None and operator_cap != stated:
        raise GateRefused(f"--token-budget-cap {operator_cap} differs from the cap {stated} the recall help states")
    if stated is not None:
        return stated, "help"
    if operator_cap is not None:
        return operator_cap, "operator"
    raise GateRefused("the recall help in the copy states no token-budget cap and no --token-budget-cap was given")


def gate_recall_config(cap: int) -> dict:
    """The request config of every gate recall: the budget at the pack cap, and no weighted reranker."""
    return {"scoring": {"default_token_budget": cap}, "reranker_weights": {}}


def gate_recall_op(case: dict, profile_id: str | None, config: dict) -> str:
    return recall_op(config=config, query=case["query"], top_k=GATE_TOP_K, created_before=case["T"],
                     include_breakdown=True, include_timings=True, profile_id=profile_id)


def check_gate_attribution(hits: list[dict], arm: str, want_profile: str | None) -> None:
    """Every no-profile hit reads `unspecified` with no served_by_profile_id; every candidate hit reads `profile` and
    names the candidate. Anything else stops the run: the arm would not be the arm its name says."""
    for h in hits:
        att, by = h.get("serve_attribution"), h.get("served_by_profile_id")
        ok = (att == "unspecified" and by is None) if want_profile is None else (att == "profile" and by == want_profile)
        if not ok:
            raise GateStop(f"arm {arm} expects {'serve_attribution unspecified and no served_by_profile_id' if want_profile is None else 'profile ' + want_profile}"
                           f", a hit was served as {att!r} by {by!r}")


def check_not_late(hits: list[dict], before: str) -> None:
    t = parse_instant(before)
    late = [h for h in hits if parse_instant(h.get("created_at")) >= t]
    if late:
        raise GateStop(f"{len(late)} hit(s) created at or after {before} despite created_before")


def split_recall(result) -> tuple[list[dict], dict]:
    """The hits and the envelope of a recall response: a bare array (no envelope), or an object with a results array."""
    if isinstance(result, list):
        hits, env = result, {}
    elif isinstance(result, dict) and isinstance(result.get("results"), list):
        hits, env = result["results"], result
    else:
        raise GateStop(f"unexpected recall response shape {type(result).__name__}")
    if not all(isinstance(h, dict) for h in hits):
        raise GateStop("a recall hit is not an object")
    return hits, env


def retrieval_rounds(timings: dict) -> int:
    n = timings.get("retrieval_rounds")
    if isinstance(n, int) and not isinstance(n, bool):
        return n
    rounds = timings.get("rounds")
    if not isinstance(rounds, list):
        raise GateStop("the timings report no retrieval rounds")
    return len(rounds)


def gate_recall_read(result, arm: str, want_profile: str | None, before: str | None = None) -> dict:
    """Read one gate recall. Returns {"hits", "void", "degraded", "rounds", "capped"}; `void` is the reason this
    recall voids its round, or None.

    A recall voids its round when it reports degraded retrieval (each hit stamped `degraded`, or an empty response with
    `degraded: true`), when it is empty in the verbose envelope returned for more than one embedding model (that shape
    carries no top-level `degraded`, so an empty response cannot be told from a degraded one), or when it is a
    no-profile recall with no hits. A candidate recall with no hits is a miss. A hit attributed to the wrong profile, a
    late hit, a missing timings report and a hit without the breakdown terms stop the gate instead."""
    hits, env = split_recall(result)
    check_gate_attribution(hits, arm, want_profile)
    if before:
        check_not_late(hits, before)
    timings = env.get("timings")
    if not isinstance(timings, dict):
        raise GateStop("the response carries no timings although include_timings was asked")
    for h in hits:
        b = h.get("breakdown")
        for field in ("pre_entity_score", "entity_term"):
            v = b.get(field) if isinstance(b, dict) else None
            if isinstance(v, bool) or not isinstance(v, (int, float)):
                raise GateRefused(f"a hit has no breakdown.{field} although include_breakdown was asked: this build "
                                  "does not report the terms of rank_score")
    stamped, flagged = any(h.get("degraded") for h in hits), bool(env.get("degraded"))
    void = None
    if stamped or flagged:
        void = "degraded:" + ("hits_stamped" if stamped else "empty_flagged" if not hits else "envelope_flagged")
    elif not hits and "candidates" in env:
        void = "empty_multi_model_envelope"
    elif not hits and arm == "none":
        void = "no_hits_without_profile"
    return {"hits": hits, "void": void, "degraded": stamped or flagged, "rounds": retrieval_rounds(timings),
            "capped": any(h.get("truncated") for h in hits) or bool(env.get("truncated"))}


def summarize_recall(read: dict, what: str) -> dict:
    """What the report keeps of one clean recall: the ranked ids, how many hits came back, how many retrieval rounds
    ran, the top-10 hits whose composite sits at its clamp, and how many hits carry an entity term other than 1."""
    hits = read["hits"]
    return {"ids": list(dict.fromkeys(hit_id(h, what) for h in hits)), "returned": len(hits), "rounds": read["rounds"],
            "capped": read["capped"],
            "clamped_top10": sum(1 for h in hits[:GATE_TOP] if h["breakdown"]["pre_entity_score"] == 1.0),
            "entity_hits": len(hits), "entity_not_one": sum(1 for h in hits if h["breakdown"]["entity_term"] != 1.0),
            "weights_served": sorted({str(h["breakdown"].get("weights_served")) for h in hits})}


def play_bank(cases: list[dict], ask, cand: str, bank_id: str) -> dict:
    """One pass over a bank. For each case, in order, the no-profile recall and then the candidate recall, adjacent in
    time. `ask(case, profile_id)` returns the raw response. A recall that voids its round is listed and its case is
    left out; the caller discards the pass when anything voided."""
    questions, voids = [], []
    counts = {a: {"recalls": 0, "degraded": 0, "returned_fewer_than_top_k": 0, "capped": 0, "widened": []}
              for a in GATE_ARMS}
    for case in cases:
        got = {}
        for arm, pid in (("none", None), ("candidate", cand)):
            what = f"{bank_id} {case['case_id']} {arm}"
            read = gate_recall_read(ask(case, pid), arm, pid, case["T"])
            c = counts[arm]
            c["recalls"] += 1
            c["degraded"] += int(read["degraded"])
            if read["void"]:
                voids.append({"bank": bank_id, "case_id": case["case_id"], "arm": arm, "reason": read["void"]})
                continue
            got[arm] = summarize_recall(read, what)
            c["returned_fewer_than_top_k"] += int(got[arm]["returned"] < GATE_TOP_K)
            c["capped"] += int(got[arm]["capped"])
            if got[arm]["rounds"] > 1:
                c["widened"].append(case["case_id"])
        if len(got) == len(GATE_ARMS):
            questions.append({"case_id": case["case_id"], **got})
    return {"bank_id": bank_id, "questions": questions, "voids": voids, "counts": counts}


def next_streaks(prev: dict, voided: set) -> dict:
    """Runs in a row in which each question voided, after one more run: a question that voided this run adds one,
    every other question starts again from zero."""
    return {k: prev.get(k, 0) + 1 for k in voided}


def first_gold_rank(ids: list[str], gold: set) -> int | None:
    return next((i for i, x in enumerate(ids, 1) if x in gold), None)


def question_outcome(case: dict, q: dict) -> dict:
    gold = set().union(*(set(m["ids"]) for m in case["needed_info"]))
    out = {"case_id": case["case_id"]}
    for arm in GATE_ARMS:
        r = first_gold_rank(q[arm]["ids"], gold)
        inside = r is not None and r <= GATE_TOP
        out[arm] = {"rank": r, "hit": inside, "mrr": Fraction(1, r) if inside else Fraction(0)}
    return out


def bank_metrics(cases: list[dict], questions: list[dict]) -> dict:
    """MRR@10 and hit@10 per arm as exact fractions, the questions whose answer is in the no-profile top ten and outside
    the candidate's (lost) and the reverse (gained), each with where the answer was in the arm that had it outside its
    top ten: absent from its 50 hits, or at ranks 11 to 50."""
    by_id = {c["case_id"]: c for c in cases}
    outs = [question_outcome(by_id[q["case_id"]], q) for q in questions]
    n = len(outs)
    if n == 0:
        raise GateStop("a bank scored no question")

    def moved(a: str, b: str) -> list[dict]:
        return [{"case_id": o["case_id"], f"{a}_rank": o[a]["rank"], f"{b}_rank": o[b]["rank"],
                 "where": "absent_from_50" if o[b]["rank"] is None else "ranks_11_50"}
                for o in outs if o[a]["hit"] and not o[b]["hit"]]

    return {"n": n, "mrr": {a: sum(o[a]["mrr"] for o in outs) / n for a in GATE_ARMS},
            "hit10": {a: Fraction(sum(o[a]["hit"] for o in outs), n) for a in GATE_ARMS},
            "lost": moved("none", "candidate"), "gained": moved("candidate", "none")}


def own_bank_passes(m: dict) -> bool:
    return m["mrr"]["candidate"] >= m["mrr"]["none"]


def shared_bank_passes(m: dict) -> bool:
    return (m["mrr"]["none"] - m["mrr"]["candidate"] <= GATE_SHARED_MAX_LOSS
            and len(m["lost"]) <= GATE_SHARED_MAX_LOST_QUESTIONS)


def gate_verdict(rounds: list[dict]) -> dict:
    """`rounds` holds each round's {"own": metrics, "shared": metrics}. The pass is read in each round separately and
    must hold in every round, on both banks."""
    if not rounds:
        raise GateStop("no round to read")
    per = []
    for r in rounds:
        own, shared = own_bank_passes(r["own"]), shared_bank_passes(r["shared"])
        per.append({"own": own, "shared": shared, "pass": own and shared})
    return {"rounds": per, "pass": all(p["pass"] for p in per)}


def top10_differences(first: dict, second: dict) -> dict:
    """Per arm, the questions whose ordered top ten differs between two rounds of one bank."""
    a, b = ({q["case_id"]: q for q in rb["questions"]} for rb in (first, second))
    if set(a) != set(b):
        raise GateStop("the two rounds scored different questions")
    return {arm: sum(1 for cid in a if a[cid][arm]["ids"][:GATE_TOP] != b[cid][arm]["ids"][:GATE_TOP])
            for arm in GATE_ARMS}


def f6(x: Fraction) -> float:
    return round(float(x), 6)


def metrics_json(m: dict) -> dict:
    return {"n": m["n"], "MRR@10": {a: f6(m["mrr"][a]) for a in GATE_ARMS},
            "MRR@10_exact": {a: str(m["mrr"][a]) for a in GATE_ARMS},
            "hit@10": {a: f6(m["hit10"][a]) for a in GATE_ARMS},
            "mrr_loss": f6(m["mrr"]["none"] - m["mrr"]["candidate"]), "lost": m["lost"], "gained": m["gained"]}


def build_gate_report(profile: str, banks: dict, rounds: list[dict], reranker_weights: dict) -> dict:
    """The record of a finished gate: verdict, and every figure reported beside it. `rounds` holds, per round, the
    accepted pass of each bank (`banks`) and the round's bookkeeping."""
    metrics = [{role: bank_metrics(banks[role]["cases"], r["banks"][role]["questions"]) for role in GATE_ROLES}
               for r in rounds]
    verdict = gate_verdict(metrics)
    clamp_available = not reranker_weights
    out_rounds = []
    for r, m, v in zip(rounds, metrics, verdict["rounds"]):
        entry = {"round": r["round"], "started_at": r["started_at"], "accepted_attempt": r["accepted_attempt"],
                 "discarded_attempts": r["discarded_attempts"], "degraded_recalls": r["degraded_recalls"],
                 "pass": v["pass"]}
        for role in GATE_ROLES:
            rb = r["banks"][role]
            entry[role] = {
                **metrics_json(m[role]), "pass": v[role],
                "widened": {a: rb["counts"][a]["widened"] for a in GATE_ARMS},
                "returned_fewer_than_50": {a: rb["counts"][a]["returned_fewer_than_top_k"] for a in GATE_ARMS},
                "budget_capped": {a: rb["counts"][a]["capped"] for a in GATE_ARMS},
                "top10_pre_entity_score_1": {a: (sum(q[a]["clamped_top10"] for q in rb["questions"])
                                                 if clamp_available else None) for a in GATE_ARMS}}
        out_rounds.append(entry)
    questions = [q for r in rounds for role in GATE_ROLES for q in r["banks"][role]["questions"]]
    cand_hits, cand_not_one = (sum(q["candidate"][k] for q in questions) for k in ("entity_hits", "entity_not_one"))
    report = {
        "verdict": "PASS" if verdict["pass"] else "FAIL", "profile": profile, "rounds": out_rounds,
        "mean_of_rounds": {role: {"MRR@10": {a: f6(sum(m[role]["mrr"][a] for m in metrics) / len(metrics))
                                             for a in GATE_ARMS},
                                  "hit@10": {a: f6(sum(m[role]["hit10"][a] for m in metrics) / len(metrics))
                                             for a in GATE_ARMS}} for role in GATE_ROLES},
        "candidate_entity_term": {"hits": cand_hits, "not_one": cand_not_one,
                                  "is_1_for_every_hit": cand_hits > 0 and cand_not_one == 0},
        "reranker": {"reranker_weights": reranker_weights, "configured": not clamp_available,
                     "pre_entity_score_1_count": "available" if clamp_available else "not available"},
        "weights_served": sorted({w for q in questions for a in GATE_ARMS for w in q[a]["weights_served"]})}
    if len(rounds) == 2:
        report["top10_differs_between_rounds"] = {role: top10_differences(rounds[0]["banks"][role],
                                                                          rounds[1]["banks"][role])
                                                  for role in GATE_ROLES}
    return report


def render_summary(report: dict, banks: dict, candidate_line: str) -> str:
    lines = [f"rebind-gate {report['profile']}: {report['verdict']}", candidate_line]
    for role in GATE_ROLES:
        b = banks[role]
        lines.append(f"{role} bank {b['meta']['bank_id']} v{b['meta']['version']}: {len(b['cases'])} questions, "
                     f"read set {','.join(b['visible'])}")
    for r in report["rounds"]:
        for role in GATE_ROLES:
            m = r[role]
            extra = f" loss={m['mrr_loss']} lost={len(m['lost'])}" if role == "shared" else ""
            lines.append(f"round {r['round']} {role}: MRR@10 none={m['MRR@10']['none']} candidate="
                         f"{m['MRR@10']['candidate']} hit@10 none={m['hit@10']['none']} candidate="
                         f"{m['hit@10']['candidate']}{extra} gained={len(m['gained'])} -> "
                         f"{'pass' if m['pass'] else 'FAIL'}")
        lines.append(f"round {r['round']}: degraded recalls {r['degraded_recalls']}, discarded runs "
                     f"{len(r['discarded_attempts'])}")
    for role in GATE_ROLES:
        mean = report["mean_of_rounds"][role]
        lines.append(f"mean of rounds {role}: MRR@10 none={mean['MRR@10']['none']} candidate="
                     f"{mean['MRR@10']['candidate']} hit@10 none={mean['hit@10']['none']} candidate="
                     f"{mean['hit@10']['candidate']}")
    for role, d in report.get("top10_differs_between_rounds", {}).items():
        lines.append(f"top ten differs between rounds, {role}: no profile {d['none']}, candidate {d['candidate']}")
    lines.append(f"candidate entity term is 1 for every hit: {report['candidate_entity_term']['is_1_for_every_hit']} "
                 f"({report['candidate_entity_term']['not_one']} of {report['candidate_entity_term']['hits']} "
                 "hits differ)")
    lines.append(f"top-10 hits with pre_entity_score 1.0: {report['reranker']['pre_entity_score_1_count']}"
                 f" (reranker weights {json.dumps(report['reranker']['reranker_weights'])})")
    lines.append("details per round (lost and gained questions, widened questions, recalls under 50 hits) are in "
                 "the JSON record")
    return "\n".join(lines)


def gate_preflight(br, role: str, bank: dict, cand: str, args, state: dict, proc: dict) -> None:
    """Runs in each bank's server process right after the handshake, before its first recall. Records the caller's
    scope per arm and refuses arms that differ or a readable set that is not the bank's, reads the caller's recall
    bindings and refuses when one resolves, reads the candidate, and (once) reads the token-budget cap."""
    scopes = {arm: scope_of(br.one(op("whoami"))) for arm in GATE_ARMS}
    proc["scopes"] = scopes
    require_same_scope(scopes, bank["visible"])
    primary = scopes["none"]["namespace"]
    reads = {"default": br.one(op("brain.resolve", consumer_kind="recall")),
             f"namespace {primary}": br.one(op("brain.resolve", consumer_kind="recall", namespace=primary))}
    proc["bindings"] = reads
    require_no_binding(reads)
    prof = br.one(op("brain.profile", profile_id=cand))
    proc["candidate_profile"] = ({k: prof.get(k) for k in ("profile_id", "lifecycle", "local_factor", "total_events",
                                                          "posterior_means")} if isinstance(prof, dict) else prof)
    if state["config"] is None:
        try:
            help_result = br.one(op("memory.recall", help=True))
        except BankError as e:
            # A help that could not be read states nothing either way, so the cap is unknown: the operator cap is
            # never taken in its place.
            state["help_error"] = str(e.code)
            raise GateRefused(f"the recall help in the copy could not be read ({e.code}), so its token-budget cap is "
                              "unknown") from e
        cap, source = resolve_token_budget_cap(help_result, args.token_budget_cap)
        state.update(cap=cap, cap_source=source, config=gate_recall_config(cap))


def require_local_factor(value) -> float:
    """The `local_factor` a created candidate gets: a finite number in the range brain.create_profile accepts."""
    if (isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value)
            or not GATE_LOCAL_FACTOR_MIN <= value <= GATE_LOCAL_FACTOR_MAX):
        raise GateRefused(f"--create-candidate-local-factor must be a finite number in [{GATE_LOCAL_FACTOR_MIN}, "
                          f"{GATE_LOCAL_FACTOR_MAX}], got {value!r}")
    return float(value)


def trimmed(value, keys: tuple[str, ...]):
    """The named fields of a record answer, or the answer itself when it is not a record."""
    return {k: value.get(k) for k in keys} if isinstance(value, dict) else value


def require_candidate_absent(br, cand: str, created: dict) -> None:
    """The candidate is not on the copy yet. A profile `brain.profile` does not know is answered with the error
    `not found: profile "<id>"`; that is the expected case. An answer means the profile exists, and any other error
    means the copy cannot be read as holding no such profile; both refuse. The read is recorded either way."""
    try:
        found = br.one(op("brain.profile", profile_id=cand))
    except BankError as e:
        message = str(e.code)
        created["before"] = {"error": message}
        if f'not found: profile "{cand}"' in message.replace("\\", ""):
            return
        raise GateRefused(f"profile {cand} could not be read on the copy before the create ({message}), so it is not "
                          "known to be absent") from e
    created["before"] = trimmed(found, GATE_PROFILE_KEYS)
    raise GateRefused(f"--create-candidate-local-factor creates a new profile and {cand} is already on the copy")


def require_created_as_asked(cand: str, local_factor: float, profile) -> None:
    """The profile read back after the create carries the asked-for `local_factor` (an exact numeric comparison with
    the value the runtime returns: it stores the number as given), no events, and is not archived."""
    if not isinstance(profile, dict):
        raise GateRefused(f"profile {cand} read back after the create is not a record ({json.dumps(profile)[:200]})")
    got, events = profile.get("local_factor"), profile.get("total_events")
    if isinstance(got, bool) or not isinstance(got, (int, float)) or got != local_factor:
        raise GateRefused(f"profile {cand} read back after the create has local_factor {got!r}, not the "
                          f"{local_factor} asked for")
    if not isinstance(events, int) or isinstance(events, bool) or events != 0:
        raise GateRefused(f"profile {cand} read back after the create has total_events {events!r}, not 0")
    if profile.get("lifecycle") == "archived":
        raise GateRefused(f"profile {cand} read back after the create is archived, so a recall naming it is refused")


def create_gate_candidate(args, scratch: Path, db: Path, env: dict, read_set: list[str], cand: str,
                          local_factor: float, created: dict) -> None:
    """Create the candidate on the staged copy, in one server process of its own. It runs the checks every round
    process runs, as the same bench actor and with the own-namespace bank's read set, and it runs no recall: the create
    commits the server's in-memory brain state to the copy, and a recall's credits would be committed with it. No
    activation step follows, since a recall that names a profile by id serves it whatever its lifecycle but archived.
    `created` is the record, filled as the steps run so a refusal still shows what was read."""
    write_config(scratch, args, read_set)
    config_text = (scratch / "home" / ".khive" / "config.toml").read_text()
    err_name = "kkernel-mcp-gate-create.err"
    created["server_stderr"] = str(scratch / err_name)
    created["brain_profile"] = require_brain_profile_unset(config_text, env, mcp_argv(args.kkernel, db))
    created["memory_pressure"] = require_normal_pressure("before the candidate-creating server starts")
    br = Bridge(args.kkernel, db, env, scratch / err_name)
    try:
        br.handshake()
        require_candidate_absent(br, cand, created)
        created["create"] = trimmed(br.one(op("brain.create_profile", name=cand, consumer_kind="recall",
                                              local_factor=local_factor, description=GATE_CANDIDATE_DESCRIPTION)),
                                    GATE_CREATE_KEYS)
        after = br.one(op("brain.profile", profile_id=cand))
        created["after"] = trimmed(after, GATE_PROFILE_KEYS)
        require_created_as_asked(cand, local_factor, after)
    finally:
        br.close()


def candidate_origin_line(profile: str, record: dict) -> str:
    """One summary line: whether this run created the candidate or found it on the copy, and the `local_factor` the
    round processes read on it."""
    read = sorted({json.dumps(p["candidate_profile"].get("local_factor")) for p in record["processes"]
                   if isinstance(p.get("candidate_profile"), dict)})
    seen = f"local_factor read by the round processes: {', '.join(read) or 'not read'}"
    created = record.get("candidate_created")
    if created is None:
        return f"candidate {profile}: found on the copy, not created by this run; {seen}"
    return f"candidate {profile}: created by this run with local_factor {created['local_factor']}; {seen}"


def run_gate_attempt(args, scratch: Path, db: Path, env: dict, banks: dict, cand: str, state: dict, tag: str,
                     record: dict) -> dict:
    """One pass over both banks, each in its own fresh server process on the same staged copy."""
    results = {}
    for role in GATE_ROLES:
        bank = banks[role]
        write_config(scratch, args, bank["visible"])
        config_text = (scratch / "home" / ".khive" / "config.toml").read_text()
        err_name = f"kkernel-mcp-gate-{tag}-{role}.err"
        proc = {"tag": tag, "bank": role, "server_stderr": str(scratch / err_name)}
        record["processes"].append(proc)
        proc["brain_profile"] = require_brain_profile_unset(config_text, env, mcp_argv(args.kkernel, db))
        proc["memory_pressure"] = require_normal_pressure("before this bank's server starts")
        br, ready = open_bridge(args, db, env, scratch, bank["cases"][0]["query"], [cand],
                                served_check=lambda b, role=role, bank=bank, proc=proc: gate_preflight(
                                    b, role, bank, cand, args, state, proc),
                                err_name=err_name)
        proc.update(ready)
        try:
            results[role] = play_bank(bank["cases"], lambda case, pid: br.one(gate_recall_op(case, pid, state["config"])),
                                      cand, bank["meta"]["bank_id"])
        finally:
            br.close()
    return results


def gate_sleep(seconds: float) -> None:
    time.sleep(seconds)


def run_gate_rounds(args, scratch: Path, db: Path, env: dict, banks: dict, cand: str, state: dict,
                    record: dict) -> list[dict]:
    """Two rounds, a gap apart. A pass over both banks with any recall that voids its round is discarded and run
    again in fresh processes; a question that voids three runs in a row stops the gate."""
    streaks: dict = {}
    rounds = []
    for round_no in range(1, GATE_ROUNDS + 1):
        if round_no > 1:
            gate_sleep(args.round_gap_s)
        rnd = {"round": round_no, "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
               "discarded_attempts": [], "degraded_recalls": {a: 0 for a in GATE_ARMS}}
        record["rounds"].append(rnd)
        for attempt in range(1, args.max_round_runs + 1):
            results = run_gate_attempt(args, scratch, db, env, banks, cand, state, f"r{round_no}a{attempt}", record)
            voids = [v for r in results.values() for v in r["voids"]]
            for r in results.values():
                for a in GATE_ARMS:
                    rnd["degraded_recalls"][a] += r["counts"][a]["degraded"]
            if not voids:
                rnd.update(accepted_attempt=attempt, banks=results)
                streaks = {}
                break
            rnd["discarded_attempts"].append({"attempt": attempt, "voids": voids})
            streaks = next_streaks(streaks, {(v["bank"], v["case_id"]) for v in voids})
            stuck = sorted(k for k, n in streaks.items() if n >= GATE_MAX_VOIDS_IN_A_ROW)
            if stuck:
                rnd["stopped_on"] = [{"bank": b, "case_id": c, "voided_runs_in_a_row": streaks[(b, c)]}
                                     for b, c in stuck]
                raise GateStop(f"round {round_no}: " + "; ".join(
                    f"question {c} of bank {b} voided {streaks[(b, c)]} runs in a row ("
                    + ", ".join(sorted({v['reason'] for v in voids if (v['bank'], v['case_id']) == (b, c)})) + ")"
                    for b, c in stuck))
        else:
            raise GateStop(f"round {round_no} did not run clean in {args.max_round_runs} runs")
        rounds.append(rnd)
    return rounds


def run_rebind_gate(args) -> int:
    require_store_source(args)
    own_meta, own_cases = load_bank(Path(args.own_bank))
    shared_meta, shared_cases = load_bank(Path(args.shared_bank))
    require_distinct([own_meta["bank_id"], shared_meta["bank_id"]], "bank ids")
    safe_name(args.profile, "profile")
    local_factor = (None if args.create_candidate_local_factor is None
                    else require_local_factor(args.create_candidate_local_factor))
    require_bank_roles(own_cases, shared_cases)
    visible = {own_meta["bank_id"]: parse_read_set(args.own_visible, "--own-visible"),
               shared_meta["bank_id"]: parse_read_set(args.shared_visible, "--shared-visible")}
    require_bank_visible([(own_meta, own_cases), (shared_meta, shared_cases)], visible)
    banks = {"own": {"meta": own_meta, "cases": own_cases, "visible": visible[own_meta["bank_id"]],
                     "namespaces": namespace_counts(own_cases)},
             "shared": {"meta": shared_meta, "cases": shared_cases, "visible": visible[shared_meta["bank_id"]],
                        "namespaces": namespace_counts(shared_cases)}}
    read_engines(args)
    args.bank = [args.own_bank, args.shared_bank]
    run_id = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dt%H%M%Sz")
    out = Path(args.out)
    if out.exists():
        raise BankError(f"refusing to overwrite {out}")
    scratch = scratch_dir(args, run_id)
    out.mkdir(parents=True)
    record = {"status": "incomplete", "profile": args.profile, "recall_shape": {
        "top_k": GATE_TOP_K, "include_breakdown": True, "include_timings": True, "created_before": "each case's T"},
              "banks": {role: {"bank_id": b["meta"]["bank_id"], "version": b["meta"]["version"],
                               "cases_sha256": b["meta"]["cases_sha256"], "population": len(b["cases"]),
                               "namespaces": b["namespaces"], "read_set": b["visible"]}
                        for role, b in banks.items()},
              "processes": [], "rounds": []}
    for role, b in banks.items():
        print(f"{role} bank {b['meta']['bank_id']} v{b['meta']['version']}: {len(b['cases'])} questions, read set "
              f"{','.join(b['visible'])}", flush=True)
    state = {"cap": None, "cap_source": None, "config": None}
    code, stop = EXIT_REFUSED, None
    try:
        db, env, staged = stage_once(args, scratch)
        record.update({"snapshot_id": args.snapshot_id, "kkernel_version": kkernel_version(args.kkernel),
                       "run_id": run_id, **staged})
        if local_factor is not None:
            record["candidate_created"] = created = {"local_factor": local_factor}
            create_gate_candidate(args, scratch, db, env, banks["own"]["visible"], args.profile, local_factor, created)
        rounds = run_gate_rounds(args, scratch, db, env, banks, args.profile, state, record)
        report = build_gate_report(args.profile, banks, rounds, state["config"]["reranker_weights"])
        record.update(status="complete", report=report)
        text = render_summary(report, banks, candidate_origin_line(args.profile, record))
        (out / "rebind-gate.txt").write_text(text + "\n")
        print(text, flush=True)
        code = EXIT_PASS if report["verdict"] == "PASS" else EXIT_FAIL
    except GateStop as e:
        stop, code = str(e), e.code
        record.update(status="stopped", stop={"kind": type(e).__name__, "message": stop, "exit_code": code})
    except BankError as e:
        stop, code = str(e.code), EXIT_REFUSED
        record.update(status="stopped", stop={"kind": "BankError", "message": stop, "exit_code": code})
    finally:
        try:
            finish_scratch(args, scratch, record)
        finally:
            record["token_budget"] = {"cap": state["cap"], "source": state["cap_source"],
                                      "help_error": state.get("help_error")}
            (out / "rebind-gate.json").write_text(json.dumps(record, indent=1) + "\n")
    if stop is not None:
        print(f"rebind-gate: {stop}", file=sys.stderr)
    return code


def cmd_rebind_gate(args) -> int:
    """Exit status: 0 the profile passes in both rounds, 10 it does not, 11 the run cannot be read (a wrong
    attribution, a question that voided three runs in a row), 12 a condition the gate requires does not hold or the
    run could not complete."""
    try:
        return run_rebind_gate(args)
    except GateStop as e:
        print(f"rebind-gate: {e}", file=sys.stderr)
        return e.code
    except BankError as e:
        print(f"rebind-gate: {e.code}", file=sys.stderr)
        return EXIT_REFUSED


def gate_self_checks(arm, with_patches) -> None:
    """The rebind-gate arms of the self-test: the decision functions over synthetic responses, and the whole command
    over fake server processes. `arm(name, fn)` records one check; `with_patches(patches, fn)` runs fn with module
    globals replaced (the mutation controls)."""
    import contextlib
    import io

    T = "2026-10-28T12:00:00Z"
    CAND = "cand-p"
    HELP = {"verb": "memory.recall", "params": [{"name": "config", "description":
            "Default token budget (tokens). Server-side cap: 12345. Default: 4000."}]}

    def gid(i):
        return f"{i:08x}-0000-4000-8000-000000000000"

    def hit(i, att="unspecified", by=None, pre=0.5, term=1.0, **extra):
        idv = gid(i) if isinstance(i, int) else i
        h = {"id": idv, "full_id": idv, "created_at": "2026-01-01T00:00:00Z", "serve_attribution": att,
             "rank_score": 0.5, "breakdown": {"pre_entity_score": pre, "entity_term": term, "weights_served": "default"},
             **extra}
        if by is not None:
            h["served_by_profile_id"] = by
        return h

    def response(hits, rounds=1, multi=False, degraded=False, truncated=False):
        body = {"results": hits, "timings": {"total_us": 1, "retrieval_rounds": rounds, "rounds": [{}] * rounds}}
        if multi:
            body.update(candidates={"vector_candidates_per_model": []}, budget_capped=False, truncated_for_budget=0)
        if degraded:
            body.update(degraded=True, degraded_reason="ann_unavailable")
        if truncated:
            body["truncated"] = True
        return body

    def none_hits(n=3, **kw):
        return [hit(i, **kw) for i in range(n)]

    def cand_hits(n=3, **kw):
        return [hit(i, att="profile", by=CAND, **kw) for i in range(n)]

    def read(resp, which):
        return gate_recall_read(resp, which, None if which == "none" else CAND, T)

    def stops(fn, exc=GateStop):
        try:
            fn()
        except exc:
            return True
        return False

    def code_of(fn):
        try:
            fn()
        except GateStop as e:
            return e.code
        return None

    stamped = [hit(0, degraded="ann_unavailable"), hit(1)]
    arm("rebind-gate: a clean recall in either arm does not void its round",
        lambda: read(response(none_hits()), "none")["void"] is None
        and read(response(cand_hits()), "candidate")["void"] is None)
    arm("rebind-gate: hits stamped degraded void the round, in both arms",
        lambda: read(response(stamped), "none")["void"] == "degraded:hits_stamped"
        and read(response([dict(h, serve_attribution="profile", served_by_profile_id=CAND) for h in stamped]),
                 "candidate")["void"] == "degraded:hits_stamped"
        and read(response(stamped), "none")["degraded"])
    arm("rebind-gate: an empty response with degraded true voids the round, in both arms",
        lambda: all(read(response([], degraded=True), w)["void"] == "degraded:empty_flagged"
                    and read(response([], degraded=True), w)["degraded"] for w in GATE_ARMS))
    arm("rebind-gate: an empty verbose multi-model envelope voids the round in both arms, a non-empty one does not",
        lambda: all(read(response([], multi=True), w)["void"] == "empty_multi_model_envelope" for w in GATE_ARMS)
        and not read(response([], multi=True), "none")["degraded"]
        and read(response(cand_hits(), multi=True), "candidate")["void"] is None
        and read(response(none_hits(), multi=True), "none")["void"] is None)
    arm("rebind-gate: a candidate recall with no hits is a miss, a no-profile recall with no hits voids its round",
        lambda: read(response([]), "candidate")["void"] is None
        and read(response([], truncated=True), "candidate")["void"] is None
        and read(response([]), "none")["void"] == "no_hits_without_profile"
        and read(response([], truncated=True), "none")["void"] == "no_hits_without_profile")

    def attribution_stops():
        return (stops(lambda: read(response([hit(0, att="profile", by=CAND)]), "none"))
                and stops(lambda: read(response([hit(0, att="unspecified", by="other")]), "none"))
                and stops(lambda: read(response([hit(0, att="unattributed")]), "none"))
                and stops(lambda: read(response([hit(0, att="unspecified")]), "candidate"))
                and stops(lambda: read(response([hit(0, att="profile", by="other")]), "candidate"))
                and stops(lambda: read(response(none_hits() + [hit(9, att="profile", by=CAND)]), "none"))
                and code_of(lambda: read(response([hit(0, att="profile", by=CAND)]), "none")) == EXIT_VOID
                and not stops(lambda: read(response(none_hits()), "none"))
                and not stops(lambda: read(response(cand_hits()), "candidate")))

    arm("rebind-gate: a hit with any other attribution than its arm's voids the run (stop, exit 11)", attribution_stops)
    arm("rebind-gate: mutation control, with the attribution check removed the attribution arm reports failure",
        lambda: with_patches({"check_gate_attribution": lambda *a: None}, attribution_stops) is False)
    arm("rebind-gate: a late hit, a missing timings report and a hit without the breakdown terms stop the gate",
        lambda: stops(lambda: gate_recall_read(response([hit(0, created_at="2026-10-28T12:00:00Z")]), "none", None, T))
        and stops(lambda: read({"results": none_hits()}, "none"))
        and stops(lambda: read(none_hits(), "none"))
        and code_of(lambda: read(response([dict(hit(0), breakdown={})]), "none")) == EXIT_REFUSED)

    def brain_profile_refused():
        clean = "[actor]\nid = \"a\"\n\n[[engines]]\nname = \"e\"\n"
        env = {"HOME": "/h"}
        argv = ["k", "mcp", "--db", "/h/khive.db"]
        return (require_brain_profile_unset(clean, env, argv).get("config_toml") == "unset"
                and stops(lambda: require_brain_profile_unset("[runtime]\nbrain_profile = \"p\"\n" + clean, env, argv),
                          GateRefused)
                and stops(lambda: require_brain_profile_unset(clean, {"KHIVE_BRAIN_PROFILE": "p"}, argv), GateRefused)
                and stops(lambda: require_brain_profile_unset(clean, env, argv + ["--brain-profile", "p"]), GateRefused)
                and stops(lambda: require_brain_profile_unset(clean, env, argv + ["--brain-profile=p"]), GateRefused)
                and not stops(lambda: require_brain_profile_unset(clean + "# brain_profile = \"p\"\n",
                                                                  {"KHIVE_BRAIN_PROFILE": "  "}, argv))
                and code_of(lambda: require_brain_profile_unset(clean, {"KHIVE_BRAIN_PROFILE": "p"}, argv))
                == EXIT_REFUSED)

    arm("rebind-gate: the harness refuses to start when the runtime has a brain_profile (config, environment, flag)",
        brain_profile_refused)
    arm("rebind-gate: mutation control, with the brain_profile check removed that arm reports failure",
        lambda: with_patches({"require_brain_profile_unset": lambda *a: {}}, brain_profile_refused) is False)

    def binding_refused():
        free = {"matched_binding": False, "profile_id": None}
        return (not binding_resolves(free) and binding_resolves({"matched_binding": True, "profile_id": "p"})
                and stops(lambda: require_no_binding({"default": free, "namespace local": {"matched_binding": True}}),
                          GateRefused)
                and stops(lambda: require_no_binding({"default": {"profile_id": None}}), GateRefused)
                and not stops(lambda: require_no_binding({"default": free, "namespace local": free}))
                and code_of(lambda: require_no_binding({"default": {"matched_binding": True}})) == EXIT_REFUSED)

    arm("rebind-gate: the harness refuses to start when the caller has a recall binding that resolves", binding_refused)
    arm("rebind-gate: mutation control, with the binding check removed that arm reports failure",
        lambda: with_patches({"require_no_binding": lambda reads: None}, binding_refused) is False)

    def scope_refused():
        s = {"actor_id": "a", "namespace": "local", "visible_namespaces": ["team-a", "local"]}
        same = {"none": scope_of(s), "candidate": scope_of(s)}
        other = {"none": scope_of(s), "candidate": scope_of({**s, "actor_id": "b"})}
        narrower = {"none": scope_of(s), "candidate": scope_of({**s, "visible_namespaces": ["local"]})}
        return (scope_of(s)["visible_namespaces"] == ["local", "team-a"]
                and not stops(lambda: require_same_scope(same, ["team-a", "local"]))
                and not stops(lambda: require_same_scope(same, ["team-a"]))
                and stops(lambda: require_same_scope(other, ["team-a", "local"]), GateRefused)
                and stops(lambda: require_same_scope(narrower, ["team-a", "local"]), GateRefused)
                and stops(lambda: require_same_scope(same, ["team-b"]), GateRefused)
                and stops(lambda: scope_of({"namespace": "local"}), GateRefused))

    arm("rebind-gate: arms whose caller or namespace sets differ, or a readable set that is not the bank's, are refused",
        scope_refused)
    help_ok = {"params": [{"description": "Default token budget (tokens). Server-side cap: 16000. Default: 4000."}]}
    arm("rebind-gate: the token-budget cap is read from the help text, an operator cap is used only when the help "
        "states none and must agree when it does",
        lambda: token_budget_cap_from_help(help_ok) == 16000 and token_budget_cap_from_help({"d": "no cap"}) is None
        and resolve_token_budget_cap(help_ok, None) == (16000, "help")
        and resolve_token_budget_cap(help_ok, 16000) == (16000, "help")
        and resolve_token_budget_cap({"d": "no cap"}, 9000) == (9000, "operator")
        and stops(lambda: resolve_token_budget_cap(help_ok, 9000), GateRefused)
        and stops(lambda: resolve_token_budget_cap(None, None), GateRefused)
        and stops(lambda: resolve_token_budget_cap({"d": "no cap"}, 0), GateRefused)
        and stops(lambda: token_budget_cap_from_help(
            {"a": "token budget cap: 1000", "b": "token budget cap: 2000"}), GateRefused))

    # ---- scoring and the pass rules
    def mkcases(n, prefix="c"):
        return [{"case_id": f"c{i}", "query": f"{prefix}-q{i}", "T": T,
                 "needed_info": [{"statement": "s", "ids": [gid(1000 + i)]}]} for i in range(n)]

    def ranked(i, rank, salt):
        ids = [gid(100_000 + i * 1000 + salt * 100 + j) for j in range(GATE_TOP_K)]
        if rank is not None:
            ids[rank - 1] = gid(1000 + i)
        return ids

    def summary(ids, entity_not_one=0, clamped=0):
        return {"ids": ids, "returned": len(ids), "rounds": 1, "capped": False, "clamped_top10": clamped,
                "entity_hits": len(ids), "entity_not_one": entity_not_one, "weights_served": ["default"]}

    def bank_round(n, moves, cand_salt=1, entity_not_one=0, clamped=0):
        """A bank's accepted pass: every question has its answer at rank 1 in both arms except those in `moves`
        ({question: (no-profile rank, candidate rank)}, None for absent from the 50 hits)."""
        qs = []
        for i in range(n):
            nr, cr = moves.get(i, (1, 1))
            qs.append({"case_id": f"c{i}", "none": summary(ranked(i, nr, 0), clamped=clamped),
                       "candidate": summary(ranked(i, cr, cand_salt), entity_not_one=entity_not_one, clamped=clamped)})
        counts = {a: {"recalls": n, "degraded": 0, "returned_fewer_than_top_k": 0, "capped": 0, "widened": []}
                  for a in GATE_ARMS}
        return {"bank_id": "b", "questions": qs, "voids": [], "counts": counts}

    def metrics(n, moves):
        rb = bank_round(n, moves)
        return bank_metrics(mkcases(n), rb["questions"])

    exact = metrics(25, {0: (1, 2), 1: (1, 2)})
    arm("rebind-gate: a shared-pool MRR@10 loss of exactly 0.04 passes, above 0.04 fails",
        lambda: exact["mrr"]["none"] - exact["mrr"]["candidate"] == Fraction(4, 100) and not exact["lost"]
        and shared_bank_passes(exact) and not shared_bank_passes(metrics(25, {0: (1, 2), 1: (1, 2), 2: (1, 2)})))
    arm("rebind-gate: one lost shared-pool question passes, two fail even when the loss is under 0.04",
        lambda: len(metrics(30, {0: (1, None)})["lost"]) == 1 and shared_bank_passes(metrics(30, {0: (1, None)}))
        and not shared_bank_passes(metrics(30, {0: (3, None), 1: (3, 12)}))
        and metrics(30, {0: (3, None), 1: (3, 12)})["mrr"]["none"] - metrics(30, {0: (3, None), 1: (3, 12)})["mrr"][
            "candidate"] < Fraction(4, 100))
    arm("rebind-gate: the own-namespace bank passes at equal or higher MRR@10 and fails below it",
        lambda: own_bank_passes(metrics(30, {})) and own_bank_passes(metrics(30, {0: (2, 1)}))
        and not own_bank_passes(metrics(30, {0: (1, 2)})))
    split = metrics(30, {0: (1, None), 1: (1, 30), 2: (None, 4), 3: (45, 5)})
    arm("rebind-gate: each lost question is split into absent from the 50 hits or at ranks 11 to 50, each gained one "
        "the same way for the no-profile arm",
        lambda: [(x["case_id"], x["where"]) for x in split["lost"]] == [("c0", "absent_from_50"), ("c1", "ranks_11_50")]
        and [(x["case_id"], x["where"]) for x in split["gained"]] == [("c2", "absent_from_50"), ("c3", "ranks_11_50")])
    one_loss_each_way = [{"own": metrics(30, {}), "shared": metrics(30, {0: (1, None)})}]
    two_losses = {"own": metrics(30, {}), "shared": metrics(30, {0: (3, None), 1: (3, None)})}

    def one_round_only():
        both = gate_verdict(one_loss_each_way * 2)
        split_rounds = gate_verdict(one_loss_each_way + [two_losses])
        return (both["pass"] and not split_rounds["pass"] and [r["pass"] for r in split_rounds["rounds"]] == [True, False]
                and not gate_verdict([two_losses, one_loss_each_way[0]])["pass"]
                and not gate_verdict([{"own": metrics(30, {0: (1, 2)}), "shared": metrics(30, {})}] * 2)["pass"]
                and stops(lambda: gate_verdict([])))

    arm("rebind-gate: a profile that passes one round and not the other fails the gate", one_round_only)
    arm("rebind-gate: mutation control, with the shared-pool rule removed that arm reports failure",
        lambda: with_patches({"shared_bank_passes": lambda m: True}, one_round_only) is False)

    # ---- one pass over a bank
    def ask_from(table, order):
        def ask(case, pid):
            order.append((case["case_id"], "candidate" if pid else "none"))
            return table[(case["case_id"], "candidate" if pid else "none")]
        return ask

    full = [hit(0, pre=1.0)] + [hit(i) for i in range(1, 20)] + [hit(20, pre=1.0)] + [hit(i) for i in range(21, 50)]
    cand_full = ([hit(0, att="profile", by=CAND, pre=1.0), hit(1, att="profile", by=CAND, pre=1.0),
                  hit(2, att="profile", by=CAND, term=0.9)]
                 + [hit(i, att="profile", by=CAND) for i in range(3, 50)])
    order = []
    played = play_bank(mkcases(2), ask_from({
        ("c0", "none"): response(full), ("c0", "candidate"): response(cand_full, rounds=2),
        ("c1", "none"): response(none_hits(40)),
        ("c1", "candidate"): response(cand_hits(50, truncated=True))}, order), CAND, "bank-x")
    c = played["counts"]
    arm("rebind-gate: per arm the widened questions, recalls under 50 hits, capped recalls, clamped top-10 hits and "
        "entity terms other than 1 are counted from the responses",
        lambda: played["voids"] == [] and c["none"]["widened"] == [] and c["candidate"]["widened"] == ["c0"]
        and c["none"]["returned_fewer_than_top_k"] == 1 and c["candidate"]["returned_fewer_than_top_k"] == 0
        and c["none"]["capped"] == 0 and c["candidate"]["capped"] == 1
        and played["questions"][0]["none"]["clamped_top10"] == 1 and played["questions"][0]["candidate"]["clamped_top10"] == 2
        and played["questions"][0]["candidate"]["entity_not_one"] == 1 and played["questions"][0]["none"]["entity_not_one"] == 0)
    arm("rebind-gate: the arms run adjacent in time, no profile first, in the bank's question order",
        lambda: order == [("c0", "none"), ("c0", "candidate"), ("c1", "none"), ("c1", "candidate")])
    degraded_play = play_bank(mkcases(2), ask_from({
        ("c0", "none"): response([hit(0, degraded="ann_unavailable")]), ("c0", "candidate"): response(cand_hits()),
        ("c1", "none"): response(none_hits()), ("c1", "candidate"): response(cand_hits())}, []), CAND, "bank-x")
    arm("rebind-gate: a degraded recall is counted, listed with its bank, case and arm, and its case is not scored",
        lambda: degraded_play["counts"]["none"]["degraded"] == 1 and degraded_play["counts"]["candidate"]["degraded"] == 0
        and degraded_play["voids"] == [{"bank": "bank-x", "case_id": "c0", "arm": "none", "reason": "degraded:hits_stamped"}]
        and [q["case_id"] for q in degraded_play["questions"]] == ["c1"])
    arm("rebind-gate: runs in a row restart for every question that did not void",
        lambda: next_streaks({}, {("b", "c1")}) == {("b", "c1"): 1}
        and next_streaks({("b", "c1"): 2, ("b", "c2"): 1}, {("b", "c1")}) == {("b", "c1"): 3}
        and next_streaks({("b", "c1"): 2}, {("b", "c2")}) == {("b", "c2"): 1} and next_streaks({("b", "c1"): 2}, set()) == {})

    # ---- the report
    def report_for(rounds_spec, reranker=None):
        banks_info = {"own": {"cases": mkcases(30)}, "shared": {"cases": mkcases(30)}}
        rounds = []
        for n, (own, shared, salt) in enumerate(rounds_spec, 1):
            rounds.append({"round": n, "started_at": "t", "accepted_attempt": 1, "discarded_attempts": [],
                           "degraded_recalls": {a: 0 for a in GATE_ARMS},
                           "banks": {"own": own if "questions" in own else bank_round(30, own, salt),
                                     "shared": shared if "questions" in shared else bank_round(30, shared, salt)}})
        return build_gate_report(CAND, banks_info, rounds, reranker or {})

    rep = report_for([({}, {0: (1, None)}, 1), ({}, {0: (1, None)}, 2)])
    arm("rebind-gate: the report gives MRR@10 and hit@10 per arm per round and their mean, and the verdict",
        lambda: rep["verdict"] == "PASS" and rep["rounds"][0]["own"]["MRR@10"] == {"none": 1.0, "candidate": 1.0}
        and rep["rounds"][0]["shared"]["hit@10"] == {"none": 1.0, "candidate": round(29 / 30, 6)}
        and rep["mean_of_rounds"]["shared"]["hit@10"]["candidate"] == round(29 / 30, 6)
        and report_for([({}, {0: (1, None)}, 1), ({}, {0: (3, None), 1: (3, None)}, 1)])["verdict"] == "FAIL")
    arm("rebind-gate: the questions whose top ten differs between the rounds are counted per arm",
        lambda: rep["top10_differs_between_rounds"]["own"] == {"none": 0, "candidate": 30}
        and report_for([({}, {}, 1), ({}, {}, 1)])["top10_differs_between_rounds"]["shared"] == {"none": 0, "candidate": 0})
    arm("rebind-gate: the count of clamped top-10 hits is reported per arm, and not available when a weighted reranker "
        "is configured",
        lambda: report_for([(bank_round(30, {}, clamped=2), {}, 1)])["rounds"][0]["own"]["top10_pre_entity_score_1"]
        == {"none": 60, "candidate": 60}
        and report_for([(bank_round(30, {}, clamped=2), {}, 1)], {"cross_encoder": 0.5})["rounds"][0]["own"][
            "top10_pre_entity_score_1"] == {"none": None, "candidate": None}
        and report_for([({}, {}, 1)], {"cross_encoder": 0.5})["reranker"]["pre_entity_score_1_count"] == "not available")
    arm("rebind-gate: the report states whether the candidate's entity term is 1 for every hit",
        lambda: rep["candidate_entity_term"]["is_1_for_every_hit"] is True
        and report_for([(bank_round(30, {}, entity_not_one=1), {}, 1)])["candidate_entity_term"]
        == {"hits": 3000, "not_one": 30, "is_1_for_every_hit": False})

    # ---- the whole command over fake server processes
    def fake_stage(args, scratch):
        kdir = scratch / "home" / ".khive"
        kdir.mkdir(parents=True)
        return kdir / "khive.db", {"HOME": str(scratch / "home"), "KHIVE_NO_DAEMON": "1"}, [
            {"staged_as": "khive.db", "source_mtime_ns": 1}, {"staged_as": "khive.db.events.db", "source_mtime_ns": 1}]

    def write_bank(root, bank_id, prefix, tag):
        d = root / bank_id
        d.mkdir()
        (d / "cases.jsonl").write_text("".join(json.dumps({**cs, "tags": [f"namespace:{tag}"]}) + "\n"
                                               for cs in mkcases(3, prefix)))
        (d / "bank.json").write_text(json.dumps({"bank_id": bank_id, "version": 1,
                                                 "cases_sha256": sha256_file(d / "cases.jsonl")}))
        return d

    def drive(plan=None, voids=None, binding=False, wrong_none=False, engines='[[engines]]\nname = "e"\n',
              help_text=HELP, token_budget_cap=None, patches=None, out_exists=False, create_factor=None,
              on_copy=True, readback=None, shared_visible="team-a,local", read_error=None):
        """cmd_rebind_gate over fake processes. `plan[(role, question, nth)]` is (no-profile rank, candidate rank) the
        nth time that question is asked; `voids[(role, question)]` is how many times the no-profile recall answers
        degraded before it answers clean. The fake server holds a profile table: the candidate is on the copy at
        local_factor 0.7 when `on_copy`, `brain.profile` of an absent profile is a not-found error worded as the
        runtime words it, and `brain.create_profile` adds a profile (refusing a name already there and a factor outside
        [0.5, 1]) with `readback` fields overriding what it then reads back; `read_error` makes the creating process's
        `brain.profile` fail with that text. `create_factor` is the new flag."""
        plan, voids = plan or {}, voids or {}
        asked, procs, sleeps, closes, ops_log = {}, [], [], [], []
        creators, creator_closes, events, staged_calls = [], [], [], []
        profiles = {CAND: {"profile_id": CAND, "lifecycle": "active", "local_factor": 0.7, "total_events": 0}} if on_copy else {}

        class Fake:
            def __init__(self, role, visible):
                self.role, self.visible, self.ops = role, visible, []

            def handshake(self):
                self.ops.append("handshake")
                return {}

            def one(self, text, timeout=900):
                self.ops.append(text)
                verb = text.split("(", 1)[0]
                if verb == "whoami":
                    return {"actor_id": "bench:unbound", "namespace": "local",
                            "visible_namespaces": sorted(set(self.visible) | {"local"})}
                if verb == "brain.resolve":
                    return {"matched_binding": binding, "profile_id": None}
                if verb == "brain.profile":
                    name = re.search(r'profile_id="([^"]+)"', text).group(1)
                    if read_error is not None and self.role == "create":
                        raise BankError(f"{text[:80]}: {read_error!r}")
                    if name not in profiles:
                        err = "not found: profile " + json.dumps(name)
                        raise BankError(f"{text[:80]}: {err!r}")
                    return dict(profiles[name])
                if verb == "brain.create_profile":
                    name = re.search(r'\(name="([^"]+)"', text).group(1)
                    factor = json.loads(re.search(r"local_factor=([^,)]+)", text).group(1))
                    if name in profiles:
                        err = f"profile {json.dumps(name)} already exists"
                        raise BankError(f"{text[:80]}: {err!r}")
                    if not 0.5 <= factor <= 1:
                        raise BankError(f"{text[:80]}: 'local_factor must be a finite number in [0.5, 1]'")
                    profiles[name] = {"profile_id": name, "lifecycle": "inactive", "local_factor": factor,
                                      "total_events": 0, **(readback or {})}
                    return {"created": True, "profile_id": name, "consumer_kind": "recall", "lifecycle": "inactive",
                            "description": re.search(r'description="([^"]*)"', text).group(1)}
                if "help=true" in text:
                    if isinstance(help_text, BaseException):
                        raise help_text
                    return help_text
                query = re.search(r'query="([^"]*)"', text).group(1)
                pid = re.search(r'profile_id="([^"]+)"', text)
                pid = pid.group(1) if pid else None
                idx = int(query.rsplit("q", 1)[1])
                key = (self.role, query, pid)
                nth = asked[key] = asked.get(key, -1) + 1
                if pid is None and nth < voids.get((self.role, idx), 0):
                    return response([hit(0, degraded="ann_unavailable")])
                nr, cr = plan.get((self.role, idx, nth), (1, 1))
                att, by = ("profile", pid) if (pid or wrong_none) else ("unspecified", None)
                ids = ranked(idx, cr if pid else nr, 1 if pid else 0)
                return response([hit(x, att=att, by=by or (CAND if wrong_none else None)) for x in ids])

            def close(self):
                (creator_closes if self.role == "create" else closes).append(self.role)

        def fake_bridge(kkernel, db, env, err_path):
            cfg = (err_path.parent / "home" / ".khive" / "config.toml").read_text()
            visible = json.loads(re.search(r"visible_namespaces\s*=\s*(\[[^\]]*\])", cfg).group(1))
            fb = Fake("create", visible)
            fb.config, fb.err_path = cfg, err_path
            creators.append(fb)
            events.append("create")
            return fb

        def counting_stage(args, scratch):
            staged_calls.append(1)
            return fake_stage(args, scratch)

        def fake_open_bridge(a, db, env, scr, probe, profile_ids, served_check=None, err_name="x"):
            cfg = (scr / "home" / ".khive" / "config.toml").read_text()
            visible = json.loads(re.search(r"visible_namespaces\s*=\s*(\[[^\]]*\])", cfg).group(1))
            fb = Fake("own" if probe.startswith("own") else "shared", visible)
            procs.append(fb)
            events.append("round")
            if served_check is not None:
                try:
                    served_check(fb)
                except BaseException:
                    fb.close()
                    raise
            return fb, {"ready_s": 0.0, "control": {}}

        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            (td / "engines.toml").write_text(engines)
            if out_exists:
                (td / "out").mkdir()
            own_dir, shared_dir = write_bank(td, "own-bank", "own", "team-a"), write_bank(td, "shared-bank", "shared", "local")
            a = types.SimpleNamespace(kkernel="k", own_bank=str(own_dir), shared_bank=str(shared_dir),
                                      own_visible="team-a,local", shared_visible=shared_visible, profile=CAND,
                                      store_main="m", store_events="e", staged_home=None, snapshot_id="s",
                                      engines_file=str(td / "engines.toml"), models_dir=None, control_id=gid(0),
                                      ready_max_s=5, scratch_parent=str(td), remove_scratch=False, copy_wal=False,
                                      round_gap_s=7.0, max_round_runs=6, token_budget_cap=token_budget_cap,
                                      create_candidate_local_factor=create_factor, out=str(td / "out"))
            out_buf, err_buf = io.StringIO(), io.StringIO()
            patched = {"stage": counting_stage, "memory_pressure": lambda: {"source": "t", "reading": "1", "normal": True},
                       "open_bridge": fake_open_bridge, "Bridge": fake_bridge, "kkernel_version": lambda p: "test",
                       "gate_sleep": lambda s: sleeps.append(s), **(patches or {})}
            with contextlib.redirect_stdout(out_buf), contextlib.redirect_stderr(err_buf):
                code = with_patches(patched, lambda: cmd_rebind_gate(a))
            record = json.loads((td / "out" / "rebind-gate.json").read_text()) if (td / "out" / "rebind-gate.json").exists() else None
            text = (td / "out" / "rebind-gate.txt").read_text() if (td / "out" / "rebind-gate.txt").exists() else None
            made = sorted(p.name for p in td.iterdir() if p.name.startswith("khive-question-bank-") or p.name == "out")
        return {"code": code, "record": record, "text": text, "stdout": out_buf.getvalue(), "stderr": err_buf.getvalue(),
                "procs": procs, "sleeps": sleeps, "closes": closes, "creators": creators,
                "creator_closes": creator_closes, "events": events, "staged_calls": staged_calls, "profiles": profiles,
                "made": made}

    def recall_ops(proc):
        return [t for t in proc.ops if t.startswith("memory.recall(") and "help=true" not in t]

    def passes_end_to_end():
        r = drive()
        if r["code"] != EXIT_PASS or r["record"] is None or r["record"]["report"]["verdict"] != "PASS":
            return False
        ops = [recall_ops(p) for p in r["procs"]]
        first = ops[0]
        return (len(r["procs"]) == 4 and [p.role for p in r["procs"]] == ["own", "shared", "own", "shared"]
                and r["closes"] == ["own", "shared", "own", "shared"] and r["sleeps"] == [7.0]
                and len(r["record"]["processes"]) == 4 and len(r["record"]["report"]["rounds"]) == 2
                and r["record"]["token_budget"]["cap"] == 12345 and r["record"]["token_budget"]["source"] == "help"
                and all(len(o) == 6 for o in ops)
                and ["profile_id" in t for t in first] == [False, True] * 3
                and [re.search(r'query="own-q(\d)"', t).group(1) for t in first] == ["0", "0", "1", "1", "2", "2"]
                and all('"default_token_budget": 12345' in t and "top_k=50" in t and "include_breakdown=true" in t
                        and "include_timings=true" in t and f'created_before="{T}"' in t and '"reranker_weights": {}' in t
                        for o in ops for t in o)
                and all(f'profile_id="{CAND}"' in t for o in ops for t in o[1::2])
                and [t.split("(", 1)[0] for t in r["procs"][0].ops[:6]] == [
                    "whoami", "whoami", "brain.resolve", "brain.resolve", "brain.profile", "memory.recall"]
                and "PASS" in r["stdout"] and r["text"].startswith("rebind-gate cand-p: PASS")
                and r["record"]["banks"]["own"]["population"] == 3 and "own bank own-bank v1: 3 questions" in r["stdout"])

    arm("rebind-gate: a profile that matches on both banks in both rounds passes (exit 0); every recall asks 50 hits "
        "with breakdowns and timings at the budget the help stated, in two rounds each in fresh processes",
        passes_end_to_end)

    def fails_in_round_two_only():
        shared_loss = {("shared", 0, 1): (1, None), ("shared", 1, 1): (1, None)}
        r = drive(plan=shared_loss)
        rounds = r["record"]["report"]["rounds"] if r["record"] else []
        return (r["code"] == EXIT_FAIL and [x["pass"] for x in rounds] == [True, False]
                and [x["shared"]["pass"] for x in rounds] == [True, False] and r["record"]["report"]["verdict"] == "FAIL"
                and len(rounds[1]["shared"]["lost"]) == 2)

    arm("rebind-gate: a profile that fails the shared-pool rule in the second round only fails the gate (exit 10)",
        fails_in_round_two_only)
    arm("rebind-gate: mutation control, with the shared-pool rule removed the second-round failure passes",
        lambda: with_patches({"shared_bank_passes": lambda m: True}, fails_in_round_two_only) is False)

    def two_voids_then_clean():
        r = drive(voids={("own", 1): 2})
        rounds = r["record"]["report"]["rounds"] if r["record"] else []
        return (r["code"] == EXIT_PASS and len(rounds[0]["discarded_attempts"]) == 2 and rounds[0]["accepted_attempt"] == 3
                and rounds[0]["degraded_recalls"] == {"none": 2, "candidate": 0} and len(r["record"]["processes"]) == 8
                and rounds[1]["discarded_attempts"] == [])

    def three_voids_stop():
        r = drive(voids={("own", 1): 3})
        return (r["code"] == EXIT_VOID and r["record"]["status"] == "stopped" and len(r["record"]["processes"]) == 6
                and "question c1 of bank own-bank voided 3 runs in a row" in r["stderr"]
                and r["record"]["rounds"][0]["stopped_on"] == [
                    {"bank": "own-bank", "case_id": "c1", "voided_runs_in_a_row": 3}])

    arm("rebind-gate: a round with a degraded recall is run again in fresh processes and counted, two voids then a "
        "clean run passes", two_voids_then_clean)
    arm("rebind-gate: a question that voids three runs in a row stops the gate (exit 11) and is named", three_voids_stop)
    arm("rebind-gate: mutation control, with the void check removed the three-voids arm reports failure",
        lambda: with_patches({"next_streaks": lambda prev, voided: {}}, three_voids_stop) is False)

    def attribution_stops_run():
        r = drive(wrong_none=True)
        return r["code"] == EXIT_VOID and "arm none expects" in r["stderr"] and r["record"]["status"] == "stopped"

    arm("rebind-gate: a wrong attribution in a run stops it with exit 11", attribution_stops_run)
    arm("rebind-gate: mutation control, with the attribution check removed the run is not stopped",
        lambda: with_patches({"check_gate_attribution": lambda *a: None}, attribution_stops_run) is False)

    def binding_stops_run():
        r = drive(binding=True)
        return (r["code"] == EXIT_REFUSED and "recall binding" in r["stderr"]
                and not any(recall_ops(p) for p in r["procs"]) and r["record"]["processes"][0]["bindings"]["default"]
                == {"matched_binding": True, "profile_id": None})

    arm("rebind-gate: a caller with a recall binding is refused (exit 12) before any recall, the bindings read are recorded",
        binding_stops_run)
    arm("rebind-gate: mutation control, with the binding check removed the run is not refused",
        lambda: with_patches({"require_no_binding": lambda reads: None}, binding_stops_run) is False)

    def brain_profile_stops_run():
        r = drive(engines='[[engines]]\nname = "e"\nbrain_profile = "p"\n')
        return (r["code"] == EXIT_REFUSED and "brain_profile" in r["stderr"] and r["procs"] == []
                and r["record"]["status"] == "stopped")

    arm("rebind-gate: a runtime config with a brain_profile is refused (exit 12) before any process starts",
        brain_profile_stops_run)
    arm("rebind-gate: mutation control, with the brain_profile check removed the run is not refused",
        lambda: with_patches({"require_brain_profile_unset": lambda *a: {}}, brain_profile_stops_run) is False)

    def cap_arms():
        none_stated = drive(help_text={"d": "no cap here"})
        by_operator = drive(help_text={"d": "no cap here"}, token_budget_cap=9000)
        return (none_stated["code"] == EXIT_REFUSED and "states no token-budget cap" in none_stated["stderr"]
                and not any(recall_ops(p) for p in none_stated["procs"])
                and by_operator["code"] == EXIT_PASS and by_operator["record"]["token_budget"]["source"] == "operator"
                and all('"default_token_budget": 9000' in t for p in by_operator["procs"] for t in recall_ops(p)))

    arm("rebind-gate: without a cap in the help or a --token-budget-cap the gate refuses before any recall, an operator "
        "cap is sent and recorded as such", cap_arms)

    def unread_help_arms():
        runs = [drive(help_text=BankError("tool error: help unavailable"), token_budget_cap=cap) for cap in (None, 9000)]
        return all(r["code"] == EXIT_REFUSED and "could not be read" in r["stderr"]
                   and not any(recall_ops(p) for p in r["procs"]) and r["record"]["status"] == "stopped"
                   and r["record"]["token_budget"]["cap"] is None
                   and "help unavailable" in (r["record"]["token_budget"]["help_error"] or "") for r in runs)

    arm("rebind-gate: a recall help that cannot be read refuses before any recall, with or without a "
        "--token-budget-cap, and the record names the read error", unread_help_arms)
    def existing_out_refused():
        r = drive(out_exists=True)
        return (r["code"] == EXIT_REFUSED and "refusing to overwrite" in r["stderr"] and r["procs"] == []
                and r["record"] is None)

    arm("rebind-gate: an output directory that already exists is refused (exit 12) before any process starts",
        existing_out_refused)
    arm("rebind-gate: exit statuses are distinct: pass 0, fail 10, void 11, refused 12",
        lambda: len({EXIT_PASS, EXIT_FAIL, EXIT_VOID, EXIT_REFUSED}) == 4 and issubclass(GateRefused, GateStop)
        and GateStop.code == EXIT_VOID and GateRefused.code == EXIT_REFUSED)

    # ---- creating the candidate on the copy (--create-candidate-local-factor)
    def verbs_of(proc):
        return [t.split("(", 1)[0] for t in proc.ops]

    def creation_ops(r):
        return verbs_of(r["creators"][0]) if len(r["creators"]) == 1 else None

    def creates_candidate():
        r = drive(create_factor=0.7, on_copy=False, shared_visible="local")
        rec = r["record"] or {}
        made = rec.get("candidate_created", {})
        c = r["creators"][0] if len(r["creators"]) == 1 else None
        return (r["code"] == EXIT_PASS and c is not None
                and verbs_of(c) == ["handshake", "brain.profile", "brain.create_profile", "brain.profile"]
                and c.ops[2].startswith(f'brain.create_profile(name="{CAND}", consumer_kind="recall", local_factor=0.7')
                and not any(t.startswith("memory.recall(") for t in c.ops)
                and r["creator_closes"] == ["create"] and r["events"] == ["create", "round", "round", "round", "round"]
                and c.visible == ["team-a", "local"] and 'id = "bench:unbound"' in c.config
                and len(r["procs"]) == 4 and r["profiles"][CAND]["lifecycle"] == "inactive"
                and all(p["candidate_profile"]["local_factor"] == 0.7 and p["candidate_profile"]["lifecycle"] == "inactive"
                        for p in rec["processes"])
                and made["local_factor"] == 0.7 and f'not found: profile "{CAND}"' in made["before"]["error"]
                and made["create"] == {"created": True, "profile_id": CAND, "consumer_kind": "recall",
                                       "lifecycle": "inactive", "description": GATE_CANDIDATE_DESCRIPTION}
                and (made["after"]["local_factor"], made["after"]["total_events"], made["after"]["lifecycle"])
                == (0.7, 0, "inactive")
                and made["memory_pressure"]["normal"] is True and made["brain_profile"]["config_toml"] == "unset"
                and made["server_stderr"].endswith("/kkernel-mcp-gate-create.err")
                and CAND not in Path(made["server_stderr"]).name
                and "created by this run with local_factor 0.7" in r["text"])

    arm("rebind-gate: with --create-candidate-local-factor and no candidate on the copy, one process of its own reads "
        "(not found), creates, reads back, and runs no recall; the rounds then read the candidate at that factor",
        creates_candidate)

    def existing_candidate_refused():
        r = drive(create_factor=0.9, on_copy=True)
        rec = r["record"] or {}
        return (r["code"] == EXIT_REFUSED and creation_ops(r) == ["handshake", "brain.profile"] and r["procs"] == []
                and "already on the copy" in r["stderr"] and r["creator_closes"] == ["create"]
                and r["profiles"][CAND]["local_factor"] == 0.7 and rec.get("status") == "stopped"
                and rec["candidate_created"]["before"]["local_factor"] == 0.7)

    arm("rebind-gate: --create-candidate-local-factor with the candidate already on the copy is refused (exit 12) "
        "before any create and before any round process", existing_candidate_refused)
    arm("rebind-gate: mutation control, with the existence check removed the already-on-the-copy arm reports failure",
        lambda: with_patches({"require_candidate_absent": lambda br, cand, created: None},
                             existing_candidate_refused) is False)

    def unreadable_candidate_refused():
        r = drive(create_factor=0.7, on_copy=False, read_error="storage: database is locked")
        return (r["code"] == EXIT_REFUSED and creation_ops(r) == ["handshake", "brain.profile"] and r["procs"] == []
                and "could not be read on the copy before the create" in r["stderr"]
                and "database is locked" in r["stderr"] and r["profiles"] == {} and r["creator_closes"] == ["create"])

    arm("rebind-gate: --create-candidate-local-factor with a candidate read that fails for any reason but not found is "
        "refused (exit 12) before any create", unreadable_candidate_refused)

    def bad_factors_refused():
        runs = [drive(create_factor=f) for f in (0.4, 1.5, float("nan"), float("inf"), -0.7)]
        return (all(r["code"] == EXIT_REFUSED and "--create-candidate-local-factor" in r["stderr"]
                    and r["staged_calls"] == [] and r["procs"] == [] and r["creators"] == [] and r["made"] == []
                    and r["record"] is None for r in runs)
                and all(stops(lambda v=v: require_local_factor(v), GateRefused) for v in (True, "0.7", None))
                and require_local_factor(1) == 1.0 and isinstance(require_local_factor(1), float))

    arm("rebind-gate: a --create-candidate-local-factor outside [0.5, 1] or not finite is refused (exit 12) before any "
        "copy, scratch directory, output or process", bad_factors_refused)

    def boundary_factors_create():
        runs = [(f, drive(create_factor=f, on_copy=False)) for f in (0.5, 1.0)]
        return all(r["code"] == EXIT_PASS and r["record"]["candidate_created"]["after"]["local_factor"] == f
                   and r["profiles"][CAND]["local_factor"] == f for f, r in runs)

    arm("rebind-gate: the ends of the range, 0.5 and 1, are created and gated", boundary_factors_create)

    def readback_refused(override, said):
        r = drive(create_factor=0.7, on_copy=False, readback=override)
        rec = r["record"] or {}
        return (r["code"] == EXIT_REFUSED and r["procs"] == [] and said in r["stderr"]
                and creation_ops(r) == ["handshake", "brain.profile", "brain.create_profile", "brain.profile"]
                and r["creator_closes"] == ["create"] and rec.get("status") == "stopped"
                and rec["candidate_created"]["create"]["created"] is True)

    def readbacks_refused():
        return all(readback_refused(o, s) for o, s in (
            ({"local_factor": 0.6}, "local_factor 0.6"), ({"total_events": 3}, "total_events 3"),
            ({"lifecycle": "archived"}, "is archived")))

    arm("rebind-gate: a created candidate read back with another local_factor, with events, or archived is refused "
        "(exit 12) before any round process", readbacks_refused)
    arm("rebind-gate: mutation control, with the read-back check removed the read-back arm reports failure",
        lambda: with_patches({"require_created_as_asked": lambda *a: None}, readbacks_refused) is False)

    def no_flag_creates_nothing():
        r = drive()
        ops = [t for p in r["procs"] for t in p.ops]
        return (r["code"] == EXIT_PASS and r["creators"] == [] and r["events"] == ["round"] * 4
                and not any(t.startswith("brain.create_profile(") for t in ops)
                and "candidate_created" not in r["record"]
                and "candidate cand-p: found on the copy, not created by this run; local_factor read by the round "
                    "processes: 0.7" in r["text"])

    arm("rebind-gate: without the flag the gate creates nothing, records no creation, and says the candidate was "
        "found on the copy", no_flag_creates_nothing)

    def absent_without_flag_refused():
        r = drive(on_copy=False)
        return (r["code"] == EXIT_REFUSED and r["creators"] == [] and f'not found: profile "{CAND}"' in r["stderr"]
                and not any(recall_ops(p) for p in r["procs"]) and r["profiles"] == {}
                and not any(t.startswith("brain.create_profile(") for p in r["procs"] for t in p.ops))

    arm("rebind-gate: without the flag a candidate that is not on the copy is refused (exit 12) by the round process, "
        "and nothing is created", absent_without_flag_refused)


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
    capped = [{"id": u[i], "truncated": True} for i in range(6)]
    try:
        got = hit_list(capped, "t", 5)
    except BankError:
        got = []
    check("hits: a capped prefix with at least the depth is accepted and reported capped",
          len(got) == 6 and any(h.get("truncated") for h in got))
    check("hits: a capped prefix shorter than the depth is refused", refused(hit_list, capped[:4], "t", 5))
    check("hits: a capped hit is refused when no depth is given", refused(hit_list, capped, "t"))
    check("hits: a capped-empty envelope is refused even with a depth",
          refused(hit_list, {"results": [], "truncated": True}, "t", 5))
    check("hits: a degraded hit is refused even with a depth",
          refused(hit_list, capped[:5] + [{"id": u[5], "degraded": "ann_unavailable"}], "t", 5))
    try:
        hit_list(capped[:4], "t", 5)
        refusal = ""
    except BankError as e:
        refusal = str(e)
    check("hits: the refusal names the flag, the hits returned and the depth required",
          "truncated" in refusal and "4 hit(s) returned" in refusal and "depth required: 5" in refusal)
    check("hits: a hit without a full id is refused", refused(hit_id, {"id": "0000abcd"}, "t"))
    check("served: an unbound arm refuses a profile-served hit",
          refused(check_served, [{"serve_attribution": "profile", "served_by_profile_id": "p"}], "x", None, "t"))
    check("served: a profile arm refuses another profile",
          refused(check_served, [{"serve_attribution": "profile", "served_by_profile_id": "q"}], "x", "p", "t"))
    check("namespace: absent and first-colon values", case_namespace({"case_id": "x"}) is None and
          case_namespace({"case_id": "x", "tags": ["namespace:team:a"]}) == "team:a")
    check("namespace: two tags refused", refused(case_namespace, {"case_id": "x", "tags": ["namespace:a", "namespace:b"]}))
    check("namespace: empty value refused", refused(case_namespace, {"case_id": "x", "tags": ["namespace:"]}))
    namespaced_call = op("memory.recall", query="q", namespace="team:a")
    plain_call = op("memory.recall", query="q", namespace=None)
    check("namespace: recall op includes only a supplied namespace", 'namespace="team:a"' in namespaced_call and "namespace=" not in plain_call)
    check("namespace: visible union sorted, distinct, and rejects empty",
          visible_namespace_union("z,a,z", [[{"tags": ["namespace:m"]}, {"tags": ["namespace:a"]}]]) == ["a", "m", "z"]
          and refused(visible_namespace_union, "", [[]]))
    check("names: repeated bank ids refused", refused(require_distinct, ["b", "b"], "bank ids"))
    check("names: repeated profiles refused", refused(require_distinct, ["p", "p"], "profile values"))
    check("names: path-unsafe names refused", refused(safe_name, "../bad", "profile"))
    check("feedback_asof: multiple profiles refused", refused(require_profile_count, ["p", "q"], "feedback_asof"))
    check("calibrate-replay: multiple banks refused", refused(require_calibration_shape, ["a", "b"], False))
    check("calibrate-replay: multiple profiles refused", refused(require_calibration_profiles, ["p", "q"]))
    check("composed: namespaced cases refused because search cannot filter reads",
          refused(require_composed_namespace, "team:a") and not refused(require_composed_namespace, None))
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
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        class RunBridge:
            def __init__(self): self.calls = []
            def one(self, text):
                self.calls.append(text)
                profile = next((p for p in ("p1", "p2") if f'profile_id="{p}"' in text), None)
                return [{"id": u[0], "full_id": u[0], "created_at": "2026-01-01T00:00:00Z",
                         "serve_attribution": "profile" if profile else "unspecified",
                         **({"served_by_profile_id": profile} if profile else {})}]
        rb = RunBridge()
        args = types.SimpleNamespace(profile=["p1", "p2"], feedback_actors="", feedback_since="2025-01-01T00:00:00Z")
        cases = [{"case_id": "q1", "query": "q", "T": "2026-10-28T12:00:00Z", "tags": ["namespace:team:a"], "needed_info": ni}]
        summary = {"results": {"a": {}, "b": {}}}
        for bid in ("a", "b"):
            run_arms(args, ["profile_off", "profile_on"], "test", cases, rb, d / bid, summary,
                     {"bank_id": bid, "bank_version": 1, "population": 1}, bid)
        files = sorted(str(p.relative_to(d)) for p in d.rglob("*.jsonl"))
        expected = ["a/profile_off.jsonl", "a/profile_on.p1.jsonl", "a/profile_on.p2.jsonl",
                    "b/profile_off.jsonl", "b/profile_on.p1.jsonl", "b/profile_on.p2.jsonl"]
        rows = [json.loads(path.read_text().splitlines()[0]) if path.exists() else {}
                for bid in ("a", "b") for profile in ("p1", "p2")
                for path in [d / bid / f"profile_on.{profile}.jsonl"]]
        check("multi-bank: exact output files and profile/namespace row fields",
              files == expected and all(r["namespace"] == "team:a" and r["profile"] == profile
                  and r["bank_id"] == bid for r, (bid, profile) in zip(rows, [(b,p) for b in ("a","b") for p in ("p1","p2")]))
              and all(sum(f'profile_id="{profile}"' in call for call in rb.calls) == 2 for profile in ("p1", "p2"))
              and not any("namespace=" in call for call in rb.calls))
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)

        class AsofBridge:
            def __init__(self):
                self.calls = []

            def one(self, text):
                self.calls.append(text)
                if text.startswith("brain.create_profile"):
                    return {}
                served = re.search(r'profile_id="([^"]+)"', text)
                return [{"id": u[0], "created_at": "2026-01-01T00:00:00Z", "serve_attribution": "profile",
                         "served_by_profile_id": served.group(1) if served else None}]
        ab = AsofBridge()
        args = types.SimpleNamespace(profile=["p1"], feedback_actors="", feedback_since="2025-01-01T00:00:00Z")
        cases = [{"case_id": "q1", "query": "q", "T": "2026-10-28T12:00:00Z", "needed_info": ni}]
        summary = {"results": {"a": {}, "b": {}}}
        for bid in ("a", "b"):
            run_arms(args, ["feedback_asof"], "test", cases, ab, d / bid, summary,
                     {"bank_id": bid, "bank_version": 1, "population": 1}, bid)
        made = [c for c in ab.calls if c.startswith("brain.create_profile")]
        check("feedback_asof: one as-of profile per bank, and its replay counts kept in the summary",
              made == ['brain.create_profile(name="bench-asof-test-a", description="question-bank arm: feedback '
                       'replayed as of T")', 'brain.create_profile(name="bench-asof-test-b", description="question-bank '
                       'arm: feedback replayed as of T")']
              and summary["results"]["b"]["feedback_asof"].get("profile") == "bench-asof-test-b"
              and summary["results"]["a"]["feedback_asof"].get("replay", {}).get("replayed") == 0)
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        src_dir = d / "src"
        src_dir.mkdir()
        for sname, body in (("khive.db", b"main"), ("khive.db.events.db", b"events"),
                            ("khive.db.events.db-wal", b"pending")):
            (src_dir / sname).write_bytes(body)
        (d / "engines.toml").write_text('[[engines]]\nname = "e"\n')
        sargs = types.SimpleNamespace(staged_home=None, store_main=str(src_dir / "khive.db"),
                                      store_events=str(src_dir / "khive.db.events.db"), copy_wal=False,
                                      models_dir=None, engines_file=str(d / "engines.toml"))
        try:
            stage(sargs, d / "refused")
            wal_msg = ""
        except BankError as e:
            wal_msg = str(e)
        check("staging: a WAL refusal on the second source leaves no copy of the first",
              "WAL is not empty" in wal_msg and "khive.db.events.db" in wal_msg
              and not list((d / "refused" / "home" / ".khive").glob("khive.db*")))
        (src_dir / "khive.db.events.db-wal").unlink()
        try:
            _, _, staged_manifest = stage(sargs, d / "ok")
        except BankError:
            staged_manifest = []
        check("staging: with no pending WAL both sources are copied in order",
              [m["staged_as"] for m in staged_manifest] == ["khive.db", "khive.db.events.db"]
              and (d / "ok" / "home" / ".khive" / "khive.db.events.db").is_file())

    class OpBridge:
        def __init__(self):
            self.calls = []

        def one(self, text):
            self.calls.append(text)
            if text.startswith("get("):
                return {"content": "c"}
            if text.startswith("brain.create_profile"):
                return {}
            served = re.search(r'profile_id="([^"]+)"', text)
            return [{"id": u[0], "full_id": u[0], "created_at": "2026-01-01T00:00:00Z", "rank_score": 1.0,
                     "serve_attribution": "profile" if served else "unspecified",
                     **({"served_by_profile_id": served.group(1)} if served else {})}]

        def ops(self, texts):
            self.calls.extend(texts)
            return [{"ok": True, "result": [{"id": u[0]}]} for _ in texts]

    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        ob = OpBridge()
        oargs = types.SimpleNamespace(profile=["p1"], feedback_actors="", feedback_since="2025-01-01T00:00:00Z")
        ocases = [{"case_id": "q1", "query": "q", "T": "2026-10-28T12:00:00Z", "needed_info": ni}]
        run_arms(oargs, list(ARMS), "test", ocases, ob, d / "a", {"results": {"a": {}}},
                 {"bank_id": "a", "bank_version": 1, "population": 1}, "a")
        wait_ready(ob, "q", 1)
        control(ob, u[0])
        calibrate_case(ob, {"case_id": "q1", "query": "q"}, "live-p", "twin-p")
        recall_ops = [c for c in ob.calls if c.startswith("memory.recall(")]
        check("recall ops: every memory.recall op the runner builds carries the budget config",
              len(recall_ops) == 14 and all("config=" + json.dumps(RECALL_CONFIG) in c for c in recall_ops))

    class CappedBridge:
        def __init__(self, n, truncated):
            self.n, self.truncated = n, truncated

        def one(self, text):
            ids = [f"{i + 1:08x}-0000-4000-8000-000000000000" for i in range(self.n)]
            return [{"id": i, "full_id": i, "created_at": "2026-01-01T00:00:00Z", "serve_attribution": "unspecified",
                     **({"truncated": True} if self.truncated else {})} for i in ids]

    with tempfile.TemporaryDirectory() as d:
        d = Path(d)

        def scored_row(arm, n, truncated):
            out = d / f"{arm}-{n}-{truncated}"
            cargs = types.SimpleNamespace(profile=["p1"], feedback_actors="", feedback_since="2025-01-01T00:00:00Z")
            ccases = [{"case_id": "q1", "query": "q", "T": "2026-10-28T12:00:00Z", "needed_info": ni}]
            try:
                run_arms(cargs, [arm], "test", ccases, CappedBridge(n, truncated), out, {"results": {"a": {}}},
                         {"bank_id": "a", "bank_version": 1, "population": 1}, "a")
            except BankError:
                return None
            return json.loads((out / f"{arm}.jsonl").read_text().splitlines()[0])

        capped_row = scored_row("profile_off", SCORED_DEPTH, True)
        plain_row = scored_row("profile_off", SCORED_DEPTH, False)
        check("capped: a scored row from a capped response carries capped true, a plain one false",
              capped_row is not None and capped_row["capped"] is True
              and plain_row is not None and plain_row["capped"] is False)
        check("capped: a prefix shorter than the scored depth is refused by the runner",
              scored_row("profile_off", SCORED_DEPTH - 1, True) is None)
        check("composed: a capped response is refused by the runner, an uncapped one is scored",
              scored_row("composed", SCORED_DEPTH, True) is None
              and scored_row("composed", SCORED_DEPTH, False) is not None)
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        (d / "cases.jsonl").write_text(json.dumps({"case_id": "c1", "query": "q", "T": "2026-10-28T12:00:00Z",
                                                   "tags": ["namespace:team-a"], "needed_info": ni}) + "\n")
        (d / "bank.json").write_text(json.dumps({"bank_id": "b", "version": 1,
                                                 "cases_sha256": sha256_file(d / "cases.jsonl")}))
        a = types.SimpleNamespace(arms="composed", bank=[str(d)], profile=["p"], bank_visible=["b=team-a"],
                                  expect_profiles=None, expect_namespace="local", out=str(d / "out"))
        try:
            cmd_run(a)
            early = False
        except BankError:
            early = True
        except Exception:  # a refusal that comes later than the bank check reaches the incomplete args here
            early = False
        check("composed: a namespaced case is refused before anything is staged", early and not (d / "out").exists())
    # --- bank-visible and served-state arms (begin)
    def arm(name, fn):
        try:
            ok = bool(fn())
        except (Exception, SystemExit) as e:
            ok, name = False, f"{name} (raised {e!r})"
        check(name, ok)

    def bank_error(fn, *a):
        """The message of the BankError that fn raises, or None when it raises nothing."""
        try:
            fn(*a)
        except BankError as e:
            return f"{e} {getattr(e, 'code', '')}"
        return None

    def with_patches(patches, fn):
        g = globals()
        saved = {k: g[k] for k in patches if k in g}
        g.update(patches)
        try:
            return fn()
        finally:
            for k in patches:
                if k in saved:
                    g[k] = saved[k]
                else:
                    g.pop(k, None)

    tagged_case = {"case_id": "q1", "query": "q", "T": "2026-10-28T12:00:00Z", "tags": ["namespace:team:a"],
                   "needed_info": ni}

    def no_namespace_on_recalls():
        tb = OpBridge()
        targs = types.SimpleNamespace(profile=["p1"], feedback_actors="", feedback_since="2025-01-01T00:00:00Z")
        with tempfile.TemporaryDirectory() as td:
            run_arms(targs, ["profile_off", "profile_on"], "t", [tagged_case], tb, Path(td) / "o",
                     {"results": {"a": {}}}, {"bank_id": "a", "bank_version": 1, "population": 1}, "a")
        calls = [c for c in tb.calls if c.startswith("memory.recall(")]
        return (bool(calls) and any('profile_id="p1"' in c for c in calls)
                and not any("namespace=" in c for c in calls))

    arm("recall: a namespace-tagged case's recall ops carry no namespace argument (profile_off, profile_on)",
        no_namespace_on_recalls)
    arm("recall: a namespace argument handed to recall() is refused",
        lambda: refused(lambda: recall(OpBridge(), "q", None, "w", namespace="team:a")))
    arm("bank-visible: repeated specs parse to one set per bank",
        lambda: parse_bank_visible(["b=team:a,team:b", "c=local"]) == {"b": ["team:a", "team:b"], "c": ["local"]})
    arm("bank-visible: a spec without a bank, a set, or with an empty or repeated name is refused",
        lambda: all("--bank-visible" in (bank_error(parse_bank_visible, [s]) or "")
                    for s in ("b", "=x", "b=", "b=a,,c", "b=a,a", ))
        and "more than once" in (bank_error(parse_bank_visible, ["b=a", "b=c"]) or ""))
    arm("bank-visible: a set that covers every case tag is accepted",
        lambda: require_bank_visible([({"bank_id": "bank-a"}, [tagged_case])], {"bank-a": ["team:a", "local"]}) is None)

    default_profile = "balanced-recall-v1"
    real_open_bridge = open_bridge

    def recalls_first_open_bridge(args, db, env, scr, probe, profile_ids, served_check=None, err_name="kkernel-mcp.err"):
        """The order a bank must not use: the real open_bridge runs the readiness and control recalls, and the served-state
        check runs after them."""
        br, ready = real_open_bridge(args, db, env, scr, probe, profile_ids, err_name=err_name)
        if served_check is not None:
            try:
                served_check(br)
            except BaseException:
                br.close()
                raise
        return br, ready

    def fake_stage(events=None):
        """A stand-in for stage(): it makes the staged home directory and returns a two-store manifest."""
        def staged(args, scr):
            if events is not None:
                events.append("stage")
            (scr / "home" / ".khive").mkdir(parents=True)
            return (scr / "home" / ".khive" / "khive.db", {},
                    [{"staged_as": "khive.db", "source_mtime_ns": 1},
                     {"staged_as": "khive.db.events.db", "source_mtime_ns": 1}])
        return staged

    def normal_pressure():
        return {"source": "test", "reading": "1", "normal": True}

    def drive_banks(visible_by_bank, expect=None, served=0, moved_to=None, credit=None, profiles=("p1",),
                    recalls_first=False, trace=None):
        """run_banks over a fake copy and fake server processes. The config writer, stage_once and the pressure check are
        the real ones, over a fake stage() and a fake pressure reader. Without `credit`, open_bridge and the arm runner
        are fakes. With `credit` ({profile: stored total_events}), open_bridge, wait_ready, control and
        check_served_state are the real ones over fake server processes that model the credit, and the fake arm runner
        sends one recall that names no profile and one that names the last profile. `recalls_first` runs the check after
        the readiness and control recalls, the order a bank must not use. `trace` receives the server processes
        ("procs") and the pressure reads ("pressure_calls"); its "abnormal_at" is the number of the pressure read that
        reports a pressure that is not normal."""
        events = []
        trace = {} if trace is None else trace
        trace["procs"], trace["pressure_calls"] = [], []
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            (td / "engines.toml").write_text('[[engines]]\nname = "e"\n')

            class ServedBridge:
                def __init__(self, total):
                    self.total = total

                def one(self, text):
                    m = re.search(r'profile_id="([^"]+)"', text)
                    if not text.startswith("brain.profile") or m is None:
                        return {}
                    if self.total is None:
                        raise BankError(f"profile {m.group(1)} is not a known profile")
                    return {"total_events": self.total}

                def close(self):
                    events.append("close")

            class CreditBridge:
                """A fake `kkernel mcp` process that models the credit. A recall that names no profile adds one to the default
                profile's count in this process's memory, a recall that names a profile adds one to that profile, and a
                profile read serves the current count. Every process starts from the stored counts (`credit`), as a new
                server process does after read-only arms. `log` holds the handshake and every op text, in order."""

                def __init__(self, kkernel, db, env, err_path):
                    err_path.touch()
                    self.err_path, self.counts, self.log = err_path, dict(credit), []
                    trace["procs"].append(self)

                def handshake(self):
                    self.log.append("handshake")
                    return {}

                def ops(self, texts, timeout=900):
                    results = []
                    for text in texts:
                        self.log.append(text)
                        verb = text.split("(", 1)[0]
                        named = re.search(r'profile_id="([^"]+)"', text)
                        if verb == "get":
                            results.append({"ok": True, "result": {"content": "c"}})
                        elif verb == "brain.profile":
                            if named is None or named.group(1) not in self.counts:
                                results.append({"ok": False, "error": "no such profile"})
                            else:
                                results.append({"ok": True, "result": {"total_events": self.counts[named.group(1)]}})
                        elif verb == "memory.recall":
                            who = named.group(1) if named else default_profile
                            self.counts[who] = self.counts.get(who, 0) + 1
                            hit = {"id": u[0], "full_id": u[0], "created_at": "2026-01-01T00:00:00Z",
                                   "serve_attribution": "profile" if named else "unspecified"}
                            if named:
                                hit["served_by_profile_id"] = who
                            results.append({"ok": True, "result": [hit]})
                        else:
                            results.append({"ok": True, "result": {}})
                    return results

                def one(self, text, timeout=900):
                    r = self.ops([text])[0]
                    if not r.get("ok"):
                        raise BankError(f"{text[:80]}: {r.get('error')!r}")
                    return r.get("result")

                def close(self):
                    events.append("close")

            def fake_open_bridge(args, db, env, scr, probe, profile_ids, served_check=None, err_name="kkernel-mcp.err"):
                cfg = scr / "home" / ".khive" / "config.toml"
                found = re.search(r"visible_namespaces\s*=\s*(\[[^\]]*\])", cfg.read_text()) if cfg.exists() else None
                events.append(("start", json.loads(found.group(1)) if found else None))
                sb = ServedBridge(served)
                if served_check is not None:
                    try:
                        served_check(sb)
                    except BaseException:
                        sb.close()
                        raise
                return sb, {"ready_s": 0.0, "control": {}}

            def fake_run_arms(args, arms, run_id, cases, br, out, summary, base, bank_id):
                events.append(("arms", bank_id))
                if moved_to is not None:
                    br.total = moved_to
                if credit is not None:
                    br.one(recall_op(query="q", top_k=5))
                    br.one(recall_op(query="q", top_k=5, profile_id=args.profile[-1]))

            banks = [({"bank_id": b, "version": 1, "cases_sha256": "x"},
                      [{"case_id": "c", "query": "q", "T": "t", "tags": []}]) for b in visible_by_bank]
            dargs = types.SimpleNamespace(kkernel="k", snapshot_id="s", engines_file=str(td / "engines.toml"),
                                          profile=list(profiles), feedback_actors="", ready_max_s=5, control_id=u[0])
            summary = {"banks": [], "results": {}}

            def fake_pressure():
                trace["pressure_calls"].append(1)
                n = len(trace["pressure_calls"])
                return {"source": "test", "reading": f"read-{n}", "normal": n != trace.get("abnormal_at")}

            patches = {"stage": fake_stage(events), "memory_pressure": fake_pressure, "open_bridge": fake_open_bridge,
                       "run_arms": fake_run_arms, "kkernel_version": lambda p: "test"}
            if credit is not None:
                patches["Bridge"] = CreditBridge
                patches["open_bridge"] = recalls_first_open_bridge if recalls_first else real_open_bridge
            err = None
            try:
                with_patches(patches, lambda: run_banks(dargs, ["profile_off"], "rid", banks, visible_by_bank, expect,
                                                        td / "scratch", td / "out", summary))
            except BankError as e:
                err = e
            return events, summary, err

    def ran_arms(events):
        return any(isinstance(e, tuple) and e[0] == "arms" for e in events)

    def two_configs():
        vis = {"bank-a": ["team:a"], "bank-b": ["team:b", "local"]}
        events, summary, err = drive_banks(vis)
        return (err is None
                and events == ["stage", ("start", ["team:a"]), ("arms", "bank-a"), "close",
                               ("start", ["team:b", "local"]), ("arms", "bank-b"), "close"]
                and [b["visible_namespaces"] for b in summary["banks"]] == [["team:a"], ["team:b", "local"]])

    arm("banks: two banks get two configs, each listing exactly its own set, written before each process starts, "
        "one copy", two_configs)

    def cmd_refusal(bank_specs, visible, needle, arms="profile_off", expect=None, patches=None):
        """cmd_run over banks in a temp dir: the refusal names `needle`, no copy was attempted, no output exists. `expect`
        is the --expect-profiles value; `patches` are globals replaced for the call (a stand-in that raises `needle`)."""
        copied = []
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            dirs = []
            for bank_id, tag in bank_specs:
                bd = td / bank_id
                bd.mkdir()
                (bd / "cases.jsonl").write_text(json.dumps({"case_id": "c1", "query": "q", "T": "2026-10-28T12:00:00Z",
                                                            "tags": [f"namespace:{tag}"], "needed_info": ni}) + "\n")
                (bd / "bank.json").write_text(json.dumps({"bank_id": bank_id, "version": 1,
                                                          "cases_sha256": sha256_file(bd / "cases.jsonl")}))
                dirs.append(str(bd))
            cargs = types.SimpleNamespace(arms=arms, bank=dirs, profile=["p1"], bank_visible=visible,
                                          expect_profiles=expect, expect_namespace="local", out=str(td / "out"),
                                          engines_file=str(td / "missing-engines.toml"))

            def no_copy(*a, **k):
                copied.append(1)
                raise RuntimeError("copy attempted")

            msg = with_patches({"stage": no_copy, **(patches or {})}, lambda: bank_error(cmd_run, cargs))
            return msg is not None and needle in msg and not copied and not (td / "out").exists()

    arm("banks: a bank without a --bank-visible set is refused before any copy",
        lambda: cmd_refusal([("bank-a", "team:a"), ("bank-b", "team:b")], ["bank-a=team:a"], "no --bank-visible set"))
    arm("banks: a set that names a bank which is not given is refused before any copy",
        lambda: cmd_refusal([("bank-a", "team:a")], ["bank-a=team:a", "bank-z=team:z"], "not given with --bank"))
    arm("banks: a case's namespace tag outside its bank's set is refused before any copy",
        lambda: cmd_refusal([("bank-a", "team:a")], ["bank-a=team:b"], "is not in the bank's visible set"))
    arm("served state: the profile_on arm without --expect-profiles is refused before any copy",
        lambda: cmd_refusal([("bank-a", "team:a")], ["bank-a=team:a"], "--expect-profiles is required",
                            arms="profile_on"))

    snap = json.dumps({"profiles": {"p1": {"total_events": 3596}, "p2": {"total_events": 12}}})
    other = json.dumps({"profiles": {"p1": {"total_events": 1}}})

    def with_expect_file(rows, fn):
        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "brain_profile_snapshots.jsonl"
            p.write_text("".join(json.dumps(r) + "\n" for r in rows))
            return fn(p)

    def expected_file_ok(p):
        got = load_expected_events(str(p), "local", ["p1", "p2"])
        return (got["events"] == {"p1": 3596, "p2": 12} and got["sha256"] == sha256_file(p)
                and got["path"] == str(p) and got["namespace"] == "local")

    rows_ok = [["other", "local", "{}", "t0"], ["__brain__", "team:a", other, "t1"], ["__brain__", "local", snap, "t2"]]
    arm("served state: the expected file yields each profile's total_events from the __brain__ row of its namespace",
        lambda: with_expect_file(rows_ok, expected_file_ok))
    arm("served state: a profile the expected row lacks is refused",
        lambda: with_expect_file(rows_ok, lambda p: "absent" in (
            bank_error(load_expected_events, str(p), "local", ["p1", "absent"]) or "")))
    arm("served state: a file with no __brain__ row for the namespace is refused",
        lambda: with_expect_file([["__brain__", "team:a", snap, "t"], ["p1", "local", snap, "t"]],
                                 lambda p: "no __brain__ row" in (
                                     bank_error(load_expected_events, str(p), "local", ["p1"]) or "")))

    expect_3596 = {"events": {"p1": 3596}}
    one_bank = {"bank-a": ["team:a"]}

    def inequality_stops():
        events, summary, err = drive_banks(one_bank, expect=expect_3596, served=0)
        msg = f"{err} {getattr(err, 'code', '')}" if err is not None else ""
        return (err is not None and "p1" in msg and "3596" in msg and "total_events 0" in msg
                and not ran_arms(events) and events[-1] == "close"
                and summary["banks"][0]["served_state"]["pairs"] == [{"profile": "p1", "expected": 3596, "served": 0}])

    def equality_passes():
        events, summary, err = drive_banks(one_bank, expect=expect_3596, served=3596)
        return (err is None and ran_arms(events)
                and summary["banks"][0]["served_state"]["pairs"] == [{"profile": "p1", "expected": 3596, "served": 3596}])

    def after_is_recorded_not_enforced():
        events, summary, err = drive_banks(one_bank, expect=expect_3596, served=3596, moved_to=3600)
        state = summary["banks"][0]["served_state"]
        return (err is None and state["pairs"][0]["served"] == 3596
                and state["after_in_process"] == [{"profile": "p1", "in_process_total_events": 3600}])

    def missing_profile_stops():
        events, summary, err = drive_banks(one_bank, expect=expect_3596, served=None)
        return err is not None and "p1" in f"{err} {getattr(err, 'code', '')}" and not ran_arms(events)

    arm("served state: total_events 0 served against 3596 expected stops the run before any arm", inequality_stops)
    arm("served state: equal total_events passes and the compared pair is recorded", equality_passes)
    arm("served state: the profile read again after the arms is recorded and never stops the run",
        after_is_recorded_not_enforced)
    arm("served state: a profile the server does not find stops the run before any arm", missing_profile_stops)
    arm("served state: mutation control, with the comparison replaced by a no-op the inequality arm reports failure",
        lambda: with_patches({"require_served_events": lambda profile, expected, served: None},
                             inequality_stops) is False)
    # --- served-state order, arms that write brain state, per-bank hygiene and the calibrate setup path
    cr_profiles = (default_profile, "p1")
    cr_stored = {default_profile: 3596, "p1": 12}
    cr_expect = {"events": dict(cr_stored)}
    two_banks = {"bank-a": ["team:a"], "bank-b": ["team:b", "local"]}

    def credit_run(visible=two_banks, recalls_first=False):
        """run_banks over fake server processes that model the credit; every process starts from the stored counts."""
        trace = {}
        events, summary, err = drive_banks(visible, expect=cr_expect, credit=cr_stored, profiles=cr_profiles,
                                           recalls_first=recalls_first, trace=trace)
        return events, summary, err, trace

    def served_pairs(summary, i=0):
        return summary["banks"][i]["served_state"]["pairs"]

    def check_before_recalls_passes():
        events, summary, err, trace = credit_run()
        after = {r["profile"]: r["in_process_total_events"]
                 for r in summary["banks"][0]["served_state"]["after_in_process"]}
        stored_pairs = [{"profile": default_profile, "expected": 3596, "served": 3596},
                        {"profile": "p1", "expected": 12, "served": 12}]
        return (err is None and ran_arms(events) and served_pairs(summary, 0) == stored_pairs
                and served_pairs(summary, 1) == stored_pairs
                and after["p1"] == 13 and after[default_profile] >= 3596 + 4)

    def check_after_recalls_fails():
        events, summary, err, trace = credit_run(recalls_first=True)
        pairs = served_pairs(summary)
        log = trace["procs"][0].log
        first_recall = next(i for i, t in enumerate(log) if t.startswith("memory.recall"))
        first_read = next(i for i, t in enumerate(log) if t.startswith("brain.profile"))
        return (err is not None and not ran_arms(events) and len(pairs) == 1
                and pairs[0]["profile"] == default_profile and pairs[0]["expected"] == 3596
                and pairs[0]["served"] >= 3596 + 3 and first_recall < first_read)

    def reads_precede_recalls():
        events, summary, err, trace = credit_run()
        if err is not None or len(trace["procs"]) != 2:
            return False
        for proc in trace["procs"]:
            log = proc.log
            reads = [i for i, t in enumerate(log) if t.startswith("brain.profile")]
            recalls = [i for i, t in enumerate(log) if t.startswith("memory.recall")]
            if not (log[0] == "handshake" and reads[:2] == [1, 2] and recalls and recalls[0] > 2
                    and default_profile in log[1] and "p1" in log[2]):
                return False
        return True

    arm("served state: with the credit modelled, the check run before the first recall of each bank's process passes "
        "on the stored counts", check_before_recalls_passes)
    arm("served state: with the credit modelled, the check run after the readiness and control recalls (the earlier "
        "order, patched in here) reads the stored count plus the probe recalls and stops", check_after_recalls_fails)
    arm("served state: in each bank's process every profile read of the check comes before the first memory.recall",
        reads_precede_recalls)

    for written_arm in ("feedback_fresh", "feedback_asof"):
        arm(f"served state: --expect-profiles with {written_arm} is refused before any copy, naming the brain-state write",
            lambda a=written_arm: cmd_refusal(
                [("bank-a", "team:a")], ["bank-a=team:a"],
                f"combined with {a}: those arms write brain state to the staged copy",
                arms=f"profile_off,{a}", expect="expected.jsonl"))

    def stop_at_expected_file(*a, **k):
        raise BankError("reached the expected-file read")

    arm("served state: profile_off,profile_on with --expect-profiles is accepted and reaches the expected-file read",
        lambda: cmd_refusal([("bank-a", "team:a")], ["bank-a=team:a"], "reached the expected-file read",
                            arms="profile_off,profile_on", expect="expected.jsonl",
                            patches={"load_expected_events": stop_at_expected_file}))

    def pressure_per_bank():
        events, summary, err, trace = credit_run()
        return (err is None and len(trace["pressure_calls"]) == 3
                and summary["memory_pressure"]["before_copy"]["reading"] == "read-1"
                and [b["memory_pressure"]["before_server"]["reading"] for b in summary["banks"]] == ["read-2", "read-3"])

    def pressure_stops_second_bank():
        trace = {"abnormal_at": 3}
        events, summary, err = drive_banks(two_banks, trace=trace)
        return (err is not None and len(trace["pressure_calls"]) == 3
                and events == ["stage", ("start", ["team:a"]), ("arms", "bank-a"), "close"])

    def stderr_per_bank():
        events, summary, err, trace = credit_run()
        paths = [b["server_stderr"] for b in summary["banks"]]
        names = [Path(p).name for p in paths]
        return (err is None and len(set(paths)) == 2 and names == ["kkernel-mcp-bank1.err", "kkernel-mcp-bank2.err"]
                and paths == [str(p.err_path) for p in trace["procs"]]
                and not any(bank_id in n for bank_id in two_banks for n in names))

    arm("pressure: read once before the copy and once before each bank's server (three reads for two banks), each "
        "recorded in that bank's record", pressure_per_bank)
    arm("pressure: a pressure that is not normal before the second bank's server stops the run before that process",
        pressure_stops_second_bank)
    arm("stderr: two banks write to two distinct index-named files, each recorded in its bank's record", stderr_per_bank)

    def calibrate_setup_reaches_write_config():
        seen = []
        real_write_config = write_config

        def recording_write_config(scratch, args, visible):
            seen.append(list(visible))
            real_write_config(scratch, args, visible)

        def stop_at_open_bridge(*a, **k):
            raise BankError("stopped at open_bridge")

        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            (td / "engines.toml").write_text('[[engines]]\nname = "e"\n')
            argv = ["bank.py", "calibrate-replay", "--kkernel", "k", "--store-main", "m.db", "--store-events", "e.db",
                    "--snapshot-id", "s", "--engines-file", str(td / "engines.toml"), "--profile", "p1",
                    "--control-id", u[0], "--feedback-actors", "a", "--queries-from-events",
                    "--namespaces", "team:a", "--scratch-parent", str(td), "--out", str(td / "report.json")]
            saved_argv = sys.argv
            sys.argv = argv
            try:
                msg = with_patches({"stage": fake_stage(), "memory_pressure": normal_pressure,
                                    "write_config": recording_write_config, "open_bridge": stop_at_open_bridge,
                                    "finish_scratch": lambda a, s, r: None, "kkernel_version": lambda p: "test"},
                                   lambda: bank_error(main))
            except AttributeError:
                return False
            finally:
                sys.argv = saved_argv
            report = json.loads((td / "report.json").read_text())
        return (msg is not None and "stopped at open_bridge" in msg and seen == [["team:a"]]
                and report["visible_namespaces"] == ["team:a"])

    arm("calibrate-replay: the setup path reaches write_config with a visible set and raises no AttributeError",
        calibrate_setup_reaches_write_config)
    # --- bank-visible and served-state arms (end)
    gate_self_checks(arm, with_patches)
    print("SELF-TEST " + ("PASS" if not fails else f"FAIL ({len(fails)})"))
    return 1 if fails else 0


def add_server_args(p) -> None:
    """The arguments every subcommand that stages a store copy and serves it takes."""
    p.add_argument("--kkernel", default="kkernel")
    p.add_argument("--store-main", default=None, help="the staged copy's main store file")
    p.add_argument("--store-events", default=None, help="the staged copy's events store file")
    p.add_argument("--staged-home", default=None,
                   help="a directory holding home/.khive/{khive.db,khive.db.events.db} and transfer.json from a "
                        "guarded transfer; verified and used in place instead of copied, and never removed")
    p.add_argument("--snapshot-id", required=True, help="written into every row")
    p.add_argument("--engines-file", required=True, help="the [[engines]] tables the store was embedded with")
    p.add_argument("--models-dir", default=None, help="linked as the scratch home's ~/.lattice/models")
    p.add_argument("--control-id", required=True, help="a memory that must come back for its own content")
    p.add_argument("--ready-max-s", type=int, default=3600)
    p.add_argument("--scratch-parent", default=None)
    p.add_argument("--remove-scratch", action="store_true",
                   help="remove the scratch home and its store copy at the end (kept by default)")
    p.add_argument("--copy-wal", action="store_true", help="stage a non-empty WAL with its store (no writer active)")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    cb = sub.add_parser("check-bank")
    cb.add_argument("bank")
    for name in ("run", "calibrate-replay"):
        p = sub.add_parser(name)
        add_server_args(p)
        p.add_argument("--bank", action="append", required=(name == "run"),
                       help="bank directory; repeat for several banks")
        p.add_argument("--profile", action="append", required=True, help="live profile id; repeat for comparisons")
        p.add_argument("--feedback-actors", default="", help="comma-separated actors whose feedback events replay")
        p.add_argument("--feedback-since", default="2025-01-01T00:00:00Z")
        if name == "run":
            p.add_argument("--arms", default=",".join(ARMS))
            p.add_argument("--bank-visible", action="append", default=None, metavar="BANK_ID=NS[,NS...]",
                           help="the namespaces the bench actor reads while serving that bank; required once for "
                                "every --bank")
            p.add_argument("--expect-profiles", default=None,
                           help="a brain_profile_snapshots.jsonl export holding the frozen profiles' total_events; "
                                "required when the profile_on arm runs")
            p.add_argument("--expect-namespace", default="local",
                           help="the namespace whose __brain__ row of --expect-profiles is the expected state")
            p.add_argument("--out", required=True, help="a new directory: <bank_id>/<arm>.jsonl and run.json")
        else:
            p.add_argument("--namespaces", default="", help="comma-separated namespaces the bench actor reads")
            p.add_argument("--n", type=int, default=50)
            p.add_argument("--queries-from-events", action="store_true",
                           help="compare on n distinct queries the profile served (recall_executed), not a bank")
            p.add_argument("--out", required=True, help="the calibration report (JSON)")
    g = sub.add_parser("rebind-gate", help="decide whether one profile may serve recall again")
    add_server_args(g)
    g.add_argument("--profile", required=True, help="the candidate profile id")
    g.add_argument("--own-bank", required=True, help="bank whose answers are in the reader's own namespace")
    g.add_argument("--shared-bank", required=True, help="bank whose answers are in the shared pool (local)")
    g.add_argument("--own-visible", required=True, metavar="NS[,NS...]",
                   help="the namespaces the reader reads while the own-namespace bank runs")
    g.add_argument("--shared-visible", required=True, metavar="NS[,NS...]",
                   help="the namespaces the reader reads while the shared-pool bank runs")
    g.add_argument("--round-gap-s", type=float, default=300.0,
                   help="seconds between the two rounds (default 300)")
    g.add_argument("--max-round-runs", type=int, default=6,
                   help="runs of one round before the gate stops, void runs included (default 6)")
    g.add_argument("--token-budget-cap", type=int, default=None,
                   help="the recall token-budget cap, used only when the recall help in the copy is read and states none")
    g.add_argument("--create-candidate-local-factor", type=float, default=None, metavar="F",
                   help="create the candidate on the copy with this local_factor (a finite number in [0.5, 1]) before "
                        "round 1; refused when the candidate is already on the copy. Without it the candidate must "
                        "already be on the copy")
    g.add_argument("--out", required=True, help="a new directory: rebind-gate.json and rebind-gate.txt")
    sub.add_parser("self-test")
    args = ap.parse_args()
    if args.cmd == "check-bank":
        return cmd_check_bank(args)
    if args.cmd == "self-test":
        return self_test()
    if args.cmd == "rebind-gate":
        return cmd_rebind_gate(args)
    require_store_source(args)
    if args.cmd == "calibrate-replay":
        require_calibration_shape(args.bank, args.queries_from_events)
        require_calibration_profiles(args.profile)
    require_distinct(args.profile, "profile values")
    for p in args.profile:
        safe_name(p, "profile")
    if args.cmd == "calibrate-replay":
        banks = [load_bank(Path(args.bank[0]))] if args.bank else []
        args.visible_namespaces = visible_namespace_union(args.namespaces, [c for _, c in banks])
    elif "feedback_asof" in args.arms.split(","):
        require_profile_count(args.profile, "feedback_asof")
    if (args.cmd == "calibrate-replay" or "feedback_asof" in args.arms.split(",")) and not args.feedback_actors:
        raise BankError("--feedback-actors is required to replay feedback events; an empty list would replay none")
    return cmd_run(args) if args.cmd == "run" else cmd_calibrate_replay(args)


if __name__ == "__main__":
    sys.exit(main())
