#!/usr/bin/env python3
"""Check checked-in binding declarations against the latest numeric vMAJOR.MINOR.PATCH tag.

Python 3.11+, standard library only. Equal versions and patch bumps preserve existing
names. New exports and class/interface members are allowed. TypeScript and C
declarations must match exactly. A Python function may add defaulted positional
parameters after the existing ones, defaulted keyword-only parameters, or *args/
**kwargs that were not there before. Decorators, the return annotation, existing
parameters, and existing *args/**kwargs must stay the same. Reviewed exceptions live
in .github/api-compatibility-exceptions.json and apply only when the baseline tag
equals the entry's baseline. TypeScript aliases and C struct layouts are compared
conservatively as whole declarations. Python uses AST signatures (including defaults,
decorators and bases); TypeScript and cbindgen C use a small declaration parser, not
a type checker or ABI analyzer. Comments/formatting are ignored; equivalent type
rewrites may fail. Runtime behavior, conditional C preprocessing, overload
compatibility and binary layout are not proven. Unsupported declaration syntax fails
closed. Keep parsers/tests in sync with these three files; the generated C header's
freshness is checked separately by cbindgen.
"""

from __future__ import annotations

import argparse
import ast
from collections import Counter
import json
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
EXCEPTIONS = ".github/api-compatibility-exceptions.json"
EXCEPTION_KEYS = frozenset({"file", "name", "baseline", "reason"})
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


def python_items(text: str) -> dict[str, list[ast.AST]]:
    """Declaration nodes keyed by the qualified name the gate reports."""
    items: dict[str, list[ast.AST]] = {}

    def add_node(name: str, node: ast.AST) -> None:
        items.setdefault(name, []).append(node)

    def walk(nodes: list[ast.stmt], prefix: str = "") -> None:
        for node in nodes:
            if isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant):
                continue  # Docstrings and ellipsis bodies have no signature.
            if isinstance(node, ast.ClassDef):
                add_node(prefix + node.name, ast.ClassDef(
                    name=node.name, bases=node.bases, keywords=node.keywords,
                    body=[], decorator_list=node.decorator_list))
                walk(node.body, prefix + node.name + ".")
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                node.body = []
                add_node(prefix + node.name, node)
            elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
                add_node(prefix + node.target.id, node)
            elif isinstance(node, ast.Assign) and all(isinstance(target, ast.Name) for target in node.targets):
                for target in node.targets:
                    if isinstance(target, ast.Name):
                        add_node(prefix + target.id, node)
            elif isinstance(node, (ast.Import, ast.ImportFrom)):
                add_node(prefix + "import:" + ast.dump(node), node)
            elif isinstance(node, ast.Pass):
                continue
            else:
                raise CompatibilityError("Unsupported Python declaration")

    walk(ast.parse(text).body)
    if not items:
        raise CompatibilityError("Empty Python declarations")
    return items


def python_surface(text: str) -> Surface:
    return {name: [ast.dump(node) for node in nodes] for name, nodes in python_items(text).items()}


def dumped(node: ast.AST | None) -> str:
    return "" if node is None else ast.dump(node)


def positional_parameters(fn: ast.FunctionDef | ast.AsyncFunctionDef) -> list[tuple[str, str, str, str, str | None]]:
    """Positional parameters in source order, including those before `/`."""
    args = fn.args
    positional = [*args.posonlyargs, *args.args]
    defaults = list(args.defaults)
    first_default = len(positional) - len(defaults)
    kinds = ["posonly"] * len(args.posonlyargs) + ["arg"] * len(args.args)
    result: list[tuple[str, str, str, str, str | None]] = []
    for index, arg in enumerate(positional):
        default = defaults[index - first_default] if index >= first_default else None
        result.append((arg.arg, dumped(arg.annotation), dumped(default), kinds[index], arg.type_comment))
    return result


def keyword_parameters(fn: ast.FunctionDef | ast.AsyncFunctionDef) -> list[tuple[str, str, str, str | None]]:
    if len(fn.args.kwonlyargs) != len(fn.args.kw_defaults):
        raise CompatibilityError("Unsupported Python declaration")
    return [
        (arg.arg, dumped(arg.annotation), dumped(default), arg.type_comment)
        for arg, default in zip(fn.args.kwonlyargs, fn.args.kw_defaults)
    ]


def star_parameter(arg: ast.arg | None) -> tuple[str, str, str | None] | None:
    if arg is None:
        return None
    return arg.arg, dumped(arg.annotation), arg.type_comment


def function_compatible(old: ast.FunctionDef | ast.AsyncFunctionDef,
                        new: ast.FunctionDef | ast.AsyncFunctionDef) -> bool:
    """True when `new` is `old` plus defaulted parameters and optional new *args/**kwargs."""
    if type(old) is not type(new):
        return False
    if [dumped(item) for item in old.decorator_list] != [dumped(item) for item in new.decorator_list]:
        return False
    if dumped(old.returns) != dumped(new.returns) or old.type_comment != new.type_comment:
        return False
    old_types = [dumped(item) for item in getattr(old, "type_params", [])]
    new_types = [dumped(item) for item in getattr(new, "type_params", [])]
    if old_types != new_types:
        return False
    old_positional = positional_parameters(old)
    new_positional = positional_parameters(new)
    if new_positional[:len(old_positional)] != old_positional:
        return False
    if any(default == "" for _, _, default, _, _ in new_positional[len(old_positional):]):
        return False
    old_keywords = keyword_parameters(old)
    keyword_index = 0
    for parameter in keyword_parameters(new):
        if keyword_index < len(old_keywords) and parameter == old_keywords[keyword_index]:
            keyword_index += 1
        elif parameter[2] == "":
            return False
    if keyword_index != len(old_keywords):
        return False
    old_varargs = star_parameter(old.args.vararg)
    if old_varargs is not None and star_parameter(new.args.vararg) != old_varargs:
        return False
    old_kwargs = star_parameter(old.args.kwarg)
    return old_kwargs is None or star_parameter(new.args.kwarg) == old_kwargs


def node_compatible(old: ast.AST, new: ast.AST) -> bool:
    if isinstance(old, (ast.FunctionDef, ast.AsyncFunctionDef)) and isinstance(new, (ast.FunctionDef, ast.AsyncFunctionDef)):
        return function_compatible(old, new)
    return ast.dump(old) == ast.dump(new)


def nodes_compatible(old_nodes: list[ast.AST], new_nodes: list[ast.AST]) -> bool:
    def search(remaining_old: list[ast.AST], remaining_new: list[ast.AST]) -> bool:
        if not remaining_old:
            return True
        current, *rest = remaining_old
        return any(
            node_compatible(current, candidate) and search(rest, remaining_new[:index] + remaining_new[index + 1:])
            for index, candidate in enumerate(remaining_new)
        )

    return search(old_nodes, new_nodes)


def python_incompatibilities(path: str, before_text: str, after_text: str) -> list[str]:
    before = python_items(before_text)
    after = python_items(after_text)
    return [
        f"{path}: removed or changed {name}"
        for name, old_nodes in before.items()
        if not nodes_compatible(old_nodes, after.get(name, []))
    ]


def release_tag(value: str) -> bool:
    return value.startswith("v") and VERSION.fullmatch(value[1:]) is not None


def load_exceptions(root: Path) -> list[dict[str, str]]:
    path = root / EXCEPTIONS
    if not path.is_file():
        return []
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise CompatibilityError("Malformed API compatibility exceptions") from error
    if not isinstance(data, list):
        raise CompatibilityError("Malformed API compatibility exceptions")
    entries: list[dict[str, str]] = []
    seen: set[tuple[str, str, str]] = set()
    for item in data:
        if not isinstance(item, dict):
            raise CompatibilityError("Malformed API compatibility exceptions")
        keys = set(item)
        if not keys <= EXCEPTION_KEYS:
            raise CompatibilityError("Unknown API compatibility exception field")
        if keys != EXCEPTION_KEYS:
            raise CompatibilityError("Malformed API compatibility exceptions")
        values: dict[str, str] = {}
        for key in ("file", "name", "baseline", "reason"):
            value = item[key]
            if not isinstance(value, str) or value.strip() == "":
                raise CompatibilityError("Malformed API compatibility exceptions")
            values[key] = value
        if not release_tag(values["baseline"]):
            raise CompatibilityError("Malformed API compatibility exception baseline")
        identity = (values["file"], values["name"], values["baseline"])
        if identity in seen:
            raise CompatibilityError("Duplicate API compatibility exception")
        seen.add(identity)
        entries.append(values)
    return entries


def apply_exceptions(failures: list[str], entries: list[dict[str, str]], baseline: str) -> list[str]:
    """Drop differences named for this baseline, and report entries that name nothing."""
    applicable = [entry for entry in entries if entry["baseline"] == baseline]
    consumed = [False] * len(applicable)
    remaining: list[str] = []
    for failure in failures:
        match = next((
            index for index, entry in enumerate(applicable)
            if not consumed[index] and failure == f"{entry['file']}: removed or changed {entry['name']}"
        ), None)
        if match is None:
            remaining.append(failure)
        else:
            consumed[match] = True
    for entry, used in zip(applicable, consumed):
        if not used:
            remaining.append(f"Stale API compatibility exception: {entry['file']} {entry['name']}")
    return remaining


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
    if not release_tag(tag):
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
    entries = load_exceptions(root)
    checked = 0
    failures: list[str] = []
    for path, parse in zip(FILES, (typescript_surface, python_surface, c_surface)):
        before_text = git(root, "show", f"{commit}:{path}")
        after_text = (root / path).read_text(encoding="utf-8")
        before = parse(before_text)
        after = parse(after_text)
        checked += len(before)
        if path == FILES[1]:
            failures.extend(python_incompatibilities(path, before_text, after_text))
        else:
            for name, signatures in before.items():
                if Counter(signatures) - Counter(after.get(name, [])):
                    failures.append(f"{path}: removed or changed {name}")
    failures = apply_exceptions(failures, entries, tag)
    if not patch:
        # Minor and major releases may change declarations. A named exception still
        # has to match a real difference against this baseline, or it is stale.
        failures = [item for item in failures if item.startswith("Stale API compatibility exception:")]
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
