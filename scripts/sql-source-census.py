#!/usr/bin/env python3
"""Static SQL source census for explicitly selected crates at one Git commit.

Python 3.11+; standard library only. This does not prepare or execute SQL.
SQL assets, SQL-shaped inline literals, and reader references are separate units.
The Rust lexer/module walker is deliberately not a macro or Cargo feature expander.
"""

from __future__ import annotations

import argparse
from bisect import bisect_right
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path, PurePosixPath
import posixpath
import re
import subprocess
import sys
import tomllib
import unicodedata


class CensusError(ValueError):
    """Invalid input or a source population that cannot be read reliably."""


def crate_name(value: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9_-]+", value):
        raise CensusError(f"invalid crate directory name: {value!r}")
    return value


def full_revision(value: str) -> str:
    if not re.fullmatch(r"[0-9a-fA-F]{40}", value):
        raise CensusError("--revision must be a full 40-hex commit id")
    return value.lower()


class DirectoryTree:
    """Filesystem source adapter for synthetic unit fixtures, not the CLI."""

    def __init__(self, root: Path, crates: list[str]):
        self.root = root.resolve()
        self.crates = sorted({crate_name(c) for c in crates})
        self.paths: set[str] = set()
        for name in self.crates:
            base = self.root / "crates" / name
            if not base.is_dir() or base.is_symlink():
                raise CensusError(f"missing or symlinked crate: {name}")
            for path in base.rglob("*"):
                if path.is_file() and not path.is_symlink():
                    self.paths.add(path.relative_to(self.root).as_posix())

    def read(self, path: str) -> str:
        if path not in self.paths:
            raise CensusError(f"source not in selected population: {path}")
        resolved = (self.root / path).resolve()
        if not resolved.is_relative_to(self.root):
            raise CensusError(f"source escapes fixture root: {path}")
        return resolved.read_text(encoding="utf-8")


class GitTree:
    """Read only immutable blobs below the explicitly selected crate prefixes."""

    def __init__(self, root: Path, revision: str, crates: list[str]):
        self.root = root
        self.revision = full_revision(revision)
        self.crates = sorted({crate_name(c) for c in crates})
        if not self.crates:
            raise CensusError("at least one explicit --crate is required")
        actual = self.git("rev-parse", "--verify", self.revision + "^{commit}").strip()
        if actual != self.revision:
            raise CensusError("revision does not identify the requested commit")
        entries = self.git(
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            self.revision,
            "--",
            *(f"crates/{c}/" for c in self.crates),
        )
        self.paths: set[str] = set()
        for entry in entries.split("\0"):
            if not entry:
                continue
            metadata, path = entry.split("\t", 1)
            mode, kind, _oid = metadata.split()
            if kind == "blob" and mode in {"100644", "100755"}:
                self.paths.add(path)
        for name in self.crates:
            if f"crates/{name}/Cargo.toml" not in self.paths:
                raise CensusError(f"no regular crate manifest at revision: {name}")

    def git(self, *args: str) -> str:
        result = subprocess.run(
            ["git", "-C", str(self.root), *args],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if result.returncode:
            raise CensusError(result.stderr.decode("utf-8", "replace").strip())
        return result.stdout.decode("utf-8", "strict")

    def read(self, path: str) -> str:
        if path not in self.paths:
            raise CensusError(f"source not in selected population: {path}")
        return self.git("show", f"{self.revision}:{path}")


@dataclass(frozen=True)
class Token:
    kind: str
    text: str
    value: str
    line: int
    column: int


def rust_tokens(source: str) -> list[Token]:
    """Lex spans without interpreting comments, string contents or chars as Rust."""
    line_starts = [0] + [i + 1 for i, ch in enumerate(source) if ch == "\n"]
    tokens = []
    raw_pattern = re.compile(r'(?:br|cr|r)(#+)?"')
    string_pattern = re.compile(r'(?:b|c)?"')
    char_pattern = re.compile(
        r"'(?:\\(?:u\{[0-9A-Fa-f_]+\}|x[0-9A-Fa-f]{2}|.)|[^'\\])'"
    )
    ident_pattern = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*")
    unicode_pattern = re.compile(r"u\{([0-9A-Fa-f_]+)\}")
    i = 0
    while i < len(source):
        start = i
        ch = source[i]
        if ch.isspace():
            i += 1
            continue
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end
            continue
        if source.startswith("/*", i):
            depth = 1
            i += 2
            while depth and i < len(source):
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise CensusError("unterminated Rust block comment")
            continue
        raw = raw_pattern.match(source, i)
        string = string_pattern.match(source, i)
        kind = "punct"
        value = ch
        if raw:
            opener = raw.group()
            hashes = raw.group(1) or ""
            body = i + len(opener)
            end = source.find('"' + hashes, body)
            if end < 0:
                raise CensusError("unterminated Rust raw string")
            value = source[body:end]
            i = end + 1 + len(hashes)
            kind = "string"
        elif string:
            i += len(string.group())
            out = []
            while i < len(source) and source[i] != '"':
                if source[i] != "\\":
                    out.append(source[i])
                    i += 1
                    continue
                i += 1
                if i >= len(source):
                    raise CensusError("unterminated Rust string escape")
                escaped = source[i]
                simple = {
                    "n": "\n",
                    "r": "\r",
                    "t": "\t",
                    "0": "\0",
                    "\\": "\\",
                    '"': '"',
                    "'": "'",
                }
                if escaped in simple:
                    out.append(simple[escaped])
                    i += 1
                elif escaped == "x" and re.match(
                    r"[0-9A-Fa-f]{2}", source[i + 1 : i + 3]
                ):
                    out.append(chr(int(source[i + 1 : i + 3], 16)))
                    i += 3
                elif escaped == "u":
                    match = unicode_pattern.match(source, i)
                    if not match:
                        raise CensusError("invalid Rust Unicode escape")
                    out.append(chr(int(match.group(1).replace("_", ""), 16)))
                    i += len(match.group())
                elif escaped == "\n" or source.startswith("\r\n", i):
                    while i < len(source) and source[i].isspace():
                        i += 1
                else:
                    raise CensusError("unsupported or invalid Rust string escape")
            if i >= len(source):
                raise CensusError("unterminated Rust string")
            i += 1
            kind, value = "string", "".join(out)
        elif ch == "'" and (match := char_pattern.match(source, i)):
            i += len(match.group())
            kind = "char"
            value = ""
        elif match := ident_pattern.match(source, i):
            i += len(match.group())
            kind = "ident"
            value = match.group().removeprefix("r#")
        else:
            i += 1
        line_index = bisect_right(line_starts, start) - 1
        tokens.append(
            Token(
                kind,
                source[start:i],
                value,
                line_index + 1,
                start - line_starts[line_index] + 1,
            )
        )
    return tokens


def pairs(tokens: list[Token]) -> dict[int, int]:
    stack = []
    found = {}
    for i, token in enumerate(tokens):
        if token.text in {"(", "[", "{"}:
            stack.append((i, token.text))
        elif token.text in {")", "]", "}"}:
            if not stack or {"(": ")", "[": "]", "{": "}"}[stack[-1][1]] != token.text:
                raise CensusError(f"unbalanced Rust delimiters at line {token.line}")
            start, _ = stack.pop()
            found[start] = i
            found[i] = start
    if stack:
        raise CensusError("unclosed Rust delimiter")
    return found


def cfg_value(tokens: list[Token]) -> bool | None:
    """Evaluate only test and boolean composition; target/features stay unknown."""
    if not tokens:
        return None
    if len(tokens) == 1 and tokens[0].value == "test":
        return False
    if len(tokens) >= 3 and tokens[1].text == "(" and tokens[-1].text == ")":
        name = tokens[0].value
        groups = []
        start = 2
        depth = 0
        for i in range(2, len(tokens) - 1):
            if tokens[i].text in {"(", "[", "{"}:
                depth += 1
            elif tokens[i].text in {")", "]", "}"}:
                depth -= 1
            elif tokens[i].text == "," and depth == 0:
                if i > start:
                    groups.append(tokens[start:i])
                start = i + 1
        if start < len(tokens) - 1:
            groups.append(tokens[start:-1])
        values = [cfg_value(group) for group in groups]
        if name == "not" and len(values) == 1:
            return None if values[0] is None else not values[0]
        if name == "all":
            return False if False in values else (None if None in values else True)
        if name == "any":
            return True if True in values else (None if None in values else False)
    return None


def item_end(tokens: list[Token], start: int, matching: dict[int, int]) -> int:
    """End of an attributed item; ignore punctuation inside balanced arguments."""
    head = start
    if head < len(tokens) and tokens[head].value == "pub":
        head += 1
        if head < len(tokens) and tokens[head].text == "(":
            head = matching[head] + 1
    if head < len(tokens) and tokens[head].value == "default":
        head += 1
    keyword = tokens[head].value if head < len(tokens) else ""
    semicolon_item = keyword in {"static", "let"}
    if keyword == "const":
        after_const = head + 1
        while after_const < len(tokens) and (
            tokens[after_const].value in {"unsafe", "extern"}
            or tokens[after_const].kind == "string"
        ):
            after_const += 1
        semicolon_item = after_const >= len(tokens) or tokens[after_const].value != "fn"
    i = start
    while i < len(tokens):
        if tokens[i].text == ";":
            return i + 1
        if tokens[i].text == "{":
            if not semicolon_item:
                return matching[i] + 1
            i = matching[i] + 1
            continue
        if tokens[i].text in {"(", "["}:
            i = matching[i] + 1
        elif tokens[i].text == "}":
            return i
        else:
            i += 1
    return i


def production_tokens(tokens: list[Token]) -> tuple[list[Token], list[dict]]:
    """Remove false test-gated items before SQL classification or module following."""
    matching = pairs(tokens)
    keep = [True] * len(tokens)
    diagnostics = []
    enclosing = []
    region_end = [len(tokens)] * len(tokens)
    for i, token in enumerate(tokens):
        region_end[i] = enclosing[-1] if enclosing else len(tokens)
        if token.text == "{":
            enclosing.append(matching[i])
        elif token.text == "}":
            enclosing.pop()
    i = 0
    while i < len(tokens):
        if not keep[i] or tokens[i].text != "#":
            i += 1
            continue
        first = i
        attrs = []
        while i < len(tokens) and tokens[i].text == "#":
            inner = i + 1 < len(tokens) and tokens[i + 1].text == "!"
            bracket = i + 2 if inner else i + 1
            if bracket >= len(tokens) or tokens[bracket].text != "[":
                break
            end = matching[bracket]
            attrs.append((i, end + 1, inner, tokens[bracket + 1 : end]))
            i = end + 1
        if not attrs:
            i += 1
            continue
        exclude = False
        inner_exclude = False
        for a, b, inner, attr in attrs:
            if not attr:
                continue
            name = attr[0].value
            value = True
            if name == "cfg" and len(attr) >= 3 and attr[1].text == "(":
                value = cfg_value(attr[2:-1])
                if value is None:
                    diagnostics.append(
                        {
                            "line": tokens[a].line,
                            "kind": "unresolved_cfg",
                            "detail": "feature/target condition retained conservatively",
                        }
                    )
            elif (
                (
                    path := attr[
                        : next(
                            (i for i, token in enumerate(attr) if token.text == "("),
                            len(attr),
                        )
                    ]
                )
                and path[-1].value in {"test", "bench"}
                and all(token.kind == "ident" or token.text == ":" for token in path)
            ):
                value = False
            elif name == "cfg_attr":
                diagnostics.append(
                    {
                        "line": tokens[a].line,
                        "kind": "unresolved_cfg_attr",
                        "detail": "attribute expansion not evaluated",
                    }
                )
            if value is False:
                if inner:
                    inner_exclude = True
                else:
                    exclude = True
            # Keep path attributes for the module resolver; other attributes are not SQL.
            if name != "path":
                keep[a:b] = [False] * (b - a)
        if inner_exclude:
            end = region_end[first]
            keep[first:end] = [False] * (end - first)
            i = end
        elif exclude:
            end = item_end(tokens, i, matching)
            keep[first:end] = [False] * (end - first)
            i = max(end, i)
    return [token for token, active in zip(tokens, keep) if active], diagnostics


def sql_shape(text: str) -> dict | None:
    """Classify lexical SQL shapes, not SQL validity or statement execution."""
    words = []
    pattern = re.compile(
        r"--[^\n]*|/\*.*?\*/|'(?:''|[^'])*'|\"(?:\"\"|[^\"])*\"|"
        r"`(?:``|[^`])*`|\[[^\]]*\]|[A-Za-z_][A-Za-z0-9_]*|\?[0-9]*|"
        r"[0-9]+(?:\.[0-9]+)?|[^\s]",
        re.S,
    )
    for match in pattern.finditer(text):
        token = match.group()
        if token.startswith(("--", "/*")):
            continue
        if token.startswith("'"):
            words.append("<literal>")
        elif token[0] in {'"', "`", "["}:
            words.append(token[1:-1])
        else:
            words.append(token)
    if not words:
        return None
    upper = [word.upper() for word in words]
    head = upper[0]
    identifier = lambda value: bool(re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", value))
    table_after = lambda i: i + 1 < len(words) and identifier(words[i + 1])
    complete = False
    fragment = False
    if head == "SELECT":
        complete = any(
            word == "FROM" and table_after(i) for i, word in enumerate(upper)
        )
        complete |= len(words) > 1 and (
            words[1] == "<literal>"
            or words[1].startswith("?")
            or words[1][0].isdigit()
            or (len(words) > 2 and identifier(words[1]) and words[2] == "(")
        )
        fragment = not complete and len(words) > 1
    elif head in {"INSERT", "REPLACE"}:
        complete = (
            "INTO" in upper
            and any(word == "INTO" and table_after(i) for i, word in enumerate(upper))
            and any(word in {"VALUES", "SELECT", "DEFAULT"} for word in upper)
        )
    elif head == "UPDATE":
        complete = table_after(0) and "SET" in upper and "=" in words
    elif head == "DELETE":
        complete = len(words) > 2 and upper[1] == "FROM" and table_after(1)
    elif head == "WITH":
        # CTE requires a named AS(...) term, not English such as 'with reason'.
        cte = any(
            upper[i] == "AS" and i + 1 < len(words) and words[i + 1] == "("
            for i in range(1, len(words))
        )
        complete = cte and any(
            word in {"SELECT", "INSERT", "UPDATE", "DELETE"} for word in upper[2:]
        )
        fragment = not complete and (cte or "{" in words)
    elif head in {"CREATE", "ALTER", "DROP"}:
        complete = any(
            word in {"TABLE", "INDEX", "VIEW", "TRIGGER"} and table_after(i)
            for i, word in enumerate(upper)
        )
    elif head == "PRAGMA":
        complete = table_after(0)
    elif head in {"BEGIN", "COMMIT", "ROLLBACK", "SAVEPOINT", "RELEASE", "VACUUM"}:
        # Bare words also name VCS operations, labels and prose. Require a
        # terminator or transaction qualification; assets still count separately.
        complete = len(words) > 1 and (
            words[-1] == ";"
            or upper[1] in {"TRANSACTION", "DEFERRED", "IMMEDIATE", "EXCLUSIVE"}
        )
    elif head in {"FROM", "JOIN"}:
        fragment = table_after(0) or "{" in words
    elif head == "WHERE":
        fragment = any(
            word in {"=", "<", ">", "IN", "IS", "LIKE", "EXISTS"} for word in upper[1:]
        )
    elif head in {"ORDER", "GROUP"}:
        fragment = len(words) > 2 and upper[1] == "BY"
    if not (complete or fragment):
        return None
    dynamic = bool(re.search(r"\{[^}]*\}|%[sd]", text))
    table_tokens = sorted(
        {
            words[i + 1]
            for i, word in enumerate(upper)
            if word in {"FROM", "JOIN", "UPDATE", "INTO"} and table_after(i)
        }
    )
    return {
        "shape": "fragment" if fragment or dynamic else "statement_candidate",
        "head": head,
        "dynamic": dynamic,
        "table_tokens": table_tokens,
    }


def resolve_path(base: str, relative: str, crate_root: str) -> str | None:
    if relative.startswith("/") or "\\" in relative:
        return None
    path = posixpath.normpath(posixpath.join(base, relative))
    return path if path.startswith(crate_root + "/") else None


def rust_roots(tree, crate_root: str) -> list[str]:
    manifest_path = crate_root + "/Cargo.toml"
    if manifest_path not in tree.paths:
        raise CensusError(f"missing crate manifest: {manifest_path}")
    try:
        manifest = tomllib.loads(tree.read(manifest_path))
    except tomllib.TOMLDecodeError as error:
        raise CensusError(f"{manifest_path}: {error}") from error

    def explicit_root(relative, label):
        if not isinstance(relative, str):
            raise CensusError(
                f"{manifest_path}: explicit {label} path must be a string"
            )
        target = resolve_path(crate_root, relative, crate_root)
        if target is None:
            raise CensusError(
                f"{manifest_path}: explicit {label} path is outside selected crate"
            )
        if target not in tree.paths:
            raise CensusError(
                f"{manifest_path}: explicit {label} path is missing: {target}"
            )
        return target

    roots = set()
    package = manifest.get("package", {})
    if "lib" in manifest:
        roots.add(explicit_root(manifest["lib"].get("path", "src/lib.rs"), "library"))
    else:
        roots.add(crate_root + "/src/lib.rs")
    if package.get("autobins", True):
        roots.add(crate_root + "/src/main.rs")
        prefix = crate_root + "/src/bin/"
        roots.update(
            path
            for path in tree.paths
            if path.startswith(prefix)
            and (
                len(PurePosixPath(path).relative_to(prefix).parts) == 1
                and path.endswith(".rs")
                or len(PurePosixPath(path).relative_to(prefix).parts) == 2
                and path.endswith("/main.rs")
            )
        )
    for binary in manifest.get("bin", []):
        if "path" in binary:
            roots.add(explicit_root(binary["path"], "binary"))
    if "build" in package and package["build"] is not False:
        build = "build.rs" if package["build"] is True else package["build"]
        roots.add(explicit_root(build, "build script"))
    elif "build" not in package:
        roots.add(crate_root + "/build.rs")
    return sorted(path for path in roots if path in tree.paths)


def census(tree, revision: str) -> dict:
    results = []
    for name in tree.crates:
        root = f"crates/{name}"
        assets = []
        inline = []
        readers = []
        diagnostics = []
        visited = set()
        pending = [(path, posixpath.dirname(path)) for path in rust_roots(tree, root)]
        for path in sorted(tree.paths):
            if path.startswith(root + "/sql/") and path.endswith(".sql"):
                source = tree.read(path)
                assets.append(
                    {
                        "path": path,
                        "sha256": hashlib.sha256(source.encode()).hexdigest(),
                        "classification": sql_shape(source),
                    }
                )

        def diagnostic(path, token, kind, detail):
            diagnostics.append(
                {"path": path, "line": token.line, "kind": kind, "detail": detail}
            )

        while pending:
            path, module_dir = pending.pop()
            if path in visited:
                continue
            visited.add(path)
            try:
                tokens, notices = production_tokens(rust_tokens(tree.read(path)))
                matching = pairs(tokens)
            except CensusError as error:
                raise CensusError(f"{path}: {error}") from error
            diagnostics.extend({"path": path, **notice} for notice in notices)
            attribute_strings = set()
            for i, token in enumerate(tokens):
                if (
                    token.text == "#"
                    and i + 1 < len(tokens)
                    and tokens[i + 1].text == "["
                ):
                    attribute_strings.update(range(i, matching[i + 1] + 1))
            reader_strings = set()
            for i, token in enumerate(tokens[:-2]):
                if (
                    token.value not in {"sql", "include_str", "include"}
                    or tokens[i + 1].text != "!"
                ):
                    continue
                opening = i + 2
                if tokens[opening].text not in {"(", "[", "{"}:
                    continue
                args = tokens[opening + 1 : matching[opening]]
                if args and args[-1].text == ",":
                    args = args[:-1]
                literal = (
                    args[0].value
                    if len(args) == 1 and args[0].kind == "string"
                    else None
                )
                if token.value == "include":
                    target = (
                        resolve_path(posixpath.dirname(path), literal, root)
                        if literal is not None
                        else None
                    )
                    if target in tree.paths and target.endswith(".rs"):
                        pending.append((target, module_dir))
                    else:
                        diagnostic(
                            path,
                            token,
                            "unresolved_include",
                            "nonliteral, missing, or outside selected crate",
                        )
                    continue
                if (
                    token.value == "include_str"
                    and literal is not None
                    and not literal.endswith(".sql")
                ):
                    continue
                reader_strings.update(range(opening + 1, matching[opening]))
                target = (
                    resolve_path(root + "/sql", literal + ".sql", root)
                    if (token.value == "sql" and literal is not None)
                    else resolve_path(posixpath.dirname(path), literal, root)
                    if literal is not None
                    else None
                )
                readers.append(
                    {
                        "path": path,
                        "line": token.line,
                        "column": token.column,
                        "macro": token.value + "!",
                        "asset": target,
                        "resolution": "resolved"
                        if target in tree.paths
                        else "unresolved",
                    }
                )
            for i, token in enumerate(tokens):
                if (
                    token.kind == "string"
                    and i not in attribute_strings | reader_strings
                ):
                    shape = sql_shape(token.value)
                    if shape:
                        inline.append(
                            {
                                "path": path,
                                "line": token.line,
                                "column": token.column,
                                **shape,
                            }
                        )

            def modules(start: int, end: int, directory: str, attribute_base: str):
                i = start
                while i < end:
                    attr_path = None
                    while i < end and tokens[i].text == "#":
                        bracket = i + 1
                        if bracket >= end or tokens[bracket].text != "[":
                            break
                        close = matching[bracket]
                        attr = tokens[bracket + 1 : close]
                        if (
                            len(attr) == 3
                            and attr[0].value == "path"
                            and attr[1].text == "="
                            and attr[2].kind == "string"
                        ):
                            attr_path = attr[2].value
                        i = close + 1
                    j = i
                    if j < end and tokens[j].value == "pub":
                        j += 1
                        if j < end and tokens[j].text == "(":
                            j = matching[j] + 1
                    if (
                        j + 2 < end
                        and tokens[j].value == "mod"
                        and tokens[j + 1].kind == "ident"
                    ):
                        name_token = tokens[j + 1]
                        name_dir = posixpath.join(directory, name_token.value)
                        if tokens[j + 2].text == "{":
                            close = matching[j + 2]
                            inline_dir = (
                                resolve_path(attribute_base, attr_path, root)
                                if attr_path is not None
                                else name_dir
                            )
                            if inline_dir is None:
                                diagnostic(
                                    path,
                                    name_token,
                                    "unresolved_module",
                                    "inline path leaves selected crate",
                                )
                            else:
                                modules(j + 3, close, inline_dir, inline_dir)
                            i = close + 1
                            continue
                        if tokens[j + 2].text == ";":
                            choices = (
                                [resolve_path(attribute_base, attr_path, root)]
                                if attr_path is not None
                                else [name_dir + ".rs", name_dir + "/mod.rs"]
                            )
                            found = [
                                choice for choice in choices if choice in tree.paths
                            ]
                            if len(found) == 1:
                                child = found[0]
                                child_dir = (
                                    posixpath.dirname(child)
                                    if attr_path is not None
                                    or child.endswith("/mod.rs")
                                    else child[:-3]
                                )
                                pending.append((child, child_dir))
                            else:
                                diagnostic(
                                    path,
                                    name_token,
                                    "unresolved_module",
                                    "missing or ambiguous literal module path",
                                )
                            i = j + 3
                            continue
                    next_i = item_end(tokens, i, matching)
                    i = next_i if next_i > i else i + 1

            modules(0, len(tokens), module_dir, posixpath.dirname(path))
        rust_paths = {
            path
            for path in tree.paths
            if path.startswith(root + "/") and path.endswith(".rs")
        }
        for records in (assets, inline, readers, diagnostics):
            records.sort(
                key=lambda row: (
                    row["path"],
                    row.get("line", 0),
                    row.get("column", 0),
                    row.get("kind", ""),
                )
            )
        results.append(
            {
                "crate": name,
                "counts": {
                    "sql_assets": len(assets),
                    "inline_statement_candidates": sum(
                        row["shape"] == "statement_candidate" for row in inline
                    ),
                    "inline_fragments": sum(
                        row["shape"] == "fragment" for row in inline
                    ),
                    "reader_references": len(readers),
                    "unresolved_readers": sum(
                        row["resolution"] == "unresolved" for row in readers
                    ),
                    "rust_files_read": len(visited),
                    "rust_files_not_followed": len(rust_paths - visited),
                },
                "rust_files": sorted(visited),
                "sql_assets": assets,
                "inline_candidates": inline,
                "reader_references": readers,
                "diagnostics": diagnostics,
            }
        )
    return {
        "schema": 1,
        "revision": revision,
        "population": "explicit crates; production module graph with cfg(test)=false",
        "limits": [
            "static source units, not executed SQL or validated statements",
            "unknown feature/target cfg retained; cfg_attr and macros not expanded",
            "literal modules/includes only; unresolved paths reported",
            "SQL shape/table tokens are lexical candidates, not semantic query plans",
            "bare transaction/admin words excluded without terminator or transaction qualification",
        ],
        "crates": results,
    }


def escape_terminal_text(value: str) -> str:
    parts = []
    for character in value:
        if unicodedata.category(character) in ("Cc", "Cf", "Zl", "Zp"):
            codepoint = ord(character)
            parts.append(
                f"\\u{codepoint:04x}"
                if codepoint <= 0xFFFF
                else f"\\U{codepoint:08x}"
            )
        else:
            parts.append(character)
    return "".join(parts)


def text_report(report: dict) -> str:
    lines = [f"Static SQL source census at {report['revision']} (not executed SQL)"]
    for row in report["crates"]:
        c = row["counts"]
        lines.append(
            f"{row['crate']}: assets={c['sql_assets']} inline={c['inline_statement_candidates']} "
            f"fragments={c['inline_fragments']} readers={c['reader_references']} "
            f"unresolved_readers={c['unresolved_readers']} rust_files={c['rust_files_read']}"
        )
        for asset in row["sql_assets"]:
            lines.append(f"  {asset['path']} SQL asset")
        for candidate in row["inline_candidates"]:
            lines.append(
                f"  {candidate['path']}:{candidate['line']}:{candidate['column']} "
                f"{candidate['shape']} {candidate['head']}"
            )
        for reader in row["reader_references"]:
            lines.append(
                f"  {reader['path']}:{reader['line']} {reader['macro']} "
                f"{reader['resolution']} {reader['asset'] or '<dynamic>'}"
            )
        for notice in row["diagnostics"]:
            lines.append(
                f"  {notice['path']}:{notice['line']} {notice['kind']}: {notice['detail']}"
            )
    lines.extend("limit: " + limitation for limitation in report["limits"])
    return "\n".join(escape_terminal_text(line) for line in lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--revision", required=True, help="full 40-hex commit id")
    parser.add_argument(
        "--crate",
        action="append",
        required=True,
        dest="crates",
        help="crate directory; repeat explicitly",
    )
    parser.add_argument("--format", choices=("json", "text"), default="json")
    args = parser.parse_args(argv)
    try:
        tree = GitTree(args.repo, args.revision, args.crates)
        report = census(tree, tree.revision)
        sys.stdout.write(
            json.dumps(report, indent=2, sort_keys=True) + "\n"
            if args.format == "json"
            else text_report(report)
        )
    except (CensusError, OSError, UnicodeError) as error:
        print(escape_terminal_text(f"sql-source-census: {error}"), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
