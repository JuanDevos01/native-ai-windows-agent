# Metis — Comprehensive Testing & Operations Guide

> **Generated**: 2026-02-12
> **Source**: Full codebase analysis of `Metis/` workspace

---

## Table of Contents

- [1. Project Overview](#1-project-overview)
- [2. Prerequisites](#2-prerequisites)
- [3. Building](#3-building)
- [4. Configuration](#4-configuration)
  - [4.1 Config File Location & Format](#41-config-file-location--format)
  - [4.2 Full Config JSON Schema](#42-full-config-json-schema)
  - [4.3 Environment Variable Overrides](#43-environment-variable-overrides)
  - [4.4 Config Loading Precedence](#44-config-loading-precedence)
- [5. CLI Commands Reference](#5-cli-commands-reference)
  - [5.1 `Metis onboard`](#51-Metis-onboard)
  - [5.2 `Metis status`](#52-Metis-status)
  - [5.3 `Metis agent`](#53-metis-agent)
  - [5.4 `Metis gateway`](#54-Metis-gateway)
  - [5.5 `Metis cron`](#55-metis-cron)
  - [5.6 `Metis channels`](#56-metis-channels)
- [6. Providers (LLM Backends)](#6-providers-llm-backends)
- [7. Channels (Chat Integrations)](#7-channels-chat-integrations)
- [8. Skills (Bundled)](#8-skills-bundled)
- [9. Workspace Templates](#9-workspace-templates)
- [10. Docker](#10-docker)
- [11. Testing Procedures](#11-testing-procedures)

---

## 1. Project Overview

Metis is a Rust reimplementation of [nanobot](https://github.com/HKUDS/nanobot) (Python). It produces a **single static binary** with the goals of:

| Metric | Target |
|--------|--------|
| RAM | < 5 MB |
| Boot time | < 1 s |
| Binary distribution | Single file, no runtime deps |
| Cross-compile | x86_64, ARM64, RISC-V |

### Workspace Crates

```
Metis/
├── Cargo.toml                     # Workspace root
├── Dockerfile                     # Multi-stage build (Rust + Node.js bridge)
├── bridge/                        # Node.js WhatsApp bridge (Baileys)
└── crates/
    ├── metis-core/               # Traits, types, bus, config, utils, heartbeat, session
    ├── metis-agent/              # Agent loop, tools, context, memory, skills
    ├── metis-providers/          # HTTP clients for 12 LLM providers
    ├── metis-channels/           # Chat channel integrations (Telegram, Discord, etc.)
    ├── metis-cron/               # Scheduled task engine
    └── metis-cli/                # Binary entry point — all CLI commands
```

---

## 2. Prerequisites

- **Rust** ≥ 1.84 (edition 2021, resolver v2)
- **Node.js** 20 (only for WhatsApp bridge)
- **Git** (runtime dependency for some agent tools)
- **tmux** (optional, for the tmux skill)
- **gh** CLI (optional, for the GitHub skill)
- **curl** (optional, for the weather skill)

---

## 3. Building

### Default build (no channels — CLI-only mode)

```bash
cargo build --release
```

Binary output: `target/release/Metis`

### Build with specific channel features

```bash
# Single channel
cargo build --release --features "telegram"

# Multiple channels
cargo build --release --features "telegram,discord,slack"

# All channels (what the Dockerfile uses)
cargo build --release --features "telegram,discord,whatsapp,slack,email"
```

### Available Feature Flags

| Feature | Crate Gate | Description |
|---------|-----------|-------------|
| `telegram` | `metis-channels/telegram` | Telegram bot via teloxide |
| `discord` | `metis-channels/discord` | Discord bot via WebSocket gateway |
| `whatsapp` | `metis-channels/whatsapp` | WhatsApp via Node.js bridge (Baileys) |
| `slack` | `metis-channels/slack` | Slack bot via Socket Mode (WebSocket) |
| `email` | `metis-channels/email` | Email channel via IMAP polling + SMTP sending |

### Release Profile Optimizations

```toml
[profile.release]
opt-level = "z"      # Size-optimized
lto = true           # Link-time optimization
strip = true         # Strip debug symbols
codegen-units = 1    # Better optimization
panic = "abort"      # Smaller binary
```

### Run tests

```bash
cargo test --workspace
```

---

## 4. Configuration

### 4.1 Config File Location & Format

| Item | Value |
|------|-------|
| **Data directory** | `~/.metis/` |
| **Config file** | `~/.metis/config.json` |
| **Format** | JSON with **camelCase** keys |
| **Default workspace** | `~/.metis/workspace/` |
| **Sessions** | `~/.metis/sessions/` |
| **REPL history** | `~/.metis/history/cli_history` |

### 4.2 Full Config JSON Schema

Below is the **complete** `config.json` structure with every field and its default value:

```json
{
  "agents": {
    "defaults": {
      "workspace": "~/.metis/workspace",
      "model": "anthropic/claude-sonnet-4-20250514",
      "maxTokens": 8192,
      "temperature": 0.7,
      "maxToolIterations": 20
    }
  },
  "providers": {
    "anthropic": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "openai": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "openrouter": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "deepseek": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "groq": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "zhipu": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "dashscope": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "vllm": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "gemini": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "moonshot": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "minimax": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    },
    "aihubmix": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": null
    }
  },
  "channels": {
    "telegram": {
      "token": "",
      "allowedUsers": []
    },
    "discord": {
      "token": "",
      "allowedUsers": []
    },
    "whatsapp": {
      "bridgeUrl": "",
      "allowedUsers": []
    },
    "feishu": {
      "appId": "",
      "appSecret": "",
      "allowedUsers": []
    },
    "dingtalk": {
      "clientId": "",
      "clientSecret": "",
      "allowedUsers": []
    },
    "slack": {
      "botToken": "",
      "appToken": "",
      "allowedUsers": [],
      "groupPolicy": "mention",
      "groupAllowFrom": [],
      "dm": {
        "enabled": true,
        "policy": "open",
        "allowFrom": []
      }
    },
    "email": {
      "imapHost": "",
      "imapPort": 993,
      "imapUsername": "",
      "imapPassword": "",
      "imapMailbox": "INBOX",
      "imapUseSsl": true,
      "smtpHost": "",
      "smtpPort": 587,
      "smtpUsername": "",
      "smtpPassword": "",
      "smtpUseTls": true,
      "smtpUseSsl": false,
      "fromAddress": "",
      "pollIntervalSeconds": 30,
      "markSeen": true,
      "maxBodyChars": 12000,
      "subjectPrefix": "Re: ",
      "allowedUsers": []
    },
    "qq": {
      "appId": "",
      "token": "",
      "appSecret": "",
      "allowedUsers": []
    },
    "mochat": {
      "url": "",
      "allowedUsers": [],
      "mention": { "enabled": false },
      "groups": {}
    }
  },
  "tools": {
    "web": {
      "search": {
        "apiKey": "",
        "maxResults": 5
      }
    },
    "exec": {
      "timeout": 60,
      "shell": "powershell",
      "permissionMode": "unsafe_only"
    },
    "restrictToWorkspace": false
  },
  "gateway": {
    "host": "0.0.0.0",
    "port": 18790
  },
  "transcription": {
    "enabled": true,
    "provider": "groq",
    "apiBase": null,
    "apiKey": "",
    "model": "whisper-large-v3"
  }
}
```

### 4.3 Environment Variable Overrides

All env vars use the `METIS_` prefix with `__` (double underscore) as the section delimiter.

| Environment Variable | Config Path | Example |
|---------------------|-------------|---------|
| `metis_agentS__DEFAULTS__MODEL` | `agents.defaults.model` | `openai/gpt-4o` |
| `metis_agentS__DEFAULTS__MAX_TOKENS` | `agents.defaults.maxTokens` | `4096` |
| `metis_agentS__DEFAULTS__TEMPERATURE` | `agents.defaults.temperature` | `0.5` |
| `metis_agentS__DEFAULTS__MAX_TOOL_ITERATIONS` | `agents.defaults.maxToolIterations` | `30` |
| `metis_agentS__DEFAULTS__WORKSPACE` | `agents.defaults.workspace` | `/data/workspace` |
| `metis_providers__ANTHROPIC__API_KEY` | `providers.anthropic.apiKey` | `sk-ant-...` |
| `metis_providers__ANTHROPIC__API_BASE` | `providers.anthropic.apiBase` | `https://custom/v1` |
| `metis_providers__OPENAI__API_KEY` | `providers.openai.apiKey` | `sk-...` |
| `metis_providers__OPENROUTER__API_KEY` | `providers.openrouter.apiKey` | `sk-or-...` |
| `metis_providers__DEEPSEEK__API_KEY` | `providers.deepseek.apiKey` | `ds-...` |
| `metis_providers__GROQ__API_KEY` | `providers.groq.apiKey` | `gsk_...` |
| `metis_providers__GEMINI__API_KEY` | `providers.gemini.apiKey` | `AI...` |
| `metis_providers__ZHIPU__API_KEY` | `providers.zhipu.apiKey` | |
| `metis_providers__DASHSCOPE__API_KEY` | `providers.dashscope.apiKey` | |
| `metis_providers__VLLM__API_KEY` | `providers.vllm.apiKey` | |
| `metis_providers__MOONSHOT__API_KEY` | `providers.moonshot.apiKey` | |
| `metis_providers__MINIMAX__API_KEY` | `providers.minimax.apiKey` | |
| `metis_providers__AIHUBMIX__API_KEY` | `providers.aihubmix.apiKey` | |
| `METIS_GATEWAY__HOST` | `gateway.host` | `127.0.0.1` |
| `METIS_GATEWAY__PORT` | `gateway.port` | `9090` |
| `METIS_TOOLS__RESTRICT_TO_WORKSPACE` | `tools.restrictToWorkspace` | `true` / `1` |
| `METIS_TOOLS__EXEC__TIMEOUT` | `tools.exec.timeout` | `120` |
| `METIS_TOOLS__EXEC__SHELL` | `tools.exec.shell` | `powershell` / `cmd` / `sh` |
| `METIS_TOOLS__EXEC__PERMISSION_MODE` | `tools.exec.permissionMode` | `unsafe_only` / `always` / `poweruser` |

`tools.exec.permissionMode`:
- `unsafe_only` (default): asks permission for unsafe commands only.
- `always`: asks permission before every command.
- `poweruser`: no safety restrictions, except data-exfiltration commands still require approval.

Security invariant:
- Potential data exfiltration commands (mail/ftp/scp/upload patterns) always require approval.

**`exec` tool output contract:** When the tool actually ran the subprocess (normal exit, non-zero exit), or exhausted the configured timeout, the result includes markers `<<<EXEC_RESULT>>>` … `<<<END_EXEC_RESULT>>>` with at least `COMMAND`, `WORKING_DIR`, `SHELL_BACKEND`, and `STATUS`. Completed runs include `EXIT_CODE` and labeled `STDOUT` / `STDERR` sections; timeouts include `TIMEOUT_SECONDS` instead of streaming output. Approval-required prompts and outright blocks omit this fence—do not treat free-form assistant text alone as proof that something ran.

### 4.4 Config Loading Precedence

1. **Defaults** — `Config::default()` (hardcoded in `schema.rs`)
2. **JSON file** — `~/.metis/config.json` (merged on top of defaults)
3. **Environment variables** — `METIS_*` (override everything)

If the JSON file doesn't exist or is invalid, defaults are used silently (with a `warn!` log).

**Legacy migration**: `tools.exec.restrictToWorkspace` is auto-migrated to `tools.restrictToWorkspace`.

### 4.5 Common Config Pitfall (Telegram `allowedUsers`)

`channels.telegram.allowedUsers` must be an array of **strings**, not integers.

Correct:

```json
"telegram": {
  "token": "...",
  "allowedUsers": ["8582973375"]
}
```

Incorrect (breaks config deserialization):

```json
"allowedUsers": [8582973375]
```

If deserialization fails, Metis falls back to defaults. That can cascade into provider/model errors like:

`No configured provider found for model 'anthropic/claude-sonnet-4-20250514'`

because the default model is loaded while your intended configured provider key/model are skipped.

---

## 5. CLI Commands Reference

Binary name: `Metis`

### 5.1 `Metis onboard`

Initialize configuration and workspace. Safe to run multiple times (idempotent).

```bash
Metis onboard
```

Important:
- Main config is `~/.metis/config.json` (not `~/.metis/workspace/config.json`).
- Cron is a built-in feature. Use `Metis cron add/list/run` with `Metis gateway` for scheduled automation.
- Cron commands are always nested: `Metis cron <subcommand> ...` (example: `Metis cron remove <id>`).
- `Metis remove <id>` is invalid and will fail with "unrecognized subcommand".
- If PATH/cwd is ambiguous (especially for agent shell calls), run the explicit binary path:
  `target/release/Metis.exe cron ...` and `target/release/Metis.exe gateway --restart`.

**Creates:**

| Path | Description |
|------|-------------|
| `~/.metis/config.json` | Default config (if not exists) |
| `~/.metis/workspace/` | Agent workspace directory |
| `~/.metis/workspace/memory/` | Long-term memory directory |
| `~/.metis/workspace/memory/MEMORY.md` | Memory template |
| `~/.metis/workspace/AGENTS.md` | Agent personality config |
| `~/.metis/workspace/SOUL.md` | Agent soul/personality |
| `~/.metis/workspace/USER.md` | User profile template |
| `~/.metis/workspace/HEARTBEAT.md` | Periodic task definitions |
| `~/.metis/workspace/skills/skill-creator/SKILL.md` | Skill creation instructions |
| `~/.metis/sessions/` | Session persistence |
| `~/.metis/history/` | REPL history |

### 5.2 `Metis status`

Display configuration status — config path, workspace, model, provider API key status, Brave Search status.

```bash
Metis status
```

Sample output:
```
🦀 Metis Status

  Config:            ~/.metis/config.json ✓
  Workspace:         ~/.metis/workspace ✓
  Model:             anthropic/claude-sonnet-4-20250514
  Parameters:        temp: 0.7 | max_tokens: 8192

  Providers:
    OpenRouter           · not configured
    AiHubMix             · not configured
    Anthropic            ✓ (key set)
    OpenAI               · not configured
    ...

  Brave Search:      · not configured
```

### 5.3 `Metis agent`

Chat with the AI agent. Two modes: **single-shot** and **interactive REPL**.

```bash
# Interactive REPL (default)
Metis agent

# Single-shot message
Metis agent -m "What is Rust?"

# Custom session ID
Metis agent -s "project:myapp"

# With debug logging
Metis agent --logs

# Disable markdown rendering
Metis agent --no-markdown
```

**Arguments:**

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--message` | `-m` | *(none → REPL)* | Single message (non-interactive) |
| `--session` | `-s` | `cli:default` | Session identifier (`channel:id` format) |
| `--no-markdown` | | `false` | Disable Markdown rendering |
| `--logs` | | `false` | Enable debug logging |

**REPL exit commands**: `exit`, `quit`, `/exit`, `/quit`, `:q`, Ctrl-C, Ctrl-D

**REPL history**: Persisted to `~/.metis/history/cli_history` (max 1000 entries).

### 5.4 `Metis gateway`

Start the full gateway — agent loop + all configured channels + cron service + heartbeat, running concurrently via `tokio::select!`.

```bash
Metis gateway
Metis gateway --logs
```

**Startup sequence:**
1. Load config
2. Resolve workspace (create if needed)
3. Create message bus (mpsc, capacity 100)
4. Create LLM provider from model name
5. Create session manager
6. Create and wire `AgentLoop`
7. Create `CronService` with agent callback
8. Create `HeartbeatService` (checks `HEARTBEAT.md` every 30 min)
9. Register enabled channels (feature-gated + config-gated)
10. Run all concurrently: agent loop, channel manager, cron, heartbeat
11. Ctrl-C triggers graceful shutdown

**Channel registration logic** — a channel is registered only if:
1. The feature flag was enabled at compile time (e.g., `--features telegram`)
2. The channel has required config values set (e.g., `telegram.token` is non-empty)

**Exposed ports:**
- `18790` — Gateway HTTP port (configurable)
- `3001` — WhatsApp bridge WebSocket (internal)

### 5.5 `Metis cron`

Manage scheduled tasks.

```bash
# List enabled jobs
Metis cron list

# List all jobs (including disabled)
Metis cron list --all

# Add interval-based job (every 600 seconds)
Metis cron add --name "check-stars" --message "Check GitHub stars" --every 600

# Add cron-expression job
Metis cron add --name "morning-brief" --message "Daily summary" --cron "0 9 * * *"

# Add one-time job at a specific time
Metis cron add --name "reminder" --message "Call dentist" --at "2026-03-01T09:00:00"

# Add job with channel delivery
Metis cron add --name "alert" --message "Server health check" --every 300 \
  --deliver --channel telegram --to "123456789"

# Remove a job
Metis cron remove abc12345

# Enable/disable a job
Metis cron enable abc12345
Metis cron enable abc12345 --disable

# Manually trigger a job
Metis cron run abc12345
```

Agent/operator reliability notes:
- Prefer explicit, full subcommands (`Metis cron remove <id>`, not shorthand).
- `Metis cron ...` edits `~/.metis/cron/jobs.json`; restarting gateway reloads changed jobs.
- On Windows, if `Metis` is not discoverable in PATH for a shell tool, use:
  `target/release/Metis.exe cron ...`.

### 5.6 `Metis channels`

Manage chat channels.

```bash
# Show channel configuration status
Metis channels status

# Link WhatsApp via QR code (starts the bridge)
Metis channels login
```

---

## 6. Providers (LLM Backends)

12 providers supported, matched by keyword in the model name (priority order):

| # | Provider | Keywords | Env Var | Default API Base | Notes |
|---|----------|----------|---------|-----------------|-------|
| 1 | **OpenRouter** | `openrouter` | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1` | Gateway; auto-detected by `sk-or-` prefix |
| 2 | **AiHubMix** | `aihubmix` | `OPENAI_API_KEY` | `https://aihubmix.com/v1` | Gateway; strips model prefix, re-adds `openai/` |
| 3 | **Anthropic** | `anthropic`, `claude` | `ANTHROPIC_API_KEY` | *(default)* | |
| 4 | **OpenAI** | `openai`, `gpt` | `OPENAI_API_KEY` | *(default)* | |
| 5 | **DeepSeek** | `deepseek` | `DEEPSEEK_API_KEY` | *(default)* | Prefix: `deepseek/` |
| 6 | **Gemini** | `gemini` | `GEMINI_API_KEY` | *(default)* | Prefix: `gemini/` |
| 7 | **ZhiPu** | `zhipu`, `glm`, `zai` | `ZAI_API_KEY` | *(default)* | Prefix: `zai/` |
| 8 | **DashScope** | `qwen`, `dashscope` | `DASHSCOPE_API_KEY` | *(default)* | Prefix: `dashscope/` |
| 9 | **Moonshot** | `moonshot`, `kimi` | `MOONSHOT_API_KEY` | `https://api.moonshot.ai/v1` | Kimi K2.5 forces temp=1.0 |
| 10 | **MiniMax** | `minimax` | `MINIMAX_API_KEY` | `https://api.minimax.io/v1` | |
| 11 | **vLLM** | `vllm` | `HOSTED_VLLM_API_KEY` | *(custom)* | Self-hosted, requires `apiBase` |
| 12 | **Groq** | `groq` | `GROQ_API_KEY` | *(default)* | Also used for voice transcription |

### Model Resolution

The model string format is `provider/model-name` (e.g., `anthropic/claude-sonnet-4-20250514`).

1. The provider is extracted by keyword matching against model name
2. If `strip_model_prefix` is set (AiHubMix), the existing prefix is stripped
3. If the provider has a `prefix` and the model doesn't start with a `skip_prefix`, the prefix is prepended

### Transcription

Telegram voice/audio uses an **HTTP** endpoint compatible with OpenAI’s multipart `audio/transcriptions` API.

### whisper.cpp (no HTTP, Windows-native)

If you prefer running `whisper.cpp` locally (no Groq, no HTTP bridge), set:

Prerequisite: download/build `whisper.cpp` first so `whisper-cli.exe` exists, and download a local model file (e.g. `ggml-base.bin`).
This setup guide is focused on **Windows-native** operation rather than Linux-first deployment.

```json
"transcription": {
  "enabled": true,
  "provider": "whisper_cpp",
  "whisperCpp": {
    "exePath": "whisper-cli.exe",
    "modelPath": "models/ggml-base.bin",
    "extraArgs": []
  }
}
```

**Provider `groq` (default):** Groq-hosted Whisper (`whisper-large-v3` default model). Key resolution:

1. `transcription.apiKey`
2. `providers.groq.apiKey`
3. `GROQ_API_KEY` env var

**Provider `local` / `openai_compatible` / `custom`:** self-hosted Whisper / gateway. Requires non-empty **`transcription.apiBase`** (`http://127.0.0.1:8080/v1`-style preferred). Optional `transcription.apiKey` sends a Bearer token.

Env overrides: `METIS_TRANSCRIPTION__ENABLED`, `METIS_TRANSCRIPTION__PROVIDER`, `METIS_TRANSCRIPTION__API_BASE`, `METIS_TRANSCRIPTION__API_KEY`, `METIS_TRANSCRIPTION__MODEL`.

---

## 7. Channels (Chat Integrations)

Each channel is behind a **compile-time feature flag** AND a **runtime config check**.

| Channel | Feature Flag | Required Config | Compile Deps |
|---------|-------------|-----------------|--------------|
| **Telegram** | `telegram` | `channels.telegram.token` | teloxide, futures-util |
| **Discord** | `discord` | `channels.discord.token` | tokio-tungstenite, reqwest |
| **WhatsApp** | `whatsapp` | `channels.whatsapp.bridgeUrl` | tokio-tungstenite (+ bridge) |
| **Slack** | `slack` | `channels.slack.botToken` + `appToken` | tokio-tungstenite, reqwest |
| **Email** | `email` | `channels.email.imapHost` | lettre, mailparse, tokio-rustls |

### Slack Access Control

- **Group policy**: `"mention"` (default — respond to @mentions), `"open"`, or `"allowlist"`
- **DM policy**: `"open"` (default) or `"allowlist"`
- `allowedUsers`: Flat list of user IDs (empty = everyone)

### Email Channel

- Inbound: IMAP polling (configurable interval, default 30s)
- Outbound: SMTP with STARTTLS (port 587) or implicit TLS/SMTPS (port 465)
- Thread tracking via subject prefix + `In-Reply-To` headers

---

## 8. Skills (Bundled)

Skills are Markdown instruction files loaded by the agent. They are bundled in `crates/metis-agent/skills/`.

| Skill | Description |
|-------|-------------|
| **skill-creator** | Meta-skill: guides the agent on how to create new skills |
| **weather** | Get weather via `wttr.in` (no API key, uses `curl`) |
| **cron** | Schedule reminders and recurring tasks via the `cron` tool |
| **tmux** | Remote-control tmux sessions for interactive CLIs |
| **github** | Interact with GitHub via the `gh` CLI |
| **summarize** | Summarize URLs, articles, YouTube videos via `summarize.sh` |

### Skill Directory Structure

```
skill-name/
├── SKILL.md          # Required — instructions for the agent
├── scripts/          # Optional — shell/python scripts
├── references/       # Optional — extra docs
└── assets/           # Optional — templates, configs
```

Skills are also created in the user's workspace at `~/.metis/workspace/skills/`.

---

## 9. Workspace Templates

Created by `Metis onboard`:

### SOUL.md
```markdown
# Soul

I am Metis, a lightweight AI assistant built in Rust.

## Personality
- Helpful and friendly
- Concise and to the point
- Curious and eager to learn

## Values
- Accuracy over speed
- User privacy and safety
- Transparency in actions
```

### USER.md
```markdown
# User Profile

Tell Metis about yourself so it can personalize its responses.

## About Me
- **Name**: (your name)
- **Role**: (your role/profession)
- **Preferences**: (communication preferences)
```

### HEARTBEAT.md
```markdown
# Heartbeat Tasks

This file is checked every 30 minutes by your Metis agent.
Add tasks below that you want the agent to work on periodically.

If this file has no tasks (only headers and comments), the agent will skip the heartbeat.

## Active Tasks
<!-- Add your periodic tasks below this line -->

## Completed
<!-- Move completed tasks here or delete them -->
```

### AGENTS.md
```markdown
# Agents

Configuration and personality for your AI agents.

## Default Agent: Metis
- **Name**: Metis
- **Role**: Personal AI assistant
- **Style**: Concise, helpful, technical when needed
```

### MEMORY.md (`memory/`)
```markdown
# Long-term Memory

Metis persists important information here automatically.
You can also edit this file directly.
```

---

## 10. Docker

### Multi-stage build (3 stages)

```
Stage 1 (builder):        rust:1.84-bookworm        — compiles Rust binary with all features
Stage 2 (bridge-builder): node:20-bookworm-slim      — compiles TypeScript WhatsApp bridge
Stage 3 (runtime):        debian:bookworm-slim       — minimal runtime with Node.js 20 for bridge sidecar
```

### Build

```bash
docker build -t Metis .
```

### Run

```bash
# Basic run (shows status by default)
docker run --rm Metis

# Override to start gateway
docker run -d \
  -e metis_providers__ANTHROPIC__API_KEY=sk-ant-xxx \
  -v metis-data:/home/metis/.metis \
  -p 18790:18790 \
  Metis gateway

# With Telegram channel
docker run -d \
  -e metis_providers__ANTHROPIC__API_KEY=sk-ant-xxx \
  -e metis_agentS__DEFAULTS__MODEL=anthropic/claude-sonnet-4-20250514 \
  -v metis-data:/home/metis/.metis \
  -p 18790:18790 \
  Metis gateway --logs

# With WhatsApp bridge (expose both ports)
docker run -d \
  -e metis_providers__ANTHROPIC__API_KEY=sk-ant-xxx \
  -v metis-data:/home/metis/.metis \
  -p 18790:18790 \
  -p 3001:3001 \
  Metis gateway --logs
# Note: start the bridge sidecar separately inside the container:
# docker exec -d <container> node /usr/share/Metis/bridge/dist/index.js
```

### Docker details

| Item | Value |
|------|-------|
| User | `Metis` (non-root) |
| Config | `/home/metis/.metis/` |
| Workspace | `/home/metis/workspace/` |
| Skills (bundled) | `/usr/share/Metis/skills/` |
| Bridge dist | `/usr/share/Metis/bridge/dist/` |
| Bridge modules | `/usr/share/Metis/bridge/node_modules/` |
| Ports | `18790` (gateway HTTP), `3001` (WhatsApp bridge WS) |
| Default CMD | `Metis status` |
| Entrypoint | `Metis` |
| Binary | `/usr/local/bin/Metis` |
| Runtime deps | `ca-certificates`, `curl`, `git`, `tmux`, `nodejs` |

---

## 11. Testing Procedures

### Quick Smoke Test

```bash
# 1. Build
cargo build --release

# 2. Initialize
./target/release/Metis onboard

# 3. Check status
./target/release/Metis status

# 4. Test single-shot (requires a provider API key)
metis_providers__ANTHROPIC__API_KEY=sk-ant-xxx \
  ./target/release/Metis agent -m "Hello, what are you?"

# 5. Test REPL
metis_providers__ANTHROPIC__API_KEY=sk-ant-xxx \
  ./target/release/Metis agent
```

### Unit Tests

```bash
# All workspace tests
cargo test --workspace

# Specific crate tests
cargo test -p metis-core
cargo test -p metis-agent
cargo test -p metis-providers
cargo test -p metis-channels
cargo test -p metis-cron
cargo test -p metis-cli
```

### Test with Features

```bash
# Test with telegram feature
cargo test --workspace --features "telegram"

# Test with all features
cargo test --workspace --features "telegram,discord,whatsapp,slack,email"
```

### Config Verification Tests

```bash
# Verify default config generates correctly
cargo test -p metis-core test_default_config
cargo test -p metis-core test_config_json_uses_camel_case
cargo test -p metis-core test_config_serialization_round_trip

# Verify env overrides work
cargo test -p metis-core test_env_override_model
cargo test -p metis-core test_env_override_provider_key
cargo test -p metis-core test_env_override_gateway_port

# Verify partial JSON loads correctly
cargo test -p metis-core test_partial_json_uses_defaults
cargo test -p metis-core test_empty_json_gives_defaults

# Verify legacy migration
cargo test -p metis-core test_migrate_restrict_to_workspace
```

### Onboard Test

```bash
# Verify template creation
cargo test -p metis-cli create_template_new_file
cargo test -p metis-cli create_template_existing_file
cargo test -p metis-cli templates_not_empty
```

### Integration Test (Gateway Channels)

```bash
# With a real Telegram token
metis_providers__ANTHROPIC__API_KEY=sk-ant-xxx \
  Metis gateway --logs

# Verify in logs:
#   "registered telegram channel"
#   "gateway starting"
#   Model, workspace, and channel count printed
```

### Minimal Config for Testing

Create `~/.metis/config.json`:

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-sonnet-4-20250514",
      "maxTokens": 4096,
      "temperature": 0.5
    }
  },
  "providers": {
    "anthropic": {
      "apiKey": "sk-ant-YOUR-KEY-HERE"
    }
  }
}
```

Or using only environment variables (no config file needed):

```bash
export metis_providers__ANTHROPIC__API_KEY=sk-ant-YOUR-KEY-HERE
export metis_agentS__DEFAULTS__MODEL=anthropic/claude-sonnet-4-20250514
```

### Full Channel Test Config

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-sonnet-4-20250514",
      "maxTokens": 8192,
      "temperature": 0.7,
      "maxToolIterations": 20
    }
  },
  "providers": {
    "anthropic": { "apiKey": "sk-ant-..." },
    "groq": { "apiKey": "gsk_..." }
  },
  "channels": {
    "telegram": {
      "token": "bot123:ABC...",
      "allowedUsers": ["your_telegram_id"]
    },
    "slack": {
      "botToken": "xoxb-...",
      "appToken": "xapp-...",
      "groupPolicy": "mention",
      "dm": { "enabled": true, "policy": "open" }
    },
    "email": {
      "imapHost": "imap.gmail.com",
      "imapPort": 993,
      "imapUsername": "you@gmail.com",
      "imapPassword": "app-password",
      "smtpHost": "smtp.gmail.com",
      "smtpPort": 587,
      "smtpUsername": "you@gmail.com",
      "smtpPassword": "app-password"
    }
  },
  "tools": {
    "web": {
      "search": {
        "apiKey": "brave-api-key",
        "maxResults": 5
      }
    },
    "exec": {
      "timeout": 120,
      "shell": "powershell",
      "permissionMode": "unsafe_only"
    },
    "restrictToWorkspace": false
  },
  "transcription": {
    "enabled": true,
    "provider": "groq",
    "model": "whisper-large-v3"
  },
  "gateway": {
    "host": "0.0.0.0",
    "port": 18790
  }
}
```
