//! Manual check: can edit_file modify a real file whose line endings are
//! mixed, given `old_text` with plain LF (the way a model writes it)?
//!
//! cargo run -p metis-agent --example edit_probe -- <file> <first-line-of-block> [lines]

use std::collections::HashMap;

use metis_agent::tools::base::Tool;
use metis_agent::tools::filesystem::EditFileTool;
use serde_json::Value;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: edit_probe <file> <anchor> [lines]");
    let anchor = args.get(2).expect("need an anchor line");
    let take: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);

    let raw = std::fs::read(path)?;
    let crlf = raw.windows(2).filter(|w| w == b"\r\n").count();
    let lf = raw.iter().filter(|b| **b == b'\n').count() - crlf;
    println!("{path}: CRLF={crlf} LF-only={lf}");

    let content = String::from_utf8_lossy(&raw).to_string();
    // Pull a real block out of the file, then hand it back with LF endings
    // only — exactly what a model reproducing code does.
    let lf_content = content.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = lf_content.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with(anchor.as_str()))
        .unwrap_or_else(|| panic!("anchor {anchor:?} not found"));
    let block: Vec<&str> = lines[start..(start + take).min(lines.len())].to_vec();
    let old_text = block.join("\n");
    // A no-op replacement: proves matching works without changing meaning.
    let new_text = format!("{old_text}  # probe");

    println!("--- old_text (LF, {} lines) ---\n{old_text}", block.len());

    let mut params: HashMap<String, Value> = HashMap::new();
    params.insert("path".into(), Value::String(path.clone()));
    params.insert("old_text".into(), Value::String(old_text));
    params.insert("new_text".into(), Value::String(new_text));

    match EditFileTool::new(None).execute(params).await {
        Ok(msg) => println!("\nRESULT: {msg}"),
        Err(e) => println!("\nFAILED: {e}"),
    }

    let after = std::fs::read(path)?;
    let crlf2 = after.windows(2).filter(|w| w == b"\r\n").count();
    let lf2 = after.iter().filter(|b| **b == b'\n').count() - crlf2;
    println!("after: CRLF={crlf2} LF-only={lf2}");
    Ok(())
}
