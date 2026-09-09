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
# Overwriting bin/model-capacity's content in place corrupts macOS's
# code-signature validation for that inode if a previous instance (e.g. an
# open capacity pane) still has it mapped for execution, permanently
# breaking every subsequent launch of that path with EBADEXEC. Build to a
# temp file in the same directory and rename it into place instead: rename
# swaps the directory entry to a new inode, so an already-running mapped
# process keeps using its old inode untouched while new launches get a
# fresh, correctly-signed one.
TMP="$ROOT/bin/.model-capacity.tmp.$$"
cp "$ROOT/target/release/model-capacity" "$TMP"
mv -f "$TMP" "$ROOT/bin/model-capacity"
