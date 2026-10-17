mod herdr_client;

use anyhow::Result;

use herdr_client::{CliHerdr, HerdrApi};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--dump-workspaces") => {
            let api = CliHerdr::new();
            let workspaces = api.workspaces()?;
            println!("{} workspace(s):", workspaces.len());
            for w in workspaces {
                println!(
                    "  {} [{}] tabs={} panes={}{}",
                    w.workspace_id,
                    w.label,
                    w.tab_count,
                    w.pane_count,
                    if w.focused { " *focused" } else { "" }
                );
            }
        }
        _ => {
            println!("herdr-nav-preview: hello");
        }
    }
    Ok(())
}
