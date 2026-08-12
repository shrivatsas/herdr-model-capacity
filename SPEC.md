# Model Capacity design specification

> **Implementation note:** The original brief below records the initial research
> direction. The shipped v1 decisions are narrower: the explicit account
> registry is authoritative; the normal pane is account-only; agent linkage is
> optional and disabled by default; ChatGPT quota is collected only through
> `codex app-server --stdio` for each configured `CODEX_HOME`; and Claude
> setup-token credentials referenced through macOS Keychain are shown as
> quota-unsupported because the OAuth usage endpoint rejects that credential
> type. The persistent pane defaults to a right split and can be replaced with a
> down split. README.md is the normative setup and security documentation.

## Goal

Add a lightweight Herdr view answering:

> **Which of my model/provider accounts has capacity available right now?**

This is deliberately different from AgentsView:

- **AgentsView:** historical consumption — tokens × model → estimated $ spent.
- **Herdr:** current capacity — quota/credits remaining + reset time.

Do **not** turn this into another usage analytics system.

---

## Starting Point

Use:

`gecm0/herdr-plugin-agents-usage`

as the primary implementation/base.

Also inspect:

`senna-lang/herdr-agent-usage`

for useful patterns around:

- provider/backend resolution
- sidebar integration
- low-quota warnings
- upstream contract/drift tests
- multiple agents sharing one billing account

Prefer extending the existing plugin rather than creating another collector.

---

# Agents / Harnesses

Support these coding agents:

```text
Claude Code
Codex
Pi
Ampcode
```

Pi and Ampcode are **agent harnesses**, not necessarily billing providers.

Resolve them to the underlying provider/account where possible.

Example:

```text
Claude Code ───────────────► Claude subscription
Claude Code ───────────────► Anthropic API credits

Codex ─────────────────────► ChatGPT/Codex subscription
Codex ─────────────────────► OpenAI API credits

Pi ─────────┬──────────────► Claude subscription
            ├──────────────► Codex subscription
            └──────────────► OpenRouter balance

Ampcode ────┬──────────────► Anthropic/OpenAI backend
            └──────────────► OpenRouter balance
```

The dashboard should represent **capacity at the billing-account level**, regardless of which harness consumes it.

---

# Providers

V1 providers:

1. **Anthropic / Claude**
2. **OpenAI / Codex**
3. **OpenRouter**

Do not add OpenCode Go yet.

---

# Multiple Accounts / Credit Sources

Assume there can be **multiple capacity lines for the same provider**.

For example:

```text
Claude
  Personal Max subscription
  Work Max subscription
  Anthropic API account

OpenAI
  Personal ChatGPT subscription
  Work ChatGPT subscription
  OpenAI API project

OpenRouter
  Personal account
  Work account
```

Do **not** collapse these into one provider-level number.

The identity should roughly be:

```text
Provider
    └── Account / credential
            └── Capacity lines
```

Example:

```text
Anthropic
├── personal-max
│   ├── 5h quota
│   └── weekly quota
│
├── work-max
│   ├── 5h quota
│   └── weekly quota
│
└── personal-api
    └── $42.18 credits/balance

OpenAI
├── personal-chatgpt
│   ├── 5h quota
│   └── weekly quota
│
└── work-api
    └── $73.40 credits/balance

OpenRouter
└── personal
    └── $18.72 balance
```

---

# Subscription vs API Capacity

Claude Code and Codex may run using either:

```text
subscription auth
```

or:

```text
API billing
```

The plugin should surface whichever capacity source corresponds to the credential/account.

### Subscription

Show quota windows:

```text
5h       72% remaining     resets 2h12m
weekly   48% remaining     resets Monday
```

### API

Show available monetary balance/credits where the provider exposes it:

```text
API credits    $42.18 remaining
```

If an API provider exposes rate-limit headroom separately, it may be included, but **balance/credits are the primary signal**.

---

# Normalized Model

Model this around accounts rather than agents:

```ts
type CapacityAccount = {
  provider: "anthropic" | "openai" | "openrouter"

  accountId: string
  label: string

  authType:
    | "subscription"
    | "api"
    | "oauth"
    | "unknown"

  limits: CapacityLimit[]

  fetchedAt: Date
}

type CapacityLimit = {
  name: string

  kind:
    | "quota"
    | "credits"
    | "balance"
    | "rate_limit"

  remaining?: number
  total?: number
  remainingPercent?: number

  unit:
    | "percent"
    | "credits"
    | "usd"
    | "requests"

  resetsAt?: Date
}
```

Separately maintain harness → account resolution:

```ts
type AgentBinding = {
  agent: "claude-code" | "codex" | "pi" | "amp"

  paneId?: string

  provider: string
  accountId: string

  model?: string
}
```

This separation is important.

---

# UI

The primary view should remain extremely compact.

```text
┌─ Model Capacity ─────────────────────────┐
│                                         │
│ CLAUDE                                  │
│                                         │
│ Personal Max                            │
│ 5h      ███████░░░ 72%    ↻ 2h12m     │
│ 7d      █████░░░░░ 48%    ↻ Mon       │
│                                         │
│ Work Max                                │
│ 5h      █████████░ 91%    ↻ 4h03m     │
│ 7d      ███████░░░ 74%    ↻ Sun       │
│                                         │
│ Anthropic API                           │
│ balance                  $42.18         │
│                                         │
│ CODEX                                   │
│                                         │
│ Personal ChatGPT                        │
│ 5h      ██████░░░░ 63%                 │
│ 7d      ████████░░ 82%                 │
│                                         │
│ OpenAI API                              │
│ balance                  $73.40         │
│                                         │
│ OPENROUTER                              │
│ Personal                 $18.72         │
└─────────────────────────────────────────┘
```

## Compact/sidebar view

Something like:

```text
Capacity
────────────────────
Claude P    72%
Claude W    91%
Claude API  $42

Codex P     63%
OpenAI API  $73

OpenRouter  $18
```

The detailed popup/pane shows all windows and reset times.

---

# Agent Awareness

Herdr should ideally show which capacity source an active agent is consuming.

Example:

```text
Agents
────────────────────────────
● claude-1
  Claude · Personal Max
  72% remaining

● pi-2
  OpenRouter · Personal
  $18.72

● codex-3
  Codex · Work ChatGPT
  91% remaining

● amp-4
  Anthropic API · Work
  $42.18
```

This does **not** mean maintaining separate quota for each agent.

Both:

```text
Pi → Personal Claude Max
Claude Code → Personal Claude Max
```

should point to the **same capacity object**.

---

# Collection

Prefer data sources in this order:

1. provider/account-level quota or balance API
2. authenticated CLI/provider endpoint
3. structured local auth/state
4. local-session inference

Avoid scraping rendered terminal output.

Provider adapters:

```text
providers/
  anthropic/
  openai/
  openrouter/
```

Harness/account resolution:

```text
agents/
  claude-code/
  codex/
  pi/
  amp/
```

The two layers should remain separate.

---

# Refresh / State

Keep the system mostly stateless:

```text
Provider APIs / local auth
          ↓
     adapters
          ↓
 CapacityAccounts
          ↑
 agent bindings
          ↓
      Herdr UI
```

Refresh approximately every 1–5 minutes, respecting provider limits.

Cache the latest successful response so temporary provider/API failures do not blank the pane.

No historical database in V1.

---

# Failure Handling

Provider interfaces may be undocumented or unstable.

Each provider must fail independently:

```text
Claude Personal       72%
Claude Work           unavailable ⚠
OpenAI Personal       84%
OpenRouter            $18.72
```

Never make one broken provider prevent the rest of the pane rendering.

Also distinguish:

```text
unknown
unavailable
stale
zero remaining
```

These are different states.

---

# Low-Capacity Warnings

Simple thresholds are enough initially.

Examples:

```text
subscription < 20%      → warning
subscription < 10%      → critical

API balance < configured threshold → warning
```

API thresholds should be configurable because `$10 remaining` means very different things to different users.

---

# Explicitly Out of Scope

Do not implement:

- historical token tracking
- historical $ spent
- per-session analytics
- context-window tracking
- conversation history
- charts
- billing reconciliation
- automatic agent routing

Those belong elsewhere, particularly AgentsView.

---

# Future Direction

The normalized model should eventually make this possible:

```text
Available Capacity

Claude Personal    12% ⚠
Claude Work        76% ●
Codex Personal     68% ●
OpenAI API         $73 ●
OpenRouter         $18 ●
```

and later:

```text
Starting a new Pi agent...

Claude Personal    low
Claude Work        healthy
Codex Personal     healthy
OpenRouter         healthy

Suggested backend → Claude Work
```

Eventually routing policy could use this information, but **do not implement routing in V1**.

---

# Definition of Done

While working in Herdr, I should be able to glance at one place and answer:

> **Across all of my Claude, Codex/OpenAI and OpenRouter accounts, how much usable capacity is left and when do subscription quotas reset?**

And for an active Claude Code, Codex, Pi or Ampcode pane:

> **Which account/provider is this agent currently consuming?**

## First implementation task

Before writing significant new code:

1. inspect `gecm0/herdr-plugin-agents-usage`
2. inspect `senna-lang/herdr-agent-usage`
3. inspect how Claude Code, Codex, Pi and Ampcode persist/resolve their authentication/provider
4. identify reliable quota/balance sources for Anthropic, OpenAI and OpenRouter
5. map those findings onto the `CapacityAccount + AgentBinding` model above
6. propose the smallest patch to the existing `gecm0` plugin

**Patch and reuse before redesigning.**

---

# Implementation

This directory now contains a Rust Herdr plugin based on the process/ANSI pane
design from `gecm0/herdr-plugin-agents-usage`, with the data model changed from
provider usage tuples to account-level remaining capacity.

## Run locally

Building from source requires a current stable Rust toolchain with Cargo.

```bash
cargo test
bash bin/build.sh
herdr plugin link "$PWD"
herdr plugin pane open --plugin shrivatsa.model-capacity --entrypoint capacity
```

The compact entrypoint is `capacity-compact`. In either pane, `r` bypasses the
1–5 minute cache and refreshes all accounts; any other key closes the pane.

Without configuration the plugin conservatively discovers the default Claude
Code and Codex homes plus `OPENROUTER_API_KEY`. Multiple accounts and agent
bindings are configured in `~/.config/herdr/model-capacity.json` (override with
`HERDR_CAPACITY_CONFIG`):

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

An OpenRouter inference key uses the documented `/api/v1/key` endpoint and
shows its key limit. An OpenRouter management key uses `/api/v1/credits` and
shows account-wide purchased credits minus usage. Secrets are referenced by
environment-variable name and are never stored in plugin state or diagnostics.

Claude and Codex subscription quota endpoints are currently undocumented. The
plugin caches their normalized responses, lets each account fail independently,
and falls back to Codex's structured local rate-limit snapshots. It deliberately
does not refresh rotating OAuth tokens; run the owning CLI to refresh them.
Anthropic/OpenAI ordinary API keys do not expose a reliable prepaid balance, so
those lines are shown as `unknown` rather than presenting historical spend as
available capacity.

Agent bindings are inferred only when one billing account unambiguously matches
Claude Code, Codex, or Pi's configured provider. Multiple matching accounts
require an explicit binding. Ampcode routing is dynamic server-side state and is
never guessed from model branding, so Amp panes require an explicit binding.
