//! Live smoke test for the pagetree browser tool (phases 1–4).
//!
//! Run with a Chromium-family browser installed (or point at one):
//! ```text
//! $env:METIS_BROWSER_EXECUTABLE = "C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"
//! cargo run -p metis-agent --example pagetree_smoke
//! ```

use std::collections::HashMap;

use metis_agent::tools::browser::BrowserTool;
use metis_agent::tools::Tool;
use serde_json::{json, Value};

async fn run(tool: &BrowserTool, params: Value) -> String {
    let map: HashMap<String, Value> = serde_json::from_value(params).unwrap();
    match tool.execute(map).await {
        Ok(out) => out,
        Err(e) => format!("ERROR: {e}"),
    }
}

#[tokio::main]
async fn main() {
    let tool = BrowserTool::new(std::env::temp_dir(), false);

    println!("=== 1. open a form (httpbin) ===");
    let form = run(
        &tool,
        json!({"action": "open", "url": "https://httpbin.org/forms/post"}),
    )
    .await;
    println!("{form}");

    let re_textbox = regex::Regex::new(r"textbox[^\[]*\[(e\d+)\]").unwrap();
    let re_radio = regex::Regex::new(r#"radio "[^"]+" \[(e\d+)\]"#).unwrap();
    let re_submit = regex::Regex::new(r#"button "Submit[^"]*" \[(e\d+)\]"#).unwrap();

    let textbox = re_textbox.captures(&form).map(|c| c[1].to_string());
    let radio = re_radio.captures(&form).map(|c| c[1].to_string());
    let submit = re_submit.captures(&form).map(|c| c[1].to_string());

    if let Some(r) = &textbox {
        println!("=== 2. type into {r} (expect a DIFF, not a tree) ===");
        let out = run(&tool, json!({"action": "type", "ref": r, "text": "Metis pagetree"})).await;
        println!("{out}\n");
    }
    if let Some(r) = &radio {
        println!("=== 3. click radio {r} (expect a DIFF) ===");
        let out = run(&tool, json!({"action": "click", "ref": r})).await;
        println!("{out}\n");
    }
    if let Some(r) = &submit {
        println!("=== 4. click submit {r} (navigation → full snapshot) ===");
        let out = run(&tool, json!({"action": "click", "ref": r})).await;
        println!("{}\n", out.lines().take(8).collect::<Vec<_>>().join("\n"));
    }

    // Phase 2: reload the form. Every backend node id is new, but fingerprints
    // should re-bind the original refs — typing into the OLD textbox ref must
    // still work.
    println!("=== 5. reload form; old refs should re-bind ===");
    let reloaded = run(
        &tool,
        json!({"action": "open", "url": "https://httpbin.org/forms/post"}),
    )
    .await;
    let same_refs = match (&textbox, re_textbox.captures(&reloaded)) {
        (Some(old), Some(c)) => old == &c[1],
        _ => false,
    };
    println!("first textbox has its old ref again: {same_refs}");
    if let Some(r) = &textbox {
        let out = run(&tool, json!({"action": "type", "ref": r, "text": "rebound!"})).await;
        println!("{out}\n");
    }

    // Phase 3: expand a truncated list on a list-heavy page.
    println!("=== 6. truncation + expand (Wikipedia) ===");
    let hn = run(
        &tool,
        json!({"action": "open", "url": "https://en.wikipedia.org/wiki/Rust_(programming_language)", "maxChars": 20000}),
    )
    .await;
    let re_expand = regex::Regex::new(r"\[expand: (e\d+)\]").unwrap();
    let marker_lines: Vec<&str> = hn.lines().filter(|l| l.contains("[expand:")).take(3).collect();
    println!("expand markers found: {}\n{}\n", marker_lines.len(), marker_lines.join("\n"));
    if let Some(c) = re_expand.captures(&hn) {
        let r = c[1].to_string();
        println!("--- expanding {r} ---");
        let out = run(&tool, json!({"action": "expand", "ref": r, "maxChars": 3000})).await;
        println!("{}\n", out.lines().take(25).collect::<Vec<_>>().join("\n"));
    } else {
        println!("(no [expand: eN] marker found on this page)\n");
    }

    println!("=== 7. stale ref error ===");
    let out = run(&tool, json!({"action": "click", "ref": "e9999"})).await;
    println!("{out}");

    // Phase 4a: file upload.
    println!("=== 8. file upload ===");
    let upload_path = std::env::temp_dir().join("pagetree_upload_test.txt");
    std::fs::write(&upload_path, b"hello from pagetree phase 4").unwrap();
    let up = run(
        &tool,
        json!({"action": "open", "url": "https://the-internet.herokuapp.com/upload", "session": "upload"}),
    )
    .await;
    println!("{}\n", up.lines().take(8).collect::<Vec<_>>().join("\n"));
    // Chrome exposes <input type=file> in the AX tree as a button named
    // "Choose File" (or similar) — not as a distinct "file" role.
    let re_file_input = regex::Regex::new(r#"button "Choose File"[^\[]*\[(e\d+)\]"#).unwrap();
    if let Some(c) = re_file_input.captures(&up) {
        let r = c[1].to_string();
        let out = run(
            &tool,
            json!({
                "action": "upload",
                "session": "upload",
                "ref": r,
                "files": [upload_path.display().to_string()]
            }),
        )
        .await;
        println!("upload to {r}:\n{out}\n");
    } else {
        println!("(no file input ref found on the page — skipped)\n");
    }
    run(&tool, json!({"action": "close", "session": "upload"})).await;

    // Phase 4b: replayable plan — export refs from one session, import into a
    // completely separate one that has never seen this page's backend ids,
    // and confirm the SAME ref numbers come back and stay functional.
    println!("=== 9. replayable plan across sessions ===");
    let opened_a = run(
        &tool,
        json!({"action": "open", "url": "https://httpbin.org/forms/post", "session": "planA"}),
    )
    .await;
    let a_textbox_ref = re_textbox.captures(&opened_a).map(|c| c[1].to_string());

    let export_out = run(&tool, json!({"action": "export_plan", "session": "planA"})).await;
    let plan_json = export_out.splitn(2, '\n').nth(1).unwrap_or("").to_string();
    println!(
        "exported plan ({} chars): {}...\n",
        plan_json.len(),
        &plan_json.chars().take(150).collect::<String>()
    );

    let import_out = run(
        &tool,
        json!({"action": "import_plan", "session": "planB", "plan": plan_json}),
    )
    .await;
    println!("{import_out}");
    let opened_b = run(
        &tool,
        json!({"action": "open", "url": "https://httpbin.org/forms/post", "session": "planB"}),
    )
    .await;
    let b_textbox_ref = re_textbox.captures(&opened_b).map(|c| c[1].to_string());
    println!(
        "planA ref = {a_textbox_ref:?}, planB ref (never saw this page before import) = {b_textbox_ref:?}, match = {}",
        a_textbox_ref == b_textbox_ref
    );
    if let Some(r) = &b_textbox_ref {
        let out = run(
            &tool,
            json!({"action": "type", "session": "planB", "ref": r, "text": "replayed plan!"}),
        )
        .await;
        println!("type into planB using the imported ref:\n{out}\n");
    }
    run(&tool, json!({"action": "close", "session": "planA"})).await;
    run(&tool, json!({"action": "close", "session": "planB"})).await;

    // Phase 4c: iframe boundaries — cross-origin (labeled, not inspectable)
    // vs. same-origin (content pierced transparently). Best-effort against
    // live third-party pages; informative rather than a strict assertion.
    println!("=== 10. cross-origin iframe labeling (reCAPTCHA demo) ===");
    let rc = run(
        &tool,
        json!({"action": "open", "url": "https://www.google.com/recaptcha/api2/demo", "session": "iframe1", "maxChars": 4000}),
    )
    .await;
    let iframe_lines: Vec<&str> = rc.lines().filter(|l| l.to_lowercase().contains("iframe")).take(5).collect();
    println!("iframe-related lines:\n{}\n", iframe_lines.join("\n"));
    run(&tool, json!({"action": "close", "session": "iframe1"})).await;

    println!("=== 11. same-origin iframe content pierced transparently ===");
    let so = run(
        &tool,
        json!({"action": "open", "url": "https://the-internet.herokuapp.com/iframe", "session": "iframe2"}),
    )
    .await;
    println!("{}\n", so.lines().take(15).collect::<Vec<_>>().join("\n"));
    run(&tool, json!({"action": "close", "session": "iframe2"})).await;

    println!("=== 12. close ===");
    let out = run(&tool, json!({"action": "close"})).await;
    println!("{out}");
}
