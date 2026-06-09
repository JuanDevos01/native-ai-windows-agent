# Metis Guide — Models, Agents, Subagents, Cron & Heartbeat

Practical, copy-paste reference for configuring Metis. All configuration lives in
`~/.metis/config.json` (Windows: `C:\Users\<you>\.metis\config.json`). Restart the
gateway after editing config.

> Subscriptions vs. API: ChatGPT Plus/Pro and Claude Pro/Max are for the consumer
> apps only. Metis talks to the **developer API**, which uses a separate, pay-as-you-go
> **API key** (from platform.openai.com / console.anthropic.com). Your chat
> subscription does **not** work with the standard API.

### "Can I use my Plus/Pro/Ultra subscription instead of paying per token?" (OAuth)

Short answer: **not in a way Metis supports, and not recommended.**

- The standard OpenAI/Anthropic APIs that Metis uses authenticate with **API
  keys** (Bearer tokens), billed per token. There is no OAuth for them.
- First-party tools (Anthropic's *Claude Code*, OpenAI's *Codex CLI*) do let
  subscribers sign in via OAuth and use their plan — but those use **special
  endpoints tied to those specific products**. Re-using that OAuth from a
  third-party app like Metis is **against the providers' Terms of Service**,
  can get your account rate-limited or banned, and breaks whenever they rotate
  the flow. For that reason Metis does **not** implement it.
- **The cost-free, safe alternative is local models** — run Ollama or LM Studio
  (sections below). They're free, offline, and fully supported. A common setup
  is a small paid API key for the main agent plus a **local subagent model** to
  keep token usage low (see §3), or run everything locally.

---

## 1. The main model

Set `agents.defaults.model` to `provider/model`, and put the key under `providers.<name>`:

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-sonnet-4-20250514"
    }
  },
  "providers": {
    "anthropic": { "apiKey": "sk-ant-..." }
  }
}
```

### Supported providers

| Provider   | `model` example                          | Key field            |
|------------|------------------------------------------|----------------------|
| Anthropic  | `anthropic/claude-sonnet-4-20250514`     | `providers.anthropic.apiKey` |
| OpenAI     | `openai/gpt-4o`                          | `providers.openai.apiKey` |
| OpenRouter | `openrouter/...` (gateway)               | `providers.openrouter.apiKey` |
| DeepSeek   | `deepseek/deepseek-chat`                 | `providers.deepseek.apiKey` |
| Gemini     | `gemini-2.0-flash`                       | `providers.gemini.apiKey` |
| Groq       | `groq/llama-3.3-70b`                     | `providers.groq.apiKey` |
| Moonshot   | `kimi-k2.5-preview`                      | `providers.moonshot.apiKey` |
| MiniMax    | `minimax/...`                            | `providers.minimax.apiKey` |
| ZhiPu      | `glm-4-flash`                            | `providers.zhipu.apiKey` |
| DashScope  | `qwen-turbo`                             | `providers.dashscope.apiKey` |
| AiHubMix   | `aihubmix/...` (gateway)                 | `providers.aihubmix.apiKey` |
| Moonshot/Kimi | `moonshot/kimi-k2-0905-preview`       | `providers.moonshot.apiKey` |
| **Ollama** | `ollama/llama3.1` (**local, no key**)    | `providers.ollama.apiBase` (optional) |
| **LM Studio** | `lmstudio/<model>` (**local, no key**) | `providers.lmstudio.apiBase` (optional) |
| vLLM       | `vllm/...` (local)                       | `providers.vllm.apiBase` |

All cloud providers authenticate with an **API key** (`Authorization: Bearer <key>`).
There is no OAuth flow. A custom endpoint can be set per provider with
`providers.<name>.apiBase`, and custom headers with `providers.<name>.extraHeaders`.

---

## 2. Local models with Ollama (free, offline)

1. Install Ollama, pull a model, and run the server:
   ```bash
   ollama pull llama3.1
   ollama serve
   ```
2. Point Metis at it — **no API key needed** (defaults to `http://localhost:11434/v1`):
   ```json
   { "agents": { "defaults": { "model": "ollama/llama3.1" } } }
   ```
3. Remote Ollama host? Set the base URL:
   ```json
   { "providers": { "ollama": { "apiBase": "http://192.168.1.50:11434/v1" } } }
   ```

The `ollama/` prefix is stripped automatically, so the bare model name (e.g.
`llama3.1`) is sent to the Ollama server.

### LM Studio (local, free)

LM Studio also exposes an OpenAI-compatible server. Load a model in LM Studio,
start its local server, then:

```json
{ "agents": { "defaults": { "model": "lmstudio/qwen2.5-7b-instruct" } } }
```

- Default endpoint is `http://localhost:1234/v1` — no API key.
- Only the leading `lmstudio/` token is stripped, so `org/model` identifiers
  survive (e.g. `lmstudio/lmstudio-community/Meta-Llama-3.1-8B-Instruct`).
- Custom host/port: `{ "providers": { "lmstudio": { "apiBase": "http://localhost:1234/v1" } } }`.

> Use the exact model id LM Studio shows for the loaded model.

---

## Changing the model later (no re-onboarding)

You don't need to re-run `metis onboard` just to switch models. Use `metis model`:

```bash
metis model                                   # show current model + provider status
metis model anthropic/claude-sonnet-4-20250514
metis model ollama/llama3.1                   # local, no key
metis model lmstudio/qwen2.5-7b-instruct      # local, no key
```

It updates `agents.defaults.model` in your config and warns if no provider/key
matches yet. Restart the gateway/agent to apply. Inside the interactive REPL
(`metis agent`) you can also type `/model` (show) or `/model <provider/model>` (set).

---

## 3. A second model for subagents

The main agent can delegate self-contained subtasks to **subagents** (via the
`spawn` tool). Subagents can run on a **different model and even a different
provider** — e.g. keep the main agent on Anthropic and run subagents locally on
Ollama to save cost:

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-sonnet-4-20250514",
      "subagentModel": "ollama/llama3.1"
    }
  },
  "providers": {
    "anthropic": { "apiKey": "sk-ant-..." }
  }
}
```

- Leave `subagentModel` empty (or omit it) to make subagents use the main model.
- If the subagent provider can't be built (missing key, etc.), Metis logs a
  warning and subagents fall back to the main provider.

### How subagents work (and what they can't do)

- Flow is one delegation, not a live chat: **main → subagent (task) → main (result)**.
- The subagent runs its own loop (limited tools: read/write file, list_dir, exec,
  web search/fetch), then posts its result back; the main agent summarizes it.
- Subagents **cannot** message the user directly, spawn further subagents, or
  `edit_file` in place (they use `write_file`).
- Subagent progress is written to the logs (`metis gateway --logs`), not the chat.

> Note: There is **no** named multi-agent system (e.g. `agents.invoice_bot`).
> Only `agents.defaults` is read; extra keys are ignored. "Another agent" =
> subagents via `spawn`, optionally on a different model.

---

## 4. Scheduling with cron (built-in)

Metis has its **own** scheduler — use it instead of Windows Task Scheduler,
`crontab`, or `systemd` timers. Jobs persist across restarts.

```bash
# Recurring (standard 5-field cron expression)
metis cron add --name "morning-report" --message "Summarize overnight emails" --cron "0 9 * * *"

# Interval (seconds)
metis cron add --name "stars" --message "Check repo stars and report" --every 600

# One-shot at a specific time (ISO 8601)
metis cron add --name "reminder" --message "Call Bob" --at "2026-03-01T09:00:00"

# Deliver the result to a chat
metis cron add --name "daily" --message "Daily standup" --cron "0 8 * * 1-5" \
  --deliver --channel telegram --to <chat_id>

# Manage
metis cron list --all          # list (including disabled)
metis cron run <ID>            # trigger now
metis cron enable <ID>         # enable
metis cron enable <ID> --disable   # disable
metis cron remove <ID>        # delete
```

Each **task** job runs its `--message` as a prompt to the agent. A **reminder**
job (`--deliver`) just sends the message to the chat. Job IDs are the 8-char hex
shown by `metis cron list`.

The cron store is at `~/.metis/cron.json`. If it ever contains an unrecognized
value, Metis keeps the readable jobs instead of discarding the whole file.

---

## 5. Heartbeat (periodic self-wake)

Metis can wake itself on an interval and act on `HEARTBEAT.md` in the workspace.
It is **enabled by default every 30 minutes** and is now configurable:

```json
{
  "heartbeat": {
    "enabled": true,
    "intervalMinutes": 15
  }
}
```

- `enabled: false` turns it off.
- `intervalMinutes` sets the cadence (minimum 1).
- The gateway banner prints the active interval (or `disabled`).
- Put recurring maintenance instructions in `<workspace>/HEARTBEAT.md`.

---

## 6. Channels (how users reach the agent)

Channels are independent transports (Telegram, Discord, WhatsApp, Slack, email…)
configured under `channels.<name>` and enabled via build features. They all feed
**one** agent loop — **adding a model has nothing to do with channels**, and you
can run several channels at once. Build with the features you need, e.g.:

```bash
cargo build --release -p metis-cli --features "telegram,discord,slack,email"
```

---

## 7. Build version

`metis --version` and the gateway banner show the version, git hash, and build
time, so you can confirm exactly which build is running.
