//! Vision tool — read/describe images with a vision-capable model.
//!
//! The agent model itself is often text-only (MiniMax-M2.7, for example,
//! accepts no image input at all), so an attached photo would otherwise be
//! invisible to it. Rather than switching the whole agent to a multimodal
//! model — which would change the behaviour of everything else — this
//! delegates just the image to a vision model and returns plain text the
//! agent can reason about. That is the pattern MiniMax's own docs recommend.
//!
//! Defaults to a local Ollama vision model (free, offline, no API key).
//! Both the endpoint and the model are overridable:
//!   - `METIS_VISION_API_BASE` (default `http://localhost:11434/v1`)
//!   - `METIS_VISION_MODEL`    (default `gemma3:4b`)
//!   - `METIS_VISION_API_KEY`  (optional; needed only for hosted endpoints)

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{optional_string, require_string, Tool};

const DEFAULT_API_BASE: &str = "http://localhost:11434/v1";
const DEFAULT_MODEL: &str = "gemma3:4b";
const DEFAULT_PROMPT: &str =
    "Describe this image. If it contains any text, transcribe the text exactly and completely.";

/// Analyze an image with a vision model and return a text description.
pub struct VisionTool {
    workspace: PathBuf,
    client: reqwest::Client,
}

impl VisionTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            client: reqwest::Client::builder()
                // Local vision models are slow on CPU; be patient but bounded.
                .timeout(Duration::from_secs(180))
                .build()
                .unwrap_or_default(),
        }
    }

    fn api_base() -> String {
        std::env::var("METIS_VISION_API_BASE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
    }

    fn model() -> String {
        std::env::var("METIS_VISION_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.workspace.join(p)
        }
    }

    fn guess_mime(path: &str) -> &'static str {
        let lower = path.to_lowercase();
        if lower.ends_with(".png") {
            "image/png"
        } else if lower.ends_with(".gif") {
            "image/gif"
        } else if lower.ends_with(".webp") {
            "image/webp"
        } else if lower.ends_with(".bmp") {
            "image/bmp"
        } else {
            "image/jpeg"
        }
    }

    /// Build and send the vision request once (kept separate so the retry
    /// path can rebuild it instead of cloning a multi-megabyte body).
    async fn post_once(&self, url: &str, body: &Value) -> reqwest::Result<reqwest::Response> {
        let mut req = self.client.post(url).json(body);
        if let Ok(key) = std::env::var("METIS_VISION_API_KEY") {
            if !key.trim().is_empty() {
                req = req.bearer_auth(key.trim());
            }
        }
        req.send().await
    }

    fn is_local_endpoint(url: &str) -> bool {
        url.contains("localhost") || url.contains("127.0.0.1")
    }

    /// Spawn `ollama serve` detached and wait briefly for it to accept
    /// connections. Returns whether the server became reachable.
    async fn try_start_ollama() -> bool {
        let spawned = std::process::Command::new("ollama")
            .arg("serve")
            // Ollama evicts an idle model after 5 minutes by default, so a
            // sporadic image question pays a multi-GB reload before the
            // first token. Keep it resident longer when we own the server.
            .env("OLLAMA_KEEP_ALIVE", "30m")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn();
        if spawned.is_err() {
            return false; // ollama not installed / not on PATH
        }
        let probe = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default();
        let base = Self::api_base();
        let tags = base.trim_end_matches("/v1").trim_end_matches('/').to_string();
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if probe
                .get(format!("{tags}/api/tags"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return true;
            }
        }
        false
    }

    fn base64_encode(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }
}

#[async_trait]
impl Tool for VisionTool {
    fn name(&self) -> &str {
        "analyze_image"
    }

    fn description(&self) -> &str {
        "Look at an image file and return what it shows, including any text in it (OCR). \
         Use this whenever the user sends or refers to an image — you cannot see images yourself. \
         Pass the image path exactly as given (e.g. from an '[image: <path>]' marker). \
         Optionally pass `question` to ask something specific about the image."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the image file (jpg, png, gif, webp, bmp)"
                },
                "question": {
                    "type": "string",
                    "description": "Optional specific question about the image (default: describe it and transcribe any text)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let path_arg = require_string(&params, "path")?;
        let question = optional_string(&params, "question")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_PROMPT.to_string());

        let path = self.resolve_path(&path_arg);
        if !path.is_file() {
            anyhow::bail!("Image not found: {}", path.display());
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| anyhow::anyhow!("failed to read image {}: {e}", path.display()))?;
        let data_uri = format!(
            "data:{};base64,{}",
            Self::guess_mime(&path_arg),
            Self::base64_encode(&bytes)
        );

        let model = Self::model();
        let url = format!("{}/chat/completions", Self::api_base().trim_end_matches('/'));
        let body = json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": question },
                    { "type": "image_url", "image_url": { "url": data_uri } }
                ]
            }],
            "max_tokens": 1024
        });

        // Every failure path below says exactly what to do next: a dead
        // vision backend must not turn into the agent silently giving up or
        // inventing what the image contains.
        //
        // The request is rebuilt for the retry rather than `try_clone()`d:
        // the body carries the whole base64 image (megabytes), and cloning
        // duplicated all of it on EVERY call just to keep a spare copy for a
        // retry that almost never happens.
        let mut sent = self.post_once(&url, &body).await;

        // Ollama is not a service — it exits with the terminal that started
        // it, so a local setup that worked an hour ago is often simply not
        // running now. Start it and retry once instead of surfacing a dead
        // end the user has to fix by hand.
        if sent.is_err() && Self::is_local_endpoint(&url) && Self::try_start_ollama().await {
            sent = self.post_once(&url, &body).await;
        }

        let response = sent.map_err(|e| {
            anyhow::anyhow!(
                "Could not reach the vision model at {url} ({e}), and starting it automatically \
                 did not work. Fix with: `ollama serve` and `ollama pull {model}`, or set \
                 METIS_VISION_API_BASE/METIS_VISION_MODEL to another vision endpoint. Do NOT \
                 guess what the image contains — tell the user the image could not be read and why."
            )
        })?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "Vision model '{model}' returned {status}: {}. If the model is missing, run \
                 `ollama pull {model}`; if it is not vision-capable, set METIS_VISION_MODEL to one \
                 that is. Do NOT guess what the image contains.",
                text.chars().take(300).collect::<String>()
            );
        }

        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("vision response was not JSON: {e}"))?;
        let content = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        if content.is_empty() {
            anyhow::bail!(
                "Vision model '{model}' returned an empty description. Tell the user the image \
                 could not be read rather than guessing its contents."
            );
        }

        Ok(format!(
            "Image: {}\nRead by vision model: {model}\n---\n{content}",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition() {
        let tool = VisionTool::new(PathBuf::from("."));
        let def = tool.to_definition();
        assert_eq!(def.function.name, "analyze_image");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(VisionTool::base64_encode(b"Hello"), "SGVsbG8=");
        assert_eq!(VisionTool::base64_encode(b"Hi"), "SGk=");
        assert_eq!(VisionTool::base64_encode(b"ABC"), "QUJD");
        assert_eq!(VisionTool::base64_encode(b""), "");
    }

    #[test]
    fn mime_from_extension() {
        assert_eq!(VisionTool::guess_mime("a.PNG"), "image/png");
        assert_eq!(VisionTool::guess_mime("a.jpg"), "image/jpeg");
        assert_eq!(VisionTool::guess_mime("a.unknown"), "image/jpeg");
    }

    #[tokio::test]
    async fn missing_file_is_reported() {
        let tool = VisionTool::new(PathBuf::from("."));
        let mut params = HashMap::new();
        params.insert("path".into(), json!("definitely-not-here.jpg"));
        let err = tool.execute(params).await.unwrap_err();
        assert!(err.to_string().contains("Image not found"));
    }
}
