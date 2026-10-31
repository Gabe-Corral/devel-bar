# devel-bar roadmap

- [x] Replace the prototype with a standalone layer-shell bar using devel-wm's
  read-only workspace IPC feed.
- [x] Support top, bottom, left and right placement with a reserved 32-logical-
  pixel edge; use Dracula defaults and validated TOML color overrides.
- [ ] Visually verify all four positions and exclusive zones in nested and
  native sessions, including scaled output and edge cases.
- [x] Show separate layer surfaces and output-local workspace state on both
  native displays; the user reported the two-output check worked. The
  implementation uses one shared IPC feed and output-name matching.
- [ ] Verify bar removal and return after an actual connector unplug/replug;
  powering off the DP display may not signal a logical disconnect.
- [ ] Build a quiet, minimal Dracula visual system: subtly lighter widget
  surfaces on the dark bar, consistent padding, rounded workspace indicators,
  clear typography, and restrained active/focused accents. Keep all four edge
  positions; lay out top/bottom horizontally and left/right vertically with
  upright text, not rotated horizontal content. The first pass uses padded,
  rounded workspace pills with a narrow active accent, upright edge-specific
  layout, a focused marker, active-view priority on short edges, and scale-aware
  buffers. Rendering tests pass; visual verification on actual outputs and
  later widget layout are still needed before marking this complete.
- [x] Soften workspace pill corners after the first rendering pass; retain
  the padded surface, narrow active accent, and focused-window mark. Visual
  preference still needs confirmation in a running session.
- [x] Make devel-bar's installed font family/size and logical bar thickness configurable
  with validated defaults and a bundled font fallback. Thickness controls the
  layer-shell exclusive zone and cross-axis rendering; unit tests cover all
  four placements and scale-two buffers. Live visual checks remain open.
  Keep these settings local to devel-bar; do not require a shared theme file.
- [x] Show only occupied workspaces and the active workspace on each output,
  i3-style. Keep the active workspace visible even when empty; hide other empty
  workspaces. Preserve ascending workspace order and distinct active, occupied,
  and focused-window states as windows move, close, or outputs change.
- [x] Offer opt-in `[bar] show_all_workspaces = true` for empty inactive tags,
  including unvisited 1–9 tags omitted from the compositor's lazy workspace
  snapshot, while keeping occupied-plus-active as the default. Reserve room
  for tags ahead of status widgets when enabled and keep the active tag on
  short edges. Rendering tests pass; live appearance still needs checking.
- [ ] Add a focused-window title for each output's active workspace, with a
  sensible empty state and truncation when space is limited. The current
  read-only devel-wm IPC snapshot is now version 2 with a bounded per-output
  focused title; devel-bar accepts version 1 without titles and places the
  title in its center slot by default. The title is plain text on the bar
  background, filling the free space between workspaces and status widgets,
  without a separate pill; long text is clipped or wrapped on side bars.
  Unit tests pass. Check real-client
  title changes, focus across outputs, empty workspaces and side-bar
  truncation before marking the visual workflow verified.
- [x] Add CPU usage and memory usage widgets with clear units and graceful
  unavailable states. Collect host metrics within devel-bar at modest,
  configurable intervals on background workers, not during rendering. Pure
  sampling tests pass; live visual and idle measurements remain open.
- [x] Make widget placement simple and ordered: `[layout] start`, `center`,
  and `end` lists of widget IDs; map those zones to upright vertical layouts
  on side bars. Keep workspaces visible first on short outputs, then hide
  lower-priority status items rather than allowing overlap. Validate unknown
  IDs, duplicates, and invalid intervals at startup; preserve existing
  `[widgets] clock` / `clock_format` settings. Pure layout tests pass; physical
  checks of short and side bars remain open.
- [x] Let users add a plain-text widget without recompiling: give a named
  `[widgets.custom.<id>]` an explicit command argv and update interval, then
  place its ID in `[layout]`. Run opt-in commands as the user, never in
  devel-wm, without an implicit shell or compositor-control rights. Bound
  execution time, concurrent processes, captured bytes, rendered length, and
  refresh rate; sanitize control characters and show a clear unavailable
  state on failure. Command work must not block input or bar rendering. The
  initial command worker and event channel are in place; field testing remains.
- [ ] Document and test one custom-command example (including failure,
  timeout, malformed UTF-8, output overflow, and short/vertical layouts).
  The documented `/usr/bin/printf` example, executable `test-widget.sh` demo,
  and pure failure/timeout/layout tests are in place; test the demo visually
  in a running nested and native bar before marking complete.
  Consider a push-based bar-only update interface later for event-driven
  custom widgets; do not require one for the initial single-line command case.
- [ ] Add a configurable local clock, updating only when the displayed time
  changes. Make the workspaces, focused title, CPU, memory, and clock widgets
  individually configurable, with a quiet default layout and predictable
  overflow/priority behavior on short or vertical bars. An initial local
  clock now checks at minute boundaries, is upright on all four edges, yields
  space to workspaces on short bars, defaults to unpadded 12-hour time, and
  supports `[widgets] clock_format = "24h"` or `clock = false`. The other
  widgets and their configuration remain to implement.
- [ ] Redraw only outputs whose displayed content changed; test idle activity,
  update cadence, output hotplug, scale changes, and all four placements.
- [ ] Keep the bar display-only until devel-wm exposes an authorized control
  interface; clickable workspace switching must not use the read-only feed as
  permission to issue commands.
- [ ] Revisit optional shared font/color theme precedence with devel-wm and
  devel-menu while keeping each app runnable on its own.
