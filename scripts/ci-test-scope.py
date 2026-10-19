#!/usr/bin/env python3
"""Choose the crates a pull-request CI run builds and tests (ADR-205).

usage: ci-test-scope.py <base> <head> --kkernel <path>
       ci-test-scope.py <base> <head> --paths-only

Prints one line on stdout: `full`, or the space-separated names of the crates under test. Everything
else goes to stderr: each changed path and how it was classified, the crates the changed paths belong
to, the dependents closure, and the reason for every `full`. Every failure prints `full`: the
classifier can only fail toward the full suite (ADR-205 D3).

The order is fixed (ADR-205 D4). The path rules run on the list of changed paths alone, before the
binary is touched, so a change to the workspace manifest, a workflow, a script or anything else outside
crate source goes to the full suite. Two path classes are read as data before the binary runs
(ADR-205 Amendment 1, D2 and D3):

- A crate's own `Cargo.toml` is admitted only when every difference between its base and head copies is
  inside a dependency table (plain or under `[target.*]`) or a target table; no dependency entry gains,
  loses or changes `features`, `default-features` or `optional` (a `workspace = true` entry is read
  together with the workspace manifest's entry it inherits); and every target table that differs keeps
  its `required-features` and has a `path` that, with `..` segments and symbolic links followed in the
  head tree, lies inside the crate. A dependency entry that appears or disappears goes to the full suite
  (ADR-205 Amendment 2, D1): an entry is its table, its target, the package it names and where that
  package comes from (`path`, `git` with `branch`, `tag` or `rev`, `registry`, or `workspace = true`), so
  an entry moved to another table, target, path or source counts as one removed and one added. For an
  edit to an entry that stays, the crate counts as changed, and so does every workspace member whose
  resolved dependencies in the base or head lockfile reach the package it names. The binary then ingests
  the base commit's tree, never the head's.
- `crates/Cargo.lock` is parsed on both sides with each package entry keyed by name, version and
  source. An entry on one side only, or one that differs, is changed. A changed entry for a package that
  is not a workspace member goes to the full suite, a version bump included (ADR-205 Amendment 2, D2):
  the lockfile does not record the features a package requests of its own dependencies, so the reach of
  such a change is not readable from it. Every workspace member whose own entry changed, or whose
  resolved dependencies in either lockfile reach a changed member entry, counts as changed. A changed
  `version` header, any other changed top-level key, or a change reaching every workspace member goes to
  the full suite.

Any other difference, and every failure to read either side, goes to the full suite. So does a symbolic
link anywhere under crates/ in the base or head tree, changed or not (ADR-205 Amendment 2, D3): a changed
path resolves to a crate by its own location while the compiler reads the file a link names. Only then does
the classifier run `code.ingest` with the
given `kkernel` binary (the one the newest successful push run on main built), register the map it
writes, resolve each changed crate to its project with `code.list(kind="project")`, and take the
incoming `depends_on` closure with `code.deps(direction="incoming", max_depth=1)`, one depth at a
time until the set stops growing, so the verb's depth bound never truncates a closure.

`--paths-only` stops after the path rules and the manifest and lockfile reads and prints `full` or
`map`: `map` means the change is crate source, admitted manifest and lockfile changes and Markdown
only, so the caller should fetch the binary and run the full classification. The
workflow runs it first, so a pull request the path rules send to the full suite never downloads one.

A changed path resolves to the crate whose manifest directory is its longest prefix (ADR-205 D2). A
Markdown file resolves to no crate and is admitted under the documentation-lane rule of
scripts/ci-docs-only.sh: a Markdown file that a non-Markdown file names, that a symlink's target
names, or whose verb and pack counts the smoke tests check, puts the run on the full suite.

A crate can compile or read another crate's file by naming it: `#[path = "..."]` on a module (also
inside `cfg_attr`, at any nesting depth), or `include!`, `include_str!` or `include_bytes!`. No dependency edge records that,
so the closure cannot reach the reader. The scan reads every Rust file under crates/ that names one of
them, skips comments and string and character literals, and resolves each argument in code: a plain or
raw string literal on any line after the opener, `concat!` of literals, and `concat!` led by
`env!("CARGO_MANIFEST_DIR")`, which resolves against the reader's manifest directory. A literal followed
by a part computed at compile time (a macro argument) names every path that starts with it. Other paths
resolve against the directory of the file that holds them, except a `#[path]` inside an inline `mod name { }`
block: rustc resolves that one below the inline modules' directories, starting at the file's directory for a
mod-rs file (a crate root or `mod.rs`) and at a directory named after the file otherwise. The scan does not
decide which kind of file it read, so it names both, and an inline module that carries a `#[path]` of its own
contributes both its name and that path as the directory. A changed path that a reader in another crate
names this way puts the run on the full suite. An argument in code the scan cannot resolve also does,
and so does finding no site at all: a broken scan is not a clean one. The scan prints how many sites it
read and how many it could not.
"""

import json
import os
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
# The node kinds a code.deps reply may carry (ADR-139, code.deps response fields).
NODE_KINDS = ("project", "module", "function", "datatype", "interface")
# One kkernel exec carries at most this many ops.
OPS_PER_CALL = 50
EXEC_TIMEOUT_SECONDS = 600

# Paths that change what the build or CI does (ADR-205 D3, the list scripts/ci-integration-only.sh
# carried before this script replaced it). Any of them puts the run on the full suite, except a crate's own
# Cargo.toml and crates/Cargo.lock, which are read as data (ADR-205 Amendment 1, D2 and D3).
WORKSPACE_MANIFEST = "crates/Cargo.toml"
LOCKFILE = "crates/Cargo.lock"
DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")
TARGET_TABLES = ("lib", "bin", "test", "bench", "example")
# Cargo also reads the underscore spellings, so each counts as a flag of its own.
DEPENDENCY_FLAGS = ("features", "default-features", "default_features", "optional")
REQUIRED_FEATURES = ("required-features", "required_features")
# Where a dependency entry's package comes from: with the package it names, an entry's identity (Amendment 2, D1).
DEPENDENCY_SOURCES = ("path", "git", "branch", "tag", "rev", "registry", "registry-index", "workspace")
SYMLINK_HOPS = 40
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
    """Return 'markdown', 'crate', 'crate-manifest' or 'lockfile'; raise Full for every path the rules do not admit."""
    name = path.rsplit("/", 1)[-1]
    if path == LOCKFILE:
        return "lockfile"
    if (name == "Cargo.toml" and path.startswith(CRATES) and path != WORKSPACE_MANIFEST
            and not any(path.startswith(prefix) for prefix in FULL_SUITE_PREFIXES)):
        return "crate-manifest"
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


# A coarse per-line prefilter: any file with a line naming an include macro or a path attribute is read whole, and
# the lexer below decides which of those names sit in code. It cannot see a `#[` split from `path` across lines.
INCLUDE_SITE = r"include(_str|_bytes)?[[:space:]]*!|#[[:space:]]*!?[[:space:]]*\[[[:space:]]*(path|cfg_attr)"
INCLUDE_MACROS = ("include", "include_str", "include_bytes")
MANIFEST_DIR = "CARGO_MANIFEST_DIR"


class Unreadable(Exception):
    """An include argument the scan cannot resolve to a path or a path prefix."""


def skip_trivia(text, i):
    """Index of the first character at or after i that is not whitespace or a comment."""
    while i < len(text):
        if text[i].isspace():
            i += 1
        elif text.startswith("//", i):
            end = text.find("\n", i)
            i = len(text) if end < 0 else end + 1
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < len(text) and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        else:
            break
    return i


def word_at(text, i):
    j = i
    while j < len(text) and (text[j].isalnum() or text[j] == "_"):
        j += 1
    return text[i:j], j


def string_at(text, i):
    """(value, end) for a string literal starting at i, or None. value is None for a byte, C or escaped string."""
    word, j = word_at(text, i)
    if word in ("r", "br", "cr"):
        hashes = len(text[j:]) - len(text[j:].lstrip("#"))
        if text.startswith('"', j + hashes):
            close = '"' + "#" * hashes
            end = text.find(close, j + hashes + 1)
            if end < 0:
                return None, len(text)
            body = text[j + hashes + 1:end]
            return (body if word == "r" else None), end + len(close)
        return None
    if word and word not in ("b", "c"):
        return None
    if not text.startswith('"', j):
        return None
    k, escaped = j + 1, False
    while k < len(text) and text[k] != '"':
        if text[k] == "\\":
            escaped, k = True, k + 2
        else:
            k += 1
    body = text[j + 1:k]
    return (None if escaped or word else body), k + 1


def include_argument(text, i):
    """(base, path, complete, end) for the argument of an include macro or path attribute at i.
    base is "file" (relative to the file's directory) or MANIFEST_DIR; complete is False when a literal prefix is
    followed by a part computed at compile time (a macro argument), so only the prefix is known."""
    literal = string_at(text, i)
    if literal is not None:
        value, end = literal
        if value is None:
            raise Unreadable("a byte, C or escaped string literal")
        return "file", value, True, end
    word, j = word_at(text, i)
    if word != "concat":
        raise Unreadable(f"an argument that is not a string literal or concat!: {text[i:i + 40]!r}")
    j = skip_trivia(text, j)
    k = skip_trivia(text, j + 1) if text.startswith("!", j) else -1
    if k < 0 or k >= len(text) or text[k] not in "([{":
        raise Unreadable("concat without its macro delimiter")
    close = {"(": ")", "[": "]", "{": "}"}[text[k]]
    base, parts, complete, p = "file", [], True, skip_trivia(text, k + 1)
    while p < len(text) and text[p] != close:
        start = p
        literal = string_at(text, p)
        if literal is not None and literal[0] is not None and complete:
            parts.append(literal[0])
            p = literal[1]
        elif not parts and base == "file" and word_at(text, p)[0] == "env":
            q = skip_trivia(text, word_at(text, p)[1])
            q = skip_trivia(text, q + 1) if text.startswith("!", q) else -1
            name = string_at(text, skip_trivia(text, q + 1)) if q >= 0 and q < len(text) and text[q] in "([{" else None
            if name is None or name[0] != MANIFEST_DIR:
                raise Unreadable("env! of a variable other than CARGO_MANIFEST_DIR")
            base = MANIFEST_DIR
            p = skip_trivia(text, name[1])
            if p >= len(text) or text[p] not in ")]}":
                raise Unreadable("env! with more than one argument")
            p += 1
        else:
            if not parts:
                raise Unreadable(f"concat! whose first part is computed: {text[p:p + 40]!r}")
            complete = False
            depth = 0
            while p < len(text) and (depth or text[p] not in ",)]}"):
                depth += text[p] in "([{"
                depth -= text[p] in ")]}"
                p += 1
        p = skip_trivia(text, p)
        if p < len(text) and text[p] == ",":
            p = skip_trivia(text, p + 1)
        if p == start:
            raise Unreadable(f"concat! with an unbalanced delimiter: {text[p:p + 40]!r}")
    if not parts:
        raise Unreadable("concat! with no literal path part")
    return base, "".join(parts), complete, p + 1


def include_sites(text):
    """[(line, base, path, complete, inline)] for every include macro and path attribute in code, skipping comments,
    string and character literals; an argument the scan cannot resolve is returned as (line, None, reason, False, ()).
    inline is empty except for a path attribute inside inline module blocks: one tuple per enclosing block, outermost
    first, holding the directory names that block can contribute (its name, and the value of any path attribute it
    carries)."""
    sites, i = [], 0
    depth, inline, recent = 0, [], []  # brace depth in code; open inline modules (depth, names); path values since an item boundary
    def site(start, at, attribute=False):
        line = text.count("\n", 0, start) + 1
        try:
            base, path, complete, _ = include_argument(text, skip_trivia(text, at))
        except Unreadable as reason:
            sites.append((line, None, str(reason), False, ()))
            return
        sites.append((line, base, path, complete, tuple(names for _, names in inline) if attribute else ()))
        if attribute:
            recent.append(path)
    def cfg_attr_paths(start, open_at):
        """The path attributes among cfg_attr(<predicate>, <attr>, ...) whose "(" is at open_at, at any nesting."""
        level, p = 0, open_at + 1
        while p < len(text) and not (level == 0 and text[p] == ")"):
            literal = string_at(text, p) if text[p] in '"rbc' else None
            if literal is not None:
                p = literal[1]
                continue
            if text[p] in "([{":
                level += 1
            elif text[p] in ")]}":
                level -= 1
            elif text[p] == "," and level == 0:
                q = skip_trivia(text, p + 1)
                name, after_name = word_at(text, q)
                r = skip_trivia(text, after_name)
                if name == "path" and text.startswith("=", r):
                    site(start, r + 1, attribute=True)
                elif name == "cfg_attr" and text.startswith("(", r):
                    cfg_attr_paths(start, r)
            p += 1
    while i < len(text):
        ch = text[i]
        if text.startswith("//", i) or text.startswith("/*", i):
            i = skip_trivia(text, i)
        elif ch == '"' or ((ch.isalpha() or ch == "_")
                           and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_"))):
            literal = string_at(text, i)
            if literal is not None:
                i = literal[1]
                continue
            word, j = word_at(text, i)
            if word == "r" and text.startswith("#", j):
                i = word_at(text, j + 1)[1]
                continue
            if word in INCLUDE_MACROS:
                k = skip_trivia(text, j)
                if text.startswith("!", k):
                    m = skip_trivia(text, k + 1)
                    if m < len(text) and text[m] in "([{":
                        site(i, m + 1)
            elif word == "mod":
                name, after = word_at(text, skip_trivia(text, j))
                if name and text.startswith("{", skip_trivia(text, after)):
                    # the block opens at the next "{", which the brace count below enters
                    inline.append((depth + 1, (name, *recent)))
            i = j if j > i else i + 1  # every step consumes input, so no edit here can hang the scan
        elif ch == "'":
            if text.startswith("\\", i + 1):
                end = text.find("'", i + 3)
                i = len(text) if end < 0 else end + 1
            elif i + 2 < len(text) and text[i + 2] == "'":
                i += 3
            else:
                i += 1
        elif ch == "#":
            j = skip_trivia(text, i + 1)
            if text.startswith("!", j):
                j = skip_trivia(text, j + 1)
            if text.startswith("[", j):
                k = skip_trivia(text, j + 1)
                word, after = word_at(text, k)
                m = skip_trivia(text, after)
                if word == "path" and text.startswith("=", m):
                    site(i, m + 1, attribute=True)
                elif word == "cfg_attr" and text.startswith("(", m):
                    cfg_attr_paths(i, m)
            i = j if j > i else i + 1
        else:
            if ch == "{":
                depth += 1
                recent.clear()
            elif ch == "}":
                if inline and inline[-1][0] == depth:
                    inline.pop()
                depth -= 1
                recent.clear()
            elif ch == ";":
                recent.clear()
            i += 1
    return sites


def read_blobs(head, paths):
    """{path: text} for paths at head, read in one git cat-file process."""
    request = "".join(f"{head}:{path}\n" for path in paths).encode()
    result = subprocess.run(["git", "cat-file", "--batch"], cwd=REPO, input=request, capture_output=True, check=False)
    if result.returncode != 0:
        raise Full(f"include scan: git cat-file failed (rc {result.returncode})")
    out, texts, pos = result.stdout, {}, 0
    for path in paths:
        header_end = out.index(b"\n", pos)
        header = out[pos:header_end].split()
        if len(header) != 3 or header[1] != b"blob":
            raise Full(f"include scan: cannot read {path} at {head}: {out[pos:header_end]!r}")
        size = int(header[2])
        texts[path] = out[header_end + 1:header_end + 1 + size].decode("utf-8", errors="replace")
        pos = header_end + 1 + size + 1
    return texts


def load_manifests(head):
    """Every directory under crates/ at head mapped to its package name, or None: one ls-tree and one cat-file, so
    crate_of never runs git per directory."""
    listing = git("ls-tree", "-r", "--name-only", head, "--", CRATES).splitlines()
    manifests = {}
    for path in listing:
        parts = path.split("/")[:-1]
        while len(parts) >= 2:
            manifests.setdefault("/".join(parts), None)
            parts.pop()
    tomls = [p for p in listing if p.endswith("/Cargo.toml") and p.count("/") >= 2]
    for path, text in read_blobs(head, tomls).items():
        try:
            package = tomllib.loads(text).get("package")
        except tomllib.TOMLDecodeError as error:
            raise Full(f"manifest: {path} does not parse at {head}: {error}") from error
        if isinstance(package, dict) and isinstance(package.get("name"), str):
            manifests[path.rsplit("/", 1)[0]] = package["name"]
    return manifests


def foreign_includes(head, manifests):
    """[(target, prefix, reader, site)] for every include whose target can lie under a crate other than the reader's.
    target is an exact path (a file, or a directory a #[path] names); prefix is True when a compile-time part follows
    the literal, so every path starting with target is named. Raises Full for any include in code it cannot read."""
    grep = subprocess.run(
        ["git", "grep", "-l", "-I", "-E", "-e", INCLUDE_SITE, head, "--", "crates/*.rs"],
        cwd=REPO, capture_output=True, text=True, check=False,
    )
    if grep.returncode not in (0, 1):
        raise Full(f"include scan: git grep failed (rc {grep.returncode}): {grep.stderr.strip()}")
    files = sorted({row.split(":", 1)[1] for row in grep.stdout.splitlines()})
    sites, unreadable, named = 0, [], []
    for path, text in read_blobs(head, files).items():
        reader = owner_of(path, head, manifests)
        for line, base, literal, complete, inline in include_sites(text):
            sites += 1
            if base is None:
                unreadable.append(f"{path}:{line} ({literal})")
                continue
            if any(c in literal for c in "\n\r"):
                unreadable.append(f"{path}:{line} (a path with a line break)")
                continue
            if base == MANIFEST_DIR:
                if reader is None:
                    unreadable.append(f"{path}:{line} (CARGO_MANIFEST_DIR outside any crate)")
                    continue
                root = next((d for d, n in sorted(manifests.items(), key=lambda kv: -len(kv[0]))
                             if n == reader and path.startswith(d + "/")), None)
                if root is None:
                    unreadable.append(f"{path}:{line} (no manifest directory for {reader})")
                    continue
                joined = [root + "/" + literal.lstrip("/")]
            elif inline:
                # mod-rs file: inline directories start at the file's directory; any other file: at <dir>/<stem>
                starts = [os.path.dirname(path), os.path.splitext(path)[0]]
                for names in inline:
                    starts = [os.path.join(start, name) for start in starts for name in names]
                joined = [os.path.join(start, literal) for start in starts]
            else:
                joined = [os.path.join(os.path.dirname(path), literal)]
            for candidate in dict.fromkeys(joined):
                target = os.path.normpath(candidate) + ("/" if not complete and candidate.endswith("/") else "")
                if target.startswith("../") or not target.startswith(CRATES):
                    continue
                named.append((target, not complete, reader, f"{path}:{line}"))
    if sites == 0:
        raise Full(f"include scan: BROKEN, no #[path] or include! site found under crates/ at {head}")
    # Population print: targets whose directory another crate owns. A prefix is printed by the owner of its directory;
    # a changed path under it is still matched against the reader one by one (named_by_another_crate).
    foreign = [n for n in named if owner_of(n[0], head, manifests) != n[2]]
    log(f"include scan: {sites} sites in {len(files)} files, {len(unreadable)} unreadable, "
        f"{len(foreign)} naming a path outside the reader's crate")
    for target, prefix, reader, site in sorted(foreign):
        log(f"named from another crate: {target}{'*' if prefix else ''} <- {reader} ({site})")
    if unreadable:
        raise Full(f"include scan: cannot resolve {len(unreadable)} include(s) in code: {'; '.join(unreadable[:10])}")
    return named


def named_by_another_crate(path, crate, named):
    """The readers in another crate that name path, exactly, as a directory a #[path] names, or by prefix."""
    hits = []
    for target, prefix, reader, site in named:
        if reader == crate:
            continue
        if path == target or path.startswith(target.rstrip("/") + "/") or (prefix and path.startswith(target)):
            hits.append(f"{reader} ({site})")
    return hits


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
                    raise Full(f"manifest: {directory}/Cargo.toml does not parse at {head}: {error}") from error
                if isinstance(package, dict) and isinstance(package.get("name"), str):
                    manifests[directory] = package["name"]
        if manifests[directory] is not None:
            return manifests[directory]
        parts.pop()
    raise Full(f"under no crate's manifest directory: {path}")


def read_toml(revision, path, label):
    """The parsed TOML at revision:path; raise Full when it is absent or does not parse."""
    try:
        text = git("show", f"{revision}:{path}")
    except Full as reason:
        raise Full(f"{label}: {path} cannot be read at {revision}: {reason}") from reason
    try:
        return tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise Full(f"{label}: {path} does not parse at {revision}: {error}") from error


def changed_keys(key, before, after):
    """The dotted names of the keys that differ under key, one level down where both sides are tables."""
    if isinstance(before, dict) and isinstance(after, dict):
        return [f"{key}.{sub}" for sub in sorted(set(before) | set(after)) if before.get(sub) != after.get(sub)]
    return [key]


def symbolic_links(revision, paths):
    """The paths among paths that are symbolic links (mode 120000) at revision."""
    if not paths:
        return set()
    raw = git("--literal-pathspecs", "ls-tree", "-r", "-z", "--format=%(objectmode) %(path)", revision, "--", *paths)
    return {row.split(" ", 1)[1] for row in raw.split("\0") if row.startswith("120000 ")}


def symlink_map(head):
    """{path: target} for every symbolic link in the head tree."""
    raw = git("ls-tree", "-r", "-z", "--format=%(objectmode) %(path)", head)
    links = [row.split(" ", 1)[1] for row in raw.split("\0") if row.startswith("120000 ")]
    return read_blobs(head, links) if links else {}


def resolve_in_tree(path, links):
    """path with every `..` segment and every symbolic link of the head tree followed, as rustc would open it."""
    parts, resolved, hops = path.split("/"), [], 0
    while parts:
        part = parts.pop(0)
        if part in ("", "."):
            continue
        if part == "..":
            if not resolved:
                raise Full(f"{path} leaves the repository")
            resolved.pop()
            continue
        resolved.append(part)
        target = links.get("/".join(resolved))
        if target is None:
            continue
        hops += 1
        if hops > SYMLINK_HOPS or target.startswith("/"):
            raise Full(f"{path} follows an absolute symbolic link or more than {SYMLINK_HOPS} links")
        resolved.pop()
        parts = target.split("/") + parts
    return "/".join(resolved)


def workspace_dependencies(head):
    table = read_toml(head, WORKSPACE_MANIFEST, "manifest").get("workspace", {}).get("dependencies", {})
    if not isinstance(table, dict):
        raise Full(f"manifest: {WORKSPACE_MANIFEST} has a [workspace.dependencies] that is not a table")
    return table


def dependency_flags(name, entry, inherited):
    """The feature-bearing keys of one dependency entry, a `workspace = true` entry's inherited ones kept apart."""
    if isinstance(entry, str):
        return {}
    if not isinstance(entry, dict):
        raise Full(f"dependency {name} is neither a version string nor a table")
    flags = {("local", flag): entry[flag] for flag in DEPENDENCY_FLAGS if flag in entry}
    if entry.get("workspace") is True:
        if name not in inherited:
            raise Full(f"dependency {name} inherits an entry {WORKSPACE_MANIFEST} does not declare")
        if isinstance(inherited[name], dict):
            flags.update({("workspace", flag): inherited[name][flag] for flag in DEPENDENCY_FLAGS
                          if flag in inherited[name]})
    return flags


def dependency_package(name, entry, inherited):
    """The package a dependency entry names: its `package` key, the inherited entry's, or its own key."""
    for table in (entry, inherited.get(name) if isinstance(entry, dict) and entry.get("workspace") is True else None):
        if isinstance(table, dict) and isinstance(table.get("package"), str):
            return table["package"]
    return name


def dependency_identities(table, inherited):
    """{(package, source): [keys]} for one dependency table: the package each entry names and where it comes from.
    Two entries can share an identity (two keys renaming one package), so each identity keeps every key."""
    identities = {}
    for name, entry in sorted(table.items()):
        source = tuple((field, json.dumps(entry[field], sort_keys=True)) for field in DEPENDENCY_SOURCES
                       if isinstance(entry, dict) and field in entry)
        identities.setdefault((dependency_package(name, entry, inherited), source), []).append(name)
    return identities


def dependency_change(path, label, before, after, inherited):
    """The packages named by the entries a dependency table's change edits. An entry that appears or
    disappears raises Full (ADR-205 Amendment 2, D1)."""
    before, after = before or {}, after or {}
    if not isinstance(before, dict) or not isinstance(after, dict):
        raise Full(f"manifest: {path} has a [{label}] that is not a table")
    named = set()
    for name in sorted(set(before) | set(after)):
        old, new = before.get(name), after.get(name)
        if old == new:
            continue
        old_flags = dependency_flags(name, old, inherited) if old is not None else {}
        new_flags = dependency_flags(name, new, inherited) if new is not None else {}
        if old_flags != new_flags:
            raise Full(f"manifest: {path} changes features, default-features or optional of {label}.{name}")
        named.update(dependency_package(name, entry, inherited) for entry in (old, new) if entry is not None)
    old_ids, new_ids = dependency_identities(before, inherited), dependency_identities(after, inherited)
    for verb, side, other in (("adds", new_ids, old_ids), ("removes", old_ids, new_ids)):
        moved = sorted(f"{label}.{side[identity][-1]}" for identity in side
                       if len(side[identity]) > len(other.get(identity, [])))
        if moved:
            raise Full(f"manifest: {path} {verb} the dependency entry {', '.join(moved)} (a dependency that appears "
                       f"or disappears can change features the lockfile does not record)")
    return named


def targets_by_name(path, key, value):
    if value is None:
        return {}
    if not isinstance(value, list) or not all(isinstance(t, dict) and isinstance(t.get("name"), str) for t in value):
        raise Full(f"manifest: {path} has a [[{key}]] entry without a name")
    names = [t["name"] for t in value]
    if len(set(names)) != len(names):
        raise Full(f"manifest: {path} declares two [[{key}]] targets with one name")
    return {t["name"]: t for t in value}


def target_change(path, key, before, after, crate, head, manifests, links):
    """Raise Full unless every target that differs keeps its required features and its file inside the crate."""
    if key == "lib":
        if not isinstance(before or {}, dict) or not isinstance(after or {}, dict):
            raise Full(f"manifest: {path} has a [lib] that is not a table")
        pairs = [("[lib]", before, after)]
    else:
        old, new = targets_by_name(path, key, before), targets_by_name(path, key, after)
        pairs = [(f"[[{key}]] {name}", old.get(name), new.get(name))
                 for name in sorted(set(old) | set(new)) if old.get(name) != new.get(name)]
    directory = path.rsplit("/", 1)[0]
    for label, old, new in pairs:
        if new is None:
            continue
        if any((old or {}).get(field) != new.get(field) for field in REQUIRED_FEATURES):
            raise Full(f"manifest: {path} changes the required features of {label}")
        target = new.get("path")
        if target is None:
            continue
        if not isinstance(target, str) or target.startswith("/"):
            raise Full(f"manifest: {path} gives {label} a path that is not relative: {target!r}")
        resolved = resolve_in_tree(f"{directory}/{target}", links)
        owner = owner_of(resolved, head, manifests) if resolved.startswith(directory + "/") else None
        if owner != crate:
            raise Full(f"manifest: {path} gives {label} the path {target}, which resolves to {resolved}, "
                       f"outside the crate")


def manifest_change(path, base, head, manifests, links, inherited):
    """(crate, packages) for an admitted change to a crate's own manifest: the packages its changed dependency
    entries name. Raise Full for any other difference (ADR-205 Amendment 1, D2)."""
    crate = manifests.get(path.rsplit("/", 1)[0])
    if crate is None:
        raise Full(f"manifest: {path} is not the manifest of a crate at {head}")
    old, new = read_toml(base, path, "manifest"), read_toml(head, path, "manifest")
    named = set()
    for key in sorted(set(old) | set(new)):
        before, after = old.get(key), new.get(key)
        if before == after:
            continue
        if key in DEPENDENCY_TABLES:
            named |= dependency_change(path, key, before, after, inherited)
        elif key == "target":
            before, after = before or {}, after or {}
            if not isinstance(before, dict) or not isinstance(after, dict):
                raise Full(f"manifest: {path} has a [target] that is not a table")
            for cfg in sorted(set(before) | set(after)):
                old_cfg, new_cfg = before.get(cfg) or {}, after.get(cfg) or {}
                if not isinstance(old_cfg, dict) or not isinstance(new_cfg, dict):
                    raise Full(f"manifest: {path} has a [target.{cfg}] that is not a table")
                for table in sorted(set(old_cfg) | set(new_cfg)):
                    if old_cfg.get(table) == new_cfg.get(table):
                        continue
                    if table not in DEPENDENCY_TABLES:
                        raise Full(f"manifest: {path} changes target.{cfg}.{table}, outside the dependency tables")
                    named |= dependency_change(path, f"target.{cfg}.{table}", old_cfg.get(table),
                                               new_cfg.get(table), inherited)
        elif key in TARGET_TABLES:
            target_change(path, key, before, after, crate, head, manifests, links)
        else:
            raise Full(f"manifest: {path} changes {', '.join(changed_keys(key, before, after))}, outside the "
                       f"dependency and target tables")
    return crate, named


def read_lock(revision):
    """(document, {(name, version, source): entry}) for crates/Cargo.lock at revision."""
    lock = read_toml(revision, LOCKFILE, "lockfile")
    packages = lock.get("package")
    if not isinstance(packages, list) or not all(
            isinstance(p, dict) and isinstance(p.get("name"), str) and isinstance(p.get("version"), str)
            for p in packages):
        raise Full(f"lockfile: {LOCKFILE} at {revision} has a package entry without a name and a version")
    entries = {}
    for package in packages:
        key = (package["name"], package["version"], package.get("source", ""))
        if key in entries:
            raise Full(f"lockfile: {LOCKFILE} at {revision} lists {key[0]} {key[1]} twice")
        entries[key] = {**package, "dependencies": sorted(package.get("dependencies", []))}
    return lock, entries


def lock_graph(entries, revision):
    """{key: {dependency keys}}, each dependency resolved by name, then version, then source as the lockfile writes."""
    by_name = {}
    for key in entries:
        by_name.setdefault(key[0], []).append(key)
    graph = {}
    for key, entry in entries.items():
        edges = set()
        for text in entry["dependencies"]:
            name, _, rest = text.partition(" ")
            version, _, source = rest.partition(" ")
            matches = [k for k in by_name.get(name, []) if (not version or k[1] == version)
                       and (not source or f"({k[2]})" == source)]
            if len(matches) != 1:
                raise Full(f"lockfile: {key[0]} {key[1]} at {revision} depends on {text!r}, which matches "
                           f"{len(matches)} entries")
            edges.add(matches[0])
        graph[key] = edges
    return graph


def reachable(graph, start):
    seen, stack = {start}, [start]
    while stack:
        for following in graph[stack.pop()]:
            if following not in seen:
                seen.add(following)
                stack.append(following)
    return seen


def members(entries, crates):
    return [key for key in entries if not key[2] and key[0] in crates]


def lockfiles(base, head):
    """[(revision, document, entries, graph)] for the base and head lockfiles."""
    sides = []
    for revision in (base, head):
        document, entries = read_lock(revision)
        sides.append((revision, document, entries, lock_graph(entries, revision)))
    return sides


def members_reaching(packages, sides, crates):
    """Every workspace member whose resolved dependencies in either lockfile reach one of packages, itself included."""
    listed = {key[0] for _, _, entries, _ in sides for key in entries}
    missing = sorted(packages - listed)
    if missing:
        raise Full(f"lockfile: neither side lists {', '.join(missing)}, which a changed dependency entry names")
    return {member[0] for _, _, entries, graph in sides for member in members(entries, crates)
            if any(key[0] in packages for key in reachable(graph, member))}


def lockfile_change(sides, crates):
    """The workspace members a lockfile change reaches (ADR-205 Amendment 1, D3)."""
    (_, old_doc, old, old_graph), (_, new_doc, new, new_graph) = sides
    if old_doc.get("version") != new_doc.get("version"):
        raise Full(f"lockfile: {LOCKFILE} changes its version header")
    other = sorted(key for key in set(old_doc) | set(new_doc)
                   if key not in ("version", "package") and old_doc.get(key) != new_doc.get(key))
    if other:
        raise Full(f"lockfile: {LOCKFILE} changes {', '.join(other)}")
    changed = {key for key in set(old) | set(new) if old.get(key) != new.get(key)}
    outside = sorted(key for key in changed if key[2] or key[0] not in crates)
    if outside:
        raise Full(f"lockfile: {LOCKFILE} changes {len(outside)} entries outside the workspace, first "
                   f"{outside[0][0]} {outside[0][1]} (the lockfile does not record the features they request)")
    everyone = {member[0] for entries in (old, new) for member in members(entries, crates)}
    if not everyone:
        raise Full(f"lockfile: {LOCKFILE} lists no workspace member")
    reached = {member[0] for entries, graph in ((old, old_graph), (new, new_graph))
               for member in members(entries, crates) if reachable(graph, member) & changed}
    log(f"lockfile: {len(changed)} changed entries: "
        f"{' '.join(sorted(f'{k[0]}@{k[1]}' for k in changed))}")
    log(f"lockfile: {len(reached)} of {len(everyone)} workspace members reach them: {' '.join(sorted(reached))}")
    if reached == everyone:
        raise Full(f"lockfile: the change reaches every workspace member ({len(everyone)})")
    return reached


def export_tree(revision, destination):
    """Write revision's crates/ under destination and check every file arrived."""
    archive = subprocess.run(["git", "archive", "--format=tar", revision, "crates"], cwd=REPO,
                             capture_output=True, check=False)
    if archive.returncode != 0:
        raise Full(f"git archive of {revision} failed (rc {archive.returncode})")
    unpack = subprocess.run(["tar", "-x", "-f", "-", "-C", str(destination)], input=archive.stdout,
                            capture_output=True, check=False)
    if unpack.returncode != 0:
        raise Full(f"unpacking {revision}'s crates/ failed (rc {unpack.returncode})")
    expected = len(git("ls-tree", "-r", "-z", "--name-only", revision, "--", CRATES).split("\0")) - 1
    written = sum(len(files) + sum(os.path.islink(os.path.join(root, d)) for d in dirs)
                  for root, dirs, files in os.walk(destination / "crates"))
    if written != expected:
        raise Full(f"{revision}'s crates/ unpacked {written} files where the tree has {expected}")


class Kkernel:
    def __init__(self, binary, workdir):
        self.binary = binary
        self.home = workdir / "home"
        self.home.mkdir()
        self.config = workdir / "config.toml"
        self.config.write_text("")

    def exec(self, ops, *, expected_count=1):
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
        if not isinstance(outcomes, list):
            raise Full("kkernel exec returned results that are not a list")
        if len(outcomes) != expected_count:
            raise Full(f"kkernel exec returned {len(outcomes)} outcomes for {expected_count} ops")
        values = []
        for outcome in outcomes:
            if not isinstance(outcome, dict) or outcome.get("ok") is not True:
                raise Full(f"kkernel op failed: {json.dumps(outcome)[:2000]}")
            values.append(outcome.get("result"))
        return values


def quote(value):
    return json.dumps(value)


def ingest(kernel, workdir, base=None):
    """Ingest the head checkout's crates/, or base's when a crate manifest changed, so the binary never parses a
    manifest the change wrote (ADR-205 D4, Amendment 1 D2)."""
    maps = workdir / "maps"
    maps.mkdir()
    db = maps / "workspace.db"
    # An explicit db must name an existing regular file; an empty one initializes.
    db.touch()
    if base is None:
        source = REPO / "crates"
    else:
        tree = workdir / "base-tree"
        tree.mkdir()
        export_tree(base, tree)
        source = tree / "crates"
        log(f"ingest: the base tree {base}, because a crate manifest changed")
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
            graphs = kernel.exec(f"[{ops}]", expected_count=len(chunk))
            for ident, graph in zip(chunk, graphs):
                if not isinstance(graph, dict) or graph.get("truncated") is not False:
                    raise Full(f"code.deps from {name_of.get(ident, ident)} was truncated or malformed")
                origin = name_of.get(ident, ident)
                if graph.get("map") != MAP_NAME:
                    raise Full(f"code.deps from {origin} returned wrong map: {graph.get('map')!r}")
                if graph.get("root") != ident:
                    raise Full(f"code.deps from {origin} returned wrong root: {graph.get('root')!r}")
                nodes, edges = graph.get("nodes"), graph.get("edges")
                if not isinstance(nodes, list):
                    raise Full(f"code.deps from {origin}: nodes is not a list")
                if not isinstance(edges, list):
                    raise Full(f"code.deps from {origin}: edges is not a list")
                kinds = {}
                for node in nodes:
                    if (not isinstance(node, dict) or not isinstance(node.get("id"), str)
                            or not isinstance(node.get("kind"), str)):
                        raise Full(f"code.deps from {origin} returned a node without string id and kind")
                    if node["kind"] not in NODE_KINDS:
                        raise Full(f"code.deps from {origin} returned a node of unknown kind: {node['kind']!r}")
                    if node["id"] in kinds:
                        raise Full(f"code.deps from {origin} returned duplicate node id: {node['id']}")
                    kinds[node["id"]] = node["kind"]
                for edge in edges:
                    if (not isinstance(edge, dict) or not isinstance(edge.get("source"), str)
                            or not isinstance(edge.get("target"), str)):
                        raise Full(f"code.deps from {origin} returned an edge without string source and target")
                    # Every endpoint is a node of the reply; an edge to an omitted node would otherwise be
                    # skipped below as a non-project, and the scope would narrow without a refusal.
                    for end in ("source", "target"):
                        if edge[end] not in kinds:
                            raise Full(f"code.deps from {origin} returned an edge {end} missing from nodes: "
                                       f"{edge[end]}")
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
    sorted_paths = {"markdown": [], "crate": [], "crate-manifest": [], "lockfile": []}
    for path in paths:
        sorted_paths[path_rule(path)].append(path)
    markdown, crate_paths = sorted_paths["markdown"], sorted_paths["crate"]
    manifest_paths, lockfile = sorted_paths["crate-manifest"], bool(sorted_paths["lockfile"])
    if markdown:
        targets = symlink_targets(head)
        for path in markdown:
            check_markdown(path, head, targets)
    if not crate_paths and not manifest_paths and not lockfile:
        raise Full("no crate source changed")
    # git show returns a link's target text while cargo reads the file the link names, so a manifest or lockfile
    # that is a link on either side is never read as data.
    for revision in (base, head):
        for path in sorted(symbolic_links(revision, manifest_paths + sorted_paths["lockfile"])):
            raise Full(f"{path} is a symbolic link at {revision}")
    # A changed path resolves to a crate by its own location while the compiler reads the file a link names, and an
    # unchanged link reads the new contents of whatever it names; so any link under crates/, on either side.
    for revision in (base, head):
        links = sorted(symbolic_links(revision, [CRATES]))
        if links:
            raise Full(f"{len(links)} symbolic link(s) under {CRATES} at {revision}, first {links[0]}")
    manifests = load_manifests(head)
    changed = set()
    for path in crate_paths:
        crate = crate_of(path, head, manifests)
        log(f"changed: {path} -> {crate}")
        changed.add(crate)
    packages = set()
    if manifest_paths:
        links, inherited = symlink_map(head), workspace_dependencies(head)
        for path in manifest_paths:
            crate, named_packages = manifest_change(path, base, head, manifests, links, inherited)
            log(f"changed: {path} -> {crate} (manifest; changed dependency entries name: "
                f"{' '.join(sorted(named_packages)) or 'none'})")
            changed.add(crate)
            packages |= named_packages
    if packages or lockfile:
        crates = {name for name in manifests.values() if name is not None}
        sides = lockfiles(base, head)
        if packages:
            reaching = members_reaching(packages, sides, crates)
            log(f"members reaching {' '.join(sorted(packages))} in either lockfile: {' '.join(sorted(reaching))}")
            changed |= reaching
        if lockfile:
            changed |= lockfile_change(sides, crates)
    if not changed:
        raise Full("the change reaches no crate")
    log(f"changed crates: {' '.join(sorted(changed))}")
    named = foreign_includes(head, manifests)
    for path in crate_paths + manifest_paths:
        readers = named_by_another_crate(path, crate_of(path, head, manifests), named)
        if readers:
            raise Full(f"{path} is compiled or read by another crate: {', '.join(readers)}")
    if lockfile:
        readers = named_by_another_crate(LOCKFILE, None, named)
        if readers:
            raise Full(f"{LOCKFILE} is read by a crate: {', '.join(readers)}")
    if paths_only:
        return None

    if not binary or not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        raise Full(f"no executable kkernel binary at {binary!r}")
    with tempfile.TemporaryDirectory(prefix="ci-test-scope-") as tmp:
        # The map's VFS refuses a path with a symlinked parent; macOS /tmp and /var are symlinks.
        workdir = Path(tmp).resolve()
        kernel = Kkernel(binary, workdir)
        ingest(kernel, workdir, base if manifest_paths else None)
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
