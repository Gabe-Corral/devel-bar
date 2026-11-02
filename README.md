# devel-bar

devel-bar is a standalone Wayland layer-shell bar for devel-wm. It shows per-output workspace state from devel-wm's read-only IPC feed, plus optional status widgets. When given the separate same-user `DEVEL_WM_CONTROL` socket, left-clicking a workspace pill switches that output's view without moving keyboard focus to it.

## Planned

Other priorities include reliable multi-output hotplug, visual checks at every edge and scale, and avoiding redraws when an output's content has not changed. Widget actions and further WM commands are not implemented. The workspace state feed remains read-only.

## Run

Build with `cargo build --locked` and run `target/debug/devel-bar` inside a Wayland session supporting wlr-layer-shell. For workspace state, set `DEVEL_WM_IPC` to devel-wm's snapshot socket; set `DEVEL_WM_CONTROL` to its separate control socket to enable clicks. Alternatively, from the sibling `devel-wm` repository, use `scripts/run-with-bar.sh` for a nested session or `scripts/run-native-with-bar.sh` from a separate text VT for explicit native testing; these scripts pass both sockets automatically.

Configuration is independent of devel-wm. See [docs/config.md](docs/config.md) and [config.example.toml](config.example.toml).
