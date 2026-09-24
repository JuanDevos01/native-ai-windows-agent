# dependency/ — everything that is not the Rust build

`cargo build` produces `metis.exe`. This folder is the rest: the WhatsApp
bridge, the skills, the Graph setup script, and the optional local Whisper
server. Copy `metis.exe` yourself; copy this folder next to it.

No credentials are in here — see **Secrets** at the end.

## Install layout

```
C:\Metis\
  metis.exe
  dependency\          <- this folder, copied whole
```

That is all the placement needed for skills: `metis.exe` looks for built-in
skills beside itself, then in `dependency\skills`. Two things still have to
be copied out of here by hand:

| From | To | Why |
|---|---|---|
| `dependency\workspace-skills\*` | `%USERPROFILE%\.metis\workspace\skills\` | these are *your* authored skills, and the workspace is where they live |
| `dependency\bridge\` | anywhere (e.g. `C:\Metis\bridge\`) | needs `npm ci` run inside it |

`dependency\skills\` is **not** copied anywhere — it is read in place.

## WhatsApp bridge

Run this and everything happens for you — it finds the bridge inside this
folder, installs its dependencies, starts it, and prints the QR code:

```
metis channels login
```

Needs Node 20+ on PATH. Scan the QR from WhatsApp on your phone.

Onboarding only asks for the bridge **URL** (`ws://localhost:3001`); it does
not show a QR, because the QR comes from the bridge process itself. If you
skipped onboarding's WhatsApp question, the URL can be set later in
`config.json` under `channels.whatsapp.bridgeUrl`.

`node_modules` is deliberately absent from this folder: it is 69 MB and
`package-lock.json` reproduces it exactly, which is what `channels login`
does. `dist/` **is** included, so no TypeScript build is needed — run
`npm run build` only if you edit `src/`.
Pairing creates `%USERPROFILE%\.metis\whatsapp-auth`, which is a **live
credential** — anyone holding that folder holds your WhatsApp session. Never
copy it between machines and never commit it; pair each machine separately.

To run without WhatsApp, remove `channels.whatsapp.bridgeUrl` from
`config.json`.

## Channels are a build-time choice

A plain `cargo build --release` produces a binary with **no channels at
all** — it starts cleanly and silently does nothing, which looks exactly
like a config problem. Build with:

```bash
cargo build --release -p metis-cli --features telegram,email
```

And note `metis desktop` hosts **no channels**. Email, Telegram and WhatsApp
run only under `metis gateway`.

## If the desktop GUI will not open

```
egui_glow requires opengl 2.0+
```

Windows ships only a 1.1 software OpenGL when no GPU driver is installed, so
this is normal on a fresh machine, inside a VM, or over Remote Desktop —
nothing is misconfigured. Metis now retries automatically with Direct3D,
which Windows always provides, so the window should open on its own.

To pin a renderer:

```powershell
$env:METIS_DESKTOP_RENDERER = "wgpu"   # Direct3D
$env:METIS_DESKTOP_RENDERER = "glow"   # OpenGL
```

And the GUI is never required: `metis gateway` runs the channels, `metis
agent` gives you a chat prompt, and `config.json` can be edited directly.

## Microsoft 365 mail

`scripts\setup-o365-graph.ps1` registers the Azure app, requests
`Mail.ReadWrite` / `Mail.Send`, grants admin consent, and scopes the app to a
single mailbox. Needs PowerShell 7 (`pwsh`); it checks and tells you if it is
missing.

```powershell
pwsh -File scripts\setup-o365-graph.ps1 -Mailbox user@contoso.com -WriteConfig
```

`-RepairAccess` re-applies permissions and scoping to the existing app
without minting a new secret. `-GrantSite host:/sites/Name` authorizes one
SharePoint site.

## Optional local Whisper

`contrib\local_whisper_openai_http.py` serves
`POST /v1/audio/transcriptions` so voice notes transcribe without a cloud
key. See `VOICE-TRANSCRIPTION.md` in the repo for the three backends.

## Secrets

Deliberately not here, and not in the repo:

- **`config.json`** — API keys and the Graph client secret. Per-machine;
  create it with the desktop app or the setup script above.
- **`.metis\whatsapp-auth`** — a live WhatsApp session. Pair per machine.
- **`invoice-processor\config.yaml`** — held a live mailbox password and a
  live MiniMax API key. Recreate it on the target from the template in that
  skill's `SKILL.md`.

The FTP host, user and password that had been written into
`invoice-processor\SKILL.md` are replaced with placeholders here. **The
original still contains them** at
`%USERPROFILE%\.metis\workspace\skills\invoice-processor\SKILL.md` — worth
clearing there too, since `save_skill` writes whatever the agent had in hand
and skills are therefore a credential-leak path.
