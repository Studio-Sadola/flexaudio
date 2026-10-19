#!/usr/bin/env python3
"""Validate capability claims and generate docs/binding-matrix.md; standard library only."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import sys

from binding_matrix_surfaces import compat, surfaces


FRONTENDS = ("Rust", "Node", "Python", "C", "CLI")
DATA = ".github/scripts/binding_matrix.json"
DOC = "docs/binding-matrix.md"


class MatrixError(Exception):
    """Invalid matrix data, unverified claims, or stale documentation."""


@dataclass(frozen=True)
class Supported:
    symbols: tuple[str, ...]
    note: str


@dataclass(frozen=True)
class Unavailable:
    status: str
    reason: str


@dataclass(frozen=True)
class Feature:
    name: str
    bindings: dict[str, Supported | Unavailable]


def line(value: object, context: str) -> str:
    if not isinstance(value, str) or not value.strip() or "\n" in value or "\r" in value:
        raise MatrixError(f"{context}: expected one nonempty line")
    return value


def validate(data: object, indexes: dict[str, set[str]]) -> list[Feature]:
    if not isinstance(data, dict) or set(data) != {"features"}:
        raise MatrixError("Expected a matrix object containing features")
    rows = data["features"]
    if not isinstance(rows, list) or not rows:
        raise MatrixError("Expected a nonempty features list")
    seen: set[str] = set()
    features: list[Feature] = []
    for row in rows:
        if not isinstance(row, dict) or set(row) != {"feature", "bindings"}:
            raise MatrixError("Each row needs a one-line feature and bindings")
        feature = line(row["feature"], "Feature")
        if feature in seen:
            raise MatrixError(f"Duplicate feature: {feature}")
        seen.add(feature)
        cells = row["bindings"]
        if not isinstance(cells, dict) or set(cells) != set(FRONTENDS):
            raise MatrixError(f"{feature}: expected all five front ends")
        bindings: dict[str, Supported | Unavailable] = {}
        for frontend, cell in cells.items():
            context = f"{feature} / {frontend}"
            if not isinstance(cell, dict):
                raise MatrixError(f"{context}: expected a cell object")
            status = cell.get("status")
            if status == "supported":
                if not {"status", "symbols"} <= set(cell) or set(cell) - {"status", "symbols", "note"}:
                    raise MatrixError(f"{context}: supported cells need symbols and an optional note")
                symbols = cell["symbols"]
                if not isinstance(symbols, list) or not symbols:
                    raise MatrixError(f"{context}: expected concrete symbols")
                names = tuple(line(symbol, context + " symbol") for symbol in symbols)
                for symbol in names:
                    if symbol not in indexes[frontend]:
                        raise MatrixError(f"{context}: unknown symbol {symbol}")
                note = line(cell["note"], context + " note") if "note" in cell else ""
                bindings[frontend] = Supported(names, note)
            elif status in ("missing", "not-applicable"):
                if set(cell) != {"status", "reason"}:
                    raise MatrixError(f"{context}: {status} cells need a one-line reason")
                bindings[frontend] = Unavailable(status, line(cell["reason"], context + " reason"))
            else:
                raise MatrixError(f"{context}: unknown status")
        features.append(Feature(feature, bindings))
    return features


def escape(text: str) -> str:
    return text.replace("|", "\\|")


def render(rows: list[Feature]) -> str:
    lines = [
        "# Binding capability matrix",
        "",
        "Generated from [binding_matrix.json](../.github/scripts/binding_matrix.json).",
        "Edit that file, then run `python3 .github/scripts/binding_matrix.py`.",
        "CI runs the same command with `--check` to reject stale documentation.",
        "",
        "Each supported cell names declarations checked in the corresponding front end.",
        "Rust includes the facade and the separate public add-on crates named below.",
        "CLI diagnostic symbols identify the implementation behind stderr reporting.",
        "Checks establish symbol presence, not runtime behavior, ABI compatibility or OS availability;",
        "use the capability snapshot for platform limits. Missing means an unexposed feature;",
        "not-applicable means the interface has no corresponding operation.",
        "This is a curated feature inventory, not an exhaustive public-symbol inventory.",
        "",
        "| Feature | " + " | ".join(FRONTENDS) + " |",
        "|---|" + "---|" * len(FRONTENDS),
    ]
    for row in rows:
        cells = []
        for frontend in FRONTENDS:
            cell = row.bindings[frontend]
            if isinstance(cell, Supported):
                text = "Supported: " + ", ".join(f"`{symbol}`" for symbol in cell.symbols)
                if cell.note:
                    text += "; " + cell.note
            else:
                text = cell.status + ": " + cell.reason
            cells.append(escape(text))
        lines.append("| " + escape(row.name) + " | " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


def run(root: Path, check: bool) -> int:
    data = json.loads((root / DATA).read_text(encoding="utf-8"))
    rows = validate(data, surfaces(root))
    expected = render(rows)
    doc = root / DOC
    if check:
        if not doc.is_file() or doc.read_bytes() != expected.encode("utf-8"):
            raise MatrixError("Stale docs/binding-matrix.md; run python3 .github/scripts/binding_matrix.py")
    else:
        doc.parent.mkdir(parents=True, exist_ok=True)
        doc.write_text(expected, encoding="utf-8", newline="\n")
    return len(rows)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--check", action="store_true", help="Fail instead of updating a stale document")
    args = parser.parse_args()
    try:
        count = run(args.repo_root, args.check)
    except (MatrixError, compat.CompatibilityError, OSError, ValueError, SyntaxError) as error:
        print(f"Binding matrix check failed: {error}", file=sys.stderr)
        return 1
    print(f"Binding matrix check passed: {count} features, {count * len(FRONTENDS)} cells; docs/binding-matrix.md is current")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
