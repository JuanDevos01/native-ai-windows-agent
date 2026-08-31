//! Manual check: which skills does the agent actually see?
//!
//! cargo run -p metis-agent --example list_skills -- [workspace] [builtin]

use std::path::PathBuf;

use metis_agent::skills::SkillsLoader;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let workspace = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| {
        format!("{}\\.metis\\workspace", std::env::var("USERPROFILE").unwrap_or_default())
    }));
    let builtin = args.get(2).map(PathBuf::from).or_else(|| {
        let p = PathBuf::from("skills");
        p.is_dir().then_some(p)
    });

    println!("workspace skills : {}", workspace.join("skills").display());
    match &builtin {
        Some(b) => println!("built-in skills  : {}", b.display()),
        None => println!("built-in skills  : (none found)"),
    }
    println!();

    let loader = SkillsLoader::new(&workspace, builtin);
    for s in loader.list_skills(false) {
        println!("  {:<28} [{:?}]  {}", s.name, s.source, s.path.display());
    }
}
