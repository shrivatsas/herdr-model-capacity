#!/bin/bash
set -euo pipefail
HERDR_BIN="${HERDR_BIN_PATH:-herdr}"
exec "$HERDR_BIN" plugin pane open \
  --plugin shrivatsa.model-capacity \
  --entrypoint capacity
