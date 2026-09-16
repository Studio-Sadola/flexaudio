#!/usr/bin/env bash
# flexaudio-napi のビルド + Node スモークテスト（実音不要）。
# napi CLI を使わず cargo build + 手動リネームで .node を用意（ネット最小化）。
set -euo pipefail

if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
fi

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

echo "== node dual.mjs =="
node "$TEST_DIR/dual.mjs"

echo "== node async-api.mjs =="
node "$TEST_DIR/async-api.mjs"

echo "== node exit.mjs (must self-exit within 10s) =="
node "$TEST_DIR/exit.mjs" &
exit_pid=$!
exited=0
for _ in $(seq 1 20); do
  if ! kill -0 "$exit_pid" 2>/dev/null; then
    exited=1
    break
  fi
  sleep 0.5
done
if [[ "$exited" -eq 0 ]]; then
  kill "$exit_pid" 2>/dev/null || true
  wait "$exit_pid" 2>/dev/null || true
  echo "ERROR: exit.mjs did not self-exit within 10s" >&2
  exit 1
fi
wait "$exit_pid"
echo "== exit.mjs self-exited =="
