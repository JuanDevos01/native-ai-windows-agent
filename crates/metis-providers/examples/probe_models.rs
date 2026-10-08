//! Check, for real, whether a model works through Metis and can call tools.
//!
//! cargo run -p metis-providers --example probe_models -- <model> [<model> ...]
//!
//! For each model: one plain chat call, then one call offering a tool and a
//! task that needs it. Reports whether the reply came back and whether the
//! model actually used the tool - a model that "supports" tools on paper but
//! answers in prose instead is useless for an agent that must act.

use metis_core::config::loader::load_config;
use metis_core::types::{Message, ToolDefinition};
use metis_providers::{create_provider, LlmProvider, LlmRequestConfig};
use serde_json::json;

#[tokio::main]
async fn main() {
    let models: Vec<String> = std::env::args().skip(1).collect();
    let config = load_config(None);
    let providers = config.providers.to_map();
    let req = LlmRequestConfig { max_tokens: 400, ..Default::default() };
    let tool = ToolDefinition::new(
        "list_dir",
        "List the contents of a directory.",
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
    );

    for model in models {
        print!("{model:<26} ");
        let provider = match create_provider(&model, &providers) {
            Ok(p) => p,
            Err(e) => {
                println!("NO PROVIDER: {e}");
                continue;
            }
        };
        let t0 = std::time::Instant::now();
        let plain = provider
            .chat(&[Message::user("Reply with exactly: ok")], None, &model, &req)
            .await;
        let plain_txt = plain.content.clone().unwrap_or_default();
        let chat_ok = !plain_txt.contains("Error calling LLM") && !plain_txt.is_empty();
        let chat_s = t0.elapsed().as_secs_f32();
        if !chat_ok {
            println!("chat FAILED ({chat_s:.1}s): {}", plain_txt.chars().take(160).collect::<String>());
            continue;
        }
        let t1 = std::time::Instant::now();
        let with_tool = provider
            .chat(
                &[Message::user("What files are in C:\\Windows\\Temp? Use the tool.")],
                Some(&[tool.clone()]),
                &model,
                &req,
            )
            .await;
        let tool_s = t1.elapsed().as_secs_f32();
        let used = with_tool.tool_calls.iter().any(|c| c.function.name == "list_dir");
        let err = with_tool.content.clone().unwrap_or_default();
        if used {
            println!("chat ok ({chat_s:.1}s) · TOOLS ok ({tool_s:.1}s)");
        } else if err.contains("Error calling LLM") {
            println!("chat ok ({chat_s:.1}s) · tools FAILED: {}", err.chars().take(140).collect::<String>());
        } else {
            println!("chat ok ({chat_s:.1}s) · did NOT call the tool (answered in prose)");
        }
    }
}
