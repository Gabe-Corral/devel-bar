# devel-bar

devel-bar is a standalone, display-only Wayland layer-shell bar for devel-wm. It shows per-output workspace state from devel-wm's read-only IPC feed, plus optional status widgets. It does not control the compositor.

## Planned

Workspace clicks are planned once devel-wm exposes an authorized IPC control API. Other priorities include reliable multi-output hotplug, visual checks at every edge and scale, and avoiding redraws when an output's content has not changed. The current workspace feed remains read-only.

## Run

Build with `cargo build --locked` and run `target/debug/devel-bar` inside a Wayland session supporting wlr-layer-shell. For workspace state, set `DEVEL_WM_IPC` to devel-wm's socket. Alternatively, from the sibling `devel-wm` repository, use `scripts/run-with-bar.sh` for a nested session or `scripts/run-native-with-bar.sh` from a separate text VT for explicit native testing; these scripts pass the IPC socket automatically.

Configuration is independent of devel-wm. See [docs/config.md](docs/config.md) and [config.example.toml](config.example.toml).
