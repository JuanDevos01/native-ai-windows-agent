//! Manual check: search a real OneDrive folder and confirm cloud placeholders
//! are skipped rather than downloaded.
//!
//! cargo run -p metis-agent --example search_onedrive -- <folder> <text>

use std::collections::HashMap;

use metis_agent::tools::base::Tool;
use metis_agent::tools::filesystem::SearchFilesTool;
use serde_json::Value;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let folder = args.get(1).cloned().unwrap_or_else(|| {
        format!("{}\\OneDrive", std::env::var("USERPROFILE").unwrap_or_default())
    });
    let query = args.get(2).cloned().unwrap_or_else(|| "the".to_string());

    let mut params: HashMap<String, Value> = HashMap::new();
    params.insert("path".into(), Value::String(folder.clone()));
    params.insert("query".into(), Value::String(query.clone()));

    println!("Searching {folder} for {query:?}\n");
    let started = std::time::Instant::now();
    let out = SearchFilesTool::new(None).execute(params).await?;
    println!("{out}\n\n--- took {:?} ---", started.elapsed());
    Ok(())
}
