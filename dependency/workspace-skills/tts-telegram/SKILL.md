---
name: tts-telegram
description: "Generate speech from text and send the audio to a Telegram chat. Use when asked to send a voice/audio reply or read something aloud on Telegram."
metadata: {"nanobot":{"always":false}}
---

# TTS Telegram Skill

Generate speech and automatically send to Telegram.

## Description

This skill provides text-to-speech generation with automatic Telegram delivery. It includes:
- TTS server on port 5003 (fast LJSpeech model)
- Audio file server on port 8888 (for downloads)
- Watcher script that auto-sends audio to Telegram

## Tools

### TTS API

**Endpoint:** `http://localhost:5003/synthesize`

**Request:**
```json
{
  "text": "Hello! This is a test message"
}
```

**Response:** Audio file URL (wav format)

### Telegram Audio Sender

**Module:** `tts_telegram_sender.py`

Functions:
- `send_audio_to_telegram(audio_path, chat_id)` - Send specific file
- Auto-watcher picks up new files in `audio/` folder

## Setup

1. **TTS Server (port 5003):**
   ```bash
   cd C:\Users\chack\.metis\workspace\tools\tts
   python server_fast.py
   ```

2. **File Server (port 8888):**
   ```bash
   cd C:\Users\chack\.metis\workspace\tools\tts\audio
   python -m http.server 8888
   ```

3. **Telegram Sender:**
   ```bash
   cd C:\Users\chack\.metis\workspace\tools\tts
   python tts_telegram_sender.py
   ```

## Usage

### From Python:
```python
import requests

# Generate speech
r = requests.post('http://localhost:5003/synthesize', 
                   json={'text': 'Hello Patrick!'})
# Audio auto-sent to Telegram by watcher
```

### From Command Line:
```bash
cd C:\Users\chack\.metis\workspace\tools\tts
tts --text "Hello!" --out_path audio/test.wav
```

## Files

- `server_fast.py` - Flask TTS server
- `tts_telegram_sender.py` - Telegram audio watcher/sender
- `audio/` - Generated audio files
- `skill.yaml` - Skill metadata

## Notes

- Generates English speech only (fast LJSpeech model)
- Audio ~15-20KB per second
- Auto-sends to chat ID: 8582973375
