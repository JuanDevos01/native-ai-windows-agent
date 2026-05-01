#!/usr/bin/env python3
"""
Local OpenAI-compatible /v1/audio/transcriptions for Metis (Windows friendly).

Why this exists:
  • `pip install openai-whisper` gives you Python + often a `whisper` CLI — not an HTTP URL.
  • Metis `transcription.provider = "local"` expects HTTP `POST …/audio/transcriptions`
    (multipart, JSON `{"text":"…"}`), same rough shape as OpenAI.

Usage (Windows PowerShell example):

  py -m pip install --upgrade fastapi uvicorn python-multipart openai-whisper
  py contrib/local_whisper_openai_http.py --host 127.0.0.1 --port 8090 --model base

Then in ~/.metis/config.json:

  "transcription": {
    "enabled": true,
    "provider": "local",
    "apiBase": "http://127.0.0.1:8090/v1",
    "model": "ignored-by-this-server",
    "apiKey": ""
  }

The server loads Whisper once (--model selects tiny/base/small/medium/large/large-v2).
Metis sends `model` in the multipart form; this script prefers that field when present.
"""

from __future__ import annotations

import argparse
import tempfile
from pathlib import Path

import uvicorn
from fastapi import FastAPI, File, HTTPException, UploadFile, Form


def build_app(default_model: str) -> FastAPI:
    cache: dict[str, object] = {}

    app = FastAPI(title="Local Whisper OpenAI-compat bridge for Metis")

    def get_model(model_name: str):
        model_name = (model_name or default_model).strip()
        if not model_name:
            model_name = default_model
        if model_name not in cache:
            import whisper

            cache[model_name] = whisper.load_model(model_name)
        return cache[model_name]

    @app.post("/v1/audio/transcriptions")
    async def transcribe(
        file: UploadFile = File(...),
        model: str | None = Form(None),
        language: str | None = Form(None),
    ):
        try:
            m = get_model(model or default_model)
        except Exception as e:
            raise HTTPException(status_code=500, detail=f"Failed to load Whisper model: {e}") from e

        suffix = Path(file.filename or "audio.ogg").suffix or ".audio"
        with tempfile.NamedTemporaryFile(delete=False, suffix=suffix) as tmp:
            data = await file.read()
            tmp.write(data)
            tmp_path = tmp.name

        try:
            kw = {}
            if language:
                kw["language"] = language
            result = m.transcribe(tmp_path, **kw)
            text = (result or {}).get("text") or ""
            return {"text": text.strip()}
        except Exception as e:
            raise HTTPException(status_code=500, detail=f"transcription failed: {e}") from e
        finally:
            try:
                Path(tmp_path).unlink(missing_ok=True)
            except OSError:
                pass

    @app.get("/healthz")
    def healthz():
        return {"ok": True}

    return app


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8090)
    p.add_argument(
        "--model",
        default="base",
        help="Whisper model id (tiny, base, small, medium, large, large-v2, …)",
    )
    args = p.parse_args()

    app = build_app(args.model)
    uvicorn.run(app, host=args.host, port=args.port, log_level="info")


if __name__ == "__main__":
    main()
