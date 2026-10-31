# devel-bar

A standalone, minimal Wayland layer-shell workspace bar for devel-wm. It reads
the read-only `DEVEL_WM_IPC` state feed and draws occupied workspaces plus the
active workspace (even when empty), hiding other empty workspaces. Workspace
tags can instead all be shown with `[bar] show_all_workspaces = true`: tags
1–9 appear even before they have been visited, along with any other tags
reported by devel-wm. Empty inactive tags use the `inactive` color. On short bars the active tag remains
visible and tags that cannot fit are omitted. Indicators use Dracula colors
by default, and a small orange mark shows a
workspace with a focused window. It reserves 32 logical pixels by default on
its configured edge of **each named Wayland output**, with workspace state
matched by output name. It adds/removes surfaces as outputs appear/disappear.
The compact indicators use subtly rounded corners and a small purple edge
accent for the active workspace; narrow edges prioritize that workspace. Buffers render at the
output's integer scale while keeping the configured logical-pixel exclusive zone.
An upright local clock sits at the far end of each bar by default, with two short
rows on vertical bars. It checks at the minute boundary and redraws only if
the displayed time changes. On short bars, workspaces take priority and the
clock is hidden. The default 12-hour format shows `1:28` (no leading zero or
AM/PM); set `[widgets] clock_format = "24h"` to show `13:28` instead. Set
`[widgets] clock = false` to hide it and disable its timer.
CPU and memory use `/proc/stat` and `/proc/meminfo`, respectively, and show
percentages. They update in background workers (2 and 5 seconds by default).
Status items use bundled DejaVu Sans by default; long text is clipped to its badge.
The ordered `[layout]` lists control placement. Workspaces take priority on
short bars; status items that do not fit are omitted rather than overlapping.
The default center widget shows the focused window title for that output's
active workspace when devel-wm provides a version-2 read-only IPC snapshot.
Empty workspaces leave the center clear; version-1 snapshots still work but
have no title. Unlike CPU/memory badges, the title has **no separate filled
background**: it uses the bar background and takes the available center space
between the workspace group and the status widgets. Long titles are clipped
within that space; side bars wrap short upright lines down the free column.
The font license is in `assets/fonts/LICENSE-DejaVu`.
Set `DEVEL_WM_OUTPUT=DP-4` to show only one output's bar (both placement and
workspace state); if the output is absent, the process waits for it. The
compositor must advertise output names for per-output state matching. No click
commands yet.

Build with `cargo build --locked` in this directory. Start it inside a Wayland
session with wlr-layer-shell, optionally passing `DEVEL_WM_IPC` for workspace
state. From devel-wm, `scripts/run-with-bar.sh` (nested) and
`scripts/run-native-with-bar.sh` (from a separate Linux text VT) build and start
it with the matching socket. Native mode is never started automatically.

Configuration is read on startup from `$XDG_CONFIG_HOME/devel-bar/config.toml`,
or `~/.config/devel-bar/config.toml` when `XDG_CONFIG_HOME` is unset. Missing
default config uses built-in values; `--config PATH` requires a valid file.
Copy [`config.example.toml`](config.example.toml) to the default path to begin.

```toml
[bar]
position = "top" # top, bottom, left, or right; default top
show_all_workspaces = false # true also shows empty inactive tags
thickness = 32 # bar width on left/right; bar height on top/bottom
font_size = 11 # logical pixels
# font = "JetBrainsMono Nerd Font" # optional installed font family

[layout]
start = ["workspaces"]
center = ["title"]
end = ["cpu", "memory", "clock"]

[widgets]
clock = true
clock_format = "12h" # or "24h"

[widgets.cpu]
interval_seconds = 2

[widgets.memory]
interval_seconds = 5

[colors]
background = "#282a36"
active = "#bd93f9"
occupied = "#44475a"
inactive = "#6272a4"
text = "#f8f8f2"
focused_marker = "#ffb86c"
```

All color fields are optional, but when set must be quoted `#RRGGBB` values.
`[bar] thickness` sets both the layer surface size and its exclusive zone,
in logical pixels (24–96); `font_size` accepts 8–32 logical pixels and must
fit within `thickness - 8`. `font` is an optional installed font family name
(up to 128 bytes). Like devel-wm, devel-bar resolves it once at startup using
fontconfig's `fc-match` and rejects substituted families; if the family or
`fc-match` is unavailable, it warns and uses bundled DejaVu Sans. The bundled
font is also used when `font` is omitted. The
workspace numerals and clock remain compact built-in pixel glyphs; the font
setting affects the title, CPU/memory, and custom widget text. Larger fonts
may cause text to clip or widgets to be omitted on short bars.
`active` colors the thin accent, `occupied` colors visible pill surfaces, and
`focused_marker` colors the small focus mark. `inactive` colors empty inactive
tags when `show_all_workspaces` is enabled; the default view hides them.
Unknown settings or invalid values fail startup with a diagnostic. Restart to
apply changes. This config is independent of devel-wm and devel-menu; shared
theme-file support is not implemented.

## Custom text widgets

To add a widget, give it a unique ID, an explicit executable and arguments,
then include its ID in a layout list:

```toml
[layout]
start = ["workspaces"]
end = ["weather", "cpu", "memory", "clock"]

[widgets.custom.weather]
command = ["/usr/bin/printf", "Sunny 21C\n"] # replace with your own program
interval_seconds = 600
```

The command prints one UTF-8 line, e.g. `Sunny 21C`. No shell is added unless
you explicitly configure one. Commands run as your user in devel-bar, **not**
in devel-wm, and are not sandboxed; only use programs you trust. At most eight
custom widgets may be defined; each update has a 2-second timeout, 256-byte
output limit, 64-character display limit, and an interval of 2–3600 seconds.
Invalid/failed output shows `<id>: --`. Vertical bars display short upright
label/value rows; long custom text can be truncated there. All widgets are
display-only—this does not enable compositor commands or workspace clicks.

For a quick live test, run [`test-widget.sh`](test-widget.sh) once in a terminal;
it prints a changing `Demo HH:MM:SS` line. Add it to your bar config using its
**absolute path** (the command argv does not expand `~` or shell variables):

```toml
[layout]
start = ["workspaces"]
end = ["demo", "cpu", "memory", "clock"]

[widgets.custom.demo]
command = ["/absolute/path/to/devel-bar/test-widget.sh"]
interval_seconds = 2
```

Restart devel-bar to apply this config; the demo text should change every two
seconds. Omit `demo` from `[layout].end` after testing to hide it and stop its
worker.
