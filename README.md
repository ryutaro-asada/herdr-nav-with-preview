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

- Collapsible `workspace > tab > pane` tree with per-kind icons and agent status.
- Live preview of the selected pane, refreshed about once per second.
- `Enter` focuses the selected pane and closes the navigator.
- `n` creates a new workspace; `x` closes the selected pane (with confirmation).
- Single self-contained Rust binary; installed from a prebuilt release (or built with `cargo build --release`).

## Keys

| Key | Action |
| --- | --- |
| `↑` / `↓` (or `k` / `j`) | Move selection |
| `←` / `→` (or `h` / `l`) | Collapse / expand |
| `n` | Create a new workspace (name prompt; defaults to the next number) |
| `x` | Close the selected pane (`y`/`Enter` to confirm) |
| `Enter` | Focus the selected pane and close the navigator |
| `q` / `Esc` | Quit without jumping |

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
