#!/usr/bin/env python3
"""
Voice Transcription Script
Transcribes Telegram voice messages using faster-whisper (CTranslate2-based).
Faster and more accurate than whisper.cpp.
"""

import os
import sys
import glob
from faster_whisper import WhisperModel

# Paths
MEDIA_DIR = r"C:\Users\chack\.metis\media"
MODEL_SIZE = "tiny"  # tiny/base/small/medium/large
DEVICE = "cpu"
COMPUTE_TYPE = "int8"

# Global model instance (loaded once)
_model = None

def get_model():
    """Load model once and reuse."""
    global _model
    if _model is None:
        print(f"Loading faster-whisper model '{MODEL_SIZE}'...")
        _model = WhisperModel(MODEL_SIZE, device=DEVICE, compute_type=COMPUTE_TYPE)
        print("Model loaded.")
    return _model

def transcribe_oga(oga_file):
    """Transcribe .oga file directly using faster-whisper."""
    model = get_model()
    print(f"Transcribing: {oga_file}")
    segments, info = model.transcribe(oga_file, language="en", beam_size=5)
    print(f"Detected language: {info.language} (prob: {info.language_probability:.2f})")
    text = " ".join(segment.text for segment in segments)
    return text.strip()

def get_latest_oga():
    """Find the most recent .oga file in media directory."""
    oga_files = glob.glob(os.path.join(MEDIA_DIR, "*.oga"))
    if not oga_files:
        return None
    return max(oga_files, key=os.path.getmtime)

def main():
    if len(sys.argv) > 1:
        voice_file = sys.argv[1]
    else:
        voice_file = get_latest_oga()

    if not voice_file:
        print("No .oga file found")
        sys.exit(1)

    print(f"Processing: {voice_file}")
    text = transcribe_oga(voice_file)
    print(f"Transcript: {text}")
    return text

if __name__ == "__main__":
    main()
