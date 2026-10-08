#!/usr/bin/env python3
"""Resolve the original tag-push run, including runs whose publish job failed."""
from __future__ import annotations

import json
import os
import re
import urllib.parse
import urllib.request
from pathlib import Path


def validate_version(value: str) -> str:
    if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", value) is None:
        raise ValueError("Version must be MAJOR.MINOR.PATCH")
    return value


def object_record(value: object) -> dict[str, object]:
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise ValueError("Expected a GitHub API object")
    return value


def commit_sha(value: object) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise ValueError("Invalid commit SHA")
    return value


def select_original(runs: list[object], sha: str, tag: str) -> int:
    candidates: list[int] = []
    for item in runs:
        run = object_record(item)
        if run.get("event") != "push" or run.get("head_sha") != sha or run.get("head_branch") != tag:
            continue
        if run.get("path") != ".github/workflows/release-npm.yml":
            continue
        run_id = run.get("id")
        if type(run_id) is not int or run_id <= 0:
            raise ValueError("Invalid run ID")
        candidates.append(run_id)
    if not candidates:
        raise ValueError("No original tag-push npm run found; rebuilding is forbidden")
    return min(candidates)


class GitHub:
    def __init__(self, repository: str, token: str):
        if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is None:
            raise ValueError("Invalid repository")
        self.base = f"https://api.github.com/repos/{repository}"
        self.token = token

    def get(self, path: str) -> dict[str, object]:
        request = urllib.request.Request(self.base + path, headers={
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        })
        with urllib.request.urlopen(request, timeout=30) as response:
            return object_record(json.load(response))

    def tag_commit(self, tag: str) -> str:
        obj = object_record(self.get("/git/ref/tags/" + tag).get("object"))
        for _ in range(8):
            sha = commit_sha(obj.get("sha"))
            if obj.get("type") == "commit":
                return sha
            if obj.get("type") != "tag":
                break
            obj = object_record(self.get("/git/tags/" + sha).get("object"))
        raise ValueError("Tag does not resolve to a commit")

    def original_run(self, sha: str, tag: str) -> int:
        runs: list[object] = []
        for page in range(1, 1001):
            query = urllib.parse.urlencode({"event": "push", "head_sha": sha, "per_page": 100, "page": page})
            result = self.get("/actions/workflows/release-npm.yml/runs?" + query)
            batch = result.get("workflow_runs")
            if not isinstance(batch, list):
                raise ValueError("Invalid workflow run response")
            runs.extend(batch)
            if len(batch) < 100:
                return select_original(runs, sha, tag)
        raise ValueError("Run pagination limit exceeded")


def main() -> None:
    event = os.environ["EVENT_NAME"]
    if event == "workflow_dispatch":
        version = validate_version(os.environ["INPUT_VERSION"])
    elif event == "push" and os.environ["REF_TYPE"] == "tag":
        ref = os.environ["REF_NAME"]
        if not ref.startswith("v"):
            raise ValueError("Expected version tag")
        version = validate_version(ref[1:])
    else:
        raise ValueError("Unsupported release trigger")
    tag = "v" + version
    api = GitHub(os.environ["GITHUB_REPOSITORY"], os.environ["GH_TOKEN"])
    sha = api.tag_commit(tag)
    run_id = api.original_run(sha, tag)
    if event == "push" and (str(run_id) != os.environ["GITHUB_RUN_ID"] or sha != os.environ["GITHUB_SHA"]):
        raise ValueError("Only the original tag-push run may build this release")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
        output.write(f"version={version}\nsha={sha}\nrun_id={run_id}\n")
    print(f"Release {tag}: commit {sha}, original run {run_id}")


if __name__ == "__main__":
    main()
