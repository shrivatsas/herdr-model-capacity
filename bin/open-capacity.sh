#!/bin/bash
set -euo pipefail

HERDR_BIN="${HERDR_BIN_PATH:-herdr}"
MODE="${1:-toggle}"
DIRECTION="${2:-right}"

if [[ "$DIRECTION" != "right" && "$DIRECTION" != "down" ]]; then
  echo "model-capacity: direction must be right or down" >&2
  exit 2
fi

PANES_JSON="$($HERDR_BIN pane list)"
WORKSPACE="${HERDR_WORKSPACE_ID:-}"
if [[ -z "$WORKSPACE" ]]; then
  WORKSPACE="$(printf '%s' "$PANES_JSON" | python3 -c '
import json, sys
panes = json.load(sys.stdin).get("result", {}).get("panes", [])
print(next((p.get("workspace_id", "") for p in panes if p.get("focused")), ""))
')"
fi
if [[ -z "$WORKSPACE" ]]; then
  echo "model-capacity: cannot determine the current Herdr workspace" >&2
  exit 1
fi

STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-$HOME/.local/state/herdr-model-capacity}"
SAFE_WORKSPACE="$(printf '%s' "$WORKSPACE" | tr -c 'A-Za-z0-9_.-' '-')"
mkdir -p "$STATE_DIR"
STATE_FILE="$STATE_DIR/pane-$SAFE_WORKSPACE"
LOCK_DIR="$STATE_DIR/pane-$SAFE_WORKSPACE.lock"
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
  echo "model-capacity: another pane action is already running" >&2
  exit 1
fi
trap 'rmdir "$LOCK_DIR" 2>/dev/null || true' EXIT

# Refresh after taking the lock so a completed concurrent open cannot be
# mistaken for stale state based on the pre-lock snapshot.
PANES_JSON="$($HERDR_BIN pane list)"

CAPACITY_PANE=""
if [[ -f "$STATE_FILE" ]]; then
  CAPACITY_PANE="$(cat "$STATE_FILE")"
  if ! printf '%s' "$PANES_JSON" | python3 -c '
import json, sys
pane_id, workspace = sys.argv[1:3]
panes = json.load(sys.stdin).get("result", {}).get("panes", [])
raise SystemExit(0 if any(p.get("pane_id") == pane_id and p.get("workspace_id") == workspace for p in panes) else 1)
' "$CAPACITY_PANE" "$WORKSPACE"; then
    CAPACITY_PANE=""
    rm -f "$STATE_FILE"
  fi
fi

TARGET_PANE="$(printf '%s' "$PANES_JSON" | python3 -c '
import json, sys
workspace, capacity, preferred = sys.argv[1:4]
panes = [p for p in json.load(sys.stdin).get("result", {}).get("panes", []) if p.get("workspace_id") == workspace]
survivors = [p for p in panes if p.get("pane_id") != capacity]
by_id = {p.get("pane_id"): p for p in survivors}
if preferred in by_id:
    print(preferred)
else:
    capacity_tab = next((p.get("tab_id") for p in panes if p.get("pane_id") == capacity), None)
    target = next((p for p in survivors if p.get("focused")), None)
    target = target or next((p for p in survivors if capacity_tab and p.get("tab_id") == capacity_tab), None)
    target = target or next(iter(survivors), None)
    print(target.get("pane_id", "") if target else "")
' "$WORKSPACE" "$CAPACITY_PANE" "${HERDR_PANE_ID:-}")"

if [[ -n "$CAPACITY_PANE" ]]; then
  "$HERDR_BIN" plugin pane close "$CAPACITY_PANE"
  rm -f "$STATE_FILE"
  [[ "$MODE" == "toggle" ]] && exit 0
fi

if [[ -z "$TARGET_PANE" ]]; then
  echo "model-capacity: cannot find a surviving target pane in workspace $WORKSPACE" >&2
  exit 1
fi

OPEN_RESULT="$($HERDR_BIN plugin pane open \
  --plugin shrivatsa.model-capacity \
  --entrypoint capacity \
  --placement split \
  --target-pane "$TARGET_PANE" \
  --direction "$DIRECTION" \
  --no-focus)"
PANE_ID="$(printf '%s' "$OPEN_RESULT" | python3 -c '
import json, sys
value = json.load(sys.stdin)
pane = value.get("result", {}).get("plugin_pane", {}).get("pane", {})
print(pane.get("pane_id", ""))
')"
if [[ -z "$PANE_ID" ]]; then
  echo "model-capacity: Herdr opened the pane but omitted its pane ID" >&2
  exit 1
fi
printf '%s\n' "$PANE_ID" >"$STATE_FILE.tmp"
mv "$STATE_FILE.tmp" "$STATE_FILE"
printf '%s\n' "$OPEN_RESULT"
