#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: %s [--check]\n' "${0##*/}"
    printf 'Generate third-party notices, or check that all committed notices are current.\n'
}

check=false
case $# in
    0) ;;
    1)
        case "$1" in
            --check) check=true ;;
            --help|-h) usage; exit 0 ;;
            *) usage >&2; exit 2 ;;
        esac
        ;;
    *) usage >&2; exit 2 ;;
esac

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repo_root"
export LC_ALL=C

# Resolve cargo through PATH; the maintainer's ~/.cargo/bin must be on PATH.
# Pin the generator as well as CI to avoid silent formatting/schema drift.
cargo_cmd=(cargo)
if ! command -v "${cargo_cmd[0]}" >/dev/null 2>&1; then
    printf 'Error: cargo is required and must be on PATH.\n' >&2
    exit 1
fi
version_cmd=("${cargo_cmd[@]}" about --version)
if ! about_version="$("${version_cmd[@]}")"; then
    printf 'Error: install cargo-about with: cargo install cargo-about --locked --version 0.9.2 --features cli\n' >&2
    exit 1
fi
if [[ "$about_version" != 'cargo-about 0.9.2' ]]; then
    printf 'Error: expected cargo-about 0.9.2; found %s.\n' "$about_version" >&2
    exit 1
fi

temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/flexaudio-notices.XXXXXXXX")"
trap 'rm -rf -- "$temp_dir"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

manifests=(
    'crates/flexaudio-napi/Cargo.toml'
    'bindings/flexaudio-py/Cargo.toml'
    'crates/flexaudio-ffi/Cargo.toml'
    'Cargo.toml'
)
notices=(
    'crates/flexaudio-napi/THIRD_PARTY_NOTICES.md'
    'bindings/flexaudio-py/THIRD_PARTY_NOTICES.md'
    'crates/flexaudio-ffi/THIRD_PARTY_NOTICES.md'
    'THIRD_PARTY_NOTICES.md'
)

# Stage every output before replacing any committed file, so failed generation
# does not leave partially regenerated notices or truncate an existing notice.
for index in "${!manifests[@]}"; do
    generate_cmd=("${cargo_cmd[@]}" about generate --locked --fail
        --config about.toml -m "${manifests[$index]}")
    if [[ "${manifests[$index]}" == 'Cargo.toml' ]]; then
        # Cover every workspace member, including all publishable Rust crates.
        generate_cmd+=(--workspace)
    elif [[ "${manifests[$index]}" == 'bindings/flexaudio-py/Cargo.toml' ]]; then
        # Match the wheel feature selected by maturin.
        generate_cmd+=(--features extension-module)
    fi
    generate_cmd+=(about.hbs)
    printf 'Generating %s\n' "${notices[$index]}" >&2
    "${generate_cmd[@]}" > "$temp_dir/$index.md"
    printf '\n' >> "$temp_dir/$index.md"
    cat -- third-party/extra-notices.md >> "$temp_dir/$index.md"
done

if "$check"; then
    stale=()
    for index in "${!notices[@]}"; do
        notice="${notices[$index]}"
        if [[ ! -f "$notice" ]]; then
            printf 'Stale third-party notices (missing file): %s\n' "$notice" >&2
            stale+=("$notice")
        elif diff -u -- "$notice" "$temp_dir/$index.md"; then
            :
        else
            diff_status=$?
            if [[ "$diff_status" -ne 1 ]]; then
                printf 'Error: could not compare %s (diff exited %s).\n' "$notice" "$diff_status" >&2
                exit "$diff_status"
            fi
            printf 'Stale third-party notices: %s\n' "$notice" >&2
            stale+=("$notice")
        fi
    done
    if [[ "${#stale[@]}" -gt 0 ]]; then
        printf '\nRegenerate and commit the following files with scripts/gen-third-party-notices.sh:\n' >&2
        printf '  %s\n' "${stale[@]}" >&2
        exit 1
    fi
    printf 'All third-party notices are current.\n'
else
    for index in "${!notices[@]}"; do
        cp -- "$temp_dir/$index.md" "${notices[$index]}"
        printf 'Wrote %s\n' "${notices[$index]}"
    done
fi
