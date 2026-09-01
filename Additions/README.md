# Additions — the parts that are not the Rust build

Everything here is needed to run Metis on a second machine but is **not**
produced by `cargo build`. Copy `metis.exe` yourself from `target/release`;
this folder covers the rest.

Nothing here contains credentials. See **Secrets** at the bottom.

## What goes where

The two skill folders have **different destinations** — this is the part
that is easy to get wrong.

| From | To | What it is |
|---|---|---|
| `bridge/` | `<install dir>/bridge/` | WhatsApp bridge (Node) |
| `workspace-skills/*` | `~/.metis/workspace/skills/` | skills you authored |
| the repo's own `skills/` | next to `metis.exe` | skills shipped with Metis |
| `contrib/` | anywhere | optional local Whisper server |

`~` is `C:\Users\<you>` on Windows.

Built-in skills are found by looking next to the executable, then at the
repo root. So an installed layout wants:

```
C:\Metis\
  metis.exe
  skills\            <- copied from the repo's skills/ folder
  bridge\            <- copied from Additions/bridge
```

## WhatsApp bridge

`node_modules` is deliberately **not** included — it is 69 MB, and
`package-lock.json` reproduces it exactly. On the target machine:

```bash
cd bridge
npm ci --omit=dev
npm start
```

Needs Node 20 or newer. The first run prints a QR code; scan it from
WhatsApp on your phone to pair.

`dist/` **is** included, so no TypeScript build is needed. If you change
anything in `src/`, run `npm run build` to regenerate it.

Pairing produces `~/.metis/whatsapp-auth`. That directory is a **live
credential** — anyone holding it has your WhatsApp session. Never copy it
between machines and never commit it; pair each machine separately.

To disable WhatsApp entirely, remove `channels.whatsapp.bridgeUrl` from
`config.json`.

## Channels are opt-in at build time

A default `cargo build --release` produces a binary with **no channels**.
Telegram and email are cargo features:

```bash
cargo build --release -p metis-cli --features telegram,email
```

A binary built without them starts cleanly and silently does nothing —
which looks exactly like a configuration problem.

Note also that `metis desktop` hosts **no channels at all**. Email, Telegram
and WhatsApp only run under `metis gateway`.

## Optional local Whisper

`contrib/local_whisper_openai_http.py` exposes
`POST /v1/audio/transcriptions` so Metis can transcribe voice notes without
a cloud key. See `VOICE-TRANSCRIPTION.md` in the repo for the three
supported backends.

## Secrets

Two files are deliberately **absent** from this bundle:

- `config.json` — holds API keys and the Graph client secret. It stays on
  each machine and is never committed. Set it up per machine with the
  desktop app, or `scripts/setup-o365-graph.ps1` for Microsoft Graph.
- `workspace-skills/invoice-processor/config.yaml` — held a live mailbox
  password and a live MiniMax API key. Recreate it on the target machine
  from the template in that skill's `SKILL.md`.

The FTP credentials that were written into `invoice-processor/SKILL.md`
have been replaced with placeholders.
