#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ ! -x "$ROOT/bin/model-capacity" ]]; then
  echo "model-capacity: binary missing; run bin/build.sh" >&2
  exit 127
fi
exec "$ROOT/bin/model-capacity" "$@"
