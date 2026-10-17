# herdr-nav-with-preview

A [herdr](https://herdr.dev) plugin that adds a tmux `Ctrl+b w` style navigator
with a live preview to herdr.

The native herdr navigator lists your workspaces, but it does not show what is
*inside* each session. This plugin opens a full-screen overlay TUI with a
`workspace > tab > pane` tree on the left and a **live preview** of the selected
pane on the right. Press `Enter` to jump straight to the pane.

> Status: work in progress. See the roadmap in the commit history.

## Features

- Collapsible `workspace > tab > pane` tree.
- Live preview of the selected pane, refreshed about once per second.
- `Enter` focuses the selected pane and closes the overlay.
- Single self-contained Rust binary, built with `cargo build --release`.

## Requirements

- herdr `>= 0.7.0`
- Rust toolchain (`cargo`) to build the plugin.

## Install

```sh
herdr plugin install ryutaro-asada/herdr-nav-with-preview
```

## License

Apache-2.0. See [LICENSE](./LICENSE).
