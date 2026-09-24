//! Filesystem tools — read, write, edit, list directory.
//!
//! Port of nanobot's `agent/tools/filesystem.py`.
//! Each tool optionally restricts paths to an `allowed_dir`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{require_string, Tool};

// ─────────────────────────────────────────────
// Shared path helper
// ─────────────────────────────────────────────

/// Resolve a user-supplied path, optionally restricting it to `allowed_dir`.
///
/// Returns `Err` if the resolved path is outside the allowed directory.
fn resolve_path(path: &str, allowed_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    // Expand ~ to home directory
    let expanded = if path.starts_with("~/") || path == "~" {
        if let Some(home) = dirs_like_home() {
            home.join(&path[2..])
        } else {
            PathBuf::from(path)
        }
    } else {
        PathBuf::from(path)
    };

    // Canonicalize if the path exists, otherwise use the expanded form
    let resolved = if expanded.exists() {
        expanded.canonicalize().unwrap_or(expanded)
    } else {
        // For write operations the file may not exist yet;
        // canonicalize the parent if possible.
        if let Some(parent) = expanded.parent() {
            if parent.exists() {
                let canon_parent = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
                if let Some(name) = expanded.file_name() {
                    canon_parent.join(name)
                } else {
                    expanded
                }
            } else {
                expanded
            }
        } else {
            expanded
        }
    };

    // Enforce allowed_dir restriction
    if let Some(allowed) = allowed_dir {
        let allowed_canon = if allowed.exists() {
            allowed.canonicalize().unwrap_or_else(|_| allowed.to_path_buf())
        } else {
            allowed.to_path_buf()
        };
        if !resolved.starts_with(&allowed_canon) {
            anyhow::bail!(
                "Access denied: path '{}' is outside allowed directory '{}'",
                resolved.display(),
                allowed_canon.display()
            );
        }
    }

    Ok(resolved)
}

/// Best-effort home directory (avoids pulling in the `dirs` crate).
fn dirs_like_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

// ─────────────────────────────────────────────
// ReadFileTool
// ─────────────────────────────────────────────

/// Reads and returns the entire content of a file.
pub struct ReadFileTool {
    allowed_dir: Option<PathBuf>,
}

impl ReadFileTool {
    pub fn new(allowed_dir: Option<PathBuf>) -> Self {
        Self { allowed_dir }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file at the given path. Returns the full text content."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative path to the file to read"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let path_str = require_string(&params, "path")?;
        let path = resolve_path(&path_str, self.allowed_dir.as_deref())?;

        if !path.exists() {
            anyhow::bail!("File not found: {}", path.display());
        }
        if !path.is_file() {
            anyhow::bail!("Not a file: {}", path.display());
        }

        // Read as bytes then decode leniently: log files and other artifacts often contain
        // non-UTF-8 bytes, and read_to_string would hard-fail on them. Lossy decoding lets the
        // agent still inspect the file instead of falling back to shell commands.
        let bytes = std::fs::read(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", path.display()))?;

        // Refuse binary formats instead of emitting a screenful of mojibake.
        // A PDF or JPEG decoded lossily looks like text to the model, which
        // then tries to answer from the garbage — the reason "it cannot read
        // the invoice" looked like a model failure when it was really the
        // wrong tool being used. Point at the right one.
        if let Some(hint) = binary_format_hint(&path, &bytes) {
            anyhow::bail!("{hint}");
        }

        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Identify binary files that must not be dumped as lossy text, returning a
/// message naming the tool that CAN read them. Detection is by magic bytes
/// first (extension can lie), then by NUL bytes, which never appear in real
/// text files but are everywhere in binary formats.
fn binary_format_hint(path: &std::path::Path, bytes: &[u8]) -> Option<String> {
    let name = path.display().to_string();
    let starts = |sig: &[u8]| bytes.len() >= sig.len() && &bytes[..sig.len()] == sig;

    if starts(b"%PDF") {
        return Some(format!(
            "{name} is a PDF. read_file returns binary garbage for it. Use the `read_pdf` tool              instead - it extracts the real text, which is what you need for exact amounts and dates."
        ));
    }
    // JPEG / PNG / GIF / BMP magic numbers - an extension can lie, these do not.
    let png = [0x89u8, b'P', b'N', b'G'];
    if starts(&[0xFFu8, 0xD8, 0xFF]) || starts(&png) || starts(b"GIF8") || starts(b"BM") {
        return Some(format!(
            "{name} is an image. Use the `analyze_image` tool instead - read_file cannot see pictures."
        ));
    }
    if starts(&[0x50u8, 0x4B, 0x03, 0x04]) {
        return Some(format!(
            "{name} is a zip-based file (zip/docx/xlsx/pptx). read_file cannot decode it; unzip it              with the exec tool first, or ask the user for a text export."
        ));
    }
    // Generic binary: a NUL byte in the first 8KB. Real text files do not
    // contain NUL; virtually every binary format does.
    let window = &bytes[..bytes.len().min(8192)];
    if window.contains(&0) {
        return Some(format!(
            "{name} appears to be a binary file (contains NUL bytes), so it cannot be read as text.              Do not try to interpret it - tell the user what kind of file it is, or use a tool that              understands the format."
        ));
    }
    None
}

// ─────────────────────────────────────────────
// WriteFileTool
// ─────────────────────────────────────────────

/// Creates or overwrites a file with the given content.
pub struct WriteFileTool {
    allowed_dir: Option<PathBuf>,
}

impl WriteFileTool {
    pub fn new(allowed_dir: Option<PathBuf>) -> Self {
        Self { allowed_dir }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it if it doesn't exist or overwriting if it does. \
         Parent directories are created automatically."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative path for the file"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let path_str = require_string(&params, "path")?;
        let content = require_string(&params, "content")?;
        let path = resolve_path(&path_str, self.allowed_dir.as_deref())?;

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("Failed to create directory {}: {e}", parent.display()))?;
            }
        }

        let bytes = content.as_bytes().len();
        std::fs::write(&path, &content)
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {e}", path.display()))?;
        Ok(format!("Successfully wrote {bytes} bytes to {}", path.display()))
    }
}

// ─────────────────────────────────────────────
// EditFileTool
// ─────────────────────────────────────────────

/// Replaces a text snippet within a file (single occurrence).
pub struct EditFileTool {
    allowed_dir: Option<PathBuf>,
}

impl EditFileTool {
    pub fn new(allowed_dir: Option<PathBuf>) -> Self {
        Self { allowed_dir }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing the first occurrence of `old_text` with `new_text`. \
         Include enough context in `old_text` to uniquely identify the replacement site."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "old_text": {
                    "type": "string",
                    "description": "Exact text to find (include surrounding context for uniqueness)"
                },
                "new_text": {
                    "type": "string",
                    "description": "Text to replace old_text with"
                }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let path_str = require_string(&params, "path")?;
        let old_text = require_string(&params, "old_text")?;
        let new_text = require_string(&params, "new_text")?;
        let path = resolve_path(&path_str, self.allowed_dir.as_deref())?;

        if !path.is_file() {
            anyhow::bail!("File not found: {}", path.display());
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", path.display()))?;

        // Match without caring whether the file uses CRLF, LF, or both.
        let ranges = find_ranges_ignoring_line_endings(&content, &old_text);
        let count = ranges.len();
        if count == 0 {
            anyhow::bail!(
                "old_text not found in {}.{} (Line-ending differences are already handled, so \
                 this is a real difference in the text.)",
                path.display(),
                describe_match_failure(&content, &old_text)
            );
        }

        let mut warning = String::new();
        if count > 1 {
            warning = format!(
                "Warning: old_text appears {count} times; only the first occurrence was replaced. "
            );
        }

        // Write the replacement in the file's own line-ending style, so
        // editing a CRLF file does not leave an LF island behind.
        let new_text = with_line_ending(&new_text, dominant_line_ending(&content));

        // Replace exactly one occurrence, by byte range.
        let (start, end) = ranges[0];
        let mut updated = String::with_capacity(content.len() + new_text.len());
        updated.push_str(&content[..start]);
        updated.push_str(&new_text);
        updated.push_str(&content[end..]);
        std::fs::write(&path, &updated)
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {e}", path.display()))?;

        // Read the file back and prove the edit actually landed, rather than
        // trusting that the write succeeded. Without this the tool reports
        // "Successfully edited" on faith — and a model that then says "Done"
        // to the user has no way to know better. Verification belongs here,
        // in the tool, not in an instruction asking the model to be careful.
        let verify = std::fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!(
                "Edit wrote to {} but the file could not be read back to verify: {e}",
                path.display()
            )
        })?;
        if verify != updated {
            anyhow::bail!(
                "Edit to {} did NOT apply — the file on disk differs from what was written \
                 (something else may have modified it). Re-read the file and try again.",
                path.display()
            );
        }
        if !new_text.is_empty() && !verify.contains(&new_text) {
            anyhow::bail!(
                "Edit to {} did NOT apply — new_text is not present in the file after writing. \
                 Re-read the file and try again.",
                path.display()
            );
        }

        Ok(format!(
            "{warning}Successfully edited {} (verified: file re-read, new_text confirmed present)",
            path.display()
        ))
    }
}

// ─────────────────────────────────────────────
// ListDirTool
// ─────────────────────────────────────────────

/// Lists the contents of a directory.
pub struct ListDirTool {
    allowed_dir: Option<PathBuf>,
}

impl ListDirTool {
    pub fn new(allowed_dir: Option<PathBuf>) -> Self {
        Self { allowed_dir }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the contents of a directory. Returns file and folder names with type indicators."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the directory to list"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let path_str = require_string(&params, "path")?;
        let path = resolve_path(&path_str, self.allowed_dir.as_deref())?;

        if !path.is_dir() {
            anyhow::bail!("Not a directory: {}", path.display());
        }

        let mut entries: Vec<String> = Vec::new();
        let mut dir_entries: Vec<_> = std::fs::read_dir(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read directory {}: {e}", path.display()))?
            .filter_map(|e| e.ok())
            .collect();

        // Sort by name
        dir_entries.sort_by_key(|e| e.file_name());

        for entry in dir_entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry
                .file_type()
                .map(|ft| ft.is_dir())
                .unwrap_or(false);
            if is_dir {
                entries.push(format!("📁 {name}"));
            } else {
                entries.push(format!("📄 {name}"));
            }
        }

        if entries.is_empty() {
            Ok("(empty directory)".into())
        } else {
            Ok(entries.join("\n"))
        }
    }
}

// ─────────────────────────────────────────────
// ─────────────────────────────────────────────
// Line-ending-tolerant matching
// ─────────────────────────────────────────────

/// Rewrite CR and CRLF to LF, returning the result plus a map from each byte
/// offset in the normalised string back to its offset in the original.
///
/// The map has one entry per normalised byte plus a final entry for the end,
/// so a normalised range `a..b` maps to the original range `map[a]..map[b]`.
fn normalize_newlines_with_map(s: &str) -> (String, Vec<usize>) {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut map: Vec<usize> = Vec::with_capacity(bytes.len() + 1);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            map.push(i);
            out.push(b'\n');
            // Consume CRLF as one unit; a lone CR also becomes one LF.
            i += if i + 1 < bytes.len() && bytes[i + 1] == b'\n' { 2 } else { 1 };
        } else {
            map.push(i);
            out.push(bytes[i]);
            i += 1;
        }
    }
    map.push(bytes.len());
    // Only ASCII CR bytes were dropped, and CR never appears inside a
    // multi-byte UTF-8 sequence, so the result is still valid UTF-8.
    match String::from_utf8(out) {
        Ok(norm) => (norm, map),
        Err(_) => (s.to_string(), (0..=s.len()).collect()),
    }
}

/// Normalise every line ending in `s` to LF.
fn to_lf(s: &str) -> String {
    if s.contains('\r') {
        s.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        s.to_string()
    }
}

/// Which line ending the file mostly uses, so an edit does not introduce a
/// third style into a file that already mixes two.
fn dominant_line_ending(s: &str) -> &'static str {
    let crlf = s.matches("\r\n").count();
    let total_lf = s.matches('\n').count();
    if crlf * 2 > total_lf {
        "\r\n"
    } else {
        "\n"
    }
}

/// Rewrite `text` to use `ending` for every line break.
fn with_line_ending(text: &str, ending: &str) -> String {
    let lf = to_lf(text);
    if ending == "\n" {
        lf
    } else {
        lf.replace('\n', ending)
    }
}

/// Byte ranges in `haystack` matching `needle`, ignoring line-ending style.
///
/// Exact matching is tried first and is the common case. The fallback exists
/// because a model reproducing a block of code almost always emits LF, while
/// a Windows-authored file is CRLF — and a file with *mixed* endings cannot be
/// reproduced by guessing either way. The result was `old_text not found` on
/// text that was provably in the file, which drove the agent to rewrite code
/// through PowerShell scripts instead.
fn find_ranges_ignoring_line_endings(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let exact: Vec<(usize, usize)> = haystack
        .match_indices(needle)
        .map(|(i, m)| (i, i + m.len()))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let (norm_hay, map) = normalize_newlines_with_map(haystack);
    let norm_needle = to_lf(needle);
    norm_hay
        .match_indices(&norm_needle)
        .filter_map(|(i, m)| {
            let end = i + m.len();
            Some((*map.get(i)?, *map.get(end)?))
        })
        .collect()
}

/// Best-effort hint about why `old_text` did not match, for the error message.
fn describe_match_failure(content: &str, old_text: &str) -> String {
    let first = to_lf(old_text).lines().next().unwrap_or("").trim().to_string();
    if first.is_empty() {
        return String::new();
    }
    // A unique first line means the anchor is there but the block diverges
    // further down — usually indentation or an intervening edit.
    let hits: Vec<usize> = to_lf(content)
        .lines()
        .enumerate()
        .filter(|(_, l)| l.trim() == first)
        .map(|(i, _)| i + 1)
        .collect();
    if hits.is_empty() {
        format!(
            " Its first line ({first:?}) does not appear in the file either, so re-read the file \
             — it may have changed."
        )
    } else {
        format!(
            " Its first line ({first:?}) IS present at line {}, so the rest of old_text differs \
             — most often indentation or a trailing space. Re-read that region and copy it exactly.",
            hits.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

// ─────────────────────────────────────────────
// SearchFilesTool
// ─────────────────────────────────────────────

// Windows marks OneDrive/SharePoint "Files On-Demand" stubs with these.
// A stub reports its full remote size but holds no data locally, so merely
// opening one makes Windows download it — on a synced document library that
// can mean hundreds of megabytes for a file the search then discards.
#[cfg(windows)]
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
#[cfg(windows)]
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
#[cfg(windows)]
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Is this file a cloud placeholder rather than real local data?
fn is_cloud_placeholder(md: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let a = md.file_attributes();
        a & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
    }
    #[cfg(not(windows))]
    {
        let _ = md;
        false
    }
}

/// Directories that are never worth walking and would swamp the results.
fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules"
            | "target"
            | ".git"
            | ".svn"
            | "__pycache__"
            | ".venv"
            | "venv"
            | ".cargo"
            | ".rustup"
            | "AppData"
            | "$RECYCLE.BIN"
            | "System Volume Information"
    )
}

/// Only files up to this size are scanned for content; anything larger is
/// almost certainly not a document worth searching.
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;
/// Directory depth cap, so a stray path cannot walk an entire drive.
const MAX_DEPTH: usize = 12;

/// Strip the Windows `\\?\` extended-length prefix that `canonicalize`
/// adds, which is correct but unreadable in output the user sees.
fn pretty_path(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// One match inside a file.
struct Hit {
    path: PathBuf,
    line_no: usize,
    line: String,
}

/// What the walk deliberately did not look at, so a miss is never silently
/// mistaken for "it is not there".
#[derive(Default)]
struct SearchStats {
    scanned: usize,
    skipped_cloud: usize,
    skipped_binary: usize,
    skipped_large: usize,
    truncated: bool,
}

/// Searches a directory tree by filename and/or by text inside files.
pub struct SearchFilesTool {
    allowed_dir: Option<PathBuf>,
}

impl SearchFilesTool {
    pub fn new(allowed_dir: Option<PathBuf>) -> Self {
        Self { allowed_dir }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    dir: &Path,
    depth: usize,
    name_needle: Option<&str>,
    content_needle: Option<&str>,
    include_cloud: bool,
    max_results: usize,
    name_matches: &mut Vec<PathBuf>,
    hits: &mut Vec<Hit>,
    stats: &mut SearchStats,
) {
    if depth > MAX_DEPTH || stats.truncated {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // unreadable directory: skip it rather than abort the search
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(md) = entry.metadata() else { continue };

        if md.is_dir() {
            if !is_noise_dir(&name) {
                walk(
                    &path,
                    depth + 1,
                    name_needle,
                    content_needle,
                    include_cloud,
                    max_results,
                    name_matches,
                    hits,
                    stats,
                );
                if stats.truncated {
                    return;
                }
            }
            continue;
        }
        if !md.is_file() {
            continue;
        }

        if let Some(n) = name_needle {
            if !name.to_lowercase().contains(n) {
                continue;
            }
        }

        // Filename-only search never opens the file, so placeholders cost
        // nothing and are safe to report.
        let Some(needle) = content_needle else {
            name_matches.push(path);
            if name_matches.len() >= max_results {
                stats.truncated = true;
                return;
            }
            continue;
        };

        // Content search from here on — this is where opening a placeholder
        // would trigger a download.
        if !include_cloud && is_cloud_placeholder(&md) {
            stats.skipped_cloud += 1;
            continue;
        }
        if md.len() > MAX_SCAN_BYTES {
            stats.skipped_large += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if bytes.contains(&0) {
            stats.skipped_binary += 1;
            continue;
        }
        stats.scanned += 1;
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(needle) {
                hits.push(Hit {
                    path: path.clone(),
                    line_no: i + 1,
                    line: line.trim().chars().take(200).collect(),
                });
                if hits.len() >= max_results {
                    stats.truncated = true;
                    return;
                }
            }
        }
    }
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Search a folder tree for files by name and/or by text inside them. Use this instead of \
         listing and reading files one by one. Works on any folder, including synced OneDrive and \
         SharePoint libraries. Cloud-only placeholder files are skipped by default so the search \
         does not trigger large downloads."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Folder to search in, searched recursively"
                },
                "query": {
                    "type": "string",
                    "description": "Text to find inside files (case-insensitive). Omit to search by filename only."
                },
                "name": {
                    "type": "string",
                    "description": "Only look at files whose name contains this (case-insensitive), e.g. 'invoice' or '.pdf'"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum matches to return (default 50)"
                },
                "include_cloud_files": {
                    "type": "boolean",
                    "description": "Also search OneDrive/SharePoint files that are not downloaded to this PC yet. Default false, because opening them downloads them and can be very slow."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let path_str = require_string(&params, "path")?;
        let root = resolve_path(&path_str, self.allowed_dir.as_deref())?;
        if !root.is_dir() {
            anyhow::bail!("Not a directory: {}", root.display());
        }

        let lower = |k: &str| {
            params
                .get(k)
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
        };
        let query = lower("query");
        let name = lower("name");
        if query.is_none() && name.is_none() {
            anyhow::bail!("Give at least one of 'query' (text inside files) or 'name' (filename).");
        }
        let max_results = params
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 500) as usize;
        let include_cloud = params
            .get("include_cloud_files")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut name_matches = Vec::new();
        let mut hits = Vec::new();
        let mut stats = SearchStats::default();
        walk(
            &root,
            0,
            name.as_deref(),
            query.as_deref(),
            include_cloud,
            max_results,
            &mut name_matches,
            &mut hits,
            &mut stats,
        );

        let mut out = String::new();
        if let Some(q) = query.as_deref() {
            if hits.is_empty() {
                out.push_str(&format!("No matches for '{q}' in {}.\n", pretty_path(&root)));
            } else {
                out.push_str(&format!("{} match(es):\n\n", hits.len()));
                for h in &hits {
                    out.push_str(&format!("{}:{}: {}\n", pretty_path(&h.path), h.line_no, h.line));
                }
            }
            out.push_str(&format!("\n({} file(s) scanned)", stats.scanned));
        } else if name_matches.is_empty() {
            out.push_str(&format!("No files matching that name under {}.\n", pretty_path(&root)));
        } else {
            out.push_str(&format!("{} file(s):\n\n", name_matches.len()));
            for p in &name_matches {
                out.push_str(&format!("{}\n", pretty_path(p)));
            }
        }

        // Say what was skipped. A search that quietly ignored half the folder
        // and reported "no matches" would be worse than no search at all.
        if stats.skipped_cloud > 0 {
            out.push_str(&format!(
                "\n\nNote: {} file(s) were skipped because they live in the cloud and are not \
                 downloaded to this PC. They were NOT searched, so a match could be hiding in \
                 them. Re-run with include_cloud_files=true to download and search them (slow).",
                stats.skipped_cloud
            ));
        }
        if stats.skipped_large > 0 {
            out.push_str(&format!("\nSkipped {} file(s) larger than 8 MB.", stats.skipped_large));
        }
        if stats.skipped_binary > 0 {
            out.push_str(&format!(
                "\nSkipped {} binary file(s). For PDFs use read_pdf; for images use analyze_image.",
                stats.skipped_binary
            ));
        }
        if stats.truncated {
            out.push_str("\nStopped at the result limit — narrow the search or raise max_results.");
        }
        Ok(out)
    }
}

// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(pairs: &[(&str, &str)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    // ── ReadFileTool ──

    #[tokio::test]
    async fn test_read_file_success() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "Hello, Metis!").unwrap();

        let tool = ReadFileTool::new(None);
        let result = tool
            .execute(make_params(&[("path", file.to_str().unwrap())]))
            .await
            .unwrap();
        assert_eq!(result, "Hello, Metis!");
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let tool = ReadFileTool::new(None);
        let result = tool
            .execute(make_params(&[("path", "/tmp/nonexistent_METIS_test_file.txt")]))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_read_file_restricted() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("safe");
        std::fs::create_dir(&allowed).unwrap();
        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, "nope").unwrap();

        let tool = ReadFileTool::new(Some(allowed));
        let result = tool
            .execute(make_params(&[("path", outside.to_str().unwrap())]))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Access denied"));
    }

    // ── WriteFileTool ──

    #[tokio::test]
    async fn test_write_file_create() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("output.txt");

        let tool = WriteFileTool::new(None);
        let result = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("content", "Written content"),
            ]))
            .await
            .unwrap();
        assert!(result.contains("Successfully wrote"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "Written content");
    }

    #[tokio::test]
    async fn test_write_file_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join("deep").join("file.txt");

        let tool = WriteFileTool::new(None);
        let result = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("content", "deep content"),
            ]))
            .await
            .unwrap();
        assert!(result.contains("Successfully wrote"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "deep content");
    }

    // ── EditFileTool ──

    #[tokio::test]
    async fn test_edit_file_success() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("edit.txt");
        std::fs::write(&file, "Hello World").unwrap();

        let tool = EditFileTool::new(None);
        let result = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("old_text", "World"),
                ("new_text", "Metis"),
            ]))
            .await
            .unwrap();
        assert!(result.contains("Successfully edited"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "Hello Metis");
    }

    #[tokio::test]
    async fn test_edit_file_not_found_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("edit2.txt");
        std::fs::write(&file, "ABC").unwrap();

        let tool = EditFileTool::new(None);
        let result = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("old_text", "XYZ"),
                ("new_text", "123"),
            ]))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_edit_file_multiple_occurrences_warning() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("multi.txt");
        std::fs::write(&file, "aaa bbb aaa").unwrap();

        let tool = EditFileTool::new(None);
        let result = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("old_text", "aaa"),
                ("new_text", "ccc"),
            ]))
            .await
            .unwrap();
        assert!(result.contains("Warning"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ccc bbb aaa");
    }

    // ── ListDirTool ──

    #[tokio::test]
    async fn test_list_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file_a.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let tool = ListDirTool::new(None);
        let result = tool
            .execute(make_params(&[("path", dir.path().to_str().unwrap())]))
            .await
            .unwrap();
        assert!(result.contains("📄 file_a.txt"));
        assert!(result.contains("📁 subdir"));
    }

    #[tokio::test]
    async fn test_list_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ListDirTool::new(None);
        let result = tool
            .execute(make_params(&[("path", dir.path().to_str().unwrap())]))
            .await
            .unwrap();
        assert_eq!(result, "(empty directory)");
    }

    #[tokio::test]
    async fn test_list_dir_not_a_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "").unwrap();

        let tool = ListDirTool::new(None);
        let result = tool
            .execute(make_params(&[("path", file.to_str().unwrap())]))
            .await;
        assert!(result.is_err());
    }

    // ── Tool definitions ──

    #[test]
    fn test_tool_definitions() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ReadFileTool::new(None)),
            Box::new(WriteFileTool::new(None)),
            Box::new(EditFileTool::new(None)),
            Box::new(ListDirTool::new(None)),
        ];
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["read_file", "write_file", "edit_file", "list_dir"]);

        // Each produces a valid ToolDefinition
        for tool in &tools {
            let def = tool.to_definition();
            assert_eq!(def.tool_type, "function");
            assert!(!def.function.description.is_empty());
        }
    }

    #[tokio::test]
    async fn read_file_refuses_binary_and_names_the_right_tool() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadFileTool::new(None);

        // A PDF must point at read_pdf, not dump mojibake. This is the exact
        // failure behind "the agent cannot read my invoice".
        let pdf = dir.path().join("invoice.pdf");
        std::fs::write(&pdf, b"%PDF-1.7
1 0 obj
<</Type/Catalog>>").unwrap();
        let mut p = HashMap::new();
        p.insert("path".into(), Value::String(pdf.display().to_string()));
        let err = tool.execute(p).await.unwrap_err().to_string();
        assert!(err.contains("read_pdf"), "got: {err}");

        // An image must point at analyze_image.
        let png = dir.path().join("shot.png");
        std::fs::write(&png, [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A]).unwrap();
        let mut p = HashMap::new();
        p.insert("path".into(), Value::String(png.display().to_string()));
        let err = tool.execute(p).await.unwrap_err().to_string();
        assert!(err.contains("analyze_image"), "got: {err}");

        // Plain text still reads normally.
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "hello world").unwrap();
        let mut p = HashMap::new();
        p.insert("path".into(), Value::String(txt.display().to_string()));
        assert_eq!(tool.execute(p).await.unwrap(), "hello world");
    }


    // ── Regression: edit_file could not touch a CRLF file ────────────────
    //
    // A Python project (619 CRLF lines + 26 LF-only) could not be edited at
    // all: the model reproduces code with LF, the file stored CRLF, so every
    // old_text was rejected as "not found" despite provably being present.
    // The agent worked around it by writing PowerShell scripts that used
    // .NET ReadAllText/WriteAllText to do the replacements, which is how a
    // one-line feature turned into a pile of fixN.ps1 files.

    #[tokio::test]
    async fn edits_a_crlf_file_when_old_text_uses_lf() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("script.py");
        // Written exactly as a Windows editor would.
        std::fs::write(&file, "def setup_logging():\r\n    pass\r\n\r\nx = 1\r\n").unwrap();

        let tool = EditFileTool::new(None);
        // The model supplies LF, as models do.
        let out = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("old_text", "def setup_logging():\n    pass"),
                ("new_text", "def setup_logging():\n    configure()"),
            ]))
            .await
            .expect("a CRLF file must be editable with LF old_text");
        assert!(out.contains("Successfully edited"), "{out}");

        let after = std::fs::read_to_string(&file).unwrap();
        assert!(after.contains("configure()"), "{after:?}");
        // The file's CRLF style must survive the edit.
        assert!(after.contains("configure()\r\n"), "line endings changed: {after:?}");
        assert!(!after.contains("configure()\n\n"), "introduced an LF island: {after:?}");
    }

    #[tokio::test]
    async fn edits_a_mixed_ending_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mixed.py");
        // Mostly CRLF with a couple of LF-only lines - unguessable by hand.
        std::fs::write(
            &file,
            "import os\r\nimport sys\n\r\ndef extract_encrypted_7z(path):\r\n    return None\n",
        )
        .unwrap();

        let tool = EditFileTool::new(None);
        let out = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("old_text", "def extract_encrypted_7z(path):\n    return None"),
                ("new_text", "def extract_encrypted_7z(path):\n    return unpack(path)"),
            ]))
            .await
            .expect("a mixed-ending file must be editable");
        assert!(out.contains("Successfully edited"), "{out}");
        assert!(std::fs::read_to_string(&file).unwrap().contains("unpack(path)"));
    }

    #[tokio::test]
    async fn an_lf_file_is_still_edited_and_stays_lf() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("unix.py");
        std::fs::write(&file, "a = 1\nb = 2\n").unwrap();

        let tool = EditFileTool::new(None);
        tool.execute(make_params(&[
            ("path", file.to_str().unwrap()),
            ("old_text", "a = 1\nb = 2"),
            ("new_text", "a = 1\nb = 3"),
        ]))
        .await
        .unwrap();

        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after, "a = 1\nb = 3\n", "an LF file must not gain CRLF");
    }

    #[tokio::test]
    async fn genuinely_absent_text_still_fails_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.py");
        std::fs::write(&file, "def real():\r\n    pass\r\n").unwrap();

        let tool = EditFileTool::new(None);
        let err = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("old_text", "def imaginary():\n    pass"),
                ("new_text", "x"),
            ]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        // The message must rule line endings OUT, so the next attempt does not
        // chase the wrong cause.
        assert!(err.contains("already handled"), "{err}");
        assert!(err.contains("does not appear"), "{err}");
    }

    #[tokio::test]
    async fn a_diverging_block_is_diagnosed_by_its_anchor_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.py");
        std::fs::write(&file, "def setup():\r\n        deep_indent()\r\n").unwrap();

        let tool = EditFileTool::new(None);
        let err = tool
            .execute(make_params(&[
                ("path", file.to_str().unwrap()),
                ("old_text", "def setup():\n    deep_indent()"),
                ("new_text", "x"),
            ]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("IS present at line 1"), "{err}");
        assert!(err.contains("indentation"), "{err}");
    }

    // ── the helpers themselves ──

    #[test]
    fn normalisation_map_round_trips_offsets() {
        let original = "a\r\nb\nc\r\n";
        let (norm, map) = normalize_newlines_with_map(original);
        assert_eq!(norm, "a\nb\nc\n");
        // "b" is at normalised offset 2; in the original it sits after "a\r\n".
        let i = norm.find('b').unwrap();
        assert_eq!(&original[map[i]..map[i + 1]], "b");
    }

    #[test]
    fn normalisation_preserves_multibyte_text() {
        let original = "héllo → wörld\r\nsecond\r\n";
        let (norm, map) = normalize_newlines_with_map(original);
        assert_eq!(norm, "héllo → wörld\nsecond\n");
        let i = norm.find("second").unwrap();
        assert_eq!(&original[map[i]..map[i + 6]], "second");
    }

    #[test]
    fn dominant_ending_follows_the_majority() {
        assert_eq!(dominant_line_ending("a\r\nb\r\nc\n"), "\r\n");
        assert_eq!(dominant_line_ending("a\nb\nc\r\n"), "\n");
        assert_eq!(dominant_line_ending("no newlines"), "\n");
    }

    // ── SearchFilesTool ──

    #[tokio::test]
    async fn search_finds_text_inside_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        std::fs::write(dir.path().join("sub/deeper/note.txt"), "hello\nInvoice 4711 total\nbye")
            .unwrap();
        std::fs::write(dir.path().join("other.txt"), "nothing here").unwrap();

        let tool = SearchFilesTool::new(None);
        let out = tool
            .execute(make_params(&[
                ("path", dir.path().to_str().unwrap()),
                ("query", "invoice 4711"),
            ]))
            .await
            .unwrap();

        assert!(out.contains("note.txt"), "{out}");
        assert!(out.contains(":2:"), "should report the line number: {out}");
        assert!(!out.contains("other.txt"), "{out}");
    }

    #[tokio::test]
    async fn search_by_name_does_not_need_a_query() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Invoice_2026.pdf"), "%PDF-1.4 binary").unwrap();
        std::fs::write(dir.path().join("readme.md"), "text").unwrap();

        let tool = SearchFilesTool::new(None);
        let out = tool
            .execute(make_params(&[
                ("path", dir.path().to_str().unwrap()),
                ("name", "invoice"),
            ]))
            .await
            .unwrap();

        assert!(out.contains("Invoice_2026.pdf"), "{out}");
        assert!(!out.contains("readme.md"), "{out}");
    }

    #[tokio::test]
    async fn search_requires_query_or_name() {
        let dir = tempfile::tempdir().unwrap();
        let tool = SearchFilesTool::new(None);
        let err = tool
            .execute(make_params(&[("path", dir.path().to_str().unwrap())]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("query"), "{err}");
    }

    #[tokio::test]
    async fn search_skips_binaries_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        // A NUL byte marks this as binary; grepping it would emit garbage.
        std::fs::write(dir.path().join("blob.bin"), b"secret\x00\x00payload").unwrap();
        std::fs::write(dir.path().join("plain.txt"), "secret payload").unwrap();

        let tool = SearchFilesTool::new(None);
        let out = tool
            .execute(make_params(&[
                ("path", dir.path().to_str().unwrap()),
                ("query", "secret"),
            ]))
            .await
            .unwrap();

        assert!(out.contains("plain.txt"), "{out}");
        assert!(!out.contains("blob.bin"), "binary should not be a hit: {out}");
        assert!(out.contains("binary file"), "should disclose the skip: {out}");
    }

    #[tokio::test]
    async fn search_respects_allowed_dir() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let tool = SearchFilesTool::new(Some(allowed.path().to_path_buf()));
        let err = tool
            .execute(make_params(&[
                ("path", outside.path().to_str().unwrap()),
                ("query", "x"),
            ]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Access denied"), "{err}");
    }

    #[test]
    fn ordinary_local_files_are_not_mistaken_for_cloud_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("real.txt");
        std::fs::write(&f, "on disk").unwrap();
        assert!(!is_cloud_placeholder(&std::fs::metadata(&f).unwrap()));
    }
}
