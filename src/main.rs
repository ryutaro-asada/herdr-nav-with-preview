mod herdr_client;
mod poller;
mod tree;
mod tui;
mod view;

use anyhow::Result;

use herdr_client::{CliHerdr, HerdrApi};
use tree::Tree;
use tui::LoopOutcome;
use view::TreeView;

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
        Some("--dump-tree") => {
            let api = CliHerdr::new();
            let tree = Tree::build(api.workspaces()?, api.tabs()?, api.panes()?);
            print!("{}", tree.to_indented_text());
        }
        _ => run_tui()?,
    }
    Ok(())
}

fn run_tui() -> Result<()> {
    let api = CliHerdr::new();
    let tree = Tree::build(api.workspaces()?, api.tabs()?, api.panes()?);
    let mut view = TreeView::new(tree);
    match tui::run(&mut view, &api)? {
        LoopOutcome::Quit => {}
        LoopOutcome::Jumped => {}
    }
    Ok(())
}
