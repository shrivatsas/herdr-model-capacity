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
if command -v sha256sum >/dev/null 2>&1; then
  WORKSPACE_KEY="$(printf '%s' "$WORKSPACE" | sha256sum | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  WORKSPACE_KEY="$(printf '%s' "$WORKSPACE" | shasum -a 256 | awk '{print $1}')"
else
  echo "model-capacity: requires sha256sum (Linux) or shasum (macOS)" >&2
  exit 1
fi
if [[ ! "$WORKSPACE_KEY" =~ ^[0-9a-fA-F]{64}$ ]]; then
  echo "model-capacity: could not derive a SHA-256 workspace key" >&2
  exit 1
fi
mkdir -p "$STATE_DIR"
STATE_FILE="$STATE_DIR/pane-$WORKSPACE_KEY"
LOCK_FILE="$STATE_DIR/pane-$WORKSPACE_KEY.lock"
LOCK_ATTEMPTS=50

known_lock_owner() {
  [[ "$1" =~ ^[0-9]+$ || "$1" =~ ^[0-9]+:[0-9]+:[0-9]+$ ]]
}

# An earlier release locked with a directory. ln(1) links *into* an existing
# directory rather than failing. Reclaim only a directory with a known dead
# owner; an empty directory might belong to a process that has not written yet.
if [[ -d "$LOCK_FILE" ]]; then
  LEGACY_PID="$(cat "$LOCK_FILE/pid" 2>/dev/null || true)"
  if [[ "$LEGACY_PID" =~ ^[0-9]+$ ]] && kill -0 "$LEGACY_PID" 2>/dev/null; then
    echo "model-capacity: another pane action is already running" >&2
    exit 1
  fi
  if [[ ! "$LEGACY_PID" =~ ^[0-9]+$ ]]; then
    echo "model-capacity: cannot safely reclaim pane lock at $LOCK_FILE" >&2
    exit 1
  fi
  LEGACY_LOCK="$LOCK_FILE.legacy.$$.$RANDOM"
  if ! mv "$LOCK_FILE" "$LEGACY_LOCK" 2>/dev/null; then
    echo "model-capacity: cannot acquire the pane lock at $LOCK_FILE" >&2
    exit 1
  fi
  MOVED_PID="$(cat "$LEGACY_LOCK/pid" 2>/dev/null || true)"
  if [[ "$MOVED_PID" =~ ^[0-9]+$ ]] && ! kill -0 "$MOVED_PID" 2>/dev/null; then
    rm -rf "$LEGACY_LOCK"
  else
    mv "$LEGACY_LOCK" "$LOCK_FILE" 2>/dev/null || true
    echo "model-capacity: another pane action is already running" >&2
    exit 1
  fi
fi

# Hard-linking an already-written pid file into place is atomic and fails when
# the lock exists, so the lock is never observable without its owner's pid.
# A dead lock is reclaimed by renaming it away first: only one racer can win
# that rename, so no process ever unlinks a lock another process just took.
LOCK_OWNER="$$:$RANDOM:$RANDOM"
LOCK_TMP="$STATE_DIR/pane-$WORKSPACE_KEY.lock.$LOCK_OWNER"
printf '%s\n' "$LOCK_OWNER" >"$LOCK_TMP"
release_lock() {
  if [[ -f "$LOCK_FILE" ]] && [[ "$(cat "$LOCK_FILE" 2>/dev/null || true)" == "$LOCK_OWNER" ]]; then
    rm -f "$LOCK_FILE"
  fi
  rm -f "$LOCK_TMP" 2>/dev/null || true
}
trap release_lock EXIT
LOCK_ATTEMPT=0
until ln "$LOCK_TMP" "$LOCK_FILE" 2>/dev/null; do
  LOCK_OWNER_ON_DISK="$(cat "$LOCK_FILE" 2>/dev/null || true)"
  LOCK_PID="${LOCK_OWNER_ON_DISK%%:*}"
  if [[ "$LOCK_PID" =~ ^[0-9]+$ ]] && kill -0 "$LOCK_PID" 2>/dev/null; then
    echo "model-capacity: another pane action is already running" >&2
    exit 1
  fi
  LOCK_ATTEMPT=$((LOCK_ATTEMPT + 1))
  if (( LOCK_ATTEMPT > LOCK_ATTEMPTS )); then
    echo "model-capacity: cannot acquire the pane lock at $LOCK_FILE" >&2
    exit 1
  fi
  # A missing or malformed owner is not proof that the lock is stale. Leave it
  # alone and time out rather than stealing a lock during another process's
  # creation window.
  if ! known_lock_owner "$LOCK_OWNER_ON_DISK"; then
    sleep 0.1
    continue
  fi
  STALE_LOCK="$LOCK_FILE.stale.$$.$RANDOM"
  if mv "$LOCK_FILE" "$STALE_LOCK" 2>/dev/null; then
    STALE_OWNER="$(cat "$STALE_LOCK" 2>/dev/null || true)"
    STALE_PID="${STALE_OWNER%%:*}"
    if ! known_lock_owner "$STALE_OWNER" || \
       ! [[ "$STALE_PID" =~ ^[0-9]+$ ]] || \
       kill -0 "$STALE_PID" 2>/dev/null; then
      # The lock became live, was replaced, or cannot be identified reliably.
      # Restore only if no new owner took the pathname; never delete it here.
      ln "$STALE_LOCK" "$LOCK_FILE" 2>/dev/null || true
      echo "model-capacity: another pane action is already running" >&2
      exit 1
    fi
    rm -f "$STALE_LOCK"
  fi
  sleep 0.1
done
rm -f "$LOCK_TMP"

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
