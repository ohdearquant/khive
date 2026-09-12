#!/usr/bin/env python3
"""Compile blob-upload mutants, verify discriminating failures, and restore.

Requires an isolated clean checkout, an explicit native CARGO_TARGET_DIR and
Python pytest/pydantic/blake3 dependencies. Use a fresh --out outside the checkout.
--case all runs all five controls, including daemon cleanup ownership.
--case owner runs that control alone, leaving the timer and heartbeat alive
while suppressing its sweep call. Rustup selects the pinned toolchain explicitly.
Every case retains raw logs and rebuilds the restored source before succeeding.

Run with Python dependencies in the same interpreter:
uv run --no-project --with 'pytest>=8' --with 'pydantic>=2.7' --with 'blake3>=1' \
python scripts/test-blob-upload-mutations.py --root CHECKOUT --head REVISION \
--binary TARGET/debug/kkernel --out FRESH_DIRECTORY
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys


CASES = ("tail", "expiry", "sweep", "publisher", "owner")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--case", choices=("all", *CASES), default="all")
    args = parser.parse_args()
    missing = [name for name in ("pytest", "pydantic", "blake3") if importlib.util.find_spec(name) is None]
    if missing:
        parser.error(f"missing Python dependencies: {', '.join(missing)}; use the uv command in --help")
    root, binary, out = args.root.resolve(), args.binary.resolve(), args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    if head != args.head:
        raise RuntimeError(f"unexpected head {head}")
    status = subprocess.check_output(["git", "status", "--porcelain"], cwd=root, text=True)
    if status:
        raise RuntimeError("controls require a clean source tree")
    if not os.environ.get("CARGO_TARGET_DIR"):
        raise RuntimeError("select CARGO_TARGET_DIR explicitly")

    env = dict(os.environ)
    env["KKERNEL"] = str(binary)
    env["PYTHONPATH"] = str(root / "python")
    env["RUSTUP_TOOLCHAIN"] = "1.95.0"
    target_dir = Path(env["CARGO_TARGET_DIR"])
    if not target_dir.is_absolute():
        target_dir = root / target_dir
    env["CARGO_TARGET_DIR"] = str(target_dir.resolve())
    target = target_dir.resolve() / "debug" / "kkernel"
    if env.get("CARGO_BUILD_TARGET") or binary != target:
        raise RuntimeError(f"select the native build output {target} with no CARGO_BUILD_TARGET")
    cases = CASES if args.case == "all" else (args.case,)
    results = {"head": head, "root": str(root), "target": env["CARGO_TARGET_DIR"], "selected_cases": cases, "commands": [], "cases": {}, "status": "INCOMPLETE"}

    def persist():
        (out / "results.json").write_text(json.dumps(results, indent=2) + "\n")

    def run(name, command, expected):
        completed = subprocess.run(command, cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        (out / (name + ".log")).write_bytes(completed.stdout)
        receipt = {"name": name, "command": command, "exit_code": completed.returncode, "expected_exit_code": expected}
        if binary.is_file():
            receipt["binary_sha256"] = sha(binary.read_bytes())
        results["commands"].append(receipt)
        persist()
        text = completed.stdout.decode(errors="replace")
        if completed.returncode != expected:
            raise RuntimeError(f"{name}: exit {completed.returncode}, expected {expected}; see log")
        return text

    def build(name):
        run(name, ["rustup", "run", "1.95.0", "cargo", "build", "--manifest-path", "crates/Cargo.toml", "--locked", "-p", "kkernel"], 0)
        if not binary.is_file():
            raise RuntimeError("selected candidate binary was not produced")
        target = Path(env["CARGO_TARGET_DIR"]).resolve() / "debug" / "kkernel"
        if binary != target:
            raise RuntimeError(f"selected binary {binary} differs from this build output {target}")

    test_file = "python/tests/test_blob_upload_wire_integration.py"
    names = {
        "tail": "test_blob_upload_wire_tail_mismatch_aborts[same_length_different_bytes]",
        "expiry": "test_blob_upload_wire_verb_expiry_without_sweeper[put_part]",
        "sweep": "test_blob_upload_wire_restart_orphan_sweep_and_begin_again",
        "live": "test_blob_upload_wire_daemon_expiry_without_verbs_keeps_committed_object",
        "owner": "test_blob_upload_wire_daemon_owns_expiry_after_mcp_client_exit",
    }

    def witness(label, name, red=False):
        text = run(label, [sys.executable, "-m", "pytest", "-q", "-s", "--color=no", "--tb=long", "-o", "addopts=", test_file + "::" + names[name]], 1 if red else 0)
        counts = {kind: int(value) for value, kind in re.findall(r"\b(\d+) (passed|failed|skipped|deselected|error)\b", text)}
        expected = {"failed": 1} if red else {"passed": 1}
        if counts != expected:
            raise RuntimeError(f"{label}: unexpected pytest counts {counts}")
        if red and name in ("tail", "expiry") and not re.search(
            r"^E\s+AssertionError: unexpectedly accepted blob\.put_part:", text, re.MULTILINE
        ):
            raise RuntimeError(f"{label}: failure was not acceptance of the refused write")
        if red and name in ("sweep", "owner") and not re.search(
            r"^E\s+AssertionError: daemon did not expire ", text, re.MULTILINE
        ):
            raise RuntimeError(f"{label}: failure was not retained staging")

    upload_source = root / "crates/khive-pack-blob/src/uploads.rs"
    owner_source = root / "crates/khive-mcp/src/components.rs"
    originals = {path: path.read_bytes() for path in (upload_source, owner_source)}

    def replace_one(value, before, after):
        if value.count(before) != 1:
            raise RuntimeError(f"mutation anchor is not unique: {before!r}")
        return value.replace(before, after, 1)

    def mutated(case, text):
        if case == "owner":
            return replace_one(
                text,
                "match manager.sweep().await {",
                "match Ok::<u64, khive_runtime::RuntimeError>(0) {",
            ).encode()
        if case == "tail":
            changed = replace_one(text, "bytes.len() != tail_len || part_hash != tail_hash", "bytes.len() != tail_len")
            return replace_one(changed, "let (tail_len, tail_hash)", "let (tail_len, _tail_hash)").encode()
        if case == "expiry":
            return replace_one(text, "if record.last_part.elapsed() >= self.policy.idle_for {", "if false {").encode()
        start = text.index("        match store.sweep_uploads(self.policy.idle_for).await {")
        end = text.index("        match failure {", start)
        return (text[:start] + "        let _ = store; // backend staging sweep deliberately removed\n" + text[end:]).encode()

    try:
        for case in cases:
            if case == "publisher":
                helper = Path(__file__).resolve().with_name("test-blob-upload-publish-control.py")
                run("publisher", [sys.executable, str(helper), "--root", str(root), "--out", str(out / "publisher")], 0)
                continue
            source = owner_source if case == "owner" else upload_source
            original = originals[source]
            if any(path.read_bytes() != data for path, data in originals.items()):
                raise RuntimeError("source changed between mutation cases")
            build(case + "-before-build")
            witness(case + "-before", case)
            if case == "sweep":
                witness("sweep-live-before", "live")
            changed = mutated(case, original.decode())
            if changed == original:
                raise RuntimeError("mutation did not change source")
            results["cases"][case] = {"source": str(source.relative_to(root)), "original_sha256": sha(original), "mutant_sha256": sha(changed), "restored": False}
            persist()
            try:
                source.write_bytes(changed)
                build(case + "-mutant-build")
                if case == "sweep":
                    # This arm is overdetermined by abort_upload, and must
                    # remain green when only the backend sweep is removed.
                    witness("sweep-live-mutant", "live")
                witness(case + "-mutant", case, red=True)
            finally:
                source.write_bytes(original)
                results["cases"][case]["restored"] = source.read_bytes() == original
                persist()
                build(case + "-restored-build")
                witness(case + "-restored", case)
                if case == "sweep":
                    witness("sweep-live-restored", "live")
    except BaseException as error:
        results["status"] = "FAIL"
        results["error"] = repr(error)
        raise
    finally:
        for path, original in originals.items():
            if path.read_bytes() != original:
                path.write_bytes(original)
        results["final_source_sha256"] = {
            str(path.relative_to(root)): sha(path.read_bytes()) for path in originals
        }
        results["final_git_status"] = subprocess.check_output(["git", "status", "--porcelain"], cwd=root, text=True)
        persist()
    if results["final_git_status"]:
        results["status"] = "FAIL"
        persist()
        raise RuntimeError("mutation run left a dirty source tree")
    results["status"] = "PASS"
    persist()


if __name__ == "__main__":
    main()
