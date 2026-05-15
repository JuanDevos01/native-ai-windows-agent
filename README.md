<div align="center">
  <h1>🤖 Metis: Ultra-Lightweight Personal AI Assistant in Rust</h1>
  <p>
    <img src="https://img.shields.io/badge/rust-≥1.84-orange?logo=rust" alt="Rust">
    <img src="https://img.shields.io/badge/license-MIT-green" alt="License">
    <img src="https://img.shields.io/badge/tests-273%2B%20passed-brightgreen" alt="Tests">
    <img src="https://img.shields.io/badge/RAM-<%208MB-blue" alt="RAM">
    <img src="https://img.shields.io/badge/binary-static%20single%20file-blue" alt="Binary">
  </p>
  <p><em>Metis is a Native Windows enhanced Rust reimplementation of <a href="https://github.com/HKUDS/nanobot">nanobot</a> created by oxibot for educational and research purposes.</em></p>
</div>

---

🦀 **Metis** is an **ultra-lightweight** personal AI assistant built entirely in Rust.

⚡️ Ships as a **single static binary** (~18K lines of Rust) with no Python, no pip, no runtime dependencies.

🎯 **Feature-complete** port of nanobot: same config format, same channels, same skills, same CLI.

> [!IMPORTANT]
> Metis is currently in **beta**. Some agent-side command narration/recovery paths are still being hardened, especially in cross-channel (Telegram/gateway) workflows.

| Metric | Metis | nanobot |
|--------|--------|---------|
| Language | Rust | Python |
| Binary | Single static file | pip install + 50+ deps |
| RAM | < 8 MB | ~50-100 MB |
| Startup | < 1 s | 2-4 s |
| LOC (core) | ~18,372 | ~3,510 |
| Cross-compile | x86_64, ARM64, RISC-V | Python-only |

## 📦 Install

### From source (recommended)

```bash
git clone https://github.com/DioCrafts/Metis.git
cd Metis
cargo build --release
```

Binary output: `target/release/Metis`

### With specific channel features

```bash
# Only Telegram
cargo build --release --features "telegram"

# Multiple channels
cargo build --release --features "telegram,discord,slack"

# All channels (what the Dockerfile uses)
cargo build --release --features "telegram,discord,whatsapp,slack,email"
```

### Feature Flags

| Feature | Description |
|---------|-------------|
| `telegram` | Telegram bot via teloxide |
| `discord` | Discord bot via WebSocket gateway |
| `whatsapp` | WhatsApp via Node.js bridge (Baileys) |
| `slack` | Slack bot via Socket Mode |
| `email` | Email via IMAP + SMTP |

## 🚀 Quick Start

> [!TIP]
> Set your API key in `~/.metis/config.json`.
> Get API keys: [OpenRouter](https://openrouter.ai/keys) (recommended) · [Anthropic](https://console.anthropic.com) · [Brave Search](https://brave.com/search/api/) (optional)
>
> Config is stored at `~/.metis/config.json` (not `~/.metis/workspace/config.json`).
> Scheduled tasks are built in via `Metis cron ...` (no external scheduler required).
>
> Cron command shape is strict: use `Metis cron <subcommand> ...` (for example `Metis cron remove <id>`).
> `Metis remove <id>` is invalid and will fail with "unrecognized subcommand".
> If `Metis` is not found in PATH, use the explicit binary path: `target/release/Metis.exe cron ...` (Windows).
>
> `exec` tool policy is configurable in `tools.exec`:
> - `shell`: `powershell` (default on Windows), `cmd`, or `sh`
> - `permissionMode`: `unsafe_only` (default), `always`, or `poweruser`
> - Data-exfil commands (mail/ftp/scp/upload) always require approval, even in `poweruser`.
>
> Windows usage notes:
> - Prefer the explicit binary path when PATH is ambiguous: `target/release/Metis.exe ...`
> - PowerShell is the default shell backend on Windows for `exec`.
> - `channels.*.allowedUsers` values must be strings (e.g. `"8582973375"`), not integers.
>
> Real tool executions return a fenced `<<<EXEC_RESULT>>>` … `<<<END_EXEC_RESULT>>>` block (command, working dir, shell backend, status, and normally `EXIT_CODE`; timeouts use `TIMEOUT_SECONDS` instead) plus captured stdout/stderr when applicable. Blocked commands and approval prompts omit this fence—that is the execution-proof contract for humans and agents.

**1. Initialize**

```bash
Metis onboard
```

**2. Configure** (`~/.metis/config.json`)

```json
{
  "providers": {
    "openrouter": {
      "apiKey": "sk-or-v1-xxx"
    }
  },
  "agents": {
    "defaults": {
      "model": "anthropic/claude-sonnet-4-20250514"
    }
  }
}
```

**3. Chat**

```bash
Metis agent -m "What is 2+2?"
```

That's it! Working AI assistant in 2 minutes. 🎉

## 🖥️ Local Models (vLLM)

Run Metis with your own local LLMs using vLLM or any OpenAI-compatible server.

```json
{
  "providers": {
    "vllm": {
      "apiKey": "dummy",
      "apiBase": "http://localhost:8000/v1"
    }
  },
  "agents": {
    "defaults": {
      "model": "meta-llama/Llama-3.1-8B-Instruct"
    }
  }
}
```

```bash
Metis agent -m "Hello from my local LLM!"
```

> [!TIP]
> The `apiKey` can be any non-empty string for local servers that don't require authentication.

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Metis (single binary)                       │
│                                                                     │
│  ┌──────────┐  ┌───────────────┐  ┌────────────┐  ┌─────────────┐   │
│  │ Metis-  │  │   Metis-     │  │  Metis-   │  │   Metis-   │   │
│  │  cli     │──│   agent       │──│  providers │  │   cron      │   │
│  │          │  │               │  │            │  │             │   │
│  │ commands │  │ loop, tools,  │  │ 12 LLM     │  │ scheduler,  │   │
│  │ gateway  │  │ memory, ctx   │  │ backends   │  │ jobs, store │   │
│  │ repl     │  │ skills, sub   │  │ + whisper  │  │             │   │
│  └──────────┘  └───────────────┘  └────────────┘  └─────────────┘   │
│       │                │                                    │       │
│  ┌────▼────────────────▼────────────────────────────────────▼────┐  │
│  │                     metis-core                               │  │
│  │   config · bus · session · heartbeat · types · utils          │  │
│  └──────────────────────────┬────────────────────────────────────┘  │
│                             │                                       │
│  ┌──────────────────────────▼────────────────────────────────────┐  │
│  │                    metis-channels                            │  │
│  │   telegram · discord · whatsapp · slack · email               │  │
│  └───────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────┘
         │ (WhatsApp only)
         │ WebSocket ws://localhost:3001
┌────────▼─────────────────────────┐
│  Node.js WhatsApp Bridge         │
│  (Baileys v7, TypeScript)        │
└──────────────────────────────────┘
```

## 💬 Chat Apps

Talk to Metis through Telegram, Discord, WhatsApp, Slack, or Email — anytime, anywhere.

| Channel | Setup | Requires |
|---------|-------|----------|
| **Telegram** | Easy | Bot token |
| **Discord** | Easy | Bot token + intents |
| **WhatsApp** | Medium | Node.js + QR scan |
| **Slack** | Medium | Bot + App tokens |
| **Email** | Medium | IMAP/SMTP credentials |

<details>
<summary><b>Telegram</b> (Recommended)</summary>

**1. Create a bot** — Open Telegram → `@BotFather` → `/newbot` → copy the token

**2. Configure**

```json
{
  "channels": {
    "telegram": {
      "token": "YOUR_BOT_TOKEN",
      "allowedUsers": ["YOUR_USER_ID"]
    }
  }
}
```

**3. Build & Run**

```bash
cargo build --release --features telegram
Metis gateway
```

**Voice notes (Telegram)**

Metis transcribes Telegram voice/audio by calling an **HTTP Whisper-style** endpoint (`POST …/audio/transcriptions`, OpenAI multipart format). Nothing in upstream Metis “searches disk for Whisper” or runs embedded Python STT — if something claims that in chat, treat it as a model mistake.

For a complete guide (providers, ffmpeg requirements, troubleshooting), see `VOICE-TRANSCRIPTION.md`.

**A) Hosted Groq Whisper**

1. `"transcription": { "enabled": true }`
2. `"transcription.provider": "groq"` *(default)*
3. Groq API key in **`transcription.apiKey`**, **`providers.groq.apiKey`**, or **`GROQ_API_KEY`**

**B) Local / self-hosted Whisper (no Groq)**

Metis talks to Whisper over **HTTP**, not by calling `whisper.exe` or `pip` directly.  
If Windows “already has Whisper” from **`pip install openai-whisper`**, that is usually **Python + CLI only** — you still need a tiny HTTP shim.

Easiest shipped option:

```powershell
py -m pip install --upgrade fastapi uvicorn python-multipart openai-whisper
py .\contrib\local_whisper_openai_http.py --host 127.0.0.1 --port 8090 --model base
```

Then set **`apiBase`** to `http://127.0.0.1:8090/v1` and **`provider`** to **`local`** (see below).

Other gateways (LM Studio local server, LocalAI-style stacks, Faster-Whisper HTTP wrappers, etc.) work too if they expose **`POST …/v1/audio/transcriptions`**.

Configure:

```json
"transcription": {
  "enabled": true,
  "provider": "local",
  "apiBase": "http://127.0.0.1:8080/v1",
  "model": "whisper-large-v3",
  "apiKey": ""
}
```

- **`apiBase`**: prefer ending with **`/v1`** (Metis appends **`/audio/transcriptions`** automatically).
- **`apiKey`**: leave empty when your local endpoint does not use Bearer tokens.

Environment overrides instead of editing JSON:

- `METIS_TRANSCRIPTION__PROVIDER`, `METIS_TRANSCRIPTION__API_BASE`, `METIS_TRANSCRIPTION__MODEL`, `METIS_TRANSCRIPTION__API_KEY`, `METIS_TRANSCRIPTION__ENABLED`

Gateway warns on startup when transcription cannot be wired (missing Groq key, or missing `apiBase` for local mode).

**C) Local whisper.cpp (Windows-native, no HTTP)**

If you have a local `whisper.cpp` build, Metis can spawn `whisper-cli.exe` directly.

Before using this mode, you must first download/build `whisper.cpp` and have:
- `whisper-cli.exe` available (on `PATH` or via `whisperCpp.exePath`)
- a local model file (for example `ggml-base.bin`) available on disk

This Metis agent variant is currently focused on **Windows-native** usage and validation, not Linux-first workflows.

Configure:

```json
"transcription": {
  "enabled": true,
  "provider": "whisper_cpp",
  "whisperCpp": {
    "exePath": "C:/path/to/whisper-cli.exe",
    "modelPath": "C:/path/to/models/ggml-base.bin",
    "extraArgs": []
  }
}
```

- **`whisperCpp.exePath`**: optional; defaults to `whisper-cli.exe` on `PATH`.
- **`whisperCpp.modelPath`**: required.

**Windows agent-run script (must verify, no guesswork)**

If you want the agent to do this for you, it should run this exact script and only report success when all `Test-Path` checks pass:

```powershell
$ErrorActionPreference = "Stop"

# 1) Create target dirs
$root = "C:\whisper-cpp"
$bin  = Join-Path $root "bin"
$mdl  = Join-Path $root "models"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
New-Item -ItemType Directory -Force -Path $mdl | Out-Null

# 2) Download whisper.cpp Windows binary (example artifact)
# Replace URL if your preferred release asset differs.
$exePath = Join-Path $bin "whisper-cli.exe"
$zipPath = Join-Path $env:TEMP "whispercpp-win.zip"
$releaseUrl = "https://github.com/ggerganov/whisper.cpp/releases/latest/download/whisper-bin-x64.zip"
Invoke-WebRequest -Uri $releaseUrl -OutFile $zipPath
Expand-Archive -Path $zipPath -DestinationPath $root -Force

# 3) Download model
$modelPath = Join-Path $mdl "ggml-base.bin"
Invoke-WebRequest -Uri "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin" -OutFile $modelPath

# 4) Verify files exist (hard gate)
if (!(Test-Path $exePath))   { throw "Missing whisper CLI: $exePath" }
if (!(Test-Path $modelPath)) { throw "Missing model file: $modelPath" }

# 5) Update Metis config
$cfgPath = "$HOME\.metis\config.json"
if (!(Test-Path $cfgPath)) { throw "Missing config: $cfgPath" }
$cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json

if (-not $cfg.transcription) { $cfg | Add-Member -NotePropertyName transcription -NotePropertyValue (@{}) }
$cfg.transcription.enabled  = $true
$cfg.transcription.provider = "whisper_cpp"
if (-not $cfg.transcription.whisperCpp) { $cfg.transcription | Add-Member -NotePropertyName whisperCpp -NotePropertyValue (@{}) }
$cfg.transcription.whisperCpp.exePath   = $exePath
$cfg.transcription.whisperCpp.modelPath = $modelPath
if (-not $cfg.transcription.whisperCpp.extraArgs) { $cfg.transcription.whisperCpp.extraArgs = @() }

$cfg | ConvertTo-Json -Depth 20 | Set-Content $cfgPath -Encoding UTF8

# 6) Print proof (agent should include this in its final output)
Write-Host "OK: whisper.cpp configured"
Get-Item $exePath   | Select-Object FullName,Length,LastWriteTime
Get-Item $modelPath | Select-Object FullName,Length,LastWriteTime
(Get-Content $cfgPath -Raw | ConvertFrom-Json).transcription | ConvertTo-Json -Depth 8
```

If this script fails at any point, the agent should report the exact failing line/error and stop claiming installation succeeded.

Legacy installs may still reference `~/.oxibot/workspace`; Metis loads `~/.metis/config.json` and migrates `.oxibot` → `.metis` in workspace paths automatically. Override with env `metis_agentS__DEFAULTS__WORKSPACE` only if intentional.

</details>

<details>
<summary><b>Discord</b></summary>

**1. Create a bot**
- [Discord Developer Portal](https://discord.com/developers/applications) → Create Application → Bot → Add Bot
- Enable **MESSAGE CONTENT INTENT**
- Copy the bot token

**2. Invite the bot**
- OAuth2 → URL Generator → Scopes: `bot` → Permissions: `Send Messages`, `Read Message History`
- Open the generated URL and add to your server

**3. Configure**

```json
{
  "channels": {
    "discord": {
      "token": "YOUR_BOT_TOKEN",
      "allowedUsers": ["YOUR_USER_ID"]
    }
  }
}
```

**4. Build & Run**

```bash
cargo build --release --features discord
Metis gateway
```

</details>

<details>
<summary><b>WhatsApp</b></summary>

Requires **Node.js ≥20** for the Baileys bridge.

**1. Build the bridge**

```bash
cd bridge && npm install && npm run build && cd ..
```

**2. Link device**

```bash
Metis channels login
# Scan QR with WhatsApp → Settings → Linked Devices
```

**3. Configure**

```json
{
  "channels": {
    "whatsapp": {
      "bridgeUrl": "ws://localhost:3001",
      "allowedUsers": ["+1234567890"]
    }
  }
}
```

**4. Run** (two terminals)

```bash
# Terminal 1: Start the bridge
cd bridge && npm start

# Terminal 2: Start the bot
cargo build --release --features whatsapp
Metis gateway
```

</details>

<details>
<summary><b>Slack</b></summary>

Uses **Socket Mode** — no public URL required.

**1. Create a Slack app**
- [Slack API](https://api.slack.com/apps) → Create New App → "From scratch"
- **Socket Mode**: Toggle ON → Generate App-Level Token (`xapp-...`)
- **OAuth & Permissions**: Add scopes: `chat:write`, `reactions:write`, `app_mentions:read`
- **Event Subscriptions**: Toggle ON → Subscribe: `message.im`, `message.channels`, `app_mention`
- **App Home**: Enable Messages Tab → Allow messages
- **Install to Workspace** → Copy Bot Token (`xoxb-...`)

**2. Configure**

```json
{
  "channels": {
    "slack": {
      "botToken": "xoxb-...",
      "appToken": "xapp-...",
      "groupPolicy": "mention"
    }
  }
}
```

**3. Build & Run**

```bash
cargo build --release --features slack
Metis gateway
```

> [!TIP]
> `groupPolicy`: `"mention"` (respond to @mentions), `"open"` (all messages), or `"allowlist"`.

</details>

<details>
<summary><b>Email</b></summary>

Polls **IMAP** for incoming mail, replies via **SMTP**.

**1. Get credentials** (Gmail example: enable 2FA → create [App Password](https://myaccount.google.com/apppasswords))

**2. Configure**

```json
{
  "channels": {
    "email": {
      "imapHost": "imap.gmail.com",
      "imapPort": 993,
      "imapUsername": "my-Metis@gmail.com",
      "imapPassword": "your-app-password",
      "smtpHost": "smtp.gmail.com",
      "smtpPort": 587,
      "smtpUsername": "my-Metis@gmail.com",
      "smtpPassword": "your-app-password",
      "fromAddress": "my-Metis@gmail.com",
      "allowedUsers": ["your-real-email@gmail.com"]
    }
  }
}
```

**3. Build & Run**

```bash
cargo build --release --features email
Metis gateway
```

</details>

## ⚙️ Configuration

Config file: `~/.metis/config.json`

### Providers

| Provider | Purpose | Get API Key |
|----------|---------|-------------|
| `openrouter` | LLM (recommended, access to all models) | [openrouter.ai](https://openrouter.ai) |
| `anthropic` | LLM (Claude direct) | [console.anthropic.com](https://console.anthropic.com) |
| `openai` | LLM (GPT direct) | [platform.openai.com](https://platform.openai.com) |
| `deepseek` | LLM (DeepSeek direct) | [platform.deepseek.com](https://platform.deepseek.com) |
| `groq` | LLM + **Voice transcription** (Whisper) | [console.groq.com](https://console.groq.com) |
| `gemini` | LLM (Gemini direct) | [aistudio.google.com](https://aistudio.google.com) |
| `minimax` | LLM (MiniMax direct) | [platform.minimax.io](https://platform.minimax.io) |
| `aihubmix` | LLM (API gateway) | [aihubmix.com](https://aihubmix.com) |
| `dashscope` | LLM (Qwen) | [dashscope.console.aliyun.com](https://dashscope.console.aliyun.com) |
| `moonshot` | LLM (Moonshot/Kimi) | [platform.moonshot.cn](https://platform.moonshot.cn) |
| `zhipu` | LLM (Zhipu GLM) | [open.bigmodel.cn](https://open.bigmodel.cn) |
| `vllm` | LLM (local, any OpenAI-compatible server) | — |

> [!TIP]
> **Groq** provides free voice transcription via Whisper. If configured, Telegram voice messages will be automatically transcribed.

### Environment Variables

All env vars use `METIS_` prefix with `__` as section delimiter:

```bash
export metis_providers__ANTHROPIC__API_KEY=sk-ant-xxx
export metis_agentS__DEFAULTS__MODEL=anthropic/claude-sonnet-4-20250514
export METIS_GATEWAY__PORT=9090
```

Config precedence: **Defaults** → **config.json** → **Environment variables** (env overrides all).

> [!TIP]
> `web_search` is enabled only when a Brave Search API key is configured.  
> Local browser automation via the `browser` tool does not require Brave API keys.

### Security

> For production, set `"restrictToWorkspace": true` to sandbox the agent.

| Option | Default | Description |
|--------|---------|-------------|
| `tools.restrictToWorkspace` | `false` | Restricts all agent tools to workspace directory |
| `channels.*.allowedUsers` | `[]` (allow all) | Whitelist of user IDs. Empty = allow everyone |

> [!WARNING]
> `allowedUsers` must be a string array in JSON.  
> Example: `"allowedUsers": ["8582973375"]` (correct) vs `"allowedUsers": [8582973375]` (incorrect).  
> If config deserialization fails, Metis may fall back to defaults, which can trigger model/provider mismatch errors.

See [SECURITY.md](SECURITY.md) for comprehensive security guidance.

## 📖 CLI Reference

| Command | Description |
|---------|-------------|
| `Metis onboard` | Initialize config & workspace |
| `Metis agent -m "..."` | Chat (single message) |
| `Metis agent` | Interactive REPL |
| `Metis agent --no-markdown` | Plain-text replies |
| `Metis agent --logs` | Show debug logs |
| `Metis gateway` | Start all channels + cron + heartbeat |
| `Metis serve` | Local HTTP API for the agent (Axum; see [HTTP-SERVER.md](HTTP-SERVER.md)) |
| `Metis status` | Show config & provider status |
| `Metis channels status` | Show channel status |
| `Metis channels login` | Link WhatsApp (scan QR) |
| `Metis cron list` | List scheduled jobs |
| `Metis cron add` | Add a scheduled job |
| `Metis cron remove <id>` | Remove a job |
| `Metis cron enable <id>` | Enable/disable a job |
| `Metis cron run <id>` | Manually trigger a job |

Interactive mode exits: `exit`, `quit`, `/exit`, `/quit`, `:q`, Ctrl-C, Ctrl-D.

<details>
<summary><b>Scheduled Tasks (Cron)</b></summary>

```bash
# Cron expression
Metis cron add --name "morning" --message "Daily summary" --cron "0 9 * * *"

# Interval (seconds)
Metis cron add --name "check" --message "Status update" --every 3600

# One-time at specific time
Metis cron add --name "remind" --message "Call dentist" --at "2026-03-01T09:00:00"

# With channel delivery
Metis cron add --name "alert" --message "Health check" --every 300 \
  --deliver --channel telegram --to "123456789"

# List / remove
Metis cron list
Metis cron remove <job_id>
```

</details>

## 🎯 Skills

Bundled skills in `crates/metis-agent/skills/`:

| Skill | Description |
|-------|-------------|
| **skill-creator** | Guides the agent on creating new skills |
| **weather** | Weather via `wttr.in` (no API key needed) |
| **cron** | Schedule reminders and recurring tasks |
| **tmux** | Remote-control tmux sessions |
| **github** | Interact with GitHub via `gh` CLI |
| **summarize** | Summarize URLs and articles |

Custom skills can be added to `~/.metis/workspace/skills/`.

## 🐳 Docker

```bash
# Build the image
docker build -t Metis .

# Initialize (first time)
docker run -v ~/.metis:/home/metis/.metis --rm metis onboard

# Edit config to add API keys
vim ~/.metis/config.json

# Run gateway
docker run -d \
  -v ~/.metis:/home/metis/.metis \
  -p 18790:18790 \
  Metis gateway

# Single command
docker run -v ~/.metis:/home/metis/.metis --rm metis agent -m "Hello!"
docker run -v ~/.metis:/home/metis/.metis --rm metis status
```

> [!TIP]
> The `-v ~/.metis:/home/metis/.metis` flag persists config and workspace across container restarts.

## 📁 Project Structure

```
Metis/
├── Cargo.toml                  # Workspace root
├── Dockerfile                  # Multi-stage build (Rust + Node.js bridge)
├── bridge/                     # 🌉 Node.js WhatsApp bridge (Baileys)
│   ├── src/
│   │   ├── index.ts            #    Entry point
│   │   ├── server.ts           #    WebSocket server
│   │   └── whatsapp.ts         #    Baileys client
│   └── package.json
└── crates/
    ├── metis-core/            # ⚙️  Config, bus, session, heartbeat, utils
    ├── metis-agent/           # 🧠  Agent loop, tools, memory, context, skills
    │   └── skills/             # 🎯  Bundled skills (weather, cron, tmux, etc.)
    ├── metis-providers/       # 🤖  12 LLM backends + Whisper transcription
    ├── metis-channels/        # 📱  Telegram, Discord, WhatsApp, Slack, Email
    ├── metis-cron/            # ⏰  Scheduled task engine
    └── metis-cli/             # 🖥️  CLI commands, gateway, REPL
```

## 🧪 Testing

```bash
# Run all tests
cargo test --workspace

# With all channel features
cargo test --workspace --features "telegram,discord,whatsapp,slack,email"

# Specific crate
cargo test -p metis-core
cargo test -p metis-agent
cargo test -p metis-providers
cargo test -p metis-channels
cargo test -p metis-cron
cargo test -p metis-cli
```

See [TESTING-GUIDE.md](TESTING-GUIDE.md) for comprehensive testing procedures, sample configs, and Docker instructions.

## 🤝 Contributing

PRs welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

**Roadmap:**

- [x] Core agent loop (tools, memory, sessions)
- [x] 13 LLM providers
- [x] 5 chat channels (Telegram, Discord, WhatsApp, Slack, Email)
- [x] Cron scheduler
- [x] Heartbeat service
- [x] Voice transcription (Groq Whisper)
- [x] WhatsApp bridge (TypeScript + Baileys)
- [ ] Multi-modal support (images, video)
- [ ] Enhanced long-term memory
- [ ] Web UI dashboard
- [ ] Plugin system for custom tools
- [ ] CI/CD with GitHub Actions

## 📜 License

[MIT](LICENSE)

<p align="center">
  <sub>Metis is a Native Windows enhanced Rust reimplementation of <a href="https://github.com/HKUDS/nanobot">nanobot</a> created by oxibot for educational and research purposes.</sub>

</p>
