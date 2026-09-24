#!/usr/bin/env python3
"""Gate that checks every pinned toolchain version in ci.yml agrees on a single version.

Why this exists
---------------
GitHub Actions `uses:` accepts neither expressions nor environment variables (primary
source: the GitHub Docs "Context availability" table has no row for
`jobs.<job_id>.steps.uses`, while `steps.with` has one). So the toolchain version has to
be written directly into the ref of `uses: dtolnay/rust-toolchain@<version>`, and the same
version appears many times in ci.yml. If the copies disagree, the result is a silent
drift where "some jobs passed the gate on a different version", and since CI stays green
nobody notices. This gate turns that disagreement red.

Rules
-----
1. Collect every `uses: dtolnay/rust-toolchain@<numeric version>` in ci.yml.
2. Exclude MSRV jobs (= jobs that build with the rust-version a crate declares), since
   they are supposed to use a different version. What counts as an MSRV job is decided
   by the declared rust-versions taken from `cargo metadata --no-deps`, not by a list of
   job names, so the exclusion rule keeps working as more crates declare an MSRV or more
   jobs are added.
3. Fail if the remaining pins (= the gate jobs) name two or more versions.
4. Fail if a job calls cargo / rustc in `run:` but declares no toolchain at all (do not
   allow a path that silently uses whatever Rust the runner ships with). This checks
   *whether* a toolchain is declared, not which version: pointing at stable on purpose,
   like the `@stable` advance-warning job does, is fine.

Why no YAML library
-------------------
On GitHub's ubuntu runners yamllint is installed via pipx (an isolated venv), and apt has
no python3-yaml either (primary sources: actions/runner-images `Ubuntu2404-Readme.md` and
`toolsets/toolset-2404.json`). So `import yaml` fails on a bare python3. A gate that
breaks depending on the environment defeats its own purpose, so this is a line scanner
that runs on the stdlib alone (see the assumptions below).

Scanning assumptions (best effort; when they do not hold, fail rather than silently pass)
-----------------------------------------------------------------------------------------
- Scanning starts on the line after `jobs:`. A `key:` indented by two spaces is a job id.
- A `uses:` line has the form `[ - ] uses: <ref>` (a trailing comment is allowed).
- Only cargo / rustc calls that appear literally on a line are seen. Lines starting with
  `#` and `name:` lines are skipped (to avoid false positives from comments and
  descriptions). Calls made through a script are invisible.
- Fail if no numeric-version `uses:` is found at all (= a broken scan turns red).

Usage:
    python3 .github/scripts/check-toolchain-pins.py [workflow.yml]

Exit code: 0 = OK / 1 = mismatch found
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# Only `uses: dtolnay/rust-toolchain@<ref>` is examined. Other actions are out of scope.
PINNED_ACTION = "dtolnay/rust-toolchain"
USES_RE = re.compile(r"^\s*(?:-\s*)?uses:\s*([^\s#]+)\s*(?:#.*)?$")
# A key directly under jobs: (indented by two spaces) = a job id.
JOB_KEY_RE = re.compile(r"^  ([A-Za-z0-9_-]+):\s*$")
# A job display name (indented by four spaces; a step name: is indented by six, so it does
# not match).
JOB_NAME_RE = re.compile(r"^    name:\s*(.+?)\s*$")
JOBS_KEY_RE = re.compile(r"^jobs:\s*$")
STEP_NAME_RE = re.compile(r"^\s*(?:-\s*)?name:")
COMMENT_RE = re.compile(r"^\s*#")
# Only numeric versions count as "pinned". @stable / @master / a commit SHA are not pins.
VERSION_RE = re.compile(r"^(\d+)\.(\d+)(?:\.(\d+))?$")
# A cargo / rustc call that appears literally on a line (requires trailing whitespace to
# reduce false positives).
CARGO_CALL_RE = re.compile(r"(?:^|[\s;&|(){}])(?:cargo|rustc)\s")


@dataclass(frozen=True)
class Pin:
    job_id: str
    version: str


def normalize_version(raw: str) -> str | None:
    """Normalize '1.98.1' / '1.91' to a comparable 'X.Y.Z'. None for a non-numeric version."""
    match = VERSION_RE.match(raw.strip())
    if match is None:
        return None
    major, minor, patch = match.group(1), match.group(2), match.group(3) or "0"
    return f"{int(major)}.{int(minor)}.{int(patch)}"


def declared_msrv(repo_root: Path) -> set[str]:
    """The set of rust-versions declared by the workspace crates.

    Globbing `crates/*` would miss the crates under bindings/, so this uses the actual
    data from cargo metadata.
    """
    try:
        completed = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=repo_root,
            capture_output=True,
            text=True,
            check=True,
        )
    except FileNotFoundError:
        sys.exit("cargo not found. Install a toolchain and run this again.")
    except subprocess.CalledProcessError as error:
        sys.exit(f"cargo metadata failed:\n{error.stderr}")

    versions: set[str] = set()
    for package in json.loads(completed.stdout)["packages"]:
        raw = package.get("rust_version")
        if raw is None:
            continue
        normalized = normalize_version(raw)
        if normalized is None:
            sys.exit(f"Unexpected rust-version format: {package['name']} = {raw!r}")
        versions.add(normalized)
    return versions


def scan_workflow(path: Path) -> tuple[list[Pin], set[str], set[str], dict[str, str]]:
    """(list of pins, jobs that call cargo, jobs that declare a toolchain, job display names)."""
    pins: list[Pin] = []
    cargo_jobs: set[str] = set()
    declaring_jobs: set[str] = set()
    job_names: dict[str, str] = {}

    in_jobs = False
    job_id = "(outside jobs:)"
    for line in path.read_text(encoding="utf-8").splitlines():
        if not in_jobs:
            in_jobs = JOBS_KEY_RE.match(line) is not None
            continue

        job_key = JOB_KEY_RE.match(line)
        if job_key is not None:
            job_id = job_key.group(1)
            continue

        job_name = JOB_NAME_RE.match(line)
        if job_name is not None and job_id not in job_names:
            job_names[job_id] = job_name.group(1).strip().strip("\"'")
            continue

        # Comments and step names are descriptions, not what gets executed.
        if COMMENT_RE.match(line) or STEP_NAME_RE.match(line):
            continue

        uses = USES_RE.match(line)
        if uses is not None:
            ref = uses.group(1).strip().strip("\"'")
            if ref.startswith(f"{PINNED_ACTION}@"):
                declaring_jobs.add(job_id)
                version = normalize_version(ref.split("@", 1)[1])
                if version is not None:
                    pins.append(Pin(job_id, version))
            continue

        if CARGO_CALL_RE.search(line):
            cargo_jobs.add(job_id)

    return pins, cargo_jobs, declaring_jobs, job_names


def render_table(pins: list[Pin], msrv: set[str], job_names: dict[str, str]) -> str:
    width = max((len(pin.job_id) for pin in pins), default=4)
    lines = []
    for pin in pins:
        kind = "MSRV (excluded)" if pin.version in msrv else "gate"
        name = job_names.get(pin.job_id, "")
        suffix = f"  ({name})" if name else ""
        lines.append(f"  {pin.job_id:<{width}}  {pin.version}  {kind}{suffix}")
    return "\n".join(lines)


def main() -> int:
    repo_root = Path(__file__).resolve().parents[2]
    workflow_path = (
        Path(sys.argv[1]) if len(sys.argv) > 1 else repo_root / ".github" / "workflows" / "ci.yml"
    )

    msrv = declared_msrv(repo_root)
    pins, cargo_jobs, declaring_jobs, job_names = scan_workflow(workflow_path)

    print(f"workflow: {workflow_path}")
    print(f"Declared MSRV (cargo metadata --no-deps): {', '.join(sorted(msrv)) or 'none'}")
    if not pins:
        print(
            f"NG: no pinned version (numeric ref) of {PINNED_ACTION} was found."
            " Either the scan is broken or the pin was removed.",
            file=sys.stderr,
        )
        return 1
    print("Pinned versions:")
    print(render_table(pins, msrv, job_names))

    gate_versions = sorted({pin.version for pin in pins if pin.version not in msrv})
    used_versions = sorted({pin.version for pin in pins})

    if len(gate_versions) > 1:
        print(
            "\nNG: the gate jobs' pinned toolchain versions disagree: "
            f"{', '.join(gate_versions)}\n"
            "    Run every gate on the same version (if only the Linux side is newer, the same"
            " mistake is caught on one platform and missed on another).",
            file=sys.stderr,
        )
        return 1

    if not gate_versions:
        if len(used_versions) > 1:
            print(
                "\nNG: the gate pins have the same value as a rust-version declared by a"
                f" crate (pins: {', '.join(used_versions)} / declared MSRV: "
                f"{', '.join(sorted(msrv))}).\n"
                "    In this state the version value alone cannot tell \"gate job pins\" from"
                " \"MSRV job pins\", so disagreement between gates cannot be detected.\n"
                "    Either move the gate version off the declared MSRVs, or pin everything"
                " to the same version.",
                file=sys.stderr,
            )
            return 1
        print("\nOK: there is only one pinned version (all equal to a declared MSRV).")
        return 0

    undeclared = sorted(cargo_jobs - declaring_jobs)
    if undeclared:
        print(
            "\nNG: some jobs call cargo / rustc without declaring a toolchain: "
            f"{', '.join(undeclared)}\n"
            "    Such a job silently uses whatever Rust the runner ships with, so an upstream"
            " update can turn it red without warning. Add `uses: "
            f"{PINNED_ACTION}@<gate version>`.",
            file=sys.stderr,
        )
        return 1

    gate_count = sum(1 for pin in pins if pin.version not in msrv)
    print(
        f"\nOK: the gates' pinned toolchain version is consistently {gate_versions[0]}"
        f" ({gate_count} gate pins / {len(pins) - gate_count} excluded as MSRV)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
