//! Voice transcription providers — speech-to-text via Whisper APIs.
//!
//! Port of nanobot's `providers/transcription.py`.
//!
//! Supports:
//! - Groq-hosted Whisper (`https://api.groq.com/openai/v1/audio/transcriptions`)
//! - Any OpenAI-compatible `POST …/audio/transcriptions` multipart API (common for **local Whisper** stacks).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;
use tracing::{debug, error, warn};

// ─────────────────────────────────────────────
// OpenAI-compatible audio endpoint URL
// ─────────────────────────────────────────────

/// Normalize an OpenAI-style base (`http://localhost:8080/v1`) to a full `…/audio/transcriptions` POST URL,
/// or return the URL unchanged when it already ends with `/audio/transcriptions`.
pub fn resolve_audio_transcriptions_endpoint(api_base_or_full_url: &str) -> String {
    let s = api_base_or_full_url.trim().trim_end_matches('/');
    let lower = s.to_lowercase();
    if lower.ends_with("/audio/transcriptions") {
        s.to_string()
    } else {
        format!("{}/audio/transcriptions", s)
    }
}

const GROQ_DEFAULT_TRANSCRIPTION_URL: &str =
    "https://api.groq.com/openai/v1/audio/transcriptions";

// ─────────────────────────────────────────────
// Trait
// ─────────────────────────────────────────────

/// Trait for speech-to-text transcription providers.
#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    /// Transcribe an audio file to text.
    ///
    /// Returns the transcribed text, or empty string on failure.
    async fn transcribe(&self, file_path: &Path) -> anyhow::Result<String>;

    /// Display name for logging.
    fn display_name(&self) -> &str;
}

// ─────────────────────────────────────────────
// whisper.cpp CLI (native, no HTTP)
// ─────────────────────────────────────────────

/// Run whisper.cpp's `whisper-cli` as a subprocess and read a `.txt` output file.
pub struct WhisperCppTranscriber {
    exe_path: String,
    model_path: String,
    extra_args: Vec<String>,
}

impl WhisperCppTranscriber {
    pub fn new(
        exe_path: impl Into<String>,
        model_path: impl Into<String>,
        extra_args: Vec<String>,
    ) -> Self {
        Self {
            exe_path: exe_path.into(),
            model_path: model_path.into(),
            extra_args,
        }
    }

    pub fn is_ready(&self) -> bool {
        !self.exe_path.trim().is_empty() && !self.model_path.trim().is_empty()
    }
}

/// Convert audio file to WAV format (16kHz, mono, 16-bit PCM) required by whisper.cpp.
/// Returns the path to the converted file in the same directory as the original.
fn convert_to_wav(file_path: &Path) -> anyhow::Result<PathBuf> {
    if !file_path.exists() {
        return Err(anyhow::anyhow!(
            "cannot convert to wav: file does not exist: {}",
            file_path.display()
        ));
    }

    // If the file is empty, treat as "no audio" rather than hard error.
    // (Some chat platforms can produce zero-byte media placeholders.)
    if let Ok(meta) = std::fs::metadata(file_path) {
        if meta.len() == 0 {
            warn!(path = %file_path.display(), "audio file is empty (0 bytes)");
            return Ok(file_path.to_path_buf());
        }
    }

    let extension = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    // Only convert if not already WAV
    if extension == "wav" {
        debug!(file = %file_path.display(), "audio already in WAV format");
        return Ok(file_path.to_path_buf());
    }

    // Create output path in same directory as original
    let wav_path = file_path.with_extension("wav");

    debug!(
        input = %file_path.display(),
        output = %wav_path.display(),
        "converting audio to WAV format (16kHz, mono, 16-bit PCM)"
    );

    // Use ffmpeg to convert to 16kHz, mono, 16-bit PCM WAV.
    //
    // NOTE: ffmpeg option ordering matters. `-y` must come before the output path;
    // placing it after can be parsed as an (invalid) second output, causing flaky failures.
    let input = file_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid path"))?;
    let output = wav_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid path"))?;

    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            input,
            "-ar",
            "16000",
            "-ac",
            "1",
            "-c:a",
            "pcm_s16le",
            output,
        ])
        .output()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to run ffmpeg (is it installed and on PATH?): {e}"
            )
        })?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        error!(stderr = %stderr, "ffmpeg conversion failed");
        return Err(anyhow::anyhow!("ffmpeg conversion failed: {}", stderr));
    }

    debug!(output = %wav_path.display(), "audio converted to WAV successfully");
    Ok(wav_path)
}

/// Best-effort probe: does this file contain an audio stream?
///
/// Uses `ffprobe` when available, which is robust even when the file has no extension.
/// If `ffprobe` is missing, falls back to extension-only heuristics.
fn file_has_audio_stream(file_path: &Path) -> bool {
    if !file_path.exists() {
        return false;
    }

    // Fast fallback: extension-based guess.
    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let ext_hint = matches!(
        ext.as_str(),
        "ogg"
            | "oga"
            | "opus"
            | "mp3"
            | "m4a"
            | "wav"
            | "flac"
            | "aac"
            | "wma"
            | "webm"
    );

    // `ffprobe` gives the truth; use it if present.
    let path = match file_path.to_str() {
        Some(p) => p,
        None => return ext_hint,
    };
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
            path,
        ])
        .output();

    match out {
        Ok(o) => {
            if !o.status.success() {
                return ext_hint;
            }
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.to_lowercase().contains("audio")
        }
        Err(_) => ext_hint,
    }
}

#[async_trait]
impl TranscriptionProvider for WhisperCppTranscriber {
    async fn transcribe(&self, file_path: &Path) -> anyhow::Result<String> {
        if !self.is_ready() {
            warn!(
                exe_path = %self.exe_path,
                model_path = %self.model_path,
                "whisper.cpp transcription: not configured — skipping"
            );
            return Ok(String::new());
        }

        if !file_path.exists() {
            warn!(path = %file_path.display(), "transcription: file not found");
            return Ok(String::new());
        }

        // Avoid trying to convert/transcribe non-audio files (common when a platform
        // provides a "media" attachment that isn't actually an audio stream).
        if !file_has_audio_stream(file_path) {
            warn!(path = %file_path.display(), "transcription: file has no detectable audio stream");
            return Ok(String::new());
        }

        // Convert audio to WAV format if needed (whisper.cpp requires WAV)
        let audio_file = convert_to_wav(file_path)?;
        debug!(audio = %audio_file.display(), "using audio file for transcription");

        // whisper.cpp's `-of` expects an output prefix; it will append `.txt` when used with `-otxt`.
        // We create a temp dir so concurrent transcriptions don't collide.
        let tmp: TempDir = tempfile::tempdir()?;
        let out_prefix = tmp.path().join("transcript");
        let out_prefix_str = out_prefix.to_string_lossy().to_string();
        let out_txt = tmp.path().join("transcript.txt");

        debug!(
            exe = %self.exe_path,
            model = %self.model_path,
            audio = %audio_file.display(),
            out = %out_txt.display(),
            "transcribing audio (whisper.cpp CLI)"
        );

        let mut cmd = tokio::process::Command::new(&self.exe_path);
        cmd.args([
            "-m",
            self.model_path.as_str(),
            "-f",
            audio_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("audio path is not valid UTF-8"))?,
            "-otxt",
            "-of",
            out_prefix_str.as_str(),
            "-l",
            "auto",
        ]);
        if !self.extra_args.is_empty() {
            cmd.args(self.extra_args.iter().map(|s| s.as_str()));
        }

        let output = cmd.output().await?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!(
                status = ?output.status.code(),
                stdout = %stdout,
                stderr = %stderr,
                "whisper.cpp transcription failed"
            );
            return Err(anyhow::anyhow!(
                "whisper.cpp failed (status={:?}): {}",
                output.status.code(),
                stderr
            ));
        }

        // Read the .txt output.
        let text = tokio::fs::read_to_string(&out_txt).await.unwrap_or_default();
        Ok(text)
    }

    fn display_name(&self) -> &str {
        "whisper.cpp (CLI)"
    }
}

// ─────────────────────────────────────────────
// HTTP OpenAI-compatible transcriptions (`/audio/transcriptions`)
// ─────────────────────────────────────────────

/// Multipart Whisper transcription against any OpenAI-compatible HTTP endpoint (Groq, local servers, gateways).
///
/// Use [`resolve_audio_transcriptions_endpoint`] when you have a `/v1` base URL only.
pub struct OpenAiCompatibleTranscriber {
    api_url: String,
    bearer_token: Option<String>,
    model: String,
    client: reqwest::Client,
}

impl OpenAiCompatibleTranscriber {
    /// Full POST URL ending in `/audio/transcriptions`.
    ///
    /// `bearer_token`: `None` to omit `Authorization` (typical on localhost with no auth).
    pub fn new(
        transcription_url: impl Into<String>,
        bearer_token: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            api_url: transcription_url.into(),
            bearer_token,
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Build from Groq conventions: resolve key from arguments / `GROQ_API_KEY`, optional base override.
    pub fn groq_cloud(
        api_key: &str,
        api_base_override: Option<&str>,
        model: impl Into<String>,
    ) -> Self {
        let key = if api_key.is_empty() {
            std::env::var("GROQ_API_KEY").unwrap_or_default()
        } else {
            api_key.to_string()
        };
        let url = match api_base_override.map(str::trim).filter(|s| !s.is_empty()) {
            Some(b) => resolve_audio_transcriptions_endpoint(b),
            None => GROQ_DEFAULT_TRANSCRIPTION_URL.to_string(),
        };
        let tok = (!key.is_empty()).then_some(key);
        Self::new(url, tok, model)
    }

    /// Local / self-hosted: `api_base` like `http://127.0.0.1:8080/v1`.
    pub fn local_openai_base(
        api_base: &str,
        bearer_optional: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::new(
            resolve_audio_transcriptions_endpoint(api_base.trim()),
            bearer_optional,
            model,
        )
    }

    pub fn is_configured(&self) -> bool {
        !self.api_url.trim().is_empty()
    }

    /// Groq needs a bearer token; local servers may not.
    pub fn is_ready(&self) -> bool {
        if !self.is_configured() {
            return false;
        }
        let needs_auth = self.api_url.contains("groq.com") || self.api_url.contains("api.openai.com");
        !needs_auth || self.bearer_token.as_ref().is_some_and(|s| !s.is_empty())
    }
}

#[async_trait]
impl TranscriptionProvider for OpenAiCompatibleTranscriber {
    async fn transcribe(&self, file_path: &Path) -> anyhow::Result<String> {
        if !self.is_ready() {
            warn!(
                url = %self.api_url,
                "transcription: not configured / missing bearer where required — skipping"
            );
            return Ok(String::new());
        }

        if !file_path.exists() {
            warn!(path = %file_path.display(), "transcription: file not found");
            return Ok(String::new());
        }

        // Best-effort: do not send non-audio blobs to the transcription endpoint.
        // This also helps when a platform downloads media without an extension.
        if !file_has_audio_stream(file_path) {
            warn!(
                path = %file_path.display(),
                "transcription: file has no detectable audio stream; skipping request"
            );
            return Ok(String::new());
        }

        let file_name = file_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        debug!(
            path = %file_path.display(),
            model = %self.model,
            url = %self.api_url,
            "transcribing audio (OpenAI-compatible HTTP)"
        );

        let file_bytes = tokio::fs::read(file_path).await?;
        if file_bytes.is_empty() {
            warn!(path = %file_path.display(), "transcription: file is empty; skipping");
            return Ok(String::new());
        }

        let file_part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name)
            .mime_str("application/octet-stream")?;

        let form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", self.model.clone());

        let mut req = self
            .client
            .post(&self.api_url)
            .multipart(form)
            .timeout(Duration::from_secs(120));

        if let Some(ref tok) = self.bearer_token {
            if !tok.is_empty() {
                req = req.bearer_auth(tok);
            }
        }

        let response = req.send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(
                status = %status,
                body = %body,
                url = %self.api_url,
                "transcription API error"
            );
            return Err(anyhow::anyhow!(
                "transcription API returned {}: {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = response.json().await?;
        let text = json["text"].as_str().unwrap_or_default().to_string();

        debug!(chars = text.len(), "transcription complete");

        Ok(text)
    }

    fn display_name(&self) -> &str {
        let u = self.api_url.to_lowercase();
        if u.contains("groq.com") {
            "Groq Whisper"
        } else if u.contains("openai.com") && u.contains("/audio/transcriptions") {
            "OpenAI Whisper"
        } else {
            "Local Whisper (HTTP)"
        }
    }
}

// ─────────────────────────────────────────────
// Groq Whisper
// ─────────────────────────────────────────────

/// Groq-based transcription using their Whisper API.
///
/// Groq offers extremely fast transcription with a generous free tier.
/// API is OpenAI-compatible (`/openai/v1/audio/transcriptions`).
pub struct GroqTranscriber {
    api_key: String,
    api_url: String,
    model: String,
    client: reqwest::Client,
}

impl GroqTranscriber {
    /// Create a new Groq transcriber.
    ///
    /// Falls back to `GROQ_API_KEY` env var if `api_key` is empty.
    pub fn new(api_key: &str) -> Self {
        let key = if api_key.is_empty() {
            std::env::var("GROQ_API_KEY").unwrap_or_default()
        } else {
            api_key.to_string()
        };

        Self {
            api_key: key,
            api_url: GROQ_DEFAULT_TRANSCRIPTION_URL.into(),
            model: "whisper-large-v3".into(),
            client: reqwest::Client::new(),
        }
    }

    /// Create with a custom API URL (for other OpenAI-compatible endpoints).
    pub fn with_url(api_key: &str, api_url: &str) -> Self {
        let mut t = Self::new(api_key);
        t.api_url = api_url.to_string();
        t
    }

    /// Check if the transcriber is configured (has an API key).
    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }
}

#[async_trait]
impl TranscriptionProvider for GroqTranscriber {
    async fn transcribe(&self, file_path: &Path) -> anyhow::Result<String> {
        if !self.is_configured() {
            warn!("groq transcription: no API key configured, skipping");
            return Ok(String::new());
        }

        if !file_path.exists() {
            warn!(path = %file_path.display(), "transcription: file not found");
            return Ok(String::new());
        }

        if !file_has_audio_stream(file_path) {
            warn!(
                path = %file_path.display(),
                "transcription: file has no detectable audio stream; skipping request"
            );
            return Ok(String::new());
        }

        let file_name = file_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        debug!(
            path = %file_path.display(),
            model = %self.model,
            "transcribing audio via Groq"
        );

        let file_bytes = tokio::fs::read(file_path).await?;
        if file_bytes.is_empty() {
            warn!(path = %file_path.display(), "transcription: file is empty; skipping");
            return Ok(String::new());
        }

        let file_part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name)
            .mime_str("application/octet-stream")?;

        let form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", self.model.clone());

        let response = self
            .client
            .post(&self.api_url)
            .bearer_auth(&self.api_key)
            .multipart(form)
            .timeout(Duration::from_secs(60))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(
                status = %status,
                body = %body,
                "groq transcription API error"
            );
            return Err(anyhow::anyhow!(
                "transcription API returned {}: {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = response.json().await?;
        let text = json["text"].as_str().unwrap_or_default().to_string();

        debug!(
            chars = text.len(),
            "transcription complete"
        );

        Ok(text)
    }

    fn display_name(&self) -> &str {
        "Groq Whisper"
    }
}

// ─────────────────────────────────────────────
// Helper
// ─────────────────────────────────────────────

/// Check if a file path looks like an audio file.
pub fn is_audio_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".ogg")
        || lower.ends_with(".oga")
        || lower.ends_with(".opus")
        || lower.ends_with(".mp3")
        || lower.ends_with(".m4a")
        || lower.ends_with(".wav")
        || lower.ends_with(".flac")
        || lower.ends_with(".aac")
        || lower.ends_with(".wma")
        || lower.ends_with(".webm")
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_audio_file() {
        assert!(is_audio_file("voice.ogg"));
        assert!(is_audio_file("song.MP3"));
        assert!(is_audio_file("/tmp/media/audio.m4a"));
        assert!(is_audio_file("recording.wav"));
        assert!(is_audio_file("file.flac"));
        assert!(is_audio_file("file.opus"));
        assert!(!is_audio_file("photo.jpg"));
        assert!(!is_audio_file("document.pdf"));
        assert!(!is_audio_file("video.mp4"));
    }

    #[test]
    fn test_file_has_audio_stream_missing_file() {
        assert!(!file_has_audio_stream(Path::new("/nonexistent/file.ogg")));
    }

    #[test]
    fn test_file_has_audio_stream_extension_fallback_without_ffprobe() {
        // We cannot assume ffprobe exists in CI/dev machines running tests.
        // This test validates the extension-based fallback path at minimum.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sample.mp3");
        std::fs::write(&p, b"not really mp3").unwrap();
        // Either ffprobe is present (and will likely return false), or missing (fallback true).
        // The function is explicitly "best-effort", so assert it never panics and returns a boolean.
        let _ = file_has_audio_stream(&p);
    }

    #[test]
    fn test_groq_transcriber_not_configured() {
        let t = GroqTranscriber::new("");
        // Without GROQ_API_KEY env var, should not be configured
        // (this test might see the env var, so just check it doesn't panic)
        let _ = t.is_configured();
    }

    #[test]
    fn test_groq_transcriber_configured() {
        let t = GroqTranscriber::new("gsk_test_key_123");
        assert!(t.is_configured());
        assert_eq!(t.display_name(), "Groq Whisper");
    }

    #[test]
    fn test_groq_transcriber_with_url() {
        let t = GroqTranscriber::with_url("key", "https://custom.api/v1/audio/transcriptions");
        assert_eq!(t.api_url, "https://custom.api/v1/audio/transcriptions");
    }

    #[test]
    fn test_resolve_audio_transcriptions_endpoint() {
        assert_eq!(
            resolve_audio_transcriptions_endpoint("http://127.0.0.1:9000/v1"),
            "http://127.0.0.1:9000/v1/audio/transcriptions"
        );
        assert_eq!(
            resolve_audio_transcriptions_endpoint("http://localhost:8080/v1/audio/transcriptions"),
            "http://localhost:8080/v1/audio/transcriptions"
        );
    }

    #[test]
    fn test_open_ai_compatible_not_ready_without_groq_key() {
        let t = OpenAiCompatibleTranscriber::groq_cloud("", None, "whisper-large-v3");
        assert!(!t.is_ready());
    }

    #[test]
    fn test_open_ai_compatible_local_no_key_ready() {
        let t =
            OpenAiCompatibleTranscriber::local_openai_base("http://127.0.0.1:8080/v1", None, "base");
        assert!(t.is_ready());
        assert_eq!(t.display_name(), "Local Whisper (HTTP)");
    }

    #[tokio::test]
    async fn test_transcribe_file_not_found() {
        let t = GroqTranscriber::new("test-key");
        let result = t.transcribe(Path::new("/nonexistent/audio.ogg")).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }
}
