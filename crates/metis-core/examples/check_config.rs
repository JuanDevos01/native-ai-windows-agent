//! Manual check: does the real config.json still load after a schema change,
//! and what is the SharePoint setup?
//!
//! cargo run -p metis-core --example check_config

fn main() {
    let cfg = metis_core::config::loader::load_config(None);
    let sp = &cfg.tools.sharepoint;
    let mail = &cfg.channels.email;

    let shown = |s: &str| if s.trim().is_empty() { "unset" } else { "set" };

    println!("config loaded OK\n");
    println!("tools.sharepoint");
    println!("  enabled : {}", sp.enabled);
    println!("  sites   : {:?}", sp.sites);
    println!(
        "  creds   : tenant {} / client {} / secret {}",
        shown(&sp.tenant_id),
        shown(&sp.client_id),
        shown(&sp.client_secret)
    );
    println!("\nchannels.email (the Graph app SharePoint falls back to)");
    println!(
        "  creds   : tenant {} / client {} / secret {}",
        shown(&mail.graph_tenant_id),
        shown(&mail.graph_client_id),
        if mail.graph_client_secret.trim().is_empty() {
            "unset".to_string()
        } else {
            format!("set, {} chars", mail.graph_client_secret.len())
        }
    );
}
