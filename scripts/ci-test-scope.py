#!/usr/bin/env python3
"""Choose the crates a pull-request CI run builds and tests (ADR-205).

usage: ci-test-scope.py <base> <head> --kkernel <path>
       ci-test-scope.py <base> <head> --paths-only

Prints one line on stdout: `full`, or the space-separated names of the crates under test. Everything
else goes to stderr: each changed path and how it was classified, the crates the changed paths belong
to, the dependents closure, and the reason for every `full`. Every failure prints `full`: the
classifier can only fail toward the full suite (ADR-205 D3).

The order is fixed (ADR-205 D4). The path rules run on the list of changed paths alone, before the
binary is touched or any manifest is read, so a change to a manifest, a lockfile, a workflow, a script
or anything else outside crate source goes to the full suite and the code pack never reads a tree in
which the pull request changed a manifest. Only then does the classifier run `code.ingest` with the
given `kkernel` binary (the one the newest successful push run on main built), register the map it
writes, resolve each changed crate to its project with `code.list(kind="project")`, and take the
incoming `depends_on` closure with `code.deps(direction="incoming", max_depth=1)`, one depth at a
time until the set stops growing, so the verb's depth bound never truncates a closure.

`--paths-only` stops after the path rules and prints `full` or `map`: `map` means the change is crate
source and Markdown only, so the caller should fetch the binary and run the full classification. The
workflow runs it first, so a pull request the path rules send to the full suite never downloads one.

A changed path resolves to the crate whose manifest directory is its longest prefix (ADR-205 D2). A
Markdown file resolves to no crate and is admitted under the documentation-lane rule of
scripts/ci-docs-only.sh: a Markdown file that a non-Markdown file names, that a symlink's target
names, or whose verb and pack counts the smoke tests check, puts the run on the full suite.

A crate can compile or read another crate's file by naming it: `#[path = "..."]` on a module, or
`include!`, `include_str!` or `include_bytes!` with a literal path. No dependency edge records that, so
the closure cannot reach the reader. Every such literal at the head is resolved against the directory
of the file that holds it, and a changed path that another crate names this way puts the run on the
full suite. The scan prints how many sites it read; finding none is a broken scan, not a clean one, and
also puts the run on the full suite. A path assembled at compile time (`concat!`, `env!`) is not read.
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CRATES = "crates/"
MAP_NAME = "workspace"
# code.deps and code.list maxima (ADR-139, numeric parameter bounds).
LIST_LIMIT = 500
DEPS_LIMIT = 1000
# One kkernel exec carries at most this many ops.
OPS_PER_CALL = 50
EXEC_TIMEOUT_SECONDS = 600

# Paths that change what the build or CI does (ADR-205 D3, the list scripts/ci-integration-only.sh
# carried before this script replaced it). Any of them puts the run on the full suite.
FULL_SUITE_NAMES = {
    "Cargo.toml": "manifest",
    "Cargo.lock": "lockfile",
    "rust-toolchain": "toolchain file",
    "rust-toolchain.toml": "toolchain file",
    "deny.toml": "lint or policy configuration",
    "rustfmt.toml": "lint or policy configuration",
    ".rustfmt.toml": "lint or policy configuration",
    "clippy.toml": "lint or policy configuration",
    ".clippy.toml": "lint or policy configuration",
}
FULL_SUITE_PREFIXES = {
    ".github/": "workflow or local action",
    "scripts/": "CI or lint script",
    "python/": "Python package",
    "apps/": "application",
    "tests/": "repository-level test",
    "benches/": "repository-level benchmark",
    "crates/.cargo/": "cargo configuration",
    "crates/.config/": "nextest configuration",
}


class Full(Exception):
    """The run goes to the full suite; the message is the reason."""


def log(message):
    print(message, file=sys.stderr, flush=True)


def git(*args, text=True):
    result = subprocess.run(["git", *args], cwd=REPO, capture_output=True, text=text, check=False)
    if result.returncode != 0:
        detail = result.stderr.strip() if text else result.stderr.decode(errors="replace").strip()
        raise Full(f"git {' '.join(args)} failed (rc {result.returncode}): {detail}")
    return result.stdout


def changed_paths(base, head):
    raw = git("diff", "--no-renames", "--name-only", "-z", base, head, "--")
    paths = [p for p in raw.split("\0") if p]
    if not paths:
        raise Full(f"no changed paths between {base} and {head}")
    return paths


def assert_no_submodule(head):
    for line in git("ls-tree", "-r", "--format=%(objectmode)", head).splitlines():
        if line == "160000":
            raise Full(f"{head} has a submodule")


def path_rule(path):
    """Return 'markdown' or 'crate'; raise Full for every path the rules do not admit."""
    name = path.rsplit("/", 1)[-1]
    if name in FULL_SUITE_NAMES:
        raise Full(f"{FULL_SUITE_NAMES[name]}: {path}")
    if path == "Makefile":
        raise Full(f"Makefile: {path}")
    for prefix, label in FULL_SUITE_PREFIXES.items():
        if path.startswith(prefix):
            raise Full(f"{label}: {path}")
    if path.endswith(".md"):
        return "markdown"
    if path.startswith(CRATES):
        return "crate"
    raise Full(f"outside crate source: {path}")


def symlink_targets(head):
    targets = []
    for line in git("ls-tree", "-r", "--format=%(objectmode) %(objectname)", head).splitlines():
        mode, _, oid = line.partition(" ")
        if mode == "120000":
            targets.append(git("cat-file", "blob", oid))
    return "\n".join(targets)


def check_markdown(path, head, targets):
    """Apply the documentation-lane rule of scripts/ci-docs-only.sh to one Markdown path."""
    name = path.rsplit("/", 1)[-1]
    grep = subprocess.run(
        ["git", "grep", "-q", "-F", "-e", name, head, "--", ":(exclude)*.md"],
        cwd=REPO, capture_output=True, check=False,
    )
    if grep.returncode == 0:
        raise Full(f"{path} is named by a non-Markdown file")
    if grep.returncode != 1:
        raise Full(f"git grep for {name} failed (rc {grep.returncode})")
    if name in targets:
        raise Full(f"{path} is named by a symlink's target")
    scanner = REPO / "tests" / "documented_verb_counts.py"
    scan = subprocess.run(
        [sys.executable, str(scanner), "--published-markdown", path],
        cwd=REPO, stdin=subprocess.DEVNULL, capture_output=True, check=False,
    )
    if scan.returncode == 0:
        raise Full(f"{path} carries verb or pack counts the smoke tests check against the live registry")
    if scan.returncode != 1:
        raise Full(f"count scanner failed for {path} (rc {scan.returncode})")
    log(f"documentation only: {path}")


INCLUDE_SITE = r"#\[path[[:space:]]*=|include(_str|_bytes)?!\("
INCLUDE_LITERAL = re.compile(r'#\[path\s*=\s*"([^"]+)"|include(?:_str|_bytes)?!\(\s*"([^"]+)"')


def foreign_includes(head, manifests):
    """{target path: [reader site]} for literal includes whose target lies under a different crate."""
    grep = subprocess.run(
        ["git", "grep", "-n", "-I", "-E", "-e", INCLUDE_SITE, head, "--", "crates/*.rs"],
        cwd=REPO, capture_output=True, text=True, check=False,
    )
    if grep.returncode not in (0, 1):
        raise Full(f"include scan: git grep failed (rc {grep.returncode}): {grep.stderr.strip()}")
    sites, files, foreign = 0, set(), {}
    for row in grep.stdout.splitlines():
        _rev, path, line, text = row.split(":", 3)
        for literal in (a or b for a, b in INCLUDE_LITERAL.findall(text)):
            sites += 1
            files.add(path)
            if literal.startswith("/") or "../" not in literal:
                continue
            target = os.path.normpath(os.path.join(os.path.dirname(path), literal))
            if target.startswith("../") or not target.startswith(CRATES):
                continue
            reader, owner = owner_of(path, head, manifests), owner_of(target, head, manifests)
            if owner is not None and owner != reader:
                foreign.setdefault(target, []).append(f"{reader} ({path}:{line})")
    if sites == 0:
        raise Full(f"include scan: BROKEN, no #[path] or include! site found under crates/ at {head}")
    log(f"include scan: {sites} sites in {len(files)} files, {len(foreign)} files named from another crate")
    for target in sorted(foreign):
        log(f"named from another crate: {target} <- {', '.join(foreign[target])}")
    return foreign


def owner_of(path, head, manifests):
    """crate_of, or None for a path under no crate's manifest directory."""
    try:
        return crate_of(path, head, manifests)
    except Full as reason:
        if str(reason).startswith("under no crate's manifest directory"):
            return None
        raise


def crate_of(path, head, manifests):
    """The crate whose manifest directory is the longest prefix of path, read at head."""
    parts = path.split("/")[:-1]
    while len(parts) >= 2:
        directory = "/".join(parts)
        if directory not in manifests:
            listing = git("ls-tree", "--name-only", head, f"{directory}/")
            manifests[directory] = None
            if f"{directory}/Cargo.toml" in listing.splitlines():
                text = git("show", f"{head}:{directory}/Cargo.toml")
                try:
                    package = tomllib.loads(text).get("package")
                except tomllib.TOMLDecodeError as error:
                    raise Full(f"{directory}/Cargo.toml does not parse: {error}") from error
                if isinstance(package, dict) and isinstance(package.get("name"), str):
                    manifests[directory] = package["name"]
        if manifests[directory] is not None:
            return manifests[directory]
        parts.pop()
    raise Full(f"under no crate's manifest directory: {path}")


class Kkernel:
    def __init__(self, binary, workdir):
        self.binary = binary
        self.home = workdir / "home"
        self.home.mkdir()
        self.config = workdir / "config.toml"
        self.config.write_text("")

    def exec(self, ops):
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home),
            "KHIVE_CONFIG": str(self.config),
            "KHIVE_NO_DAEMON": "1",
            "KHIVE_DB": str(self.home / "khive.db"),
            # Only the code pack and the kg pack it requires: no embedder or model fetch on the runner.
            "KHIVE_PACKS": "kg,code",
        }
        try:
            result = subprocess.run(
                [self.binary, "exec", "--strict", ops], env=env, capture_output=True, text=True,
                timeout=EXEC_TIMEOUT_SECONDS, check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise Full(f"kkernel exec failed to run: {error}") from error
        if result.returncode != 0:
            raise Full(
                f"kkernel exec exited {result.returncode}: {result.stdout.strip()[-2000:]} "
                f"{result.stderr.strip()[-1000:]}"
            )
        try:
            payload = json.loads(result.stdout)
            outcomes = payload["results"]
        except (json.JSONDecodeError, KeyError, TypeError) as error:
            raise Full(f"kkernel exec printed no result payload: {error}") from error
        values = []
        for outcome in outcomes:
            if not isinstance(outcome, dict) or outcome.get("ok") is not True:
                raise Full(f"kkernel op failed: {json.dumps(outcome)[:2000]}")
            values.append(outcome.get("result"))
        return values


def quote(value):
    return json.dumps(value)


def ingest(kernel, workdir):
    maps = workdir / "maps"
    maps.mkdir()
    db = maps / "workspace.db"
    # An explicit db must name an existing regular file; an empty one initializes.
    db.touch()
    source = REPO / "crates"
    (report,) = kernel.exec(
        f'code.ingest(path={quote(str(source))}, db={quote(str(db))}, '
        f'languages=["rust"], tiers=["l1"])'
    )
    if not isinstance(report, dict):
        raise Full("code.ingest returned no report")
    if report.get("warnings"):
        raise Full(f"code.ingest reported warnings: {report['warnings']}")
    if report.get("blocked_count") or report.get("blocked"):
        raise Full(f"code.ingest reported blocked writes: {report.get('blocked')}")
    for counter in ("manifest_files_refused", "source_files_refused"):
        if report.get(counter):
            raise Full(f"code.ingest reported {counter}={report[counter]}")
    if not db.is_file():
        raise Full("code.ingest wrote no map database")
    log(f"ingest: {source} -> {report.get('projects_created', 0)} projects created, "
        f"{report.get('edges_created', 0)} edges created")
    kernel.config.write_text(
        f"code_map_root = {quote(str(maps.resolve()))}\n\n"
        f"[[code_maps]]\nname = {quote(MAP_NAME)}\npath = {quote(str(db.resolve()))}\n"
        f"source_root = {quote(str(source.resolve()))}\n"
    )


def projects(kernel):
    """Map crate name to project id, from every page of code.list(kind="project")."""
    by_name = {}
    cursor = None
    while True:
        args = f'map={quote(MAP_NAME)}, kind="project", limit={LIST_LIMIT}'
        if cursor is not None:
            args += f", cursor={quote(cursor)}"
        (page,) = kernel.exec(f"code.list({args})")
        if not isinstance(page, dict) or not isinstance(page.get("items"), list):
            raise Full("code.list returned no items")
        for item in page["items"]:
            if item.get("kind") != "project":
                raise Full(f"code.list(kind=project) returned a {item.get('kind')!r} row")
            name, ident = item.get("name"), item.get("id")
            if not isinstance(name, str) or not isinstance(ident, str):
                raise Full(f"code.list returned a project without a name or id: {item}")
            if name in by_name:
                raise Full(f"two projects in the map are named {name}")
            by_name[name] = ident
        cursor = page.get("next_cursor")
        if cursor is None:
            break
    if not by_name:
        raise Full("the map holds no projects")
    return by_name


def closure(kernel, changed, by_name):
    """Every crate that depends on a changed crate, transitively, plus the changed crates."""
    name_of = {ident: name for name, ident in by_name.items()}
    seen = set()
    frontier = []
    for crate in sorted(changed):
        if crate not in by_name:
            raise Full(f"changed crate {crate} is not a project in the map")
        seen.add(by_name[crate])
        frontier.append(by_name[crate])
    while frontier:
        found = []
        for start in range(0, len(frontier), OPS_PER_CALL):
            chunk = frontier[start:start + OPS_PER_CALL]
            ops = ", ".join(
                f'code.deps(map={quote(MAP_NAME)}, id={quote(ident)}, direction="incoming", '
                f"max_depth=1, limit={DEPS_LIMIT})"
                for ident in chunk
            )
            for ident, graph in zip(chunk, kernel.exec(f"[{ops}]")):
                if not isinstance(graph, dict) or graph.get("truncated") is not False:
                    raise Full(f"code.deps from {name_of.get(ident, ident)} was truncated or malformed")
                kinds = {node.get("id"): node.get("kind") for node in graph.get("nodes", [])}
                for edge in graph.get("edges", []):
                    if edge.get("target") != ident:
                        continue
                    source = edge.get("source")
                    if kinds.get(source) != "project":
                        continue
                    if source not in name_of:
                        raise Full(f"code.deps returned a project that code.list did not: {source}")
                    if source not in seen:
                        seen.add(source)
                        found.append(source)
        frontier = found
    return sorted(name_of[ident] for ident in seen)


def classify(base, head, binary, paths_only=False):
    paths = changed_paths(base, head)
    assert_no_submodule(head)
    markdown, crate_paths = [], []
    for path in paths:
        (markdown if path_rule(path) == "markdown" else crate_paths).append(path)
    if markdown:
        targets = symlink_targets(head)
        for path in markdown:
            check_markdown(path, head, targets)
    if not crate_paths:
        raise Full("no crate source changed")
    manifests = {}
    changed = set()
    for path in crate_paths:
        crate = crate_of(path, head, manifests)
        log(f"changed: {path} -> {crate}")
        changed.add(crate)
    log(f"changed crates: {' '.join(sorted(changed))}")
    foreign = foreign_includes(head, manifests)
    for path in crate_paths:
        if path in foreign:
            raise Full(f"{path} is compiled or read by another crate: {', '.join(foreign[path])}")
    if paths_only:
        return None

    if not binary or not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        raise Full(f"no executable kkernel binary at {binary!r}")
    with tempfile.TemporaryDirectory(prefix="ci-test-scope-") as tmp:
        # The map's VFS refuses a path with a symlinked parent; macOS /tmp and /var are symlinks.
        workdir = Path(tmp).resolve()
        kernel = Kkernel(binary, workdir)
        ingest(kernel, workdir)
        by_name = projects(kernel)
        scope = closure(kernel, changed, by_name)
    if not scope:
        raise Full("the closure is empty while crate source changed")
    log(f"closure ({len(scope)} of {len(by_name)} crates): {' '.join(scope)}")
    return scope


USAGE = "usage: ci-test-scope.py <base> <head> (--kkernel <path> | --paths-only)"


def main(argv):
    if len(argv) == 4 and argv[3] == "--paths-only":
        paths_only, binary = True, None
    elif len(argv) == 5 and argv[3] == "--kkernel":
        paths_only, binary = False, argv[4]
    else:
        print(USAGE, file=sys.stderr)
        return 2
    base, head = argv[1], argv[2]
    try:
        scope = classify(base, head, binary, paths_only)
    except Full as reason:
        log(f"full suite: {reason}")
        print("full")
        return 0
    except Exception as error:  # Any unforeseen failure must still choose the full suite.
        log(f"full suite: classifier error: {error!r}")
        print("full")
        return 0
    print("map" if paths_only else " ".join(scope))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
