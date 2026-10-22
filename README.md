# herdr-nav-with-preview

A [herdr](https://herdr.dev) plugin that adds a tmux `Ctrl+b w` style navigator
with a live preview to herdr.

The native herdr navigator lists your workspaces, but it does not show what is
*inside* each session. This plugin opens a full-screen **popup** TUI with a
`workspace > tab > pane` tree on the left and a **live preview** of the selected
pane on the right. Press `Enter` to jump straight to the pane.

The popup is a session-modal window: it does not add a pane to your layout and
leaves nothing behind when it closes.

## demo
https://github.com/user-attachments/assets/8452b37e-ca67-4693-be4f-cee3e05c584d



## Features

- Collapsible `workspace > tab > pane` tree. Workspaces with similar names (`api`, `api-2`, `api (main)`) are grouped together, groups sorted by name. Per-kind icons and agent status:
  working agents get an animated spinner (also on their tab and workspace, so it
  shows when collapsed), and the tree title sums up activity across all panes.
- Agent pane rows get a status chip: `working` (yellow), `waiting` (red — the
  agent needs your input), `complete` (cyan — finished, not yet viewed), `idle`.
- The tree (statuses, added/closed panes) refreshes every 2 seconds in the
  background while the navigator is open.
- Incremental search (`/`) to filter the tree by name, agent, or id.
- Live preview of the selected pane, refreshed about once per second.
- `Enter` focuses the selected pane and closes the navigator.
- `n` adds a tab, `N` adds a workspace; `x` closes the selected pane (with confirmation).
- Single self-contained Rust binary; installed from a prebuilt release (or built with `cargo build --release`).

## Keys

| Key | Action |
| --- | --- |
| `↑` / `↓` (or `k` / `j`) | Move selection |
| `←` / `→` (or `h` / `l`) | Collapse / expand |
| `/` | Incremental search: filter the tree by name, agent, or id |
| `n` | Add a tab to the selected workspace (name prompt; leave empty to let herdr or a naming plugin name it) |
| `N` | Add a workspace: pick its directory with a fuzzy finder, then name it (defaults to the directory name; `name-2`, `name-3`, … if taken) |
| `x` | Close the selected pane (`Enter` to confirm, `Esc` to cancel) |
| `Enter` | Focus the selected pane and close the navigator |
| `q` / `Esc` | Quit without jumping |

## New-workspace directory picker

`N` opens an fzf-style picker. Matching uses
[nucleo-matcher](https://crates.io/crates/nucleo-matcher) (the fzf algorithm
as used by the Helix editor) with path-aware scoring, smart case, and fzf query
syntax: space-separated terms must all match (`tmp1 sptv`), `^prefix`,
`suffix$`, `'exact`, `!exclude`. Matched characters are highlighted.
Keys: `↑`/`↓` or `Ctrl+n`/`Ctrl+p` (`Ctrl+j`/`Ctrl+k`) move, `Ctrl+u` clears,
`Enter` chooses, `Esc`/`Ctrl+c` cancels.

Candidates, in order: the selected pane's cwd, cwds of all open panes,
[zoxide](https://github.com/ajeetdsouza/zoxide) history (if installed),
siblings of open cwds, and every directory under the scan roots (default `~`,
depth 5), walked with the [ignore](https://crates.io/crates/ignore) crate
(ripgrep's walker) — similar to `fd -t d`. Hidden and `.gitignore`d dirs,
`node_modules`/`target`/`vendor`/`dist`/`build`, and `~/Library` and the other
macOS media folders are skipped. Override in `config.toml` in the plugin config
directory (`$HERDR_PLUGIN_CONFIG_DIR`):

```toml
roots = ["~/repos", "~/job"]
depth = 6
```

## Requirements

- herdr `>= 0.7.0`
- `herdr plugin install` downloads a prebuilt binary — no Rust toolchain needed.
  A Rust toolchain (`cargo`) is only required to build from source when no
  prebuilt binary is available for your platform.

## Install

```sh
herdr plugin install ryutaro-asada/herdr-nav-with-preview
```

## Local development

Link your working copy, build the binary, then open the navigator:

```sh
cargo build --release
herdr plugin link "$(pwd)"
herdr plugin pane open --plugin ryutaroasada.nav-with-preview --entrypoint nav
```

`herdr plugin link` does not run build commands, so build the release binary
yourself first. `herdr plugin install` runs the `[[build]]` step for you.

## Keybinding

The navigator is a `popup` pane entrypoint, so bind a key in your own
`~/.config/herdr/config.toml` to the command that opens it:

```toml
[[keys.command]]
key = "prefix+W"
type = "shell"
command = "herdr plugin pane open --plugin ryutaroasada.nav-with-preview --entrypoint nav"
```

`prefix+w` (lowercase) is herdr's built-in workspace navigator, so this plugin
uses `prefix+W` (shift) to avoid clobbering it. Pick any free chord you like.

## Troubleshooting

- **"could not talk to herdr"** — the plugin calls back into herdr via
  `HERDR_BIN_PATH` (injected when launched as a plugin) and falls back to
  `herdr` on `PATH`. Make sure a herdr server is running.
- **Empty tree** — open at least one workspace in herdr first.
- **Pane won't launch** — the manifest runs `./target/release/herdr-nav-preview`
  relative to the plugin root. `herdr plugin install` puts a binary there for you
  (downloading a prebuilt release, or building with `cargo` as a fallback). When
  developing with `herdr plugin link`, build it yourself first with
  `cargo build --release`.

## License

Apache-2.0. See [LICENSE](./LICENSE).
