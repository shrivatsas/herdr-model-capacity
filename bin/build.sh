#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$ROOT/bin"
if [[ -x "$ROOT/bin/model-capacity" ]] \
  && [[ "$ROOT/bin/model-capacity" -nt "$ROOT/Cargo.toml" ]] \
  && [[ "$ROOT/bin/model-capacity" -nt "$ROOT/Cargo.lock" ]] \
  && ! find "$ROOT/src" -type f -newer "$ROOT/bin/model-capacity" -print -quit | grep -q .; then
  exit 0
fi
command -v cargo >/dev/null 2>&1 || {
  echo "model-capacity: Rust/Cargo is required to build this plugin" >&2
  exit 127
}
cargo build --release --manifest-path "$ROOT/Cargo.toml"
cp "$ROOT/target/release/model-capacity" "$ROOT/bin/model-capacity"
