//! Standalone layer-shell workspace bar for devel-wm.

mod status;

use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ab_glyph::{Font, FontArc, FontVec, ScaleFont, point};
use serde::{Deserialize, Deserializer, de};
use smithay_client_toolkit::reexports::{
    calloop::{
        EventLoop, Interest, Mode, PostAction,
        channel::{self, Event as ChannelEvent},
        generic::Generic,
        timer::{TimeoutAction, Timer},
    },
    calloop_wayland_source::WaylandSource,
};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

const DEFAULT_THICKNESS: u32 = 32;
const PILL_SIZE: usize = 24;
const PILL_STEP: usize = 30;
const CLOCK_HORIZONTAL: usize = 52;
const CLOCK_VERTICAL: usize = 48;
const STATUS_GAP: usize = 6;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Position {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

impl Position {
    fn vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }

    fn anchor(self) -> Anchor {
        match self {
            Self::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            Self::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            Self::Left => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
            Self::Right => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
        }
    }

    fn size(self, thickness: u32) -> (u32, u32) {
        if self.vertical() {
            (thickness, 0)
        } else {
            (0, thickness)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Color(u32);

impl Color {
    fn bytes(self) -> [u8; 4] {
        // wl_shm::Format::Argb8888 is little-endian BGRA bytes.
        self.0.to_le_bytes()
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let digits = value
            .strip_prefix('#')
            .ok_or_else(|| de::Error::custom("expected #RRGGBB"))?;
        if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(de::Error::custom("expected #RRGGBB"));
        }
        Ok(Self(
            0xff000000 | u32::from_str_radix(digits, 16).map_err(de::Error::custom)?,
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Colors {
    background: Color,
    active: Color,
    occupied: Color,
    inactive: Color,
    text: Color,
    focused_marker: Color,
}

impl Default for Colors {
    fn default() -> Self {
        // Dracula palette: background, purple, current line, selection,
        // foreground and orange.
        Self {
            background: Color(0xff282a36),
            active: Color(0xffbd93f9),
            occupied: Color(0xff44475a),
            inactive: Color(0xff6272a4),
            text: Color(0xfff8f8f2),
            focused_marker: Color(0xffffb86c),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BarOptions {
    position: Position,
    thickness: u32,
    font: Option<String>,
    font_size: f32,
    show_all_workspaces: bool,
}

impl Default for BarOptions {
    fn default() -> Self {
        Self {
            position: Position::Top,
            thickness: DEFAULT_THICKNESS,
            font: None,
            font_size: 11.0,
            show_all_workspaces: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Widgets {
    clock: bool,
    clock_format: ClockFormat,
    cpu: MetricOptions,
    memory: MetricOptions,
    custom: BTreeMap<String, CustomOptions>,
}

impl Default for Widgets {
    fn default() -> Self {
        Self {
            clock: true,
            clock_format: ClockFormat::default(),
            cpu: MetricOptions {
                interval_seconds: 2,
            },
            memory: MetricOptions {
                interval_seconds: 5,
            },
            custom: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MetricOptions {
    interval_seconds: u64,
}

impl Default for MetricOptions {
    fn default() -> Self {
        Self {
            interval_seconds: 2,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomOptions {
    command: Vec<String>,
    interval_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Layout {
    start: Vec<String>,
    center: Vec<String>,
    end: Vec<String>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            start: vec!["workspaces".into()],
            center: vec!["title".into()],
            end: vec!["cpu".into(), "memory".into(), "clock".into()],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
enum ClockFormat {
    #[default]
    #[serde(rename = "12h")]
    TwelveHour,
    #[serde(rename = "24h")]
    TwentyFourHour,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    bar: BarOptions,
    colors: Colors,
    widgets: Widgets,
    layout: Layout,
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if !(24..=96).contains(&self.bar.thickness) {
            return Err("bar thickness must be 24..=96 logical pixels".into());
        }
        if !self.bar.font_size.is_finite()
            || !(8.0..=32.0).contains(&self.bar.font_size)
            || self.bar.font_size > (self.bar.thickness - 8) as f32
        {
            return Err("bar font_size must be 8..=32 and fit within thickness - 8".into());
        }
        if let Some(family) = &self.bar.font
            && (family.trim().is_empty()
                || family.len() > 128
                || family.chars().any(char::is_control))
        {
            return Err("bar font must be a nonempty font family name without control characters (at most 128 bytes)".into());
        }
        let mut seen = HashSet::new();
        for id in self
            .layout
            .start
            .iter()
            .chain(&self.layout.center)
            .chain(&self.layout.end)
        {
            if !seen.insert(id) {
                return Err(format!("widget {id:?} appears more than once"));
            }
            if !matches!(
                id.as_str(),
                "workspaces" | "title" | "clock" | "cpu" | "memory"
            ) && !self.widgets.custom.contains_key(id)
            {
                return Err(format!("unknown widget {id:?}"));
            }
            if id == "workspaces" && self.layout.start.first() != Some(id) {
                return Err("workspaces must be first in layout.start".into());
            }
        }
        if self.widgets.custom.len() > 8 {
            return Err("at most eight custom widgets are supported".into());
        }
        for (name, options) in &self.widgets.custom {
            if name.is_empty()
                || name.len() > 32
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || matches!(
                    name.as_str(),
                    "workspaces" | "title" | "clock" | "cpu" | "memory"
                )
            {
                return Err(format!("invalid custom widget ID {name:?}"));
            }
            if options.command.is_empty()
                || options.command.len() > 16
                || options
                    .command
                    .iter()
                    .any(|arg| arg.is_empty() || arg.len() > 4096)
            {
                return Err(format!(
                    "custom widget {name:?} needs a nonempty command argv"
                ));
            }
            if !(2..=3600).contains(&options.interval_seconds) {
                return Err(format!(
                    "custom widget {name:?} interval must be 2..=3600 seconds"
                ));
            }
        }
        for (name, interval) in [
            ("cpu", self.widgets.cpu.interval_seconds),
            ("memory", self.widgets.memory.interval_seconds),
        ] {
            if !(1..=3600).contains(&interval) {
                return Err(format!("{name} interval must be 1..=3600 seconds"));
            }
        }
        Ok(())
    }

    fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let mut args = env::args_os().skip(1);
        let path = match args.next() {
            Some(flag) if flag == "--config" => {
                let path = args.next().ok_or("--config requires a path")?;
                if args.next().is_some() {
                    return Err("unexpected argument after config path".into());
                }
                Some((PathBuf::from(path), true))
            }
            Some(flag) if flag == "--help" || flag == "-h" => {
                println!(
                    "Usage: devel-bar [--config PATH]\nDefaults to $XDG_CONFIG_HOME/devel-bar/config.toml (or ~/.config/devel-bar/config.toml)."
                );
                std::process::exit(0);
            }
            Some(_) => return Err("usage: devel-bar [--config PATH]".into()),
            None => {
                let base = env::var_os("XDG_CONFIG_HOME")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| {
                        env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                    });
                base.map(|base| (base.join("devel-bar/config.toml"), false))
            }
        };
        let Some((path, explicit)) = path else {
            return Ok(Self::default());
        };
        let config: Self = match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !explicit => Self::default(),
            Err(error) => return Err(format!("{}: {error}", path.display()).into()),
        };
        config.validate()?;
        Ok(config)
    }
}

fn load_font(family: Option<&str>) -> FontArc {
    if let Some(family) = family {
        if let Some(face) = installed_font(family.trim()) {
            return face;
        }
        eprintln!("devel-bar: font family {family:?} unavailable; using bundled DejaVu Sans");
    }
    FontArc::try_from_slice(include_bytes!("../assets/fonts/DejaVuSans.ttf"))
        .expect("bundled bar font")
}

fn matched_font<'a>(text: &'a str, requested: &str) -> Option<(&'a str, u32)> {
    let mut lines = text.lines();
    let family = lines.next()?;
    let generic = ["sans-serif", "serif", "monospace", "system-ui"]
        .iter()
        .any(|alias| requested.eq_ignore_ascii_case(alias));
    if !generic
        && !family
            .split(',')
            .any(|name| name.trim().eq_ignore_ascii_case(requested))
    {
        return None;
    }
    let path = lines.next()?;
    let index = lines.next()?.parse().ok()?;
    (!path.is_empty()).then_some((path, index))
}

fn installed_font(family: &str) -> Option<FontArc> {
    // As in devel-wm, reject fontconfig substitutions for a missing family.
    let output = Command::new("fc-match")
        .args(["-f", "%{family}\n%{file}\n%{index}\n", "--", family])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let (path, index) = matched_font(&text, family)?;
    FontVec::try_from_vec_and_index(fs::read(path).ok()?, index)
        .ok()
        .map(FontArc::new)
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct BarSnapshot {
    #[serde(default = "legacy_snapshot_version")]
    version: u32,
    active_monitor: Option<u64>,
    monitors: Vec<BarMonitor>,
}

fn legacy_snapshot_version() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct BarMonitor {
    id: u64,
    name: String,
    active_workspace: u32,
    #[serde(default)]
    focused_title: Option<String>,
    workspaces: Vec<BarWorkspace>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct BarWorkspace {
    id: u32,
    occupied: bool,
    focused_window: Option<u64>,
}

fn parse_snapshot(line: &[u8]) -> io::Result<BarSnapshot> {
    let snapshot: BarSnapshot = serde_json::from_slice(line)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if !(1..=2).contains(&snapshot.version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported IPC snapshot version {}", snapshot.version),
        ));
    }
    Ok(snapshot)
}

fn monitor_for_output<'a>(snapshot: &'a BarSnapshot, name: &str) -> Option<&'a BarMonitor> {
    snapshot
        .monitors
        .iter()
        .find(|monitor| monitor.name == name)
}

fn matches_output(filter: Option<&str>, name: &str) -> bool {
    filter.is_none_or(|filter| filter == name)
}

fn visible_workspaces(monitor: &BarMonitor, show_all: bool) -> Vec<BarWorkspace> {
    let mut visible: Vec<_> = monitor
        .workspaces
        .iter()
        .filter(|ws| show_all || ws.occupied || ws.id == monitor.active_workspace)
        .cloned()
        .collect();
    if show_all {
        // The compositor creates workspaces lazily. Its snapshot lists only
        // visited tags, so display unvisited default tags without registering
        // them or inventing state in the read-only IPC feed.
        for id in 1..=9 {
            if !visible.iter().any(|ws| ws.id == id) {
                visible.push(BarWorkspace {
                    id,
                    occupied: false,
                    focused_window: None,
                });
            }
        }
    }
    visible.sort_by_key(|ws| ws.id);
    visible
}

fn digit_pixel(digit: u32, x: usize, y: usize) -> bool {
    const GLYPHS: [[u8; 5]; 10] = [
        [0b111, 0b101, 0b101, 0b101, 0b111], // 0
        [0b010, 0b110, 0b010, 0b010, 0b111], // 1
        [0b111, 0b001, 0b111, 0b100, 0b111], // 2
        [0b111, 0b001, 0b111, 0b001, 0b111], // 3
        [0b101, 0b101, 0b111, 0b001, 0b001], // 4
        [0b111, 0b100, 0b111, 0b001, 0b111], // 5
        [0b111, 0b100, 0b111, 0b101, 0b111], // 6
        [0b111, 0b001, 0b001, 0b001, 0b001], // 7
        [0b111, 0b101, 0b111, 0b101, 0b111], // 8
        [0b111, 0b101, 0b111, 0b001, 0b111], // 9
    ];
    (GLYPHS[digit as usize][y] & (0b100 >> x)) != 0
}

fn pill_start(major_length: usize) -> usize {
    if major_length >= 36 { 8 } else { 4 }
}

fn laid_out_workspaces(
    monitor: &BarMonitor,
    major_length: usize,
    show_all: bool,
) -> Vec<BarWorkspace> {
    let mut visible = visible_workspaces(monitor, show_all);
    let start = pill_start(major_length);
    let capacity = major_length
        .saturating_sub(start + 4)
        .saturating_add(PILL_STEP - PILL_SIZE)
        / PILL_STEP;
    if capacity == 0 {
        return Vec::new();
    }
    if visible.len() > capacity {
        // Keep the active view visible even when there is room for only one
        // pill; retain ascending order for the remaining occupied workspaces.
        let active = visible
            .iter()
            .position(|ws| ws.id == monitor.active_workspace);
        if let Some(index) = active
            && index >= capacity
        {
            let active = visible.remove(index);
            visible.truncate(capacity - 1);
            visible.push(active);
        } else {
            visible.truncate(capacity);
        }
    }
    visible
}

fn inside_rounded_pill(x: usize, y: usize) -> bool {
    if x >= PILL_SIZE || y >= PILL_SIZE {
        return false;
    }
    let dx = if x < 3 { 2 - x } else { x.saturating_sub(21) };
    let dy = if y < 3 { 2 - y } else { y.saturating_sub(21) };
    dx * dx + dy * dy <= 4
}

fn next_clock_check(now: SystemTime) -> Duration {
    let seconds = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Duration::from_secs(60 - seconds % 60)
}

fn clock_text(hour: i32, minute: i32, format: ClockFormat) -> String {
    match format {
        ClockFormat::TwelveHour => format!("{}:{minute:02}", (hour + 11) % 12 + 1),
        ClockFormat::TwentyFourHour => format!("{hour:02}:{minute:02}"),
    }
}

fn local_clock(now: SystemTime, format: ClockFormat) -> String {
    let seconds = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let Ok(seconds) = libc::time_t::try_from(seconds) else {
        return "--:--".into();
    };
    let mut time = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r initializes `time` on success before it is read.
    unsafe {
        if libc::localtime_r(&seconds, time.as_mut_ptr()).is_null() {
            return "--:--".into();
        }
        let time = time.assume_init();
        clock_text(time.tm_hour, time.tm_min, format)
    }
}

#[allow(clippy::too_many_arguments)] // Raster helpers carry explicit geometry and logical thickness.
fn clock_pixel_at(
    clock: &str,
    position: Position,
    start: usize,
    span: usize,
    x: usize,
    y: usize,
    colors: Colors,
    thickness: usize,
) -> Option<Color> {
    let (major, minor) = if position.vertical() { (y, x) } else { (x, y) };
    let cross_start = (thickness - PILL_SIZE) / 2;
    if major < start
        || major >= start + span
        || !(cross_start..cross_start + PILL_SIZE).contains(&minor)
    {
        return None;
    }
    let along = major - start;
    let cross = minor - cross_start;
    let (w, h, px, py) = if position.vertical() {
        (24, span, cross, along)
    } else {
        (span, 24, along, cross)
    };
    let dx = if px < 3 {
        2 - px
    } else {
        px.saturating_sub(w - 3)
    };
    let dy = if py < 3 {
        2 - py
    } else {
        py.saturating_sub(h - 3)
    };
    if dx * dx + dy * dy > 4 {
        return None;
    }
    let bytes = clock.as_bytes();
    let Some(hour_digits @ (1 | 2)) = bytes.iter().position(|&byte| byte == b':') else {
        return Some(colors.occupied);
    };
    if bytes.len() != hour_digits + 3
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == hour_digits || byte.is_ascii_digit())
    {
        return Some(colors.occupied);
    }
    if position.vertical() {
        let hour = (0..hour_digits)
            .map(|index| (index, 7, if hour_digits == 1 { 9 } else { 5 } + index * 8));
        let minute = (0..2).map(|index| (hour_digits + 1 + index, 30, 5 + index * 8));
        if hour.chain(minute).any(|(index, top, left)| {
            px >= left
                && py >= top
                && px < left + 6
                && py < top + 10
                && digit_pixel(
                    u32::from(bytes[index] - b'0'),
                    (px - left) / 2,
                    (py - top) / 2,
                )
        }) || px == 11 && (23..=24).contains(&py)
        {
            return Some(colors.text);
        }
    } else {
        let text_start = (span - (bytes.len() * 8 - 2)) / 2;
        for (index, byte) in bytes.iter().enumerate() {
            let left = text_start + index * 8;
            if *byte == b':' && px == left + 2 && (py == 9 || py == 13) {
                return Some(colors.text);
            }
            if byte.is_ascii_digit()
                && px >= left
                && py >= 7
                && px < left + 6
                && py < 17
                && digit_pixel(u32::from(byte - b'0'), (px - left) / 2, (py - 7) / 2)
            {
                return Some(colors.text);
            }
        }
    }
    Some(colors.occupied)
}

struct PaintState<'a> {
    visible: &'a [BarWorkspace],
    active_workspace: u32,
    colors: Colors,
    position: Position,
    clock: Option<(&'a str, usize)>,
    thickness: usize,
}

#[allow(clippy::too_many_arguments)]
fn workspace_pixel(
    visible: &[BarWorkspace],
    active_workspace: u32,
    colors: Colors,
    position: Position,
    major_length: usize,
    x: usize,
    y: usize,
    thickness: usize,
) -> Color {
    let (major, minor) = if position.vertical() { (y, x) } else { (x, y) };
    let start = pill_start(major_length);
    let cross_start = (thickness - PILL_SIZE) / 2;
    if major < start || !(cross_start..cross_start + PILL_SIZE).contains(&minor) {
        return colors.background;
    }
    let Some(workspace) = visible.get((major - start) / PILL_STEP) else {
        return colors.background;
    };
    let along = (major - start) % PILL_STEP;
    let cross = minor - cross_start;
    if !inside_rounded_pill(along, cross) {
        return colors.background;
    }
    let active = active_workspace == workspace.id;
    let mut color = if workspace.occupied || active {
        colors.occupied
    } else {
        colors.inactive
    };
    // A thin edge accent is quieter than filling the entire pill purple.
    if active
        && match position {
            Position::Top | Position::Bottom => cross >= 22,
            Position::Left => cross <= 1,
            Position::Right => cross >= 22,
        }
    {
        color = colors.active;
    }
    let (text_x, text_y) = if position.vertical() {
        (cross, along)
    } else {
        (along, cross)
    };
    let glyph_x = (text_x as i32 - 9) / 2;
    let glyph_y = (text_y as i32 - 7) / 2;
    if text_x >= 9
        && text_y >= 7
        && (0..3).contains(&glyph_x)
        && (0..5).contains(&glyph_y)
        && (1..=9).contains(&workspace.id)
        && digit_pixel(workspace.id, glyph_x as usize, glyph_y as usize)
    {
        color = colors.text;
    }
    if workspace.focused_window.is_some() && (19..=21).contains(&along) && (3..=5).contains(&cross)
    {
        color = colors.focused_marker;
    }
    color
}

fn paint_workspaces(canvas: &mut [u8], width: usize, scale: usize, state: PaintState<'_>) {
    let major_length = if state.position.vertical() {
        canvas.len() / 4 / width / scale
    } else {
        width / scale
    };
    for (index, pixel) in canvas.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = (index % width) / scale;
        let y = (index / width) / scale;
        pixel.copy_from_slice(
            &state
                .clock
                .and_then(|(clock, start)| {
                    let span = if state.position.vertical() {
                        CLOCK_VERTICAL
                    } else {
                        CLOCK_HORIZONTAL
                    };
                    clock_pixel_at(
                        clock,
                        state.position,
                        start,
                        span,
                        x,
                        y,
                        state.colors,
                        state.thickness,
                    )
                })
                .unwrap_or_else(|| {
                    workspace_pixel(
                        state.visible,
                        state.active_workspace,
                        state.colors,
                        state.position,
                        major_length,
                        x,
                        y,
                        state.thickness,
                    )
                })
                .bytes(),
        );
    }
}

struct StatusTile {
    text: String,
    start: usize,
    span: usize,
    plain_title: bool,
}

fn text_width(font: &FontArc, text: &str, size: f32) -> f32 {
    let face = font.as_scaled(size);
    text.chars()
        .map(|ch| face.h_advance(face.glyph_id(ch)))
        .sum()
}

fn status_span(font: &FontArc, font_size: f32, text: &str, position: Position) -> usize {
    if position.vertical() {
        ((font_size * 9.0 / 11.0 * 2.0 * 1.9).ceil() as usize + 10).max(44)
    } else {
        (text_width(font, text, font_size).ceil() as usize + 16).clamp(40, 160)
    }
}

fn widget_span(font: &FontArc, font_size: f32, id: &str, text: &str, position: Position) -> usize {
    if id == "clock" {
        if position.vertical() {
            CLOCK_VERTICAL
        } else {
            CLOCK_HORIZONTAL
        }
    } else if id == "title" && !position.vertical() {
        (text_width(font, text, font_size).ceil() as usize + 16).clamp(40, 320)
    } else {
        status_span(font, font_size, text, position)
    }
}

fn status_label(
    config: &Config,
    status: &BTreeMap<String, String>,
    id: &str,
    clock: &str,
    monitor: Option<&BarMonitor>,
) -> Option<String> {
    match id {
        "clock" => config.widgets.clock.then(|| clock.to_owned()),
        "cpu" => Some(status.get(id).cloned().unwrap_or_else(|| "CPU --".into())),
        "memory" => Some(status.get(id).cloned().unwrap_or_else(|| "MEM --".into())),
        "title" => monitor.and_then(|monitor| monitor.focused_title.clone()),
        "workspaces" => None,
        _ => config.widgets.custom.get(id).map(|_| {
            status
                .get(id)
                .cloned()
                .unwrap_or_else(|| format!("{id}: --"))
        }),
    }
}

fn layout_statuses(
    config: &Config,
    font: &FontArc,
    status: &BTreeMap<String, String>,
    clock: &str,
    monitor: Option<&BarMonitor>,
    major_length: usize,
) -> (Vec<BarWorkspace>, Vec<StatusTile>, Option<usize>) {
    let position = config.bar.position;
    let mut end_cursor = major_length.saturating_sub(8);
    let mut tiles = Vec::new();
    let mut clock_start = None;
    let has_workspaces = monitor.is_some()
        && config
            .layout
            .start
            .first()
            .is_some_and(|id| id == "workspaces");
    let min_start = if has_workspaces && config.bar.show_all_workspaces {
        let count = monitor.map_or(0, |monitor| visible_workspaces(monitor, true).len());
        let last_pill_end = pill_start(major_length)
            + count.saturating_sub(1) * PILL_STEP
            + if count == 0 { 0 } else { PILL_SIZE };
        (last_pill_end + STATUS_GAP).min(end_cursor)
    } else if has_workspaces {
        40
    } else {
        8
    };
    for id in config.layout.end.iter().rev() {
        let Some(text) = status_label(config, status, id, clock, monitor) else {
            continue;
        };
        let span = widget_span(font, config.bar.font_size, id, &text, position);
        if end_cursor < min_start + span {
            continue;
        }
        let start = end_cursor - span;
        if id == "clock" {
            clock_start = Some(start);
        } else {
            tiles.push(StatusTile {
                text,
                start,
                span,
                plain_title: id == "title",
            });
        }
        end_cursor = start.saturating_sub(STATUS_GAP);
    }
    let visible = monitor
        .filter(|_| {
            config
                .layout
                .start
                .first()
                .is_some_and(|id| id == "workspaces")
        })
        .map(|monitor| {
            laid_out_workspaces(
                monitor,
                end_cursor.saturating_add(STATUS_GAP),
                config.bar.show_all_workspaces,
            )
        })
        .unwrap_or_default();
    let workspace_end = if visible.is_empty() {
        8
    } else {
        pill_start(major_length) + (visible.len() - 1) * PILL_STEP + PILL_SIZE
    };
    let mut start_cursor = workspace_end + STATUS_GAP;
    for id in config.layout.start.iter().filter(|id| *id != "workspaces") {
        if let Some(text) = status_label(config, status, id, clock, monitor) {
            let span = widget_span(font, config.bar.font_size, id, &text, position);
            if start_cursor + span + STATUS_GAP <= end_cursor {
                if id == "clock" {
                    clock_start = Some(start_cursor);
                } else {
                    tiles.push(StatusTile {
                        text,
                        start: start_cursor,
                        span,
                        plain_title: id == "title",
                    });
                }
                start_cursor += span + STATUS_GAP;
            }
        }
    }
    let region_start = start_cursor.saturating_add(STATUS_GAP);
    let region_end = end_cursor.saturating_sub(STATUS_GAP);
    let region_width = region_end.saturating_sub(region_start);
    let mut center: Vec<_> = config
        .layout
        .center
        .iter()
        .filter_map(|id| {
            status_label(config, status, id, clock, monitor).map(|text| {
                (
                    id.as_str(),
                    widget_span(font, config.bar.font_size, id, &text, position),
                    text,
                )
            })
        })
        .collect();
    loop {
        let fixed: usize = center
            .iter()
            .filter(|(id, _, _)| *id != "title")
            .map(|(_, span, _)| *span)
            .sum();
        let gaps = center.len().saturating_sub(1) * STATUS_GAP;
        let title_width = if center.iter().any(|(id, _, _)| *id == "title") {
            if position.vertical() {
                config.bar.thickness as usize - 4
            } else {
                24
            }
        } else {
            0
        };
        if fixed + gaps + title_width <= region_width || center.is_empty() {
            break;
        }
        if let Some(index) = center.iter().position(|(id, _, _)| *id == "title") {
            center.remove(index); // Keep the smaller status widgets when cramped.
        } else {
            center.pop();
        }
    }
    let fixed: usize = center
        .iter()
        .filter(|(id, _, _)| *id != "title")
        .map(|(_, span, _)| *span)
        .sum();
    let gaps = center.len().saturating_sub(1) * STATUS_GAP;
    let title_width = if center.iter().any(|(id, _, _)| *id == "title") {
        region_width - fixed - gaps
    } else {
        0
    };
    let total = fixed + title_width + gaps;
    let mut cursor = if title_width > 0 {
        region_start
    } else {
        region_start + (region_width - total) / 2
    };
    for (id, fixed_span, text) in center {
        let span = if id == "title" {
            title_width
        } else {
            fixed_span
        };
        if id == "clock" {
            clock_start = Some(cursor);
        } else {
            tiles.push(StatusTile {
                text,
                start: cursor,
                span,
                plain_title: id == "title",
            });
        }
        cursor += span + STATUS_GAP;
    }
    (visible, tiles, clock_start)
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    canvas: &mut [u8],
    width: usize,
    height: usize,
    scale: usize,
    label: &str,
    rect: (usize, usize, usize, usize),
    colors: Colors,
    font: &FontArc,
    font_size: f32,
    compact: bool,
) {
    let (left, top, rect_width, rect_height) = rect;
    let (left, top, rect_width, rect_height) = (
        left * scale,
        top * scale,
        rect_width * scale,
        rect_height * scale,
    );
    let font_size = font_size * (if compact { 9.0 / 11.0 } else { 1.0 }) * scale as f32;
    let scaled = font.as_scaled(font_size);
    let baseline = top as f32
        + (rect_height as f32 - (scaled.ascent() - scaled.descent())) / 2.0
        + scaled.ascent();
    let mut pen = left as f32 + (if compact { 2.0 } else { 8.0 }) * scale as f32;
    for ch in label.chars() {
        let id = font.glyph_id(ch);
        let advance = scaled.h_advance(id);
        if pen + advance > (left + rect_width - (if compact { 2 } else { 4 }) * scale) as f32 {
            break;
        }
        if let Some(glyph) =
            font.outline_glyph(id.with_scale_and_position(font_size, point(pen, baseline)))
        {
            let bounds = glyph.px_bounds();
            glyph.draw(|dx, dy, coverage| {
                let x = bounds.min.x as i32 + dx as i32;
                let y = bounds.min.y as i32 + dy as i32;
                if x < left as i32
                    || y < top as i32
                    || x >= (left + rect_width) as i32
                    || y >= (top + rect_height) as i32
                    || x < 0
                    || y < 0
                    || x >= width as i32
                    || y >= height as i32
                {
                    return;
                }
                let offset = (y as usize * width + x as usize) * 4;
                let foreground = colors.text.bytes();
                for (channel, target) in canvas[offset..offset + 3].iter_mut().enumerate() {
                    *target = (*target as f32 * (1.0 - coverage)
                        + foreground[channel] as f32 * coverage)
                        .round() as u8;
                }
            });
        }
        pen += advance;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_vertical_title(
    canvas: &mut [u8],
    width: usize,
    height: usize,
    scale: usize,
    title: &str,
    rect: (usize, usize, usize, usize),
    colors: Colors,
    font: &FontArc,
    font_size: f32,
) {
    let (left, top, w, h) = rect;
    let mut row = top + 4;
    let line_height = (font_size * 17.0 / 11.0).ceil() as usize;
    let step = line_height.saturating_sub(1);
    let mut line = String::new();
    for ch in title.chars() {
        if ch.is_whitespace() && line.is_empty() {
            continue;
        }
        let mut candidate = line.clone();
        candidate.push(ch);
        if !line.is_empty() && text_width(font, &candidate, font_size * 9.0 / 11.0) > (w - 4) as f32
        {
            if row + line_height > top + h - 4 {
                return;
            }
            draw_text(
                canvas,
                width,
                height,
                scale,
                line.trim_end(),
                (left, row, w, line_height),
                colors,
                font,
                font_size,
                true,
            );
            row += step;
            line.clear();
            if ch.is_whitespace() {
                continue;
            }
        }
        line.push(ch);
    }
    if !line.is_empty() && row + line_height <= top + h - 4 {
        draw_text(
            canvas,
            width,
            height,
            scale,
            line.trim_end(),
            (left, row, w, line_height),
            colors,
            font,
            font_size,
            true,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_status_tiles(
    canvas: &mut [u8],
    width: usize,
    height: usize,
    scale: usize,
    position: Position,
    colors: Colors,
    tiles: &[StatusTile],
    thickness: usize,
    font: &FontArc,
    font_size: f32,
) {
    for tile in tiles {
        let (left, top, w, h) = if position.vertical() {
            (4, tile.start, thickness - 8, tile.span)
        } else {
            (tile.start, 4, tile.span, thickness - 8)
        };
        if !tile.plain_title {
            for y in top * scale..((top + h) * scale).min(height) {
                for x in left * scale..((left + w) * scale).min(width) {
                    let px = (x / scale) - left;
                    let py = (y / scale) - top;
                    let dx = if px < 3 {
                        2 - px
                    } else {
                        px.saturating_sub(w - 3)
                    };
                    let dy = if py < 3 {
                        2 - py
                    } else {
                        py.saturating_sub(h - 3)
                    };
                    if dx * dx + dy * dy <= 4 {
                        canvas[(y * width + x) * 4..(y * width + x + 1) * 4]
                            .copy_from_slice(&colors.occupied.bytes());
                    }
                }
            }
        }
        if position.vertical() {
            if tile.plain_title {
                draw_vertical_title(
                    canvas,
                    width,
                    height,
                    scale,
                    &tile.text,
                    (left, top, w, h),
                    colors,
                    font,
                    font_size,
                );
                continue;
            }
            let (first, second) = tile.text.split_once(' ').unwrap_or((&tile.text, ""));
            let line_height = (font_size * 17.0 / 11.0).ceil() as usize;
            draw_text(
                canvas,
                width,
                height,
                scale,
                first,
                (left, top + 3, w, line_height),
                colors,
                font,
                font_size,
                true,
            );
            draw_text(
                canvas,
                width,
                height,
                scale,
                second,
                (left, top + 4 + line_height, w, line_height),
                colors,
                font,
                font_size,
                true,
            );
        } else {
            draw_text(
                canvas,
                width,
                height,
                scale,
                &tile.text,
                (left, top, w, h),
                colors,
                font,
                font_size,
                false,
            );
        }
    }
}

fn start_provider(
    id: &str,
    interval_seconds: u64,
    sender: channel::SyncSender<(String, String)>,
    mut read: impl FnMut() -> String + Send + 'static,
) {
    let id = id.to_owned();
    thread::spawn(move || {
        loop {
            if sender.send((id.clone(), read())).is_err() {
                break;
            }
            thread::sleep(Duration::from_secs(interval_seconds));
        }
    });
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let font = load_font(config.bar.font.as_deref());
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<Bar> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;
    let compositor = CompositorState::bind(&globals, &qh)?;
    let shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let pool = SlotPool::new(4, &shm)?;
    let mut bar = Bar {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        compositor,
        shell,
        shm,
        pool,
        bars: Vec::new(),
        output_filter: env::var("DEVEL_WM_OUTPUT").ok(),
        clock: local_clock(SystemTime::now(), config.widgets.clock_format),
        config,
        font,
        snapshot: None,
        statuses: BTreeMap::new(),
        ipc_bytes: Vec::new(),
    };
    let (sender, receiver) = channel::sync_channel::<(String, String)>(32);
    event_loop
        .handle()
        .insert_source(receiver, |event, _, bar| {
            if let ChannelEvent::Msg((id, value)) = event
                && bar.statuses.get(&id) != Some(&value)
            {
                bar.statuses.insert(id, value);
                bar.draw_all();
            }
        })?;
    let enabled: HashSet<_> = bar
        .config
        .layout
        .start
        .iter()
        .chain(&bar.config.layout.center)
        .chain(&bar.config.layout.end)
        .map(String::as_str)
        .collect();
    if enabled.contains("cpu") {
        let mut last = None;
        start_provider(
            "cpu",
            bar.config.widgets.cpu.interval_seconds,
            sender.clone(),
            move || {
                let current = status::read_cpu();
                let value = last
                    .zip(current)
                    .and_then(|(before, after)| status::cpu_percent(before, after));
                last = current;
                value.map_or_else(|| "CPU --".into(), |percent| format!("CPU {percent}%"))
            },
        );
    }
    if enabled.contains("memory") {
        start_provider(
            "memory",
            bar.config.widgets.memory.interval_seconds,
            sender.clone(),
            || status::read_memory().unwrap_or_else(|| "MEM --".into()),
        );
    }
    for (id, options) in &bar.config.widgets.custom {
        if enabled.contains(id.as_str()) {
            let argv = options.command.clone();
            let unavailable = format!("{id}: --");
            start_provider(id, options.interval_seconds, sender.clone(), move || {
                status::run_command(&argv).unwrap_or_else(|| unavailable.clone())
            });
        }
    }
    drop(sender);
    if bar.config.widgets.clock && enabled.contains("clock") {
        event_loop.handle().insert_source(
            Timer::from_duration(next_clock_check(SystemTime::now())),
            |_, _, bar| {
                let now = SystemTime::now();
                let clock = local_clock(now, bar.config.widgets.clock_format);
                if bar.clock != clock {
                    bar.clock = clock;
                    bar.draw_all();
                }
                TimeoutAction::ToDuration(next_clock_check(now))
            },
        )?;
    }
    if let Some(path) = std::env::var_os("DEVEL_WM_IPC") {
        let mut stream = UnixStream::connect(&path).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "cannot connect to IPC socket {}: {err} (copy DEVEL_WM_IPC from the running compositor's output)",
                    std::path::Path::new(&path).display()
                ),
            )
        })?;
        stream.write_all(b"subscribe\n")?;
        stream.set_nonblocking(true)?;
        event_loop.handle().insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            |_, source, bar| {
                let mut bytes = [0; 4096];
                loop {
                    // SAFETY: the stream remains in this event source for its lifetime.
                    match unsafe { source.get_mut().read(&mut bytes) } {
                        Ok(0) => return Ok(PostAction::Remove),
                        Ok(n) => bar.ipc_bytes.extend_from_slice(&bytes[..n]),
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                        Err(err) => return Err(err),
                    }
                    if bar.ipc_bytes.len() > 65536 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "IPC message too large",
                        ));
                    }
                }
                while let Some(end) = bar.ipc_bytes.iter().position(|&byte| byte == b'\n') {
                    let line: Vec<_> = bar.ipc_bytes.drain(..=end).collect();
                    let snapshot = parse_snapshot(&line)?;
                    if bar.snapshot.as_ref() != Some(&snapshot) {
                        bar.snapshot = Some(snapshot);
                        bar.draw_all();
                    }
                }
                Ok(PostAction::Continue)
            },
        )?;
    }
    println!(
        "devel-bar: position {:?}; workspace state via DEVEL_WM_IPC if set",
        bar.config.bar.position
    );
    loop {
        event_loop.dispatch(None, &mut bar)?;
    }
}

struct OutputBar {
    output: wl_output::WlOutput,
    name: String,
    layer: LayerSurface,
    width: u32,
    height: u32,
    scale: i32,
}

struct Bar {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    bars: Vec<OutputBar>,
    output_filter: Option<String>,
    config: Config,
    font: FontArc,
    snapshot: Option<BarSnapshot>,
    clock: String,
    statuses: BTreeMap<String, String>,
    ipc_bytes: Vec<u8>,
}

impl Bar {
    fn ensure_output(&mut self, qh: &QueueHandle<Self>, output: &wl_output::WlOutput) {
        let Some(name) = self.outputs.info(output).and_then(|info| info.name) else {
            // Wait for wl_output.name (or xdg-output.name) before associating
            // this output's physical surface with the matching IPC monitor.
            return;
        };
        if !matches_output(self.output_filter.as_deref(), &name)
            || self.bars.iter().any(|bar| bar.output == *output)
        {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let layer = self.shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("devel-bar"),
            Some(output),
        );
        layer.set_anchor(self.config.bar.position.anchor());
        let (width, height) = self.config.bar.position.size(self.config.bar.thickness);
        layer.set_size(width, height);
        layer.set_exclusive_zone(self.config.bar.thickness as i32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit(); // Empty first commit requests the initial configure.
        println!("devel-bar: attached to {name}");
        self.bars.push(OutputBar {
            output: output.clone(),
            name,
            layer,
            width: 0,
            height: 0,
            scale: 1,
        });
    }

    fn draw_all(&mut self) {
        for index in 0..self.bars.len() {
            self.draw(index);
        }
    }

    fn draw(&mut self, index: usize) {
        let bar = &self.bars[index];
        if bar.width == 0 || bar.height == 0 {
            return;
        }
        let scale = bar.scale.max(1) as u32;
        let (Some(width), Some(height)) = (
            bar.width
                .checked_mul(scale)
                .and_then(|n| i32::try_from(n).ok()),
            bar.height
                .checked_mul(scale)
                .and_then(|n| i32::try_from(n).ok()),
        ) else {
            eprintln!("devel-bar: invalid buffer dimensions for {}", bar.name);
            return;
        };
        let (buffer, canvas) = self
            .pool
            .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            .expect("allocate bar buffer");
        let monitor = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| monitor_for_output(snapshot, &bar.name));
        let major_length = if self.config.bar.position.vertical() {
            bar.height as usize
        } else {
            bar.width as usize
        };
        let (visible, tiles, clock_start) = layout_statuses(
            &self.config,
            &self.font,
            &self.statuses,
            &self.clock,
            monitor,
            major_length,
        );
        let active_workspace = monitor.map_or(0, |monitor| monitor.active_workspace);
        paint_workspaces(
            canvas,
            width as usize,
            scale as usize,
            PaintState {
                visible: &visible,
                active_workspace,
                colors: self.config.colors,
                position: self.config.bar.position,
                clock: clock_start.map(|start| (self.clock.as_str(), start)),
                thickness: self.config.bar.thickness as usize,
            },
        );
        paint_status_tiles(
            canvas,
            width as usize,
            height as usize,
            scale as usize,
            self.config.bar.position,
            self.config.colors,
            &tiles,
            self.config.bar.thickness as usize,
            &self.font,
            self.config.bar.font_size,
        );
        buffer
            .attach_to(bar.layer.wl_surface())
            .expect("attach bar buffer");
        bar.layer.wl_surface().damage_buffer(0, 0, width, height);
        bar.layer.commit();
    }
}

impl CompositorHandler for Bar {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        scale: i32,
    ) {
        if let Some(index) = self
            .bars
            .iter()
            .position(|bar| bar.layer.wl_surface() == surface)
            && scale > 0
            && self.bars[index].scale != scale
        {
            self.bars[index].scale = scale;
            surface.set_buffer_scale(scale);
            self.draw(index);
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Bar {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.bars.retain(|bar| &bar.layer != layer);
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (width, height) = configure.new_size;
        if let Some(index) = self.bars.iter().position(|bar| &bar.layer == layer)
            && width != 0
            && height != 0
        {
            self.bars[index].width = width;
            self.bars[index].height = height;
            println!(
                "bar configured on {}: {width}x{height}",
                self.bars[index].name
            );
            self.draw(index);
        }
    }
}

impl ShmHandler for Bar {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl OutputHandler for Bar {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.ensure_output(qh, &output);
    }
    fn update_output(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.ensure_output(qh, &output);
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.bars.retain(|bar| bar.output != output);
    }
}

delegate_compositor!(Bar);
delegate_layer!(Bar);
delegate_output!(Bar);
delegate_registry!(Bar);
delegate_shm!(Bar);

impl ProvidesRegistryState for Bar {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_font() -> FontArc {
        load_font(None)
    }

    #[test]
    fn reads_workspace_snapshot_and_digit_glyphs() {
        let state: BarSnapshot = serde_json::from_str(r#"{"version":1,"active_monitor":0,"monitors":[{"id":0,"name":"nested","active_workspace":2,"workspaces":[{"id":1,"occupied":false,"focused_window":null,"windows":[]},{"id":2,"occupied":true,"focused_window":4,"windows":[4]}]}]}"#).unwrap();
        assert_eq!(state.version, 1);
        assert_eq!(state.monitors[0].focused_title, None);
        assert_eq!(state.monitors[0].active_workspace, 2);
        assert!(state.monitors[0].workspaces[1].occupied);
        assert!(digit_pixel(2, 0, 0));
        assert!(!digit_pixel(2, 0, 1));
    }

    #[test]
    fn v2_focused_title_is_per_output_and_old_snapshots_remain_readable() {
        let snapshot: BarSnapshot = serde_json::from_str(r#"{"version":2,"active_monitor":1,"monitors":[{"id":0,"name":"HDMI-A-2","active_workspace":1,"focused_title":"Terminal","workspaces":[]},{"id":1,"name":"DP-4","active_workspace":2,"focused_title":"Browser","workspaces":[]}]}"#).unwrap();
        let mut config = Config::default();
        assert_eq!(config.layout.center, ["title"]);
        let empty = BTreeMap::new();
        assert_eq!(
            status_label(
                &config,
                &empty,
                "title",
                "1:28",
                monitor_for_output(&snapshot, "HDMI-A-2")
            ),
            Some("Terminal".into())
        );
        assert_eq!(
            status_label(
                &config,
                &empty,
                "title",
                "1:28",
                monitor_for_output(&snapshot, "DP-4")
            ),
            Some("Browser".into())
        );
        config.bar.position = Position::Top;
        let (_, tiles, _) = layout_statuses(
            &config,
            &test_font(),
            &empty,
            "1:28",
            monitor_for_output(&snapshot, "HDMI-A-2"),
            900,
        );
        let title = tiles.iter().find(|tile| tile.text == "Terminal").unwrap();
        assert!(title.plain_title);
        assert!(title.span > 320);
        assert!(
            tiles
                .iter()
                .filter(|tile| !tile.plain_title)
                .all(|tile| title.start + title.span < tile.start)
        );
        let old: BarSnapshot = serde_json::from_str(r#"{"version":1,"active_monitor":0,"monitors":[{"id":0,"name":"nested","active_workspace":1,"workspaces":[]}]}"#).unwrap();
        assert_eq!(
            status_label(&config, &empty, "title", "1:28", old.monitors.first()),
            None
        );
        assert!(parse_snapshot(br#"{"version":1,"active_monitor":null,"monitors":[]}"#).is_ok());
        assert!(parse_snapshot(br#"{"version":2,"active_monitor":null,"monitors":[]}"#).is_ok());
        assert!(parse_snapshot(br#"{"version":3,"active_monitor":null,"monitors":[]}"#).is_err());
        assert!(parse_snapshot(br#"{"version":0,"active_monitor":null,"monitors":[]}"#).is_err());
        let mut empty = snapshot.clone();
        empty.monitors[0].focused_title = None;
        let (_, tiles, _) = layout_statuses(
            &config,
            &test_font(),
            &BTreeMap::new(),
            "1:28",
            Some(&empty.monitors[0]),
            900,
        );
        assert!(!tiles.iter().any(|tile| tile.text == "Terminal"));
    }

    #[test]
    fn positions_reserve_the_correct_edge_and_paint_upright_digits() {
        let colors = Colors::default();
        let monitor = BarMonitor {
            id: 1,
            name: "DP-4".into(),
            active_workspace: 1,
            focused_title: None,
            workspaces: vec![BarWorkspace {
                id: 1,
                occupied: true,
                focused_window: Some(3),
            }],
        };
        let visible = visible_workspaces(&monitor, false);
        for (position, size, anchor) in [
            (
                Position::Top,
                (0, 32),
                Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            ),
            (
                Position::Bottom,
                (0, 32),
                Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            ),
            (
                Position::Left,
                (32, 0),
                Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
            ),
            (
                Position::Right,
                (32, 0),
                Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
            ),
        ] {
            assert_eq!(position.size(32), size);
            assert_eq!(position.anchor(), anchor);
            let (x, y) = if position.vertical() {
                (15, 15)
            } else {
                (19, 11)
            };
            assert_eq!(
                workspace_pixel(
                    &visible,
                    monitor.active_workspace,
                    colors,
                    position,
                    160,
                    x,
                    y,
                    32,
                ),
                colors.text
            );
            let (x, y) = if position.vertical() {
                (8, 28)
            } else {
                (28, 8)
            };
            assert_eq!(
                workspace_pixel(
                    &visible,
                    monitor.active_workspace,
                    colors,
                    position,
                    160,
                    x,
                    y,
                    32,
                ),
                colors.focused_marker
            );
        }
    }

    #[test]
    fn shows_only_occupied_and_active_workspaces_in_numeric_order_on_every_edge() {
        let colors = Colors::default();
        let monitor = BarMonitor {
            id: 1,
            name: "DP-4".into(),
            active_workspace: 5,
            focused_title: None,
            workspaces: vec![
                BarWorkspace {
                    id: 9,
                    occupied: true,
                    focused_window: None,
                },
                BarWorkspace {
                    id: 1,
                    occupied: false,
                    focused_window: None,
                },
                BarWorkspace {
                    id: 5,
                    occupied: false,
                    focused_window: None,
                },
                BarWorkspace {
                    id: 3,
                    occupied: true,
                    focused_window: Some(7),
                },
                BarWorkspace {
                    id: 7,
                    occupied: false,
                    focused_window: None,
                },
            ],
        };
        let visible = visible_workspaces(&monitor, false);
        assert_eq!(
            visible.iter().map(|ws| ws.id).collect::<Vec<_>>(),
            [3, 5, 9]
        );
        for position in [
            Position::Top,
            Position::Bottom,
            Position::Left,
            Position::Right,
        ] {
            let at = |major, minor| {
                let (x, y) = if position.vertical() {
                    (minor, major)
                } else {
                    (major, minor)
                };
                workspace_pixel(
                    &visible,
                    monitor.active_workspace,
                    colors,
                    position,
                    160,
                    x,
                    y,
                    32,
                )
            };
            assert_eq!(at(20, 8), colors.occupied); // Workspace 3 takes the first slot.
            assert_eq!(at(50, 8), colors.occupied); // Active 5 stays visible when empty.
            assert_eq!(at(80, 8), colors.occupied); // Occupied 9 follows directly.
            assert_eq!(at(110, 8), colors.background); // No unused workspace slots.
            assert_eq!(at(28, 8), colors.focused_marker);
            let accent_minor = if position == Position::Left { 5 } else { 27 };
            assert_eq!(at(50, accent_minor), colors.active);
        }
    }

    #[test]
    fn all_workspace_tags_are_opt_in_and_keep_the_active_tag_visible_on_short_bars() {
        let mut config = Config::default();
        assert!(!config.bar.show_all_workspaces);
        let monitor = BarMonitor {
            id: 1,
            name: "DP-4".into(),
            active_workspace: 9,
            focused_title: None,
            workspaces: (1..=9)
                .rev()
                .map(|id| BarWorkspace {
                    id,
                    occupied: id == 2,
                    focused_window: (id == 2).then_some(7),
                })
                .collect(),
        };
        let font = test_font();
        let statuses = BTreeMap::new();
        let (default_tags, _, _) =
            layout_statuses(&config, &font, &statuses, "1:28", Some(&monitor), 350);
        assert_eq!(
            default_tags.iter().map(|ws| ws.id).collect::<Vec<_>>(),
            [2, 9]
        );
        config.bar.show_all_workspaces = true;
        for position in [
            Position::Top,
            Position::Bottom,
            Position::Left,
            Position::Right,
        ] {
            config.bar.position = position;
            let (all, tiles, clock) =
                layout_statuses(&config, &font, &statuses, "1:28", Some(&monitor), 350);
            assert_eq!(
                all.iter().map(|ws| ws.id).collect::<Vec<_>>(),
                (1..=9).collect::<Vec<_>>()
            );
            let last_end = pill_start(350) + 8 * PILL_STEP + PILL_SIZE;
            assert!(tiles.iter().all(|tile| tile.start >= last_end));
            assert!(clock.is_none_or(|start| start >= last_end));

            let (x, y) = if position.vertical() {
                (8, 20)
            } else {
                (20, 8)
            };
            assert_eq!(
                workspace_pixel(&all, 9, Colors::default(), position, 350, x, y, 32),
                Colors::default().inactive
            );
            let (short, _, _) =
                layout_statuses(&config, &font, &statuses, "1:28", Some(&monitor), 65);
            assert!(short.iter().any(|ws| ws.id == 9));
            assert!(short.len() < all.len());
        }
        let config: Config = toml::from_str("[bar]\nshow_all_workspaces = true\n").unwrap();
        config.validate().unwrap();
        assert!(config.bar.show_all_workspaces);
        assert!(toml::from_str::<Config>("[bar]\nshow_all_workspaces = 'yes'\n").is_err());
    }

    #[test]
    fn show_all_fills_unvisited_tags_missing_from_the_ipc_snapshot() {
        let mut config = Config::default();
        let monitor = BarMonitor {
            id: 1,
            name: "DP-4".into(),
            active_workspace: 1,
            focused_title: None,
            workspaces: vec![BarWorkspace {
                id: 1,
                occupied: false,
                focused_window: None,
            }],
        };
        let font = test_font();
        let status = BTreeMap::new();
        let (compact, _, _) = layout_statuses(&config, &font, &status, "1:28", Some(&monitor), 400);
        assert_eq!(compact.iter().map(|ws| ws.id).collect::<Vec<_>>(), [1]);
        config.bar.show_all_workspaces = true;
        for position in [
            Position::Top,
            Position::Bottom,
            Position::Left,
            Position::Right,
        ] {
            config.bar.position = position;
            let (all, tiles, clock) =
                layout_statuses(&config, &font, &status, "1:28", Some(&monitor), 400);
            assert_eq!(
                all.iter().map(|ws| ws.id).collect::<Vec<_>>(),
                (1..=9).collect::<Vec<_>>()
            );
            assert!(
                all.iter()
                    .skip(1)
                    .all(|ws| !ws.occupied && ws.focused_window.is_none())
            );
            let last_end = pill_start(400) + 8 * PILL_STEP + PILL_SIZE;
            assert!(tiles.iter().all(|tile| tile.start >= last_end));
            assert!(clock.is_none_or(|start| start >= last_end));
            let (short, _, _) =
                layout_statuses(&config, &font, &status, "1:28", Some(&monitor), 65);
            assert!(short.iter().any(|ws| ws.id == 1));
        }
        let mut visited = monitor;
        visited.workspaces.push(BarWorkspace {
            id: 12,
            occupied: true,
            focused_window: Some(3),
        });
        let all = visible_workspaces(&visited, true);
        assert_eq!(
            all.iter().map(|ws| ws.id).collect::<Vec<_>>(),
            (1..=9).chain([12]).collect::<Vec<_>>()
        );
        assert!(all.last().unwrap().occupied);
    }

    #[test]
    fn rounded_pills_have_padding_and_active_workspace_survives_short_edges() {
        let colors = Colors::default();
        let monitor = BarMonitor {
            id: 1,
            name: "small".into(),
            active_workspace: 7,
            focused_title: None,
            workspaces: vec![
                BarWorkspace {
                    id: 1,
                    occupied: true,
                    focused_window: None,
                },
                BarWorkspace {
                    id: 7,
                    occupied: false,
                    focused_window: None,
                },
            ],
        };
        let visible = laid_out_workspaces(&monitor, 32, false);
        assert_eq!(visible.iter().map(|ws| ws.id).collect::<Vec<_>>(), [7]);
        assert_eq!(
            workspace_pixel(&visible, 7, colors, Position::Top, 32, 4, 4, 32),
            colors.background
        );
        assert_eq!(
            workspace_pixel(&visible, 7, colors, Position::Top, 32, 16, 8, 32),
            colors.occupied
        );
        assert_eq!(
            workspace_pixel(&visible, 7, colors, Position::Top, 32, 16, 27, 32),
            colors.active
        );
        assert!(laid_out_workspaces(&monitor, 24, false).is_empty());
    }

    #[test]
    fn scale_two_raster_repeats_each_logical_pixel_without_resampling() {
        let colors = Colors::default();
        let monitor = BarMonitor {
            id: 1,
            name: "scaled".into(),
            active_workspace: 1,
            focused_title: None,
            workspaces: vec![BarWorkspace {
                id: 1,
                occupied: false,
                focused_window: None,
            }],
        };
        let visible = laid_out_workspaces(&monitor, 40, false);
        let mut canvas = vec![0; 80 * 64 * 4];
        paint_workspaces(
            &mut canvas,
            80,
            2,
            PaintState {
                visible: &visible,
                active_workspace: 1,
                colors,
                position: Position::Top,
                clock: None,
                thickness: 32,
            },
        );
        let pixel = |x: usize, y: usize| &canvas[(y * 80 + x) * 4..(y * 80 + x + 1) * 4];
        assert_eq!(pixel(0, 0), colors.background.bytes());
        for (x, y) in [(32, 16), (38, 22), (32, 54)] {
            assert_eq!(pixel(x, y), pixel(x + 1, y));
            assert_eq!(pixel(x, y), pixel(x, y + 1));
        }
        assert_eq!(pixel(32, 54), colors.active.bytes());
    }

    #[test]
    fn clock_is_upright_on_all_edges_and_does_not_overlap_workspace_space() {
        let colors = Colors::default();
        for position in [
            Position::Top,
            Position::Bottom,
            Position::Left,
            Position::Right,
        ] {
            let length = 160;
            let span = if position.vertical() {
                CLOCK_VERTICAL
            } else {
                CLOCK_HORIZONTAL
            };
            let start = length - span - 8;
            let (x, y) = if position.vertical() {
                (11, start + 7)
            } else {
                (start + 9, 11)
            };
            assert_eq!(
                clock_pixel_at("12:34", position, start, span, x, y, colors, 32),
                Some(colors.text)
            );
            let (single_x, single_y) = if position.vertical() {
                (15, start + 7)
            } else {
                (start + 13, 11)
            };
            assert_eq!(
                clock_pixel_at(
                    "1:34", position, start, span, single_x, single_y, colors, 32
                ),
                Some(colors.text)
            );
            assert_eq!(
                clock_pixel_at("12:34", position, start, span, 0, 0, colors, 32),
                None
            );
            assert!(start >= pill_start(length) + PILL_SIZE + 8);
        }
    }

    #[test]
    fn clock_waits_for_minute_boundary_and_formats_local_time() {
        assert_eq!(
            next_clock_check(UNIX_EPOCH + Duration::from_secs(120)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_clock_check(UNIX_EPOCH + Duration::from_secs(179)),
            Duration::from_secs(1)
        );
        assert_eq!(clock_text(0, 2, ClockFormat::TwelveHour), "12:02");
        assert_eq!(clock_text(1, 28, ClockFormat::TwelveHour), "1:28");
        assert_eq!(clock_text(12, 9, ClockFormat::TwelveHour), "12:09");
        assert_eq!(clock_text(13, 28, ClockFormat::TwelveHour), "1:28");
        assert_eq!(clock_text(13, 28, ClockFormat::TwentyFourHour), "13:28");
        assert_eq!(clock_text(0, 2, ClockFormat::TwentyFourHour), "00:02");
        let clock = local_clock(SystemTime::now(), ClockFormat::TwelveHour);
        let colon = clock.find(':').unwrap();
        assert!((1..=2).contains(&colon));
        assert!(
            clock
                .bytes()
                .enumerate()
                .all(|(index, byte)| index == colon || byte.is_ascii_digit())
        );
    }

    #[test]
    fn workspace_visibility_updates_when_occupancy_and_active_view_change() {
        let mut monitor = BarMonitor {
            id: 1,
            name: "HDMI-A-2".into(),
            active_workspace: 2,
            focused_title: None,
            workspaces: (1..=3)
                .map(|id| BarWorkspace {
                    id,
                    occupied: false,
                    focused_window: None,
                })
                .collect(),
        };
        assert_eq!(
            visible_workspaces(&monitor, false)
                .iter()
                .map(|ws| ws.id)
                .collect::<Vec<_>>(),
            [2]
        );
        monitor.workspaces[0].occupied = true;
        monitor.active_workspace = 3;
        assert_eq!(
            visible_workspaces(&monitor, false)
                .iter()
                .map(|ws| ws.id)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        monitor.workspaces[0].occupied = false;
        assert_eq!(
            visible_workspaces(&monitor, false)
                .iter()
                .map(|ws| ws.id)
                .collect::<Vec<_>>(),
            [3]
        );
    }

    #[test]
    fn partial_config_and_invalid_colors_or_positions() {
        let config: Config =
            toml::from_str("[bar]\nposition = 'left'\n[colors]\nactive = '#123aBC'\n").unwrap();
        assert_eq!(config.bar.position, Position::Left);
        assert_eq!(config.colors.active, Color(0xff123abc));
        assert_eq!(config.colors.active.bytes(), [0xbc, 0x3a, 0x12, 0xff]);
        assert_eq!(config.colors.background, Colors::default().background);
        assert!(config.widgets.clock);
        assert_eq!(config.widgets.clock_format, ClockFormat::TwelveHour);
        let no_clock: Config = toml::from_str("[widgets]\nclock = false\n").unwrap();
        assert!(!no_clock.widgets.clock);
        let military: Config = toml::from_str("[widgets]\nclock_format = '24h'\n").unwrap();
        assert_eq!(military.widgets.clock_format, ClockFormat::TwentyFourHour);
        for invalid in [
            "[bar]\nposition = 'center'",
            "[bar]\nposition = 'LEFT'",
            "[colors]\ntext = '#12345'",
            "[colors]\nactive = '#12ZZ90'",
            "[bar]\nunknown = 22",
            "[colors]\nunknown = '#123456'",
            "[widgets]\nclock = 'off'",
            "[widgets]\nclock_format = '13h'",
        ] {
            assert!(
                toml::from_str::<Config>(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn example_config_matches_dracula_defaults() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!(config.bar.position, Position::Top);
        let defaults = Colors::default();
        assert_eq!(config.colors.background, defaults.background);
        assert_eq!(config.colors.active, defaults.active);
        assert_eq!(config.colors.occupied, defaults.occupied);
        assert_eq!(config.colors.inactive, defaults.inactive);
        assert_eq!(config.colors.text, defaults.text);
        assert_eq!(config.colors.focused_marker, defaults.focused_marker);
        assert!(config.widgets.clock);
        assert_eq!(config.widgets.clock_format, ClockFormat::TwelveHour);
    }

    #[test]
    fn font_and_thickness_config_validate_and_load() {
        let defaults = Config::default();
        assert_eq!(defaults.bar.thickness, 32);
        assert_eq!(defaults.bar.font_size, 11.0);
        assert!(defaults.bar.font.is_none());
        let config: Config = toml::from_str("[bar]\nposition = 'right'\nthickness = 48\nfont_size = 18\nfont = 'JetBrainsMono Nerd Font'\n").unwrap();
        config.validate().unwrap();
        assert_eq!(config.bar.position.size(config.bar.thickness), (48, 0));
        assert_eq!(config.bar.font.as_deref(), Some("JetBrainsMono Nerd Font"));
        for invalid in [
            "[bar]\nthickness = 23",
            "[bar]\nthickness = 97",
            "[bar]\nfont_size = 7",
            "[bar]\nfont_size = 33",
            "[bar]\nthickness = 24\nfont_size = 17",
            "[bar]\nfont = ''",
            "[bar]\nfont = '   '",
            "[bar]\nfont = \"A\\nB\"",
            "[bar]\nfont = 'a'\nfont_size = 40",
        ] {
            assert!(
                toml::from_str::<Config>(invalid)
                    .unwrap()
                    .validate()
                    .is_err(),
                "accepted {invalid}"
            );
        }
        for position in [
            Position::Top,
            Position::Bottom,
            Position::Left,
            Position::Right,
        ] {
            assert_eq!(
                position.size(48),
                if position.vertical() {
                    (48, 0)
                } else {
                    (0, 48)
                }
            );
        }
    }

    #[test]
    fn fontconfig_match_rejects_substitutions_and_keeps_collection_index() {
        let output = "JetBrainsMono Nerd Font,JetBrainsMono NF\n/usr/share/fonts/example.ttc\n2\n";
        assert_eq!(
            matched_font(output, "JetBrainsMono Nerd Font"),
            Some(("/usr/share/fonts/example.ttc", 2))
        );
        assert_eq!(matched_font(output, "Other Font"), None);
        assert_eq!(
            matched_font(output, "sans-serif"),
            Some(("/usr/share/fonts/example.ttc", 2))
        );
        assert_eq!(matched_font("Font\n\n0\n", "Font"), None);
        assert_eq!(matched_font("Font\nfile.ttf\ninvalid\n", "Font"), None);
        // A missing family falls back to the bundled face rather than a
        // fontconfig substitute, just as it does for devel-wm titlebars.
        let fallback = load_font(Some("nonexistent-developer-font-12345"));
        let bundled = load_font(None);
        assert_eq!(fallback.glyph_id('A'), bundled.glyph_id('A'));
        assert_eq!(
            fallback.as_scaled(14.0).h_advance(fallback.glyph_id('A')),
            bundled.as_scaled(14.0).h_advance(bundled.glyph_id('A'))
        );
    }

    #[test]
    fn configured_thickness_centers_workspaces_clock_and_font_tiles_at_scale_two() {
        let font = test_font();
        let colors = Colors::default();
        let ws = BarWorkspace {
            id: 1,
            occupied: true,
            focused_window: None,
        };
        for position in [
            Position::Top,
            Position::Bottom,
            Position::Left,
            Position::Right,
        ] {
            for thickness in [24, 48] {
                let cross = (thickness - PILL_SIZE) / 2;
                let at = |major, minor| {
                    if position.vertical() {
                        (minor, major)
                    } else {
                        (major, minor)
                    }
                };
                let (x, y) = if position.vertical() {
                    at(15, cross + 11)
                } else {
                    at(19, cross + 7)
                };
                assert_eq!(
                    workspace_pixel(
                        std::slice::from_ref(&ws),
                        1,
                        colors,
                        position,
                        400,
                        x,
                        y,
                        thickness
                    ),
                    colors.text
                );
                if cross > 0 {
                    let (x, y) = at(19, cross - 1);
                    assert_eq!(
                        workspace_pixel(
                            std::slice::from_ref(&ws),
                            1,
                            colors,
                            position,
                            400,
                            x,
                            y,
                            thickness
                        ),
                        colors.background
                    );
                }
                let (x, y) = if position.vertical() {
                    at(318, cross + 7)
                } else {
                    at(320, cross + 7)
                };
                assert_eq!(
                    clock_pixel_at(
                        "12:34",
                        position,
                        311,
                        widget_span(&font, 18.0, "clock", "12:34", position),
                        x,
                        y,
                        colors,
                        thickness
                    ),
                    Some(colors.text)
                );
                let (width, height) = if position.vertical() {
                    (thickness * 2, 400)
                } else {
                    (400, thickness * 2)
                };
                let mut canvas = colors.background.bytes().repeat(width * height);
                paint_status_tiles(
                    &mut canvas,
                    width,
                    height,
                    2,
                    position,
                    colors,
                    &[StatusTile {
                        text: "CPU 8%".into(),
                        start: 40,
                        span: status_span(&font, 14.0, "CPU 8%", position),
                        plain_title: false,
                    }],
                    thickness,
                    &font,
                    14.0,
                );
                assert!(
                    canvas
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|p| *p == colors.occupied.bytes())
                );
                assert!(
                    canvas
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|p| *p != colors.background.bytes() && *p != colors.occupied.bytes())
                );
            }
        }
        assert!(
            status_span(&font, 18.0, "CPU 8%", Position::Top)
                > status_span(&font, 11.0, "CPU 8%", Position::Top)
        );
    }

    #[test]
    fn output_name_selects_its_own_workspace_independent_of_keyboard_focus() {
        let snapshot: BarSnapshot = serde_json::from_str(r#"{"active_monitor":1,"monitors":[{"id":1,"name":"HDMI-A-2","active_workspace":2,"workspaces":[]},{"id":2,"name":"DP-4","active_workspace":7,"workspaces":[]}]}"#).unwrap();
        assert_eq!(
            monitor_for_output(&snapshot, "HDMI-A-2")
                .unwrap()
                .active_workspace,
            2
        );
        assert_eq!(
            monitor_for_output(&snapshot, "DP-4")
                .unwrap()
                .active_workspace,
            7
        );
        assert!(monitor_for_output(&snapshot, "missing").is_none());
        assert!(matches_output(None, "DP-4"));
        assert!(matches_output(Some("DP-4"), "DP-4"));
        assert!(!matches_output(Some("DP-4"), "HDMI-A-2"));
    }

    #[test]
    fn layout_keeps_workspace_and_clock_apart_and_places_custom_text() {
        let config: Config = toml::from_str("[layout]\nstart = ['workspaces']\nend = ['weather', 'cpu', 'memory', 'clock']\n[widgets.custom.weather]\ncommand = ['weather-brief']\ninterval_seconds = 600\n").unwrap();
        config.validate().unwrap();
        let monitor = BarMonitor {
            id: 1,
            name: "test".into(),
            active_workspace: 1,
            focused_title: None,
            workspaces: vec![BarWorkspace {
                id: 1,
                occupied: false,
                focused_window: None,
            }],
        };
        let status = BTreeMap::from([
            ("weather".into(), "sun 21C".into()),
            ("cpu".into(), "CPU 8%".into()),
            ("memory".into(), "MEM 37%".into()),
        ]);
        for position in [
            Position::Top,
            Position::Bottom,
            Position::Left,
            Position::Right,
        ] {
            let mut config = config.clone();
            config.bar.position = position;
            let (visible, tiles, clock) =
                layout_statuses(&config, &test_font(), &status, "1:28", Some(&monitor), 600);
            assert_eq!(visible.len(), 1);
            assert_eq!(tiles.len(), 3);
            assert!(tiles.iter().any(|tile| tile.text == "sun 21C"));
            assert!(
                tiles
                    .iter()
                    .all(|tile| tile.start >= 8 + PILL_SIZE + STATUS_GAP)
            );
            assert!(clock.is_some());
            let (visible, tiles, clock) =
                layout_statuses(&config, &test_font(), &status, "1:28", Some(&monitor), 90);
            assert_eq!(visible.len(), 1);
            assert!(tiles.is_empty());
            assert!(clock.is_none());
            let length = if position.vertical() { 96 } else { 100 };
            let (visible, tiles, clock) = layout_statuses(
                &config,
                &test_font(),
                &status,
                "1:28",
                Some(&monitor),
                length,
            );
            assert_eq!(visible.len(), 1);
            assert!(tiles.is_empty());
            assert_eq!(clock, Some(40));
        }
    }

    #[test]
    fn documented_custom_widget_example_is_runnable_without_a_shell() {
        let config: Config = toml::from_str("[layout]\nend = ['weather']\n[widgets.custom.weather]\ncommand = ['/usr/bin/printf', 'Sunny 21C\\n']\ninterval_seconds = 600\n").unwrap();
        config.validate().unwrap();
        assert_eq!(
            status::run_command(&config.widgets.custom["weather"].command),
            Some("Sunny 21C".into())
        );
    }

    #[test]
    fn config_rejects_invalid_custom_widgets_and_duplicate_or_unknown_ids() {
        for text in [
            "[layout]\nend = ['clock', 'clock']",
            "[layout]\nend = ['unknown']",
            "[widgets.custom.cpu]\ncommand = ['true']\ninterval_seconds = 10",
            "[widgets.custom.weather]\ncommand = []\ninterval_seconds = 10",
            "[widgets.custom.weather]\ncommand = ['true']\ninterval_seconds = 1",
            "[widgets.cpu]\ninterval_seconds = 0",
            "[layout]\nstart = []\nend = ['workspaces']",
        ] {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.validate().is_err(), "accepted {text}");
        }
    }

    #[test]
    fn status_text_renders_on_horizontal_and_vertical_bars() {
        let colors = Colors::default();
        for position in [Position::Top, Position::Left] {
            let (width, height) = if position.vertical() {
                (32, 200)
            } else {
                (200, 32)
            };
            let mut canvas = colors.background.bytes().repeat(width * height);
            let tiles = [StatusTile {
                text: "CPU 8%".into(),
                start: 30,
                span: status_span(&test_font(), 11.0, "CPU 8%", position),
                plain_title: false,
            }];
            paint_status_tiles(
                &mut canvas,
                width,
                height,
                1,
                position,
                colors,
                &tiles,
                32,
                &test_font(),
                11.0,
            );
            let pixels = canvas.as_chunks::<4>().0;
            assert!(pixels.iter().any(|pixel| *pixel == colors.occupied.bytes()));
            assert!(
                pixels.iter().any(|pixel| *pixel != colors.occupied.bytes()
                    && *pixel != colors.background.bytes())
            );
        }
    }

    #[test]
    fn focused_title_uses_the_bars_background_across_the_available_region() {
        let colors = Colors::default();
        for position in [Position::Top, Position::Left] {
            let (width, height) = if position.vertical() {
                (32, 400)
            } else {
                (400, 32)
            };
            let mut canvas = colors.background.bytes().repeat(width * height);
            let tiles = [StatusTile {
                text: "A long title".into(),
                start: 40,
                span: 300,
                plain_title: true,
            }];
            paint_status_tiles(
                &mut canvas,
                width,
                height,
                1,
                position,
                colors,
                &tiles,
                32,
                &test_font(),
                11.0,
            );
            let pixel = |x: usize, y: usize| &canvas[(y * width + x) * 4..(y * width + x + 1) * 4];
            if position.vertical() {
                assert_eq!(pixel(12, 330), colors.background.bytes());
            } else {
                assert_eq!(pixel(140, 12), colors.background.bytes());
            }
            assert!(
                canvas
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| *pixel != colors.background.bytes())
            );
            assert!(
                !canvas
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| *pixel == colors.occupied.bytes())
            );
        }
    }

    #[test]
    fn title_fills_remaining_center_space_without_overlapping_other_center_widgets() {
        let mut config = Config::default();
        config.layout.center = vec!["title".into(), "cpu".into()];
        config.layout.end.retain(|id| id != "cpu");
        config.validate().unwrap();
        let monitor = BarMonitor {
            id: 1,
            name: "DP-4".into(),
            active_workspace: 1,
            focused_title: Some("A very long focused title".into()),
            workspaces: vec![BarWorkspace {
                id: 1,
                occupied: true,
                focused_window: Some(9),
            }],
        };
        let status = BTreeMap::from([("cpu".into(), "CPU 8%".into())]);
        for position in [Position::Top, Position::Left] {
            config.bar.position = position;
            let (visible, tiles, _) =
                layout_statuses(&config, &test_font(), &status, "1:28", Some(&monitor), 900);
            assert_eq!(visible.len(), 1);
            let title = tiles.iter().find(|tile| tile.plain_title).unwrap();
            let cpu = tiles
                .iter()
                .find(|tile| tile.text == "CPU 8%" && !tile.plain_title)
                .unwrap();
            assert!(title.span > 320);
            assert!(title.start > pill_start(900) + PILL_SIZE);
            assert!(title.start + title.span + STATUS_GAP <= cpu.start);
        }
    }
}
