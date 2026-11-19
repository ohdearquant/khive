#!/usr/bin/env python3
"""Observe llvm-profdata, and keep a merge alive past profiles it cannot read.

A profile file left truncated by a process that died mid-write makes
`llvm-profdata merge` refuse the whole set ("no profile can be merged"), which
turns one lost process into no coverage measurement at all. The merge forwarder
checks every input with `llvm-profdata show` first, excludes the unreadable
ones, and says so: each dropped file is a workflow warning with the tool's own
reason, and more than `COVERAGE_PROFILE_DROP_FLOOR` dropped files (default 1)
fails the merge before it starts, so a real loss of coverage data cannot pass
as a measurement.
"""

import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import time


def warning(message):
    try:
        print(f"coverage profile diagnostics: {message}", file=sys.stderr, flush=True)
    except OSError:
        pass


def persist(output, name, record):
    try:
        temporary = output / (name + ".tmp")
        with temporary.open("x") as stream:
            json.dump(record, stream, sort_keys=True)
            stream.write("\n")
        temporary.replace(output / name)
        return True
    except OSError as error:
        warning(f"cannot persist {name}: {error}")
        return False


def annotate(level, title, message):
    # The runner reads workflow commands from stderr as well as stdout.
    try:
        print(f"::{level} title={title}::{message}", file=sys.stderr, flush=True)
    except OSError:
        pass


def drop_floor():
    raw = os.environ.get("COVERAGE_PROFILE_DROP_FLOOR", "1")
    try:
        floor = int(raw)
    except ValueError:
        floor = -1
    if floor < 0:
        warning(f"COVERAGE_PROFILE_DROP_FLOOR {raw!r} is not a non-negative integer; using 1")
        floor = 1
    return floor


def exclude_unreadable(real, output, arguments):
    """Rewrite the merge input list without profiles `show` rejects.

    Returns the arguments to run, or None when more profiles were dropped than
    the floor allows (the caller then fails the merge without running it). A
    list that cannot be read or checked is forwarded unchanged: the merge
    itself then reports whatever is wrong, as before.
    """
    if "-f" not in arguments[1:]:
        return arguments
    position = arguments.index("-f", 1) + 1
    if position >= len(arguments):
        return arguments
    list_path = Path(arguments[position])
    try:
        entries = [line for line in list_path.read_text().splitlines() if line.strip()]
    except OSError as error:
        warning(f"cannot read merge input list {list_path}: {error}")
        return arguments
    deadline = time.monotonic() + 240
    kept, dropped, unchecked = [], [], []
    for entry in entries:
        if time.monotonic() >= deadline:
            unchecked.append(entry)
            kept.append(entry)
            continue
        try:
            result = subprocess.run(
                [real, "show", entry],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                timeout=min(10, max(1, deadline - time.monotonic())),
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            unchecked.append(entry)
            kept.append(entry)
            warning(f"could not check {entry}: {error}; keeping it")
            continue
        if result.returncode == 0:
            kept.append(entry)
            continue
        if result.returncode < 0:
            # A signal says nothing about the file; keep it for the real merge.
            unchecked.append(entry)
            kept.append(entry)
            continue
        reason = result.stderr.decode("utf-8", errors="replace").strip().splitlines()
        dropped.append({"path": entry, "show_exit": result.returncode,
                        "reason": reason[-1] if reason else ""})
    floor = drop_floor()
    persist(output, "dropped.json", {
        "total": len(entries), "kept": len(kept), "dropped": dropped,
        "unchecked": unchecked, "floor": floor,
    })
    for item in dropped:
        annotate("warning", "Unreadable coverage profile excluded",
                 f"{Path(item['path']).name}: {item['reason'] or 'llvm-profdata show failed'}"
                 f" (exit {item['show_exit']})")
    if unchecked:
        warning(f"{len(unchecked)} of {len(entries)} profiles were not checked and are forwarded as they are")
    if len(dropped) > floor:
        annotate("error", "Too many unreadable coverage profiles",
                 f"{len(dropped)} of {len(entries)} profile files are unreadable; the floor is {floor},"
                 " so no coverage measurement is produced")
        return None
    if dropped:
        warning(f"excluded {len(dropped)} of {len(entries)} profiles from the merge (floor {floor})")
    if not kept:
        annotate("error", "No readable coverage profile",
                 f"all {len(entries)} profile files are unreadable; no coverage measurement is produced")
        return None
    filtered = output / "merge-inputs.filtered"
    try:
        filtered.write_text("".join(f"{path}\n" for path in kept))
    except OSError as error:
        warning(f"cannot write the filtered input list: {error}; forwarding the original list")
        return arguments
    rewritten = list(arguments)
    rewritten[position] = str(filtered)
    return rewritten


def forward(real, output):
    arguments = [real, *sys.argv[1:]]
    if sys.argv[1:2] != ["merge"]:
        os.execvpe(real, arguments, os.environ)
    arguments = exclude_unreadable(real, output, arguments)
    if arguments is None:
        persist(output, "merge-end.json", {"state": "refused", "exit": 1})
        return 1
    started = persist(output, "merge-start.json", {"state": "observed"})
    capture = None
    errors = []
    captured_bytes = 0
    try:
        capture = (output / "merge-stderr.txt").open("xb")
    except OSError as error:
        errors.append(f"open: {error}")
        warning(f"cannot capture merge stderr: {error}")
    child = subprocess.Popen(arguments, stderr=subprocess.PIPE)

    def forward_signal(received, frame):
        try:
            child.send_signal(received)
        except ProcessLookupError:
            pass

    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, forward_signal)
    while chunk := os.read(child.stderr.fileno(), 65536):
        try:
            sys.stderr.buffer.write(chunk)
            sys.stderr.buffer.flush()
        except OSError:
            pass
        if capture is not None:
            try:
                capture.write(chunk)
                capture.flush()
                captured_bytes += len(chunk)
            except OSError as error:
                errors.append(f"write/flush: {error}")
                warning(f"merge stderr capture incomplete: {error}")
                try:
                    capture.close()
                except OSError as close_error:
                    errors.append(f"close: {close_error}")
                capture = None
    child.stderr.close()
    if capture is not None:
        try:
            capture.close()
        except OSError as error:
            errors.append(f"close: {error}")
            warning(f"merge stderr capture incomplete: {error}")
    code = child.wait()
    persist(output, "merge-end.json", {
        "state": "finished", "exit": code, "capture_complete": started and not errors,
        "captured_bytes": captured_bytes, "errors": errors,
    })
    if code < 0:
        number = -code
        if number not in (signal.SIGKILL, signal.SIGSTOP):
            signal.signal(number, signal.SIG_DFL)
        os.kill(os.getpid(), number)
    return code


def process_stat(pid):
    # comm may itself contain spaces and parentheses. Fields after its final
    # closing parenthesis start with state (3), ppid (4), and starttime (22).
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    return int(fields[1]), int(fields[19])


def observe(output):
    limit = 16 * 1024 * 1024
    deadline = time.monotonic() + 54 * 60
    uid = os.geteuid()
    summary = {"state": "starting", "attribution_complete": False, "scans": 0,
               "identities": 0, "read_races": 0, "read_errors": 0, "bytes": 0,
               "observed_uid": uid, "other_user_processes": 0}
    seen = set()
    try:
        target = Path(os.environ["COVERAGE_PROFILE_TARGET"]).absolute()
        if target.resolve() != target:
            raise ValueError("coverage target contains a symlink")
        ticks = os.sysconf("SC_CLK_TCK")
        boot = next(int(line.split()[1]) for line in Path("/proc/stat").read_text().splitlines()
                    if line.startswith("btime "))
        with (output / "observer.jsonl").open("x") as journal:
            def emit(record):
                encoded = json.dumps(record, sort_keys=True) + "\n"
                size = len(encoded.encode())
                if summary["bytes"] + size > limit:
                    return False
                journal.write(encoded)
                journal.flush()
                summary["bytes"] += size
                return True

            emit({"type": "observer_start", "target": str(target), "boot_epoch_seconds": boot,
                  "clock_ticks_per_second": ticks, "interval_seconds": 1,
                  "lifetime_seconds": 54 * 60, "identity_limit": 20000, "byte_limit": limit,
                  "observed_uid": uid, "attribution_complete": False})
            summary["state"] = "running"
            while True:
                if (output / "observer.stop").exists():
                    summary["state"] = "stop_requested"
                    break
                if time.monotonic() >= deadline:
                    summary["state"] = "time_limit"
                    break
                summary["scans"] += 1
                with os.scandir("/proc") as entries:
                    for entry in entries:
                        if not entry.name.isdecimal():
                            continue
                        if time.monotonic() >= deadline or (output / "observer.stop").exists():
                            break
                        pid = int(entry.name)
                        try:
                            # The kernel refuses another user's exe link, and the coverage
                            # processes run as this user, so the observed population is this
                            # user's processes; the rest are counted, never read.
                            if entry.stat().st_uid != uid:
                                summary["other_user_processes"] += 1
                                continue
                            before = process_stat(pid)
                            executable = os.readlink(f"/proc/{pid}/exe")
                            after = process_stat(pid)
                            if before[1] != after[1]:
                                summary["read_races"] += 1
                                continue
                            path = executable.removesuffix(" (deleted)")
                            if not Path(path).is_relative_to(target):
                                continue
                            identity = (pid, after[1], executable)
                            if identity in seen:
                                continue
                            if len(seen) >= 20000:
                                summary["state"] = "identity_limit"
                                break
                            if not emit({"type": "observed_executable", "pid": pid,
                                         "ppid": after[0], "start_ticks": after[1],
                                         "first_seen_unix_ns": time.time_ns(), "exe": executable,
                                         "start_unix_ns_estimate": boot * 10**9 + after[1] * 10**9 // ticks}):
                                summary["state"] = "byte_limit"
                                break
                            seen.add(identity)
                            summary["identities"] = len(seen)
                        except (FileNotFoundError, ProcessLookupError):
                            summary["read_races"] += 1
                        except (OSError, ValueError, IndexError):
                            summary["read_errors"] += 1
                if summary["state"] != "running":
                    break
                time.sleep(min(1, max(0, deadline - time.monotonic())))
    except (OSError, ValueError, IndexError, StopIteration) as error:
        summary["state"] = "failed"
        summary["reason"] = str(error)
    persist(output, "observer-end.json", summary)
    return 0 if summary["state"] == "stop_requested" else 1


def read_state(output, name):
    try:
        return json.loads((output / name).read_text())
    except (OSError, ValueError) as error:
        return {"state": "unavailable", "reason": str(error)}


def fold_observer(output, emit):
    try:
        (output / "observer.stop").touch(exist_ok=True)
    except OSError as error:
        emit({"type": "observer_stop_request", "state": "failed", "reason": str(error)})
    until = time.monotonic() + 5
    while not (output / "observer-end.json").exists() and time.monotonic() < until:
        time.sleep(0.1)
    emit({"type": "observer_request", "requested": (output / "observer.requested").exists(),
          "stop_requested": (output / "observer.stop").exists()})
    terminal = read_state(output, "observer-end.json")
    by_pid = {}
    journal_complete = False
    try:
        with (output / "observer.jsonl").open("rb") as journal:
            snapshot = journal.read(16 * 1024 * 1024 + 1)
        if len(snapshot) > 16 * 1024 * 1024:
            raise ValueError("observer journal exceeds its byte bound")
        records = snapshot.splitlines(keepends=True)
        for line in records:
            if not line.endswith(b"\n"):
                raise ValueError("observer journal ends with a partial record")
            record = json.loads(line)
            emit(record)
            if record.get("type") == "observed_executable":
                by_pid.setdefault(record["pid"], []).append(record)
        journal_complete = bool(records) and terminal.get("bytes") == len(snapshot)
    except (OSError, ValueError, KeyError) as error:
        emit({"type": "observer_journal", "state": "incomplete", "reason": str(error)})
    emit({"type": "observer_end", **terminal, "journal_complete": journal_complete,
          "attribution_complete": False})
    finished = (terminal.get("state") == "stop_requested" and journal_complete
                and terminal.get("read_errors") == 0)
    return by_pid, finished


def remaining(deadline):
    seconds = deadline - time.monotonic()
    if seconds <= 0:
        raise TimeoutError("diagnostic time budget exhausted")
    return seconds


def digest(stream, deadline, destination=None):
    result = hashlib.sha256()
    while True:
        remaining(deadline)
        chunk = stream.read(1024 * 1024)
        if not chunk:
            return result.hexdigest()
        result.update(chunk)
        if destination is not None:
            destination.write(chunk)


def inspect(name, directory, real, corrupt, deadline):
    record = {"name": name, "size": None, "mtime_ns": None, "sha256": None}
    match = re.fullmatch(r"crates-(\d+)-(\d+)_(\d+)\.profraw", name)
    if match:
        record.update(pid=int(match[1]), signature=match[2], pool=match[3])
    else:
        record["filename_identity"] = "unrecognized"
    temporary = None
    try:
        information = os.stat(name, dir_fd=directory, follow_symlinks=False)
        if not stat.S_ISREG(information.st_mode):
            raise ValueError("not a regular file; symlinks are not followed")
        descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory)
        with os.fdopen(descriptor, "rb") as source:
            opened = os.fstat(source.fileno())
            if (opened.st_dev, opened.st_ino) != (information.st_dev, information.st_ino):
                raise ValueError("profile changed while opening")
            record["size"] = opened.st_size
            record["mtime_ns"] = opened.st_mtime_ns
            record["sha256"] = digest(source, deadline)
            # Passing the held descriptor prevents replacement of the pathname
            # from redirecting the diagnostic tool outside the coverage target.
            result = subprocess.run(
                [real, "show", f"/proc/self/fd/{source.fileno()}"],
                pass_fds=(source.fileno(),),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                timeout=min(10, remaining(deadline)),
                check=False,
            )
            record["show_exit"] = result.returncode
            record["show_stderr"] = result.stderr[:8192].decode("utf-8", errors="replace")
            record["show_stderr_truncated"] = len(result.stderr) > 8192
            if result.returncode < 0:
                raise ValueError("show terminated by signal; corruption is not established")
            after = os.fstat(source.fileno())
            if (opened.st_size, opened.st_mtime_ns, opened.st_ctime_ns) != (
                after.st_size, after.st_mtime_ns, after.st_ctime_ns
            ):
                raise ValueError("profile changed during inspection")
            if result.returncode == 0:
                record["status"] = "readable"
                return record
            source.seek(0)
            first_bytes = source.read(64).hex()
            source.seek(0)
            temporary = corrupt / (name + ".partial")
            with temporary.open("xb") as destination:
                copied_hash = digest(source, deadline, destination)
            copied = os.fstat(source.fileno())
            if copied_hash != record["sha256"] or (
                copied.st_size, copied.st_mtime_ns, copied.st_ctime_ns
            ) != (opened.st_size, opened.st_mtime_ns, opened.st_ctime_ns):
                raise ValueError("profile changed while copying")
            record["first_64_bytes_hex"] = first_bytes
            temporary.rename(corrupt / name)
            temporary = None
            record["status"] = "show_rejected"
    except (OSError, ValueError, TimeoutError, subprocess.TimeoutExpired) as error:
        record["status"] = "incomplete"
        record["reason"] = str(error)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return record


def collect(real, output):
    deadline = time.monotonic() + 240
    target = Path(os.environ["COVERAGE_PROFILE_TARGET"])
    corrupt = output / "corrupt"
    corrupt.mkdir()
    complete = True
    counts = {"readable": 0, "show_rejected": 0, "incomplete": 0}
    with (output / "tool-versions.txt").open("x") as versions:
        versions.write(f"llvm-profdata executable: {real}\nprofile target: {target}\n")
        for arguments in ([real, "--version"], ["rustc", "-vV"], ["cargo", "llvm-cov", "--version"]):
            versions.write(f"\nargv: {json.dumps(arguments)}\n")
            try:
                result = subprocess.run(arguments, capture_output=True, timeout=10, check=False)
                versions.write(f"exit: {result.returncode}\n")
                versions.write(result.stdout.decode("utf-8", errors="replace"))
                versions.write(result.stderr.decode("utf-8", errors="replace"))
                complete &= result.returncode == 0
            except (OSError, subprocess.TimeoutExpired) as error:
                versions.write(f"incomplete: {error}\n")
                complete = False
            versions.flush()
    with (output / "profiles.jsonl").open("x") as listing:
        def emit(record):
            listing.write(json.dumps(record, sort_keys=True) + "\n")
            listing.flush()

        emit({"type": "begin", "complete": False, "target": str(target)})
        merge_start = read_state(output, "merge-start.json")
        merge_end = read_state(output, "merge-end.json")
        capture_complete = merge_start.get("state") == "observed" and (
            merge_end.get("state") == "finished" and merge_end.get("capture_complete") is True
        )
        try:
            capture_complete &= (output / "merge-stderr.txt").stat().st_size == merge_end.get("captured_bytes")
        except OSError:
            capture_complete = False
        emit({"type": "merge_capture", "observation": merge_start, "terminal": merge_end,
              "complete": capture_complete,
              "missing_start_means": "merge not observed, not proof it never ran"})
        complete &= capture_complete
        by_pid, observer_finished = fold_observer(output, emit)
        complete &= observer_finished
        records = []
        inventory_complete = True
        directory = None
        try:
            if target.resolve(strict=True) != target.absolute():
                raise ValueError("coverage target contains a symlink; refusing traversal")
            expected = target.stat()
            directory = os.open(target, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            opened = os.fstat(directory)
            if (opened.st_dev, opened.st_ino) != (expected.st_dev, expected.st_ino):
                raise ValueError("coverage target changed while opening")
            # cargo-llvm-cov 0.8.7 merges this directory's direct *.profraw files.
            with os.scandir(directory) as entries:
                for entry in entries:
                    remaining(deadline)
                    if not entry.name.endswith(".profraw"):
                        continue
                    record = inspect(entry.name, directory, real, corrupt, deadline)
                    matches = by_pid.get(record.get("pid"), [])
                    record["process_correlation"] = "unknown" if not matches else (
                        "one_observed_identity" if len(matches) == 1 else "ambiguous"
                    )
                    record["observed_identities"] = matches
                    record["writer_attribution_proven"] = False
                    emit({"type": "profile", **record})
                    records.append(record)
                    counts[record["status"]] += 1
                    inventory_complete &= record["status"] != "incomplete"
        except (OSError, ValueError, TimeoutError) as error:
            inventory_complete = False
            emit({"type": "incomplete", "reason": str(error)})
        finally:
            if directory is not None:
                os.close(directory)
        if not records:
            inventory_complete = False
            emit({"type": "incomplete", "reason": "no_profiles"})
        # One normalized group avoids repeating a large peer list for each
        # corrupt member. For a rejected file, its peers are all other members.
        signatures = {r.get("signature") for r in records if r["status"] == "show_rejected"}
        groups = {}
        for record in records:
            signature = record.get("signature")
            if signature is not None and signature in signatures:
                groups.setdefault(signature, []).append({
                    key: record.get(key) for key in ("name", "size", "mtime_ns", "status")
                })
        for signature, members in sorted(groups.items()):
            emit({"type": "same_signature_group", "signature": signature,
                  "inventory_complete": inventory_complete,
                  "relation": "for each rejected member, peers are all other members; correlation only",
                  "members": members})
        complete &= inventory_complete
        emit({"type": "summary", "complete": False, "bounded_collection_finished": complete, "counts": counts,
              "profile_inventory_complete": inventory_complete,
              "merge_capture_complete": capture_complete, "observer_finished": observer_finished,
              "process_attribution_complete": False})
    print(f"Coverage profile diagnostics: {json.dumps(counts)}, bounded_collection_finished={complete}; process attribution remains incomplete", flush=True)
    return 0 if complete else 1


def main():
    output = Path(os.environ["COVERAGE_PROFILE_DIAGNOSTICS"])
    if os.environ.get("COVERAGE_PROFILE_DIAGNOSTICS_MODE") == "observe":
        return observe(output)
    real = os.environ["COVERAGE_REAL_PROFDATA"]
    resolved = shutil.which(real)
    if resolved is None or Path(resolved).resolve() == Path(__file__).resolve():
        raise ValueError("real llvm-profdata is missing or points back to the wrapper")
    if os.environ.get("COVERAGE_PROFILE_DIAGNOSTICS_MODE") == "collect":
        return collect(real, output)
    return forward(real, output)


if __name__ == "__main__":
    sys.exit(main())
