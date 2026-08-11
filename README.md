# Herdr Model Capacity

A compact [Herdr](https://herdr.dev/) pane showing how much usable model
capacity remains across your Anthropic, OpenAI, and OpenRouter billing accounts.

```text
CLAUDE
Personal Max
  5h           ███████░░░  72%  ↻ 2h12m
  7d           █████░░░░░  48%  ↻ 4d8h

CODEX / OPENAI
Personal ChatGPT
  5h           ██████░░░░  63%
  7d           ████████░░  82%

OPENROUTER
Personal
  balance      $18.72 remaining
```

This is current capacity, not historical token or spend analytics. Capacity is
owned by a billing account and can be shared by Claude Code, Codex, Pi, or Amp
panes without being duplicated per agent.

## Features

- Multiple accounts per provider
- Claude 5-hour, weekly, and model-scoped subscription windows
- ChatGPT/Codex subscription windows with structured local snapshot fallback
- OpenRouter per-key limits or account-wide credits
- Remaining capacity and reset times, rather than percentage consumed
- Independent provider failures with last-successful-response caching
- Distinct `unknown`, `unavailable`, `stale`, and zero-remaining states
- Configurable percentage and dollar warning thresholds
- Detailed and compact panes
- Active Herdr pane-to-account attribution

## Install

Requires Herdr 0.7.4 or newer and a stable Rust toolchain with Cargo:

```bash
herdr plugin install shrivatsas/herdr-model-capacity
```

Open the pane directly:

```bash
herdr plugin pane open \
  --plugin shrivatsa.model-capacity \
  --entrypoint capacity
```

Or invoke the plugin action:

```bash
herdr plugin action invoke shrivatsa.model-capacity.open-capacity
```

Optional keybinding in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+u"
type = "plugin_action"
command = "shrivatsa.model-capacity.open-capacity"
description = "open model capacity"
```

The compact pane entrypoint is `capacity-compact`.

## Configuration

Herdr creates the plugin config directory during installation or linking. Find
it with:

```bash
herdr plugin config-dir shrivatsa.model-capacity
```

Create `model-capacity.json` there. Without a config file, the plugin
conservatively discovers the default Claude Code and Codex homes plus
`OPENROUTER_API_KEY`.

```json
{
  "refreshSeconds": 180,
  "warningPercent": 20,
  "criticalPercent": 10,
  "warningUsd": 10,
  "criticalUsd": 5,
  "accounts": [
    {
      "provider": "anthropic",
      "accountId": "personal-max",
      "label": "Personal Max",
      "authType": "oauth",
      "source": "claude-code",
      "configDir": "~/.claude-personal"
    },
    {
      "provider": "openai",
      "accountId": "work-chatgpt",
      "label": "Work ChatGPT",
      "authType": "oauth",
      "source": "codex",
      "codexHome": "~/.codex-work"
    },
    {
      "provider": "openrouter",
      "accountId": "personal-openrouter",
      "label": "OpenRouter Personal",
      "authType": "api",
      "source": "openrouter",
      "managementKeyEnv": "OPENROUTER_MANAGEMENT_KEY"
    }
  ],
  "bindings": [
    {
      "agent": "pi",
      "provider": "anthropic",
      "accountId": "personal-max"
    },
    {
      "agent": "amp",
      "paneId": "optional-exact-herdr-pane-id",
      "provider": "openai",
      "accountId": "work-chatgpt"
    }
  ]
}
```

Secrets are named through environment variables and are never written to plugin
state or diagnostics. `HERDR_CAPACITY_CONFIG` can override the config path.

### Provider notes

- **Claude:** reads Claude Code OAuth credentials. The quota endpoint is
  undocumented, so responses are cached and failures degrade to stale data.
- **Codex:** reads ChatGPT OAuth credentials and falls back to Codex's structured
  session rate-limit snapshots. The subscription endpoint is undocumented.
- **OpenRouter:** an inference key uses the documented `/api/v1/key` endpoint. A
  management key uses `/api/v1/credits` for account-wide balance.
- **Anthropic/OpenAI API keys:** ordinary keys do not expose a reliable prepaid
  balance. The plugin displays `unknown` instead of mislabeling historical spend
  as available capacity.

The plugin does not refresh rotating OAuth tokens. Run the owning CLI to refresh
expired credentials.

Amp's provider route is dynamic server-side state and cannot reliably be
inferred from model branding. Bind Amp panes explicitly when the billing account
is known.

## Development

`plugin link` does not run manifest build commands, so build first:

```bash
cargo test
bash bin/build.sh
herdr plugin link "$PWD"
```

Useful checks:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
herdr plugin log list --plugin shrivatsa.model-capacity
```

The original implementation brief and research notes are in [SPEC.md](SPEC.md).

## License

[MIT](LICENSE)
