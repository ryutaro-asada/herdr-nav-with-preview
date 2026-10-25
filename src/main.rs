mod dirs;
mod herdr_client;
mod poller;
mod tree;
mod tui;
mod view;

use anyhow::{Context, Result};

use herdr_client::{herdr_bin, CliHerdr, HerdrApi};
use tree::Tree;
use tui::LoopOutcome;
use view::TreeView;

const HELP: &str = "\
herdr-nav-preview — a tmux `Ctrl+b w` style navigator with a live preview.

USAGE:
    herdr-nav-preview [OPTIONS]

Normally launched by herdr as a plugin overlay pane. Run with no arguments to
open the full-screen navigator in the current terminal.

OPTIONS:
    --help               Show this help and exit.
    --version            Show version and exit.
    --dump-workspaces    Print the workspace list (debug).
    --dump-tree          Print the workspace > tab > pane tree (debug).

KEYS (inside the navigator):
    up/down (k/j)        Move selection
    left/right (h/l)     Collapse / expand
    space                Toggle collapse
    Enter                Focus the selected pane and close
    q / Esc              Quit

ENVIRONMENT:
    HERDR_BIN_PATH       Path to the herdr binary (injected by herdr; falls
                         back to `herdr` on PATH).
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--help") | Some("-h") => {
            print!("{HELP}");
        }
        Some("--version") | Some("-V") => {
            println!("herdr-nav-preview {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--dump-workspaces") => {
            let api = CliHerdr::new();
            let workspaces = load_or_hint(|| api.workspaces())?;
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
            let tree = load_or_hint(|| Tree::fetch(&CliHerdr::new(), None))?;
            print!("{}", tree.to_indented_text());
        }
        Some(other) => {
            eprintln!("unknown argument: {other}\n");
            print!("{HELP}");
            std::process::exit(2);
        }
        None => run_tui()?,
    }
    Ok(())
}

/// Wrap a herdr call with a clear hint when the binary itself can't be found or
/// run, which is the most common first-run failure.
fn load_or_hint<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    f().with_context(|| {
        format!(
            "could not talk to herdr (using `{}`). \
             Is herdr running, and is HERDR_BIN_PATH or `herdr` on PATH?",
            herdr_bin()
        )
    })
}

fn run_tui() -> Result<()> {
    let api = CliHerdr::new();
    let tree = load_or_hint(|| Tree::fetch(&api, None))?;
    let mut view = TreeView::new(tree);

    let ctx = herdr_client::LaunchContext::from_env();
    view.select_for_launch(
        ctx.workspace_id.as_deref(),
        ctx.tab_id.as_deref(),
        ctx.focused_pane_id.as_deref(),
    );

    match tui::run(&mut view, &api)? {
        LoopOutcome::Quit | LoopOutcome::Jumped => {}
    }
    Ok(())
}
