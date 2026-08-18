//! `read_pdf` — extract a PDF's text layer as plain text.
//!
//! Invoices are the motivating case: the agent needs the exact amount and
//! date. A vision model can *look* at a page, but it reads digits by shape
//! and will occasionally return 5,000 for 50,000 or drop a decimal — not
//! acceptable for money. A PDF's embedded text layer is the actual
//! characters, so extraction is exact and also far faster and cheaper than
//! running a vision model.
//!
//! Uses `pdftotext` (poppler — free/open source). On Windows it ships with
//! Git for Windows, so it is usually already present; the path can be
//! overridden with `METIS_PDFTOTEXT`.
//!
//! Scanned PDFs (photos of paper) have no text layer. Those genuinely need
//! OCR/vision, and this tool says so explicitly instead of returning an
//! empty string the model might fill in from imagination.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{optional_i64, optional_string, require_string, Tool};

/// Cap returned text so a 300-page PDF cannot blow up the context.
const DEFAULT_MAX_CHARS: usize = 20_000;

pub struct ReadPdfTool {
    workspace: PathBuf,
}

impl ReadPdfTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    /// Locate `pdftotext`: explicit override, then PATH, then the copy that
    /// ships inside Git for Windows (present on most Windows dev machines).
    fn find_pdftotext() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("METIS_PDFTOTEXT") {
            let p = PathBuf::from(p.trim());
            if p.is_file() {
                return Some(p);
            }
        }
        let exe = if cfg!(windows) { "pdftotext.exe" } else { "pdftotext" };
        if let Ok(path) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join(exe);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        for fallback in [
            r"C:\Program Files\Git\mingw64\bin\pdftotext.exe",
            r"C:\Program Files (x86)\Git\mingw64\bin\pdftotext.exe",
            "/usr/bin/pdftotext",
            "/usr/local/bin/pdftotext",
            "/opt/homebrew/bin/pdftotext",
        ] {
            let p = PathBuf::from(fallback);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.workspace.join(p)
        }
    }

    fn clip(text: &str, max_chars: usize) -> (String, bool) {
        if text.chars().count() <= max_chars {
            return (text.to_string(), false);
        }
        (text.chars().take(max_chars).collect(), true)
    }
}

#[async_trait]
impl Tool for ReadPdfTool {
    fn name(&self) -> &str {
        "read_pdf"
    }

    fn description(&self) -> &str {
        "Extract the text of a PDF as plain text. Use this for invoices, statements, reports — \
         anything where you need exact values like an amount, date, or reference number. Layout \
         is preserved so table columns stay aligned. Prefer this over analyze_image for PDFs: it \
         returns the real characters instead of reading digits off a picture."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the .pdf file" },
                "first_page": { "type": "integer", "description": "First page to extract (1-based, optional)" },
                "last_page": { "type": "integer", "description": "Last page to extract (optional)" },
                "max_chars": { "type": "integer", "description": "Truncate output to this many characters (default 20000)" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let path_arg = require_string(&params, "path")?;
        let path = self.resolve(&path_arg);
        if !path.is_file() {
            anyhow::bail!("PDF not found: {}", path.display());
        }
        let max_chars = optional_i64(&params, "max_chars")
            .map(|v| v.clamp(500, 200_000) as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);

        let Some(bin) = Self::find_pdftotext() else {
            anyhow::bail!(
                "pdftotext is not available, so this PDF cannot be read as text. It ships with \
                 Git for Windows (C:\\Program Files\\Git\\mingw64\\bin\\pdftotext.exe) or poppler \
                 (`choco install poppler`, `apt install poppler-utils`, `brew install poppler`); \
                 set METIS_PDFTOTEXT to its path if it lives elsewhere. Do NOT guess the contents."
            );
        };

        let mut cmd = tokio::process::Command::new(&bin);
        // -layout keeps table columns aligned, which is what makes an invoice
        // line item still read as one row. -enc UTF-8 keeps accented vendor
        // names intact.
        cmd.arg("-layout").arg("-enc").arg("UTF-8");
        if let Some(f) = optional_i64(&params, "first_page") {
            cmd.arg("-f").arg(f.max(1).to_string());
        }
        if let Some(l) = optional_i64(&params, "last_page") {
            cmd.arg("-l").arg(l.max(1).to_string());
        }
        // "-" writes to stdout instead of a sibling .txt file.
        cmd.arg(path.as_os_str()).arg("-");

        let output = cmd
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run pdftotext ({}): {e}", bin.display()))?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "pdftotext failed on {} ({}): {}. Do NOT guess the contents.",
                path.display(),
                output.status,
                err.trim().chars().take(300).collect::<String>()
            );
        }

        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.trim().is_empty() {
            anyhow::bail!(
                "{} has no extractable text layer — it is almost certainly a scan or photo of \
                 paper. Reading it needs OCR, not text extraction. Tell the user this rather than \
                 guessing any amounts or dates from it.",
                path.display()
            );
        }

        let (clipped, truncated) = Self::clip(&text, max_chars);
        let note = if truncated {
            format!(
                "\n\n[truncated at {max_chars} chars — call read_pdf again with first_page/last_page for the rest]"
            )
        } else {
            String::new()
        };
        Ok(format!(
            "PDF: {}\nExtracted with: {}\n---\n{clipped}{note}",
            path.display(),
            bin.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition() {
        let tool = ReadPdfTool::new(PathBuf::from("."));
        assert_eq!(tool.to_definition().function.name, "read_pdf");
    }

    #[tokio::test]
    async fn missing_file_reported() {
        let tool = ReadPdfTool::new(PathBuf::from("."));
        let mut p = HashMap::new();
        p.insert("path".into(), json!("nope-not-here.pdf"));
        let err = tool.execute(p).await.unwrap_err();
        assert!(err.to_string().contains("PDF not found"));
    }

    #[test]
    fn clip_marks_truncation() {
        let (s, t) = ReadPdfTool::clip("abcdef", 3);
        assert_eq!(s, "abc");
        assert!(t);
        let (s, t) = ReadPdfTool::clip("abc", 10);
        assert_eq!(s, "abc");
        assert!(!t);
    }
}
