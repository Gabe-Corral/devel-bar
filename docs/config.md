# Configuration

devel-bar reads `$XDG_CONFIG_HOME/devel-bar/config.toml`, or `~/.config/devel-bar/config.toml` if `XDG_CONFIG_HOME` is unset. A missing default file uses built-in values. Pass `--config PATH` to select a specific file; invalid or missing explicit files stop startup. Restart the bar after editing.

Copy [config.example.toml](../config.example.toml) to the default path as a starting point. For example:

```toml
[bar]
position = "top" # top, bottom, left, or right
show_all_workspaces = false # show occupied plus active by default
thickness = 32

[layout]
start = ["workspaces"]
center = ["title"]
end = ["cpu", "memory", "clock"]

[widgets]
clock_format = "24h"

[colors]
active = "#bd93f9"
```

`[layout]` lists ordered widget IDs; omitting an ID hides it. `[bar] font` can name an installed family, with bundled DejaVu Sans as fallback. Custom `[widgets.custom.ID]` entries use `command = ["/absolute/program", "argument"]` and `interval_seconds = 600`; commands run as your user without a sandbox or implicit shell, so configure only trusted programs. These widgets are display-only and do not grant compositor control.
