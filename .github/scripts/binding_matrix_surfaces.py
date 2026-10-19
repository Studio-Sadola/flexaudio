"""Declaration indexes for the binding matrix (no build or audio devices needed).

Reuse the compatibility gate's Node/Python/C parsers. Extend their indexes with
Python parameters and C struct fields. Rust indexing covers public declarations,
fields, variants and inherent methods in explicitly mapped facade/add-on modules;
it checks root re-exports, but is not a Rust type checker or cfg evaluator. CLI
indexing covers Clap long flags, source enum values and diagnostic entry points.
"""

from __future__ import annotations

import ast
import importlib.util
from pathlib import Path
import re


SPEC = importlib.util.spec_from_file_location(
    "api_compatibility", Path(__file__).with_name("check-api-compatibility.py"))
if SPEC is None or SPEC.loader is None:
    raise ImportError("Cannot load API compatibility parsers")
compat = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compat)

RUST_LITERAL = r'''r(?P<hashes>\#+)".*?"(?P=hashes)|r"[^"]*"|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])' '''.strip()
RUST_LEXEME = re.compile(RUST_LITERAL + r"|/\*.*?\*/|//[^\n]*", re.S)


def block(text: str, opening: int) -> str:
    depth = 1
    for index in range(opening + 1, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return text[opening + 1:index]
    raise ValueError("Unbalanced Rust declaration")


def depth_at(text: str, position: int) -> int:
    prefix = text[:position]
    return prefix.count("{") - prefix.count("}")


def rust_text(path: Path) -> str:
    # Preserve strings for Clap attributes, but hide their braces from indexing.
    # Rust lifetimes must not be mistaken for the compatibility parser's quoted C/TS strings.
    return RUST_LEXEME.sub(
        lambda match: " " if match[0].startswith(("//", "/*"))
        else match[0].replace("{", " ").replace("}", " "),
        path.read_text(encoding="utf-8"))


def rust_code(text: str) -> str:
    return re.sub(RUST_LITERAL, lambda match: re.sub(r"[^\n]", " ", match[0]), text, flags=re.S)


def rust_declarations(text: str) -> set[str]:
    text = rust_code(text)
    names: set[str] = set()
    for match in re.finditer(r"\bpub\s+(?:async\s+)?fn\s+(\w+)", text):
        if depth_at(text, match.start()) == 0:
            names.add(match[1])
    for match in re.finditer(r"\bpub\s+(struct|enum)\s+(\w+)[^{;]*\{", text):
        if depth_at(text, match.start()) != 0:
            continue
        kind, name = match.groups()
        names.add(name)
        body = block(text, match.end() - 1)
        pattern = r"\bpub\s+(\w+)\s*:" if kind == "struct" else r"^\s*(\w+)\s*(?:[,=({])"
        for member in re.finditer(pattern, body, re.M):
            if depth_at(body, member.start()) == 0:
                names.add(f"{name}::{member[1]}")
    # Only inherent impls; trait methods and private methods are not public API.
    for match in re.finditer(r"\bimpl\s+(\w+)\s*\{", text):
        if depth_at(text, match.start()) != 0:
            continue
        body = block(text, match.end() - 1)
        for method in re.finditer(r"\bpub\s+(?:async\s+)?fn\s+(\w+)", body):
            if depth_at(body, method.start()) == 0:
                names.add(f"{match[1]}::{method[1]}")
    return names


def reexports(text: str) -> set[str]:
    text = rust_code(text)
    names: set[str] = set()
    for match in re.finditer(r"\bpub\s+use\s+([^;]+);", text):
        if depth_at(text, match.start()) != 0:
            continue
        declaration = match[1]
        if "{" in declaration:
            names.update(item.strip().split("::")[-1]
                         for item in declaration.split("{", 1)[1].split("}", 1)[0].split(",")
                         if item.strip())
        else:
            names.add(declaration.split("::")[-1].strip())
    return names


def rust_surface(root: Path) -> set[str]:
    # Explicit module map keeps private implementation types out of the index.
    modules = {
        "flexaudio": (
            "crates/flexaudio/src/lib.rs",
            "crates/flexaudio/src/capabilities.rs",
            "crates/flexaudio/src/processes.rs",
            "crates/flexaudio/src/open_options.rs",
            "crates/flexaudio/src/stream.rs",
            "crates/flexaudio/src/stream/delivery.rs",
            "crates/flexaudio/src/stream/source.rs",
            "crates/flexaudio/src/stream/lifecycle.rs",
            "crates/flexaudio-core/src/types.rs",
        ),
        "flexaudio_vad": (
            "crates/flexaudio-vad/src/lib.rs",
            "crates/flexaudio-vad/src/tap.rs",
            "crates/flexaudio-vad/src/stream.rs",
        ),
        "flexaudio_denoise": ("crates/flexaudio-denoise/src/lib.rs",),
        "flexaudio_encode": (
            "crates/flexaudio-encode/src/lib.rs",
            "crates/flexaudio-encode/src/writer.rs",
        ),
    }
    surface: set[str] = set()
    for crate, paths in modules.items():
        facade = rust_text(root / paths[0])
        exported = rust_declarations(facade) | reexports(facade)
        for path in paths:
            for name in rust_declarations(rust_text(root / path)):
                if name.split("::", 1)[0] in exported:
                    surface.add(f"{crate}::{name}")
    return surface


def cli_surface(root: Path) -> set[str]:
    text = rust_text(root / "crates/flexaudio-cli/src/main.rs")
    cli = re.search(r"\bstruct Cli\s*\{", text)
    if cli is None:
        raise ValueError("Cannot find CLI argument declarations")
    body = block(text, cli.end() - 1)
    surface: set[str] = set()
    for field in re.finditer(r"#\[arg\((.*?)\)\]\s*(\w+)\s*:", body, re.S):
        long = re.search(r'\blong\s*(?:=\s*"([\w-]+)")?(?=\s*[,)]|\s*$)', field[1])
        if long is None:
            raise ValueError("Unsupported CLI argument declaration")
        surface.add("--" + (long[1] or field[2].replace("_", "-")))
    source = re.search(r"\benum SourceArg\s*\{", text)
    if source is None:
        raise ValueError("Cannot find CLI source declarations")
    for variant in re.findall(r"^\s*(\w+)\s*,", block(text, source.end() - 1), re.M):
        surface.add("--source=" + variant.lower())
    # Diagnostics have no dedicated flag; name the concrete reporting implementation.
    diagnostics = rust_text(root / "crates/flexaudio-cli/src/capture_events.rs")
    if re.search(r"pub\(super\) fn drain_capture_events\s*\(", diagnostics):
        surface.add("capture_events::drain_capture_events")
    return surface


def surfaces(root: Path) -> dict[str, set[str]]:
    node_names = compat.typescript_surface((root / compat.FILES[0]).read_text(encoding="utf-8"))
    python = compat.python_items((root / compat.FILES[1]).read_text(encoding="utf-8"))
    python_names = set(python)
    for name, nodes in python.items():
        for node in nodes:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                for arg in (*node.args.posonlyargs, *node.args.args, *node.args.kwonlyargs):
                    python_names.add(f"{name}.{arg.arg}")
    c = compat.c_surface((root / compat.FILES[2]).read_text(encoding="utf-8"))
    c_names = set(c)
    for name, signatures in c.items():
        for signature in signatures:
            struct = re.fullmatch(r"typedef struct \w+ \{ (.*) \} \w+", signature)
            if struct:
                for field in struct[1].split(";"):
                    member = re.search(r"\b(\w+)\s*$", field.strip())
                    if member:
                        c_names.add(f"{name}.{member[1]}")
    return {"Rust": rust_surface(root), "Node": set(node_names), "Python": python_names,
            "C": c_names, "CLI": cli_surface(root)}
