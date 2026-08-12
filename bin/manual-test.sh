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
copies credentials. Providers without usable credentials render unavailable.
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

command -v cargo >/dev/null 2>&1 || {
  echo "manual-test: Rust/Cargo is required" >&2
  exit 127
}

TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/herdr-capacity-manual.XXXXXX")"
trap 'rm -rf "$TEMP_DIR"' EXIT
CONFIG="$TEMP_DIR/model-capacity.json"
STATE_DIR="$TEMP_DIR/state"

cat >"$CONFIG" <<'JSON'
{
  "refreshSeconds": 180,
  "showBindings": false,
  "accounts": [
    {
      "provider": "anthropic",
      "accountId": "claude-default",
      "label": "Claude subscription",
      "authType": "oauth",
      "source": "claude-code",
      "configDir": "~/.claude",
      "allowKeychain": true
    },
    {
      "provider": "openai",
      "accountId": "codex-default",
      "label": "ChatGPT subscription",
      "authType": "oauth",
      "source": "codex",
      "codexHome": "~/.codex"
    },
    {
      "provider": "openrouter",
      "accountId": "openrouter-default",
      "label": "OpenRouter",
      "authType": "api",
      "source": "openrouter",
      "tokenEnv": "OPENROUTER_API_KEY"
    },
    {
      "provider": "amp",
      "accountId": "amp-default",
      "label": "Amp billing",
      "authType": "cli",
      "source": "amp-cli"
    }
  ]
}
JSON

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
