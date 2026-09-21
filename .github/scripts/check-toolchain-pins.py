#!/usr/bin/env python3
"""ci.yml の中の toolchain 固定版が 1 つに揃っていることを検査する門。

なぜ必要か
----------
GitHub Actions の `uses:` には式も環境変数も書けない（一次資料: GitHub Docs
"Context availability" の表に `jobs.<job_id>.steps.uses` の行が無く、`steps.with`
にはある）。そのため toolchain の版は `uses: dtolnay/rust-toolchain@<版>` の ref に
直接書くしかなく、同じ版が ci.yml の中に何度も現れる。写しが食い違えば
「一部のジョブだけ別の版で門を通っていた」という静かなドリフトになるが、CI は
緑のままなので誰も気づかない。この門はその食い違いを赤として出す。

規則
----
1. ci.yml 内の `uses: dtolnay/rust-toolchain@<数値版>` を全部集める。
2. MSRV ジョブ（= クレートが宣言した rust-version で建てるジョブ）は別の版を使う
   のが正しいので除外する。「MSRV ジョブとは何か」は job 名の一覧ではなく
   `cargo metadata --no-deps` から取った宣言済み rust-version で判定する。だから
   MSRV を宣言するクレートが増えても、ジョブが増えても、この除外規則は破綻しない。
3. 残り（= 門のジョブ）の固定版が 2 つ以上あれば失敗。
4. `run:` で cargo / rustc を呼ぶのに、そのジョブが toolchain を 1 つも宣言して
   いなければ失敗（ランナーに最初から入っている Rust を黙って使う経路を作らせ
   ない）。ここで見ているのは「宣言しているか」であって版ではない: `@stable` の
   先行警報ジョブのように、意図して stable を指すのは構わない。

なぜ YAML ライブラリを使わないか
--------------------------------
GitHub の ubuntu ランナーに入っている yamllint は pipx（隔離 venv）で入っており、
apt にも python3-yaml が無い（一次資料: actions/runner-images の
`Ubuntu2404-Readme.md` と `toolsets/toolset-2404.json`）。つまり素の python3 で
`import yaml` は通らない。門が環境依存で落ちるのは本末転倒なので、stdlib だけで
動く行走査にしてある（下の前提を参照）。

走査の前提（best effort・外れたら黙って見逃さず落ちる側に倒す）
-------------------------------------------------------------
- `jobs:` の次の行から見る。2 スペース字下げの `key:` をジョブ id とみなす。
- `uses:` 行は `[ - ] uses: <ref>` の形（行末コメント可）。
- cargo / rustc の呼び出しは行にリテラルで現れるものだけ。`#` 始まりの行と
  `name:` 行は除外する（コメントや説明文の誤検出を避ける）。スクリプト経由の
  呼び出しは見えない。
- 数値版の `uses:` が 1 つも見つからなければ失敗（= 走査が壊れたら赤くなる）。

使い方:
    python3 .github/scripts/check-toolchain-pins.py [workflow.yml]

終了コード: 0 = OK / 1 = 食い違いあり
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# `uses: dtolnay/rust-toolchain@<ref>` だけを見る。他の action は対象外。
PINNED_ACTION = "dtolnay/rust-toolchain"
USES_RE = re.compile(r"^\s*(?:-\s*)?uses:\s*([^\s#]+)\s*(?:#.*)?$")
# jobs: の直下（2 スペース字下げ）のキー = ジョブ id。
JOB_KEY_RE = re.compile(r"^  ([A-Za-z0-9_-]+):\s*$")
# ジョブの表示名（4 スペース字下げ。ステップの name: は 6 スペースなので当たらない）。
JOB_NAME_RE = re.compile(r"^    name:\s*(.+?)\s*$")
JOBS_KEY_RE = re.compile(r"^jobs:\s*$")
STEP_NAME_RE = re.compile(r"^\s*(?:-\s*)?name:")
COMMENT_RE = re.compile(r"^\s*#")
# 数値版だけを「固定版」とみなす。@stable / @master / コミット SHA は固定版ではない。
VERSION_RE = re.compile(r"^(\d+)\.(\d+)(?:\.(\d+))?$")
# 行にリテラルで現れる cargo / rustc 呼び出し（後ろに空白を要求して誤検出を減らす）。
CARGO_CALL_RE = re.compile(r"(?:^|[\s;&|(){}])(?:cargo|rustc)\s")


@dataclass(frozen=True)
class Pin:
    job_id: str
    version: str


def normalize_version(raw: str) -> str | None:
    """'1.98.1' / '1.91' を比較可能な 'X.Y.Z' に揃える。数値版でなければ None。"""
    match = VERSION_RE.match(raw.strip())
    if match is None:
        return None
    major, minor, patch = match.group(1), match.group(2), match.group(3) or "0"
    return f"{int(major)}.{int(minor)}.{int(patch)}"


def declared_msrv(repo_root: Path) -> set[str]:
    """ワークスペースのクレートが宣言している rust-version の集合。

    `crates/*` を glob で数えると bindings/ の下のクレートが落ちるので、
    cargo metadata の実データを使う。
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
        sys.exit("cargo が見つかりません。toolchain を入れてから実行してください。")
    except subprocess.CalledProcessError as error:
        sys.exit(f"cargo metadata が失敗しました:\n{error.stderr}")

    versions: set[str] = set()
    for package in json.loads(completed.stdout)["packages"]:
        raw = package.get("rust_version")
        if raw is None:
            continue
        normalized = normalize_version(raw)
        if normalized is None:
            sys.exit(f"rust-version の書式が想定外です: {package['name']} = {raw!r}")
        versions.add(normalized)
    return versions


def scan_workflow(path: Path) -> tuple[list[Pin], set[str], set[str], dict[str, str]]:
    """(固定版の一覧, cargo を呼ぶジョブ, toolchain を宣言したジョブ, ジョブ表示名)。"""
    pins: list[Pin] = []
    cargo_jobs: set[str] = set()
    declaring_jobs: set[str] = set()
    job_names: dict[str, str] = {}

    in_jobs = False
    job_id = "(jobs: の外)"
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

        # コメント・ステップ名は説明文であり実行内容ではない。
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
        kind = "MSRV (除外)" if pin.version in msrv else "門"
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
    print(f"宣言済み MSRV (cargo metadata --no-deps): {', '.join(sorted(msrv)) or 'なし'}")
    if not pins:
        print(
            f"NG: {PINNED_ACTION} の固定版 (数値の ref) が 1 つも見つかりません。"
            " 走査が壊れているか、固定が外れています。",
            file=sys.stderr,
        )
        return 1
    print("固定版の一覧:")
    print(render_table(pins, msrv, job_names))

    gate_versions = sorted({pin.version for pin in pins if pin.version not in msrv})
    used_versions = sorted({pin.version for pin in pins})

    if len(gate_versions) > 1:
        print(
            "\nNG: 門のジョブの toolchain 固定版が食い違っています: "
            f"{', '.join(gate_versions)}\n"
            "    同じ門は同じ版で回すこと（Linux 側だけ新しい版だと、同じ誤りが"
            "プラットフォームによって見つかったり見つからなかったりする）。",
            file=sys.stderr,
        )
        return 1

    if not gate_versions:
        if len(used_versions) > 1:
            print(
                "\nNG: 門の固定版が、クレートが宣言している rust-version と同じ値に"
                f"なっています（固定版: {', '.join(used_versions)} / 宣言済み MSRV: "
                f"{', '.join(sorted(msrv))}）。\n"
                "    この状態では「門のジョブの固定」と「MSRV ジョブの固定」を版の値"
                "だけでは区別できず、門どうしの食い違いを検出できません。\n"
                "    門の版を宣言済み MSRV 以外にするか、すべての固定を同じ版にして"
                "ください。",
                file=sys.stderr,
            )
            return 1
        print("\nOK: 固定版は 1 つだけです（すべて宣言済み MSRV と同値）。")
        return 0

    undeclared = sorted(cargo_jobs - declaring_jobs)
    if undeclared:
        print(
            "\nNG: cargo / rustc を呼ぶのに toolchain を宣言していないジョブがあります: "
            f"{', '.join(undeclared)}\n"
            "    そのジョブはランナーに最初から入っている Rust を黙って使うので、"
            "上流の更新で予告なく赤くなります。`uses: "
            f"{PINNED_ACTION}@<門の版>` を足してください。",
            file=sys.stderr,
        )
        return 1

    gate_count = sum(1 for pin in pins if pin.version not in msrv)
    print(
        f"\nOK: 門の toolchain 固定版は {gate_versions[0]} の 1 つに揃っています"
        f"（門 {gate_count} 件 / MSRV として除外 {len(pins) - gate_count} 件）。"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
