# Voice transcription (Whisper) — usage & troubleshooting

Metis can transcribe **voice notes / audio attachments** (currently wired for **Telegram**).

There are **three** transcription backends:

- **Groq Whisper (HTTP)**: fastest to set up if you have a Groq key.
- **Local OpenAI-compatible Whisper (HTTP)**: you run a small HTTP server that exposes `POST /v1/audio/transcriptions`.
- **Local `whisper.cpp` (CLI)**: Metis spawns `whisper-cli(.exe)` and converts audio to WAV first.

---

## How it works (high level)

- Telegram voice/audio is downloaded to `~/.metis/media/…`
- Metis tries to transcribe it (if `transcription.enabled=true`)
- The transcript is injected into the inbound message as:
  - `[transcription: ...]`

If transcription fails (or is not configured), the inbound message contains a placeholder like:

- `[voice: <path>]` or `[audio: <path>]`

---

## Configuration

Edit `~/.metis/config.json` and enable transcription:

```json
{
  "transcription": {
    "enabled": true,
    "provider": "groq",
    "model": "whisper-large-v3"
  }
}
```

### Backend A) Groq Whisper (recommended)

```json
{
  "providers": {
    "groq": { "apiKey": "gsk_..." }
  },
  "transcription": {
    "enabled": true,
    "provider": "groq",
    "model": "whisper-large-v3"
  }
}
```

Groq key sources (first match wins):

- `transcription.apiKey`
- `providers.groq.apiKey`
- env var `GROQ_API_KEY`

### Backend B) Local Whisper over HTTP (OpenAI-compatible)

You must have an endpoint that supports OpenAI multipart format:

- `POST http://127.0.0.1:8090/v1/audio/transcriptions`

Example using the shipped helper:

```powershell
py -m pip install --upgrade fastapi uvicorn python-multipart openai-whisper
py .\contrib\local_whisper_openai_http.py --host 127.0.0.1 --port 8090 --model base
```

Config:

```json
{
  "transcription": {
    "enabled": true,
    "provider": "local",
    "apiBase": "http://127.0.0.1:8090/v1",
    "model": "base",
    "apiKey": ""
  }
}
```

### Backend C) Local `whisper.cpp` (CLI)

Config:

```json
{
  "transcription": {
    "enabled": true,
    "provider": "whisper_cpp",
    "whisperCpp": {
      "exePath": "C:/path/to/whisper-cli.exe",
      "modelPath": "C:/path/to/models/ggml-base.bin",
      "extraArgs": []
    }
  }
}
```

#### Required dependency: ffmpeg (and recommended: ffprobe)

In `whisper_cpp` mode Metis converts input audio to:

- **16kHz**, **mono**, **16-bit PCM** WAV

That conversion uses **`ffmpeg`**, and Metis may also use **`ffprobe`** to verify the file actually contains an audio stream (this is important when a platform downloads media with **no extension**, or the attachment is not really audio).

Make sure these commands work in the same shell/environment where you run Metis:

```powershell
ffmpeg -version
ffprobe -version
```

---

## Common problems & fixes

### 1) “Sometimes it says the file does not contain voice”

This usually happens when:

- the downloaded file has **no extension**, or
- the attachment is a **non-audio blob**, or
- the file is **0 bytes**

Metis now does a best-effort probe (via `ffprobe` if available) and will **skip** transcription for files without an audio stream instead of failing later during WAV conversion.

Fix:

- install `ffprobe` (usually comes with ffmpeg)
- ensure the downloaded file is actually an audio message in the chat app

### 2) “It struggles to convert to wav”

Causes:

- `ffmpeg` missing from PATH
- codec not supported by your ffmpeg build
- corrupted download / empty file

Fix:

- install ffmpeg and ensure `ffmpeg -version` works
- retry the voice note
- check logs (run with `--logs`) for the exact ffmpeg stderr

### 3) Whisper.cpp works for some files but not others

Some platforms send voice notes in containers like `ogg/opus` or `webm`.
Conversion requires a recent ffmpeg build with those demuxers/decoders enabled.

---

## Debug tips

- Run gateway with logs:

```bash
Metis gateway --logs
```

- Inspect the downloaded file path printed in your Telegram placeholders:
  - `[voice: C:\Users\...\AppData\...\media\... ]`
  - then try converting manually with ffmpeg to see raw errors.

