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
WAL unless --copy-wal says no process writes it (the WAL is then staged beside its store). It writes
its own config for an actor that has no binding row, and serves every arm from one `kkernel mcp` stdio process on the
copy, run with KHIVE_NO_DAEMON=1 so no warm daemon starts or outlives the run. Writes made during the run (serve records, the profiles the feedback arms create) land in the copy and are
removed with it. Every row carries the bank id and version, the population, the arm, the store snapshot id and the
kkernel version.

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
                      the two on the bank's queries (top-10 overlap, rank-score differences on shared ids)
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


def stage(args, scratch: Path) -> tuple[Path, dict, list[dict]]:
    """Copy the store into scratch/home/.khive, write the bench config, return (main db path, env, manifest rows)."""
    live = Path(os.path.expanduser("~/.khive")).resolve()
    home = scratch / "home"
    kdir = home / ".khive"
    (kdir / "vol-locks").mkdir(parents=True)
    manifest = []
    for src, name in ((args.store_main, "khive.db"), (args.store_events, "khive.db.events.db")):
        src = Path(src).resolve()
        if under(src, live):
            raise BankError(f"refusing {src}: it is under the caller's live store directory {live}")
        wal = Path(str(src) + "-wal")
        dest = kdir / name
        if wal.exists() and wal.stat().st_size > 0:
            if not args.copy_wal:
                raise BankError(f"refusing {src}: its WAL is not empty; stage a checkpointed copy, or pass --copy-wal "
                                "when no process writes the source")
            shutil.copyfile(wal, Path(str(dest) + "-wal"))
            manifest.append({"source": str(wal), "staged_as": name + "-wal", "bytes": wal.stat().st_size,
                             "sha256": sha256_file(Path(str(dest) + "-wal"))})
        shutil.copyfile(src, dest)
        manifest.append({"source": str(src), "staged_as": name, "bytes": dest.stat().st_size,
                         "sha256": sha256_file(dest)})
    if args.models_dir:
        (home / ".lattice").mkdir()
        os.symlink(Path(args.models_dir).resolve(), home / ".lattice" / "models")
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
    db, env, manifest = stage(args, scratch)
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
    return meta, cases, br, base, {"ready_s": round(ready_s, 1), "control": ctl, "staged": manifest}


def scratch_dir(args, run_id: str) -> Path:
    if os.environ.get("KHIVE_DB"):
        raise BankError("refusing to run with KHIVE_DB set in the caller's environment; unset it")
    if args.bank:
        load_bank(Path(args.bank))
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
        (out / "run.json").write_text(json.dumps(summary, indent=1) + "\n")
        if not args.keep_scratch:
            shutil.rmtree(scratch)
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


def cmd_calibrate_replay(args) -> int:
    run_id = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dt%H%M%Sz")
    scratch = scratch_dir(args, run_id)
    try:
        meta, cases, br, base, setup = start(args, run_id, scratch)
    except BaseException:
        if not args.keep_scratch:
            shutil.rmtree(scratch)
        raise
    try:
        twin = f"bench-replay-{run_id}"
        br.one(op("brain.create_profile", name=twin, description="question-bank calibration: full feedback replay"))
        now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        actors = [a for a in args.feedback_actors.split(",") if a]
        if args.queries_from_events:
            cases, distinct = served_queries(br, actors, args.profile, args.feedback_since, now, args.n)
            if not cases:
                raise BankError(f"no recall_executed event served by {args.profile} for {actors}: nothing to compare")
            base["query_source"] = {"kind": "recall_executed", "profile": args.profile, "actors": actors,
                                    "distinct_queries": distinct, "sampled": len(cases)}
            base["population"] = len(cases)
        events = feedback_events(br, actors, args.profile, args.feedback_since, now)
        counts = replay(br, events, twin)
        rows = []
        for c in cases[: args.n]:
            a = hit_list(br.one(op("memory.recall", query=c["query"], top_k=10, profile_id=args.profile)), "calibrate")
            b = hit_list(br.one(op("memory.recall", query=c["query"], top_k=10, profile_id=twin)), "calibrate")
            ra = {hit_id(h, "calibrate"): h.get("rank_score") for h in a}
            rb = {hit_id(h, "calibrate"): h.get("rank_score") for h in b}
            shared = [i for i in ra if i in rb]
            diffs = [abs(ra[i] - rb[i]) for i in shared if isinstance(ra[i], (int, float)) and isinstance(rb[i], (int, float))]
            rows.append({"query_id": c["case_id"], "overlap@10": len(shared) / max(len(ra), 1),
                         "same_order": list(ra) == list(rb), "max_abs_rank_score_diff": max(diffs, default=0.0)})
        n = len(rows)
        report = {**base, **setup, "events_found": len(events), **counts, "queries": n,
                  "mean_overlap@10": round(sum(r["overlap@10"] for r in rows) / n, 4),
                  "same_order_share": round(sum(r["same_order"] for r in rows) / n, 4),
                  "max_abs_rank_score_diff": max(r["max_abs_rank_score_diff"] for r in rows), "rows": rows}
        Path(args.out).write_text(json.dumps(report, indent=1) + "\n")
        print(json.dumps({k: v for k, v in report.items() if k != "rows"}), flush=True)
    finally:
        br.close()
        if not args.keep_scratch:
            shutil.rmtree(scratch)
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
        p.add_argument("--store-main", required=True, help="the staged copy's main store file")
        p.add_argument("--store-events", required=True, help="the staged copy's events store file")
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
        p.add_argument("--keep-scratch", action="store_true")
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
    if args.cmd == "calibrate-replay" and bool(args.bank) == args.queries_from_events:
        raise BankError("calibrate-replay takes exactly one of --bank and --queries-from-events")
    if (args.cmd == "calibrate-replay" or "feedback_asof" in args.arms.split(",")) and not args.feedback_actors:
        raise BankError("--feedback-actors is required to replay feedback events; an empty list would replay none")
    return cmd_run(args) if args.cmd == "run" else cmd_calibrate_replay(args)


if __name__ == "__main__":
    sys.exit(main())
