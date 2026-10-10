#!/usr/bin/env python3
"""Resolve the original tag-push run, including runs whose publish job failed."""
from __future__ import annotations

import json
import os
import re
import time
import urllib.parse
import urllib.request
from collections.abc import Mapping
from pathlib import Path


RUN_LIST_BACKOFF = (2, 4, 8, 16, 30)


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


def release_run_id(item: object, sha: str, tag: str) -> int | None:
    run = object_record(item)
    if run.get("event") != "push" or run.get("head_sha") != sha or run.get("head_branch") != tag:
        return None
    if run.get("path") != ".github/workflows/release-npm.yml":
        return None
    run_id = run.get("id")
    if type(run_id) is not int or run_id <= 0:
        raise ValueError("Invalid run ID")
    return run_id


def select_original(runs: list[object], sha: str, tag: str) -> int:
    candidates = [run_id for item in runs if (run_id := release_run_id(item, sha, tag)) is not None]
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

    def workflow_runs(self, sha: str) -> list[object]:
        runs: list[object] = []
        for page in range(1, 1001):
            query = urllib.parse.urlencode({"event": "push", "head_sha": sha, "per_page": 100, "page": page})
            result = self.get("/actions/workflows/release-npm.yml/runs?" + query)
            batch = result.get("workflow_runs")
            if not isinstance(batch, list):
                raise ValueError("Invalid workflow run response")
            runs.extend(batch)
            if len(batch) < 100:
                return runs
        raise ValueError("Run pagination limit exceeded")


    def original_run(self, sha: str, tag: str, current_run: dict[str, object] | None = None) -> int:
        current_id = None if current_run is None else release_run_id(current_run, sha, tag)
        if current_run is not None and current_id is None:
            raise ValueError("Current run is not the expected tag-push npm run")
        runs: list[object] = [] if current_run is None else [current_run]
        for attempt in range(len(RUN_LIST_BACKOFF) + 1):
            batch = self.workflow_runs(sha)
            listed_ids = [run_id for item in batch
                          if (run_id := release_run_id(item, sha, tag)) is not None]
            # Retain earlier observations if the index regresses during polling.
            runs.extend(batch)
            # An observed lower ID is already enough to forbid this push build.
            older_seen = current_id is not None and any(run_id < current_id for run_id in listed_ids)
            if (current_id is None and listed_ids) or current_id in listed_ids or older_seen:
                return select_original(runs, sha, tag)
            if attempt < len(RUN_LIST_BACKOFF):
                time.sleep(RUN_LIST_BACKOFF[attempt])
        if current_id is not None:
            # Run IDs increase monotonically: an unseen older run must win.
            # A direct lookup alone cannot prove originality, so index lag past
            # the 60-second backoff budget fails closed. Seeing the current run
            # is a convergence signal, not a consistency guarantee: GitHub could
            # still omit an older run if indexing occurs out of order. The API
            # provides no snapshot/consistency token to eliminate that risk.
            raise ValueError("Current tag-push npm run is not indexed; rebuilding is forbidden")
        return select_original(runs, sha, tag)


def main(api: GitHub | None = None, env: Mapping[str, str] | None = None) -> None:
    if env is None:
        env = os.environ
    event = env["EVENT_NAME"]
    if event == "workflow_dispatch":
        version = validate_version(env["INPUT_VERSION"])
    elif event == "push" and env["REF_TYPE"] == "tag":
        ref = env["REF_NAME"]
        if not ref.startswith("v"):
            raise ValueError("Expected version tag")
        version = validate_version(ref[1:])
    else:
        raise ValueError("Unsupported release trigger")
    tag = "v" + version
    if api is None:
        api = GitHub(env["GITHUB_REPOSITORY"], env["GH_TOKEN"])
    sha = api.tag_commit(tag)
    if event == "push":
        current_id = env["GITHUB_RUN_ID"]
        if re.fullmatch(r"[1-9][0-9]*", current_id) is None:
            raise ValueError("Invalid current run ID")
        current_run = api.get(f"/actions/runs/{current_id}")
        if release_run_id(current_run, sha, tag) != int(current_id):
            raise ValueError("Current run is not the expected tag-push npm run")
        current_attempt = current_run.get("run_attempt")
        if type(current_attempt) is not int or current_attempt != 1 or env.get("GITHUB_RUN_ATTEMPT") != "1":
            raise ValueError("original tag-push run was re-run; resealing is forbidden — cut a new patch version")
        run_id = api.original_run(sha, tag, current_run=current_run)
    else:
        run_id = api.original_run(sha, tag)
    attempt = api.get(f"/actions/runs/{run_id}").get("run_attempt")
    if (type(attempt) is not int or attempt != 1 or
            (event == "push" and env.get("GITHUB_RUN_ATTEMPT") != "1")):
        raise ValueError("original tag-push run was re-run; resealing is forbidden — cut a new patch version")
    if event == "push" and (str(run_id) != env["GITHUB_RUN_ID"] or sha != env["GITHUB_SHA"]):
        raise ValueError("Only the original tag-push run may build this release")
    with Path(env["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
        output.write(f"version={version}\nsha={sha}\nrun_id={run_id}\n")
    print(f"Release {tag}: commit {sha}, original run {run_id}")


if __name__ == "__main__":
    main()
