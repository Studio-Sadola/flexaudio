#!/usr/bin/env python3
"""Check checked-in binding declarations against the latest numeric vMAJOR.MINOR.PATCH tag.

Python 3.11+, standard library only. Equal versions and patch bumps preserve existing
names/signatures; new exports and class/interface members are allowed. TypeScript
aliases and C struct layouts are compared conservatively as whole declarations.
Python uses AST signatures (including defaults, decorators and bases); TypeScript
and cbindgen C use a small declaration parser, not a type checker or ABI analyzer.
Comments/formatting are ignored; equivalent type rewrites may fail. Runtime behavior,
conditional C preprocessing, overload compatibility and binary layout are not proven.
Unsupported declaration syntax fails closed. Keep parsers/tests in sync with these
three files; the generated C header's freshness is checked separately by cbindgen.
"""

from __future__ import annotations

import argparse
import ast
from collections import Counter
from pathlib import Path
import re
import subprocess
import sys
import tomllib


FILES = (
    "crates/flexaudio-napi/index.d.ts",
    "bindings/flexaudio-py/flexaudio.pyi",
    "crates/flexaudio-ffi/include/flexaudio.h",
)
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
TOKENS = re.compile(r'''"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[A-Za-z_$][\w$]*|[0-9]+|[^\s]''')
COMMENTS = re.compile(r'''("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')|/\*.*?\*/|//[^\n]*''', re.S)
Surface = dict[str, list[str]]


class CompatibilityError(Exception):
    """Missing baseline, unsupported syntax, or incompatible declarations."""


def version(value: object) -> tuple[int, int, int]:
    if not isinstance(value, str) or VERSION.fullmatch(value) is None:
        raise CompatibilityError("Expected a numeric MAJOR.MINOR.PATCH version")
    major, minor, patch = value.split(".")
    return int(major), int(minor), int(patch)


def workspace_version(text: str) -> tuple[int, int, int]:
    try:
        return version(tomllib.loads(text)["workspace"]["package"]["version"])
    except (KeyError, TypeError, ValueError) as error:
        raise CompatibilityError("Invalid workspace.package.version") from error


def git(root: Path, *args: str) -> str:
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True, check=False)
    if result.returncode:
        raise CompatibilityError(f"Cannot read release baseline with git {args[0]}")
    return result.stdout


def latest_tag(root: Path) -> str:
    tags = [tag for tag in git(root, "tag", "--list", "v*").splitlines()
            if VERSION.fullmatch(tag[1:]) is not None]
    if not tags:
        raise CompatibilityError("No release tags found; fetch the complete release tag history")
    return max(tags, key=lambda tag: version(tag[1:]))


def clean(text: str) -> str:
    return COMMENTS.sub(lambda match: match.group(1) or " ", text)


def normalized(text: str) -> str:
    return " ".join(TOKENS.findall(text))


def add(surface: Surface, name: str, signature: str) -> None:
    surface.setdefault(name, []).append(signature)


def members(text: str) -> list[str]:
    """Split the current TS member syntax at top-level newlines/semicolons."""
    result: list[str] = []
    depth = 0
    start = 0
    for match in re.finditer(TOKENS.pattern + r"|\n", text):
        token = match.group()
        if token in ("(", "[", "{"):
            depth += 1
        elif token in (")", "]", "}"):
            depth -= 1
        elif token in (";", "\n") and depth == 0:
            if token == "\n" and (
                text[match.end():].lstrip().startswith(("|", "&"))
                or text[start:match.start()].rstrip().endswith(("|", "&", ":", "=", ",", "=>"))
            ):
                continue
            result.append(text[start:match.start()].strip())
            start = match.end()
        if depth < 0:
            raise CompatibilityError("Unbalanced declaration")
    if depth:
        raise CompatibilityError("Unbalanced declaration")
    if text[start:].strip():
        result.append(text[start:].strip().rstrip(";"))
    return [item for item in result if item]


def typescript_surface(text: str) -> Surface:
    text = clean(text)
    exports = list(re.finditer(r"\bexport\s+(?:declare\s+)?(interface|class|type|function|const)\s+(\w+)", text))
    surface: Surface = {}
    end = 0
    for index, match in enumerate(exports):
        if text[end:match.start()].strip():
            raise CompatibilityError("Unsupported TypeScript declaration")
        end = exports[index + 1].start() if index + 1 < len(exports) else len(text)
        declaration = text[match.start():end].strip().rstrip(";")
        kind, name = match.groups()
        if kind in ("class", "interface"):
            opening, closing = declaration.find("{"), declaration.rfind("}")
            if opening < 0 or closing < opening or declaration[closing + 1:].strip():
                raise CompatibilityError(f"Unsupported TypeScript body: {name}")
            add(surface, name, normalized(declaration[:opening]))
            for member in members(declaration[opening + 1:closing]):
                member_name = re.match(r"(?:(?:static|readonly|public|protected|private|abstract)\s+)*(\w+)\s*(?:[?:=(<])", member)
                if member_name is None:
                    raise CompatibilityError(f"Unsupported TypeScript member: {name}")
                add(surface, f"{name}.{member_name[1]}", normalized(member))
        else:
            add(surface, name, normalized(declaration))
    if not exports or text[end:].strip():
        raise CompatibilityError("Unsupported or empty TypeScript declarations")
    return surface


def python_surface(text: str) -> Surface:
    surface: Surface = {}

    def walk(nodes: list[ast.stmt], prefix: str = "") -> None:
        for node in nodes:
            if isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant):
                continue  # Docstrings and ellipsis bodies have no signature.
            if isinstance(node, ast.ClassDef):
                add(surface, prefix + node.name, ast.dump(ast.ClassDef(
                    name=node.name, bases=node.bases, keywords=node.keywords,
                    body=[], decorator_list=node.decorator_list)))
                walk(node.body, prefix + node.name + ".")
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                node.body = []
                add(surface, prefix + node.name, ast.dump(node))
            elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
                add(surface, prefix + node.target.id, ast.dump(node))
            elif isinstance(node, ast.Assign) and all(isinstance(target, ast.Name) for target in node.targets):
                for target in node.targets:
                    if isinstance(target, ast.Name):
                        add(surface, prefix + target.id, ast.dump(node))
            elif isinstance(node, (ast.Import, ast.ImportFrom)):
                add(surface, prefix + "import:" + ast.dump(node), ast.dump(node))
            elif isinstance(node, ast.Pass):
                continue
            else:
                raise CompatibilityError("Unsupported Python declaration")

    walk(ast.parse(text).body)
    if not surface:
        raise CompatibilityError("Empty Python declarations")
    return surface


def c_surface(text: str) -> Surface:
    surface: Surface = {}
    text = clean(text)
    for match in re.finditer(r"^[ \t]*#define[ \t]+(\w+)[ \t]+([^\n]+)", text, re.M):
        add(surface, match[1], normalized(match[2]))
    text = re.sub(r"^\s*#.*$", "", text, flags=re.M)
    # Each cbindgen declaration ends in a semicolon, except enum/struct bodies.
    declarations = members(text.replace("\n", " "))
    for declaration in declarations:
        enum = re.fullmatch(r"(?:typedef\s+)?enum\s+(\w+)\s*\{(.*)\}\s*(\w+)?", declaration, re.S)
        if enum:
            name = enum[3] or enum[1]
            add(surface, name, "enum " + enum[1])
            previous = "-1"
            for item in enum[2].split(","):
                if not item.strip():
                    continue
                value = re.fullmatch(r"\s*(\w+)\s*(?:=\s*(.+))?", item)
                if value is None:
                    raise CompatibilityError("Unsupported C enum value")
                previous = normalized(value[2]) if value[2] else f"({previous}) + 1"
                add(surface, f"{name}.{value[1]}", previous)
        elif declaration.startswith("typedef "):
            name = re.search(r"\b(\w+)\s*$", declaration)
            if name is None:
                raise CompatibilityError("Unsupported C typedef")
            add(surface, name[1], normalized(declaration))
        else:
            function = re.fullmatch(r"([\w\s*]+?)\b(\w+)\s*\(([^{}]*)\)", declaration, re.S)
            if function is None:
                raise CompatibilityError("Unsupported C declaration")
            add(surface, function[2], normalized(declaration))
    if not surface:
        raise CompatibilityError("Empty C declarations")
    return surface


def check(root: Path, tag: str | None = None) -> tuple[str, str, int]:
    tag = tag if tag is not None else latest_tag(root)
    if not tag.startswith("v"):
        raise CompatibilityError("Baseline tag must be vMAJOR.MINOR.PATCH")
    baseline_version = version(tag[1:])
    commit = git(root, "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}").strip()
    if workspace_version(git(root, "show", f"{commit}:Cargo.toml")) != baseline_version:
        raise CompatibilityError("Release tag and baseline workspace version differ")
    current_version = workspace_version((root / "Cargo.toml").read_text(encoding="utf-8"))
    if current_version < baseline_version:
        raise CompatibilityError("Workspace version is older than the latest release")
    current_label = ".".join(map(str, current_version))
    patch = current_version[:2] == baseline_version[:2]
    checked = 0
    failures: list[str] = []
    for path, parse in zip(FILES, (typescript_surface, python_surface, c_surface)):
        before = parse(git(root, "show", f"{commit}:{path}"))
        after = parse((root / path).read_text(encoding="utf-8"))
        checked += len(before)
        if patch:
            for name, signatures in before.items():
                if Counter(signatures) - Counter(after.get(name, [])):
                    failures.append(f"{path}: removed or changed {name}")
    if failures:
        raise CompatibilityError("\n".join(failures))
    return tag, current_label, checked


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--baseline-tag", help="Explicit numeric release tag (default: latest fetched tag)")
    args = parser.parse_args(argv)
    try:
        tag, current, count = check(args.repo_root, args.baseline_tag)
    except (CompatibilityError, OSError, UnicodeError, SyntaxError) as error:
        print(f"API compatibility check failed: {error}", file=sys.stderr)
        return 1
    print(f"API compatibility check passed: {tag} -> {current} ({count} baseline names)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
