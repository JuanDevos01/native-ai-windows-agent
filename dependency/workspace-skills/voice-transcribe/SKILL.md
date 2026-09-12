---
name: voice-transcribe
description: "Transcribe Telegram voice messages to text. Use when a voice note needs converting to text or when transcription fails and needs checking."
metadata: {"nanobot":{"always":false}}
---

# Voice Transcription Skill

Automatically transcribe voice messages from Telegram.

## How It Works

1. **Trigger**: User sends a voice message (.oga/.ogg) on Telegram
2. **Convert**: Convert .oga to WAV using ffmpeg (16kHz, mono)
3. **Transcribe**: Use whisper.cpp to transcribe the audio
4. **Respond**: Send transcription back to user

## Tools Used

- **ffmpeg**: Convert audio format
- **whisper.cpp CLI**: `C:\whisper-cpp\Release\whisper-cli.exe`
- **Model**: `C:\whisper-cpp\models\ggml-base.bin`

## Commands

### Convert OGA to WAV
```bash
ffmpeg -i "input.oga" -ar 16000 -ac 1 -c:a pcm_s16le output.wav
```

### Transcribe with whisper.cpp
```bash
C:\whisper-cpp\Release\whisper-cli.exe -m C:\whisper-cpp\models\ggml-base.bin -f output.wav -np
```

## Configuration

- **Language**: English (default) - can add `-l es` for Spanish
- **Model**: base (fastest, good accuracy)
- **Threads**: Use all available (-t 0)

## File Locations

- Voice files: `C:\Users\chack\.metis\media\`
- Script: `C:\Users\chack\.metis\workspace\skills\voice-transcribe\transcribe.py`
