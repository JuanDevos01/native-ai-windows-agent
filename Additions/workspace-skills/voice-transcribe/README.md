# Voice Transcription Skill

## Quick Start

```bash
# Run manually
python C:\Users\chack\.metis\workspace\skills\voice-transcribe\transcribe.py

# Or with a specific file
python C:\Users\chack\.metis\workspace\skills\voice-transcribe\transcribe.py "C:\Users\chack\.metis\media\voice.oga"
```

## Requirements

- ffmpeg in PATH
- whisper.cpp CLI: `C:\whisper-cpp\Release\whisper-cli.exe`
- Model: `C:\whisper-cpp\models\ggml-base.bin`

## Usage

This skill is triggered automatically when you send voice messages on Telegram. It will:
1. Convert the .oga audio to WAV format
2. Transcribe using whisper.cpp
3. Send the text back to you
