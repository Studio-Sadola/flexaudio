#!/usr/bin/env bash
# flexaudio-napi のビルド + Node スモークテスト（実音不要）。
# napi CLI を使わず cargo build + 手動リネームで .node を用意（ネット最小化）。
set -euo pipefail

. "$HOME/.cargo/env"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"   # flexaudio ルート
TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "== cargo build -p flexaudio-napi (release) =="
cargo build -p flexaudio-napi --release --manifest-path "$ROOT/Cargo.toml"

# cdylib の生成物を探す（package 名 flexaudio-napi -> libflexaudio_napi.so）。
SO="$ROOT/target/release/libflexaudio_napi.so"
if [[ ! -f "$SO" ]]; then
  echo "ERROR: built cdylib not found at $SO" >&2
  ls -la "$ROOT/target/release/" | grep -i flexaudio_napi || true
  exit 1
fi

cp -f "$SO" "$TEST_DIR/flexaudio.node"
echo "== copied $SO -> $TEST_DIR/flexaudio.node =="

echo "== node smoke.mjs =="
node "$TEST_DIR/smoke.mjs"
