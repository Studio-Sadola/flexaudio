#!/usr/bin/env python3
"""CI gate that checks whether pinned toolchain versions in ci.yml are consistent.

Why this is needed
----------
Expressions and environment variables are not supported in GitHub Actions `uses:` (primary
source: the GitHub Docs "Context availability" table has no `jobs.<job_id>.steps.uses` row, but
does have `steps.with`). Therefore the toolchain version must be written directly in the
`uses: dtolnay/rust-toolchain@<version>` ref, and the same version appears repeatedly in
ci.yml. If copies drift, some jobs may silently pass the gate with another version while CI
stays green. This gate reports such mismatches as failures.

Rules
----
1. Collect every `uses: dtolnay/rust-toolchain@<numeric version>` in ci.yml.
2. Exclude MSRV jobs (jobs that build with the rust-version declared by a crate), since they
   should use a different version. Identify them by declared rust-version from
   `cargo metadata --no-deps`, not by a list of job names. This keeps the exclusion rule valid
   as crates declaring an MSRV or jobs are added.
3. Fail if the remaining (gate) jobs use two or more pinned versions.
4. Fail if a job runs cargo / rustc through `run:` but declares no toolchain. This prevents
   silently using the Rust preinstalled on the runner. This checks for a declaration, not its
   version: intentionally using stable, as in the early-warning `@stable` job, is allowed.

Why no YAML library is used
--------------------------------
yamllint on GitHub's Ubuntu runners is installed with pipx (in an isolated venv), and
python3-yaml is not available through apt (primary sources: actions/runner-images
`Ubuntu2404-Readme.md` and `toolsets/toolset-2404.json`). Therefore `import yaml` does not work
with the system Python. Since making the gate environment-dependent would defeat its purpose,
this uses a line scanner built with the standard library only (see assumptions below).

Scanner assumptions (best effort; fail rather than silently miss an unexpected format)
-------------------------------------------------------------
- Scan from the line after `jobs:`. Treat a `key:` indented by two spaces as a job ID.
- `uses:` lines have the form `[ - ] uses: <ref>` (an end-of-line comment is allowed).
- Only detect cargo / rustc calls that appear literally on a line. Ignore lines starting with
  `#` and `name:` lines to avoid matching comments or descriptions. Calls through scripts are
  not visible.
- Fail if no numeric-version `uses:` is found, so a broken scan is reported as a failure.

Usage:
    python3 .github/scripts/check-toolchain-pins.py [workflow.yml]

Exit codes: 0 = OK / 1 = mismatch
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# Only inspect `uses: dtolnay/rust-toolchain@<ref>`. Other actions are out of scope.
PINNED_ACTION = "dtolnay/rust-toolchain"
USES_RE = re.compile(r"^\s*(?:-\s*)?uses:\s*([^\s#]+)\s*(?:#.*)?$")
# Keys directly under jobs: (indented by two spaces) are job IDs.
JOB_KEY_RE = re.compile(r"^  ([A-Za-z0-9_-]+):\s*$")
# Job display name (indented by four spaces; step name: fields use six spaces and do not match).
JOB_NAME_RE = re.compile(r"^    name:\s*(.+?)\s*$")
JOBS_KEY_RE = re.compile(r"^jobs:\s*$")
STEP_NAME_RE = re.compile(r"^\s*(?:-\s*)?name:")
COMMENT_RE = re.compile(r"^\s*#")
# Only numeric versions count as pinned. @stable, @master, and commit SHAs are not pinned versions.
VERSION_RE = re.compile(r"^(\d+)\.(\d+)(?:\.(\d+))?$")
# Literal cargo / rustc calls on a line (require trailing whitespace to reduce false positives).
CARGO_CALL_RE = re.compile(r"(?:^|[\s;&|(){}])(?:cargo|rustc)\s")


@dataclass(frozen=True)
class Pin:
    job_id: str
    version: str


def normalize_version(raw: str) -> str | None:
    """Normalize '1.98.1' / '1.91' to comparable 'X.Y.Z' form. Return None for non-numeric versions."""
    match = VERSION_RE.match(raw.strip())
    if match is None:
        return None
    major, minor, patch = match.group(1), match.group(2), match.group(3) or "0"
    return f"{int(major)}.{int(minor)}.{int(patch)}"


def declared_msrv(repo_root: Path) -> set[str]:
    """Return the set of rust-version values declared by workspace crates.

    A `crates/*` glob would miss crates under bindings/, so use the data from cargo metadata.
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
        sys.exit("cargo was not found. Install the toolchain and try again.")
    except subprocess.CalledProcessError as error:
        sys.exit(f"cargo metadata failed:\n{error.stderr}")

    versions: set[str] = set()
    for package in json.loads(completed.stdout)["packages"]:
        raw = package.get("rust_version")
        if raw is None:
            continue
        normalized = normalize_version(raw)
        if normalized is None:
            sys.exit(f"unexpected rust-version format: {package['name']} = {raw!r}")
        versions.add(normalized)
    return versions


def scan_workflow(path: Path) -> tuple[list[Pin], set[str], set[str], dict[str, str]]:
    """Return (pinned versions, jobs calling cargo, jobs declaring a toolchain, job display names)."""
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

        # Comments and step names are descriptions, not executed commands.
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
            f"ERROR: No pinned version (numeric ref) found for {PINNED_ACTION}."
            " The scan may be broken or the action may be unpinned.",
            file=sys.stderr,
        )
        return 1
    print("Pinned versions:")
    print(render_table(pins, msrv, job_names))

    gate_versions = sorted({pin.version for pin in pins if pin.version not in msrv})
    used_versions = sorted({pin.version for pin in pins})

    if len(gate_versions) > 1:
        print(
            "\nERROR: Gate jobs have inconsistent pinned toolchain versions: "
            f"{', '.join(gate_versions)}\n"
            "    Run the same gate with the same version. If only Linux uses a newer version,"
            " the same error may be detected on one platform but not another.",
            file=sys.stderr,
        )
        return 1

    if not gate_versions:
        if len(used_versions) > 1:
            print(
                "\nERROR: The gate's pinned version matches a rust-version declared by a crate"
                f" (pinned: {', '.join(used_versions)} / declared MSRV: "
                f"{', '.join(sorted(msrv))}).\n"
                "    In this state, version values alone cannot distinguish gate pins from"
                " MSRV pins, so mismatches between gates cannot be detected.\n"
                "    Set the gate version to something other than a declared MSRV, or make all"
                " pins use the same version.",
                file=sys.stderr,
            )
            return 1
        print("\nOK: There is only one pinned version (all pins match a declared MSRV).")
        return 0

    undeclared = sorted(cargo_jobs - declaring_jobs)
    if undeclared:
        print(
            "\nERROR: Jobs call cargo / rustc without declaring a toolchain: "
            f"{', '.join(undeclared)}\n"
            "    Those jobs silently use the Rust preinstalled on the runner and may start failing"
            " without warning after an upstream update. Add `uses: "
            f"{PINNED_ACTION}@<gate version>`.",
            file=sys.stderr,
        )
        return 1

    gate_count = sum(1 for pin in pins if pin.version not in msrv)
    print(
        f"\nOK: Gate jobs all use the single pinned toolchain version {gate_versions[0]}"
        f" ({gate_count} gate pins / {len(pins) - gate_count} excluded as MSRV)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
