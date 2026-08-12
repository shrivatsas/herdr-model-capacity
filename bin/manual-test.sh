#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="preview"
WIDTH="${COLUMNS:-80}"

usage() {
  cat <<'EOF'
Usage: bin/manual-test.sh [--probe | --herdr] [--width COLUMNS]

Creates a four-provider test registry with agent bindings disabled.

  (no option)       Build and open an isolated interactive pane preview
  --probe           Print normalized provider collection as JSON
  --herdr           Back up Herdr's current registry, install the test registry,
                    and link this checkout
  --width COLUMNS   Set the local preview width (default: $COLUMNS or 80)
  -h, --help        Show this help

The registry references existing CLI logins and OPENROUTER_API_KEY; it never
copies credentials. Claude and Codex account homes under ~/.claude-accounts and
~/.codex-accounts are included automatically. Providers without usable
credentials render unavailable.
EOF
}

while (($#)); do
  case "$1" in
    --probe)
      MODE="probe"
      shift
      ;;
    --herdr)
      MODE="herdr"
      shift
      ;;
    --width)
      [[ $# -ge 2 ]] || {
        echo "manual-test: --width requires a value" >&2
        exit 2
      }
      WIDTH="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "manual-test: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

[[ "$WIDTH" =~ ^[1-9][0-9]*$ ]] || {
  echo "manual-test: width must be a positive integer" >&2
  exit 2
}

for command in cargo python3; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "manual-test: $command is required" >&2
    exit 127
  }
done

TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/herdr-capacity-manual.XXXXXX")"
trap 'rm -rf "$TEMP_DIR"' EXIT
CONFIG="$TEMP_DIR/model-capacity.json"
STATE_DIR="$TEMP_DIR/state"

python3 - "$CONFIG" <<'PY'
import json
import os
import pathlib
import re
import sys

home = pathlib.Path.home()

def account_homes(directory, fallback):
    homes = sorted(path for path in directory.glob("*") if path.is_dir())
    return homes or [fallback]

def identifier(value):
    return re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-") or "default"

def display_name(path):
    return "default" if path.name.startswith(".") else path.name.replace("-", " ")

accounts = []
keychain_account_added = False
for path in account_homes(home / ".claude-accounts", home / ".claude"):
    name = display_name(path)
    account = {
        "provider": "anthropic",
        "accountId": f"claude-{identifier(name)}",
        "label": f"Claude {name}",
        "authType": "oauth",
        "source": "claude-code",
    }
    if (path / ".credentials.json").is_file():
        account["configDir"] = os.fspath(path)
    elif not keychain_account_added:
        account["allowKeychain"] = True
        keychain_account_added = True
    else:
        account["configDir"] = os.fspath(path)
    accounts.append(account)

for path in account_homes(home / ".codex-accounts", home / ".codex"):
    name = display_name(path)
    accounts.append({
        "provider": "openai",
        "accountId": f"chatgpt-{identifier(name)}",
        "label": f"ChatGPT {name}",
        "authType": "oauth",
        "source": "codex",
        "codexHome": os.fspath(path),
    })

accounts.extend([
    {
        "provider": "openrouter",
        "accountId": "openrouter-default",
        "label": "OpenRouter",
        "authType": "api",
        "source": "openrouter",
        "tokenEnv": "OPENROUTER_API_KEY",
    },
    {
        "provider": "amp",
        "accountId": "amp-default",
        "label": "AMP billing",
        "authType": "cli",
        "source": "amp-cli",
    },
])

with open(sys.argv[1], "w") as output:
    json.dump({
        "refreshSeconds": 180,
        "showBindings": False,
        "accounts": accounts,
    }, output, indent=2)
    output.write("\n")
PY

echo "Building model-capacity..." >&2
bash "$ROOT/bin/build.sh"

if [[ "$MODE" == "herdr" ]]; then
  command -v herdr >/dev/null 2>&1 || {
    echo "manual-test: herdr is required for --herdr" >&2
    exit 127
  }
  CONFIG_DIR="$(herdr plugin config-dir shrivatsa.model-capacity)"
  mkdir -p "$CONFIG_DIR"
  TARGET="$CONFIG_DIR/model-capacity.json"
  if [[ -f "$TARGET" ]]; then
    BACKUP="$TARGET.manual-test-backup.$(date +%Y%m%d%H%M%S)"
    cp "$TARGET" "$BACKUP"
    echo "Backed up existing registry to: $BACKUP"
  fi
  cp "$CONFIG" "$TARGET"
  herdr plugin link "$ROOT"
  cat <<EOF

Installed the four-provider test registry at:
  $TARGET

Open or toggle the pane with:
  herdr plugin action invoke shrivatsa.model-capacity.open-capacity

The test registry has showBindings=false, so the pane displays provider cards
instead of the Active Agents/account-resolution section.
EOF
  exit 0
fi

{
  printf '\nCredential inputs (missing inputs render unavailable):\n'
  for cli in amp codex claude; do
    if command -v "$cli" >/dev/null 2>&1; then
      printf '  ✓ %s: %s\n' "$cli" "$(command -v "$cli")"
    else
      printf '  - %s: not found on PATH\n' "$cli"
    fi
  done
  if [[ -n "${OPENROUTER_API_KEY:-}" ]]; then
    echo "  ✓ OPENROUTER_API_KEY is set"
  else
    echo "  - OPENROUTER_API_KEY is not set"
  fi
} >&2

export HERDR_CAPACITY_CONFIG="$CONFIG"
export HERDR_PLUGIN_STATE_DIR="$STATE_DIR"

if [[ "$MODE" == "probe" ]]; then
  "$ROOT/bin/model-capacity" probe
  exit 0
fi

cat <<EOF

Opening all-provider preview at width $WIDTH.
Press r to force a refresh; press any other key to close.

EOF
COLUMNS="$WIDTH" "$ROOT/bin/model-capacity" pane
