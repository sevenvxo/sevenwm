//! the config file w every keybind and anything missing falls back to the defaults

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use smithay::input::keyboard::{Keysym, ModifiersState, xkb};

/// the config written on first run and used for anything ur file leaves out
pub const DEFAULT_CONFIG: &str = include_str!("../config.default.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

impl Direction {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "left" => Ok(Self::Left),
            "down" => Ok(Self::Down),
            "up" => Ok(Self::Up),
            "right" => Ok(Self::Right),
            other => Err(format!("expected left, down, up or right, got '{other}'")),
        }
    }

    /// unit step in screen coords where y grows down
    pub fn delta(self) -> (i32, i32) {
        match self {
            Self::Left => (-1, 0),
            Self::Down => (0, 1),
            Self::Up => (0, -1),
            Self::Right => (1, 0),
        }
    }

    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::Right => Self::Left,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Exec(String),
    /// run a command and its window opens beside the workspace
    ExecOutside(String),
    CloseWindow,
    Quit,
    CycleWindows,
    Home,
    Overview,
    ZoomIn,
    ZoomOut,
    ToggleFullscreen,
    ToggleMaximize,
    ToggleFloating,
    ToggleTiling,
    Focus(Direction),
    Move(Direction),
    Grow(Direction),
    Shrink(Direction),
    RelaunchWindow,
    ReloadConfig,
    Screenshot,
    Workspace(u32),
    MoveToWorkspace(u32),
    NewWorkspace,
    RemoveWorkspace,
    ToggleCollapse,
    CenterWindow,
    GoToOrigin,
    WindowToOrigin,
    WorkspaceToOrigin,
    /// close the window menu on escape and u cant bind it
    CloseMenu,
}

impl Action {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (name, arg) = match s.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, Some(arg.trim())),
            None => (s, None),
        };
        let dir = || Direction::parse(arg.ok_or(format!("{name} needs a direction"))?);
        let number = || -> Result<u32, String> {
            let max = crate::workspaces::MAX_NUMBER;
            arg.and_then(|a| a.parse().ok())
                .filter(|n| (1..=max).contains(n))
                .ok_or(format!("{name} needs a workspace number, 1 to {max}"))
        };
        Ok(match name {
            "exec" => Self::Exec(arg.ok_or("exec needs a command")?.to_string()),
            "exec-outside" => {
                Self::ExecOutside(arg.ok_or("exec-outside needs a command")?.to_string())
            }
            "close-window" => Self::CloseWindow,
            "quit" => Self::Quit,
            "cycle-windows" => Self::CycleWindows,
            "home" => Self::Home,
            "overview" => Self::Overview,
            "zoom-in" => Self::ZoomIn,
            "zoom-out" => Self::ZoomOut,
            "toggle-fullscreen" => Self::ToggleFullscreen,
            "toggle-maximize" => Self::ToggleMaximize,
            "toggle-floating" => Self::ToggleFloating,
            "toggle-tiling" => Self::ToggleTiling,
            "focus" => Self::Focus(dir()?),
            "move" => Self::Move(dir()?),
            "grow" => Self::Grow(dir()?),
            "shrink" => Self::Shrink(dir()?),
            "relaunch-window" => Self::RelaunchWindow,
            "reload-config" => Self::ReloadConfig,
            "screenshot" => Self::Screenshot,
            "workspace" => Self::Workspace(number()?),
            "move-to-workspace" => Self::MoveToWorkspace(number()?),
            "new-workspace" => Self::NewWorkspace,
            "remove-workspace" => Self::RemoveWorkspace,
            "collapse" => Self::ToggleCollapse,
            "center-window" => Self::CenterWindow,
            "origin" => Self::GoToOrigin,
            "window-to-origin" => Self::WindowToOrigin,
            "workspace-to-origin" => Self::WorkspaceToOrigin,
            other => return Err(format!("unknown action '{other}'")),
        })
    }

    /// actions worth repeating while the key is held
    pub fn repeats(&self) -> bool {
        matches!(
            self,
            Self::Move(_) | Self::Grow(_) | Self::Shrink(_) | Self::ZoomIn | Self::ZoomOut
        )
    }
}

/// the four modifiers a bind can need
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl Mods {
    pub fn from_state(state: &ModifiersState) -> Self {
        Self {
            ctrl: state.ctrl,
            alt: state.alt,
            shift: state.shift,
            logo: state.logo,
        }
    }

    /// whether every modifier in other is held here
    pub fn contains(self, other: Mods) -> bool {
        (self.ctrl || !other.ctrl)
            && (self.alt || !other.alt)
            && (self.shift || !other.shift)
            && (self.logo || !other.logo)
    }

    fn add(&mut self, name: &str) -> Result<(), String> {
        match name {
            "ctrl" | "control" => self.ctrl = true,
            "alt" => self.alt = true,
            "shift" => self.shift = true,
            "super" | "logo" | "win" => self.logo = true,
            other => return Err(format!("unknown modifier '{other}'")),
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub mods: Mods,
    pub key: Keysym,
}

/// parse mod+shift+h where mod means mod_key
fn parse_combo(spec: &str, mod_key: &str) -> Result<KeyCombo, String> {
    let lower = spec.to_lowercase();
    let mut parts: Vec<&str> = lower.split('+').map(str::trim).collect();
    let key = parts.pop().filter(|k| !k.is_empty()).ok_or("missing key")?;
    let mut mods = Mods::default();
    for part in parts {
        mods.add(if part == "mod" { mod_key } else { part })?;
    }
    let key = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
    if key.raw() == 0 {
        return Err(format!("unknown key in '{spec}'"));
    }
    Ok(KeyCombo { mods, key })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    autostart: Vec<String>,
    keep_running: Vec<String>,
    mod_key: String,
    nested_mod_key: String,
    step: i32,
    canvas: RawCanvas,
    tiling: Tiling,
    workspaces: RawWorkspaces,
    placement: Placement,
    snap: Snap,
    view: View,
    screenshot: Screenshot,
    output: RawOutput,
    xwayland: Xwayland,
    input: Input,
    idle: Idle,
    night_light: NightLight,
    anti_flashbang: AntiFlashbang,
    border: RawBorder,
    decorations: RawDecorations,
    cursor: Cursor,
    theme: RawTheme,
    #[serde(default)]
    monitors: Vec<MonitorConfig>,
    animations: Animations,
    collapse: Collapse,
    session: SessionConfig,
    #[serde(default)]
    rules: Vec<Rule>,
    keybindings: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    /// which preset the settings app last used and sevenwm ignores it
    #[allow(dead_code)]
    preset: String,
    menu_background: String,
    menu_text: String,
    menu_hover: String,
    menu_disabled: String,
    menu_edge: String,
}

/// colors for sevenwms own uhh menu as straight rgba bytes
pub struct Theme {
    pub menu_background: [u8; 4],
    pub menu_text: [u8; 4],
    pub menu_hover: [u8; 4],
    pub menu_disabled: [u8; 4],
    pub menu_edge: [u8; 4],
}

/// where a monitor sits and how it runs
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorConfig {
    /// connector name like HDMI-A-1
    pub name: String,
    /// top left in the monitor layout in logical pixels
    pub position: Option<[i32; 2]>,
    /// like 2560x1440@165 and auto or nothing uses the uhh default
    pub mode: Option<String>,
    pub scale: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Animations {
    pub enabled: bool,
    /// windows sliding to a new spot
    pub move_ms: u64,
    pub move_curve: crate::animation::Curve,
    /// new windows showing up
    pub open_style: crate::animation::Style,
    pub open_ms: u64,
    pub open_curve: crate::animation::Curve,
    /// closed windows going away
    pub close_style: crate::animation::Style,
    pub close_ms: u64,
    pub close_curve: crate::animation::Curve,
    /// the view flying somewhere
    pub fly_curve: crate::animation::Curve,
    /// floating windows bend like jelly while u drag them
    pub wobbly: bool,
    /// the bar slides away over a fullscreen window instead of js getting covered
    pub panel_slide: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collapse {
    /// collapse floating windows left off screen and unfocused on its own
    pub auto: bool,
    /// how long they sit there first
    pub auto_after_minutes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// remember the layout and put windows back after a restart
    pub restore: bool,
    /// how long after starting reopened windows still go back
    pub restore_within_seconds: u64,
}

/// settings for windows whose app id and title match and later rules win
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// pattern for the app id where * matches anything and ? one char
    pub app_id: Option<String>,
    pub title: Option<String>,
    /// true floats the window and false tiles it
    pub float: Option<bool>,
    /// window size when it floats
    pub size: Option<[i32; 2]>,
    pub hide_from_screencast: Option<bool>,
}

impl Rule {
    fn matches(&self, app_id: &str, title: &str) -> bool {
        self.app_id.as_deref().is_none_or(|p| glob(p, app_id))
            && self.title.as_deref().is_none_or(|p| glob(p, title))
    }
}

/// * matches anything and ? one char and the rest is literal
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    // matches i j means the first i pattern chars match the first j text chars
    let mut matches = vec![vec![false; t.len() + 1]; p.len() + 1];
    matches[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            matches[i][0] = matches[i - 1][0];
        }
        for j in 1..=t.len() {
            matches[i][j] = match p[i - 1] {
                '*' => matches[i - 1][j] || matches[i][j - 1],
                '?' => matches[i - 1][j - 1],
                c => matches[i - 1][j - 1] && c == t[j - 1],
            };
        }
    }
    matches[p.len()][t.len()]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub keyboard: Keyboard,
    pub mouse: Pointer,
    pub touchpad: Touchpad,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keyboard {
    pub layout: String,
    pub variant: String,
    pub options: String,
    pub model: String,
    /// key repeats per second
    pub repeat_rate: i32,
    /// ms before a held key starts repeating
    pub repeat_delay: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccelProfile {
    /// pointer speed follows hand speed exactly maybe
    Flat,
    /// fast hand moves go way further
    Adaptive,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pointer {
    pub accel_profile: AccelProfile,
    /// from -1.0 slowest to 1.0 fastest
    pub accel_speed: f64,
    pub natural_scroll: bool,
    pub left_handed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Touchpad {
    pub accel_profile: AccelProfile,
    pub accel_speed: f64,
    pub natural_scroll: bool,
    pub left_handed: bool,
    pub tap: bool,
    pub disable_while_typing: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Idle {
    /// seconds w no input before each happens and 0 means never
    pub lock_after: u64,
    pub screen_off_after: u64,
    pub suspend_after: u64,
    /// locks the screen on idle and again if the lock screen dies while locked
    pub lock_command: String,
    pub suspend_command: String,
}

/// warmer colors thru the monitors gamma so its easier on the eyes at night
#[derive(Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NightLight {
    pub enabled: bool,
    /// kelvin where 6500 is normal and lower is warmer
    pub temperature: u32,
    /// hh:mm it turns on and off and both empty means the whole time its enabled
    pub from: String,
    pub until: String,
}

impl NightLight {
    /// whether it should be warm right now at minute of the day now
    pub fn active_at(&self, now: u32) -> bool {
        if !self.enabled {
            return false;
        }
        match (hhmm(&self.from), hhmm(&self.until)) {
            (Some(from), Some(until)) if from != until => {
                // a span over midnight like 20:00 to 07:00 wraps
                if from < until {
                    (from..until).contains(&now)
                } else {
                    now >= from || now < until
                }
            }
            _ => true,
        }
    }
}

/// minutes since midnight from hh:mm
pub fn hhmm(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// dims the brightest parts of windows so a white page cant blind u in the dark
#[derive(Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AntiFlashbang {
    pub enabled: bool,
    /// how bright a pixel can get from 0.05 to 1
    pub max_brightness: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBorder {
    width: i32,
    focused: String,
    unfocused: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDecorations {
    corner_radius: i32,
    shadow: bool,
    shadow_on_tiles: bool,
    shadow_size: i32,
    shadow_offset: [i32; 2],
    shadow_color: String,
    titlebar: bool,
    titlebar_height: i32,
    titlebar_focused: String,
    titlebar_unfocused: String,
    titlebar_text: String,
}

/// rounded corners and shadows
pub struct Decorations {
    /// canvas pixels and 0 is square
    pub corner_radius: i32,
    pub shadow: bool,
    pub shadow_on_tiles: bool,
    /// how far the shadow fades out in canvas pixels
    pub shadow_size: i32,
    pub shadow_offset: [i32; 2],
    pub shadow_color: Color,
    /// sevenwms own title bars for apps that let it draw them
    pub titlebar: bool,
    pub titlebar_height: i32,
    /// straight rgba bytes drawn on the cpu
    pub titlebar_focused: [u8; 4],
    pub titlebar_unfocused: [u8; 4],
    pub titlebar_text: [u8; 4],
}

/// the pointer as sevenwm draws it
#[derive(Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    /// a theme in ~/.icons or /usr/share/icons and default is the system one
    pub theme: String,
    /// logical pixels and the closest theme image gets uhhh scaled to it
    pub size: u32,
}

/// the outline around windows
pub struct Border {
    /// canvas pixels that scale w zoom and 0 draws none
    pub width: i32,
    pub focused: Color,
    pub unfocused: Color,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Xwayland {
    pub enabled: bool,
    /// the xwayland-satellite program
    pub path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    mode: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screenshot {
    /// shell command that picks an area and captures it
    pub command: String,
    /// show a still of the screen while the command runs
    pub freeze: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCanvas {
    size: RawCanvasSize,
    background: String,
    wallpaper: String,
    wallpaper_mode: WallpaperMode,
    region_outline: String,
    bounds_outline: String,
    drop_highlight: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawCanvasSize {
    Named(String),
    Size([i32; 2]),
}

/// how far the canvas goes around the workspace
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CanvasSize {
    Infinite,
    /// width and height centered on the workspace
    Limited(i32, i32),
}

pub type Color = [f32; 4];

/// how a wallpaper covers a monitor
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WallpaperMode {
    /// cover the monitor and crop what sticks out
    Fill,
    /// show all of it w bars of background color
    Fit,
    /// actual size centered
    Center,
    /// squash it to the monitor shape
    Stretch,
    /// repeat it at actual size
    Tile,
}

pub struct Canvas {
    pub size: CanvasSize,
    pub background: Color,
    /// an image behind everything on each monitor and empty for none
    pub wallpaper: String,
    pub wallpaper_mode: WallpaperMode,
    pub region_outline: Color,
    pub bounds_outline: Color,
    pub drop_highlight: Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NewTilePosition {
    Main,
    End,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tiling {
    pub gaps_inner: i32,
    pub gaps_outer: i32,
    pub split_ratio: f64,
    pub new_tile_position: NewTilePosition,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspaces {
    gap: i32,
    drag_modifier: String,
}

pub struct Workspaces {
    /// space kept between workspaces
    pub gap: i32,
    /// held w mod and left button to drag a workspace
    pub drag_mods: Mods,
    /// the same as written like ctrl for the cheat sheet
    pub drag_modifier: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NewWindows {
    /// tile when looking at a workspace and float where ur looking if not
    ByView,
    AlwaysTile,
    AlwaysFloat,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub new_windows: NewWindows,
    /// size a new floating window gets if it doesnt pick one
    pub float_size: [i32; 2],
    /// app id globs that tile or snap like before while other apps float centered
    pub tile_apps: Vec<String>,
    /// a new floating window this close to the screen size in px on both sides gets maximized and 0 means never
    pub maximize_tolerance: i32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snap {
    pub enabled: bool,
    pub gap: i32,
    pub threshold: i32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub zoom_min: f64,
    pub zoom_max: f64,
    pub zoom_step: f64,
    pub fly_duration_ms: u64,
    pub focus_follows_view: bool,
    pub focus_follows_mouse: bool,
    pub mouse_follows_focus: bool,
    /// mod+arrow from a tile can pick windows outside its workspace
    pub focus_leaves_workspace: bool,
    pub drag_empty_canvas_pans: bool,
    /// held w mod to pan over windows like alt
    pub pan_modifier: String,
}

pub struct Config {
    /// pixels a window moves or grows per keypress
    pub step: i32,
    /// the key mod meant when binds were built which a mouse drag needs held
    pub mod_mods: Mods,
    pub bindings: HashMap<KeyCombo, Action>,
    /// the binds as written for the cheat sheet maybe
    pub binding_list: Vec<(String, String)>,
    /// what mod means like super or the nested key
    pub mod_key: String,
    pub canvas: Canvas,
    pub tiling: Tiling,
    pub workspaces: Workspaces,
    pub placement: Placement,
    pub snap: Snap,
    pub view: View,
    /// held w mod and left button to uhh pan over windows
    pub pan_mods: Mods,
    pub screenshot: Screenshot,
    pub xwayland: Xwayland,
    pub input: Input,
    pub idle: Idle,
    pub night_light: NightLight,
    pub anti_flashbang: AntiFlashbang,
    pub border: Border,
    pub decorations: Decorations,
    pub cursor: Cursor,
    pub rules: Vec<Rule>,
    pub animations: Animations,
    pub collapse: Collapse,
    pub session: SessionConfig,
    pub theme: Theme,
    pub monitors: Vec<MonitorConfig>,
    /// commands run when sevenwm starts as a session
    pub autostart: Vec<String>,
    /// commands run then too and restarted when they crash
    pub keep_running: Vec<String>,
    /// the monitor mode as width height and hz or none for its best one
    pub output_mode: Option<(u16, u16, u32)>,
    /// problems found while loading for the log
    pub warnings: Vec<String>,
}

/// lay over onto base table by table so ur file only needs what it changes
fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(over)) => merge(base, over),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// where one pushable color lives in the config
enum ColorSlot<'a> {
    /// drawn by the gpu as floats
    Gpu(&'a mut Color),
    /// drawn on the cpu as bytes
    Cpu(&'a mut [u8; 4]),
}

impl Config {
    /// every color a shell can push over ipc by its config key
    fn color_slot(&mut self, key: &str) -> Option<ColorSlot<'_>> {
        use ColorSlot::{Cpu, Gpu};
        Some(match key {
            "border.focused" => Gpu(&mut self.border.focused),
            "border.unfocused" => Gpu(&mut self.border.unfocused),
            "canvas.background" => Gpu(&mut self.canvas.background),
            "canvas.region_outline" => Gpu(&mut self.canvas.region_outline),
            "canvas.bounds_outline" => Gpu(&mut self.canvas.bounds_outline),
            "canvas.drop_highlight" => Gpu(&mut self.canvas.drop_highlight),
            "decorations.shadow_color" => Gpu(&mut self.decorations.shadow_color),
            "decorations.titlebar_focused" => Cpu(&mut self.decorations.titlebar_focused),
            "decorations.titlebar_unfocused" => Cpu(&mut self.decorations.titlebar_unfocused),
            "decorations.titlebar_text" => Cpu(&mut self.decorations.titlebar_text),
            "theme.menu_background" => Cpu(&mut self.theme.menu_background),
            "theme.menu_text" => Cpu(&mut self.theme.menu_text),
            "theme.menu_hover" => Cpu(&mut self.theme.menu_hover),
            "theme.menu_disabled" => Cpu(&mut self.theme.menu_disabled),
            "theme.menu_edge" => Cpu(&mut self.theme.menu_edge),
            _ => return None,
        })
    }

    /// colors a shell pushed over ipc like its wallpaper palette laid over the files ones
    /// and all of them get checked first so a bad push changes nothing
    pub fn apply_colors(&mut self, colors: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
        let mut parsed = Vec::with_capacity(colors.len());
        for (key, value) in colors {
            let s = value.as_str().ok_or_else(|| format!("{key}: expected a color string"))?;
            let color = parse_color(s).map_err(|e| format!("{key}: {e}"))?;
            if self.color_slot(key).is_none() {
                return Err(format!("no color called {key}"));
            }
            parsed.push((key, s, color));
        }
        for (key, s, color) in parsed {
            match self.color_slot(key) {
                Some(ColorSlot::Gpu(slot)) => *slot = color,
                Some(ColorSlot::Cpu(slot)) => *slot = rgba_bytes(s, key)?,
                None => {}
            }
        }
        Ok(())
    }
}

/// a color as straight rgba bytes for cpu drawing
fn rgba_bytes(s: &str, what: &str) -> Result<[u8; 4], String> {
    let [r, g, b, a] = parse_color(s).map_err(|e| format!("{what}: {e}"))?;
    let un = |c: f32| {
        if a > 0.0 {
            (c / a * 255.0).round() as u8
        } else {
            0
        }
    };
    Ok([un(r), un(g), un(b), (a * 255.0).round() as u8])
}

/// auto or 1920x1080@240
fn parse_mode(s: &str) -> Result<Option<(u16, u16, u32)>, String> {
    if s == "auto" {
        return Ok(None);
    }
    let bad = || format!("output.mode: expected \"auto\" or like \"1920x1080@240\", got '{s}'");
    let (size, hz) = s.split_once('@').ok_or_else(bad)?;
    let (w, h) = size.split_once('x').ok_or_else(bad)?;
    Ok(Some((
        w.trim().parse().map_err(|_| bad())?,
        h.trim().parse().map_err(|_| bad())?,
        hz.trim().parse().map_err(|_| bad())?,
    )))
}

/// #rrggbb or #rrggbbaa returned premultiplied bc thats what the renderer blends w
fn parse_color(s: &str) -> Result<Color, String> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    let bad = || format!("expected a colour like \"#rrggbb\" or \"#rrggbbaa\", got '{s}'");
    if hex.len() != 6 && hex.len() != 8 {
        return Err(bad());
    }
    let byte = |i: usize| -> Result<f32, String> {
        hex.get(i..i + 2)
            .and_then(|b| u8::from_str_radix(b, 16).ok())
            .map(|b| b as f32 / 255.0)
            .ok_or_else(bad)
    };
    let alpha = if hex.len() == 8 { byte(6)? } else { 1.0 };
    Ok([byte(0)? * alpha, byte(2)? * alpha, byte(4)? * alpha, alpha])
}

impl Config {
    /// strip things down for the login screen w no binds no autostart no x11 and no idle locking
    pub fn restrict_for_greeter(&mut self) {
        self.bindings.clear();
        self.binding_list.clear();
        self.autostart.clear();
        self.keep_running.clear();
        self.xwayland.enabled = false;
        self.session.restore = false;
        self.idle.lock_after = 0;
        self.idle.suspend_after = 0;
        self.collapse.auto = false;
    }

    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("sevenwm").join("config.toml"))
    }

    /// load ur config over the defaults and nested picks what mod means
    pub fn load(nested: bool) -> Result<Self, String> {
        let user = match Self::path() {
            Some(path) if path.exists() => {
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?
            }
            Some(path) => {
                // first run so leave a commented copy of the defaults to edit
                let written = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&path, DEFAULT_CONFIG));
                match written {
                    Ok(()) => tracing::info!("wrote the default config to {}", path.display()),
                    Err(err) => tracing::warn!("couldn't write {}: {err}", path.display()),
                }
                String::new()
            }
            None => String::new(),
        };
        Self::from_toml(&user, nested)
    }

    pub fn from_toml(user: &str, nested: bool) -> Result<Self, String> {
        let mut table: toml::Table =
            toml::from_str(DEFAULT_CONFIG).expect("the built-in config parses");
        let user: toml::Table = toml::from_str(user).map_err(|e| e.to_string())?;
        let binding_keys = |t: &toml::Table| -> Vec<String> {
            t.get("keybindings")
                .and_then(toml::Value::as_table)
                .map(|b| b.keys().cloned().collect())
                .unwrap_or_default()
        };
        let (default_keys, user_keys) = (binding_keys(&table), binding_keys(&user));
        merge(&mut table, user);
        let raw: RawConfig = toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| e.to_string())?;

        let mod_key = if nested {
            raw.nested_mod_key
        } else {
            raw.mod_key
        };
        let mut mod_mods = Mods::default();
        mod_mods.add(&mod_key)?;

        // folding mod into another key can make combos clash so literal binds win then fewer modifiers and ur own entry replaces the default
        let user_combos: Vec<KeyCombo> = user_keys
            .iter()
            .filter_map(|k| parse_combo(k, &mod_key).ok())
            .collect();
        let replaced = |spec: &str| {
            default_keys.iter().any(|k| k == spec)
                && !user_keys.iter().any(|k| k == spec)
                && parse_combo(spec, &mod_key).is_ok_and(|c| user_combos.contains(&c))
        };
        let mut ordered: Vec<(String, String)> = raw
            .keybindings
            .into_iter()
            .filter(|(spec, _)| !replaced(spec))
            .collect();
        // the spelling last so equal combos always sort the same
        ordered.sort_by_key(|(combo, _)| {
            let lower = combo.to_lowercase();
            (lower.contains("mod"), lower.matches('+').count(), lower, combo.clone())
        });

        let mut bindings = HashMap::new();
        let mut binding_list = Vec::new();
        let mut owners: HashMap<KeyCombo, String> = HashMap::new();
        let mut warnings = Vec::new();
        for (spec, action) in ordered {
            let combo = match parse_combo(&spec, &mod_key) {
                Ok(combo) => combo,
                Err(e) => {
                    warnings.push(format!("keybinding '{spec}': {e}"));
                    continue;
                }
            };
            if action.trim() == "none" {
                continue;
            }
            let raw_action = action;
            let action = match Action::parse(&raw_action) {
                Ok(action) => action,
                Err(e) => {
                    warnings.push(format!("keybinding '{spec}': {e}"));
                    continue;
                }
            };
            if let Some(owner) = owners.get(&combo) {
                warnings.push(format!(
                    "'{spec}' is the same keys as '{owner}' while mod = {mod_key}; '{owner}' wins"
                ));
                continue;
            }
            binding_list.push((spec.clone(), raw_action.trim().to_string()));
            owners.insert(combo, spec);
            bindings.insert(combo, action);
        }

        let size = match raw.canvas.size {
            RawCanvasSize::Named(name) if name == "infinite" => CanvasSize::Infinite,
            RawCanvasSize::Named(other) => {
                return Err(format!(
                    "canvas.size: expected \"infinite\" or [width, height], got '{other}'"
                ));
            }
            RawCanvasSize::Size([w, h]) if w > 0 && h > 0 => CanvasSize::Limited(w, h),
            RawCanvasSize::Size(_) => {
                return Err("canvas.size: width and height must be positive".into());
            }
        };
        let canvas = Canvas {
            size,
            background: parse_color(&raw.canvas.background)
                .map_err(|e| format!("canvas.background: {e}"))?,
            wallpaper: raw.canvas.wallpaper.clone(),
            wallpaper_mode: raw.canvas.wallpaper_mode,
            region_outline: parse_color(&raw.canvas.region_outline)
                .map_err(|e| format!("canvas.region_outline: {e}"))?,
            bounds_outline: parse_color(&raw.canvas.bounds_outline)
                .map_err(|e| format!("canvas.bounds_outline: {e}"))?,
            drop_highlight: parse_color(&raw.canvas.drop_highlight)
                .map_err(|e| format!("canvas.drop_highlight: {e}"))?,
        };

        let view = raw.view;
        if !(view.zoom_min > 0.0 && view.zoom_min <= 1.0 && view.zoom_max >= 1.0) {
            return Err("view: zoom_min must be in (0, 1] and zoom_max at least 1".into());
        }
        if view.zoom_step <= 1.0 {
            return Err("view.zoom_step must be greater than 1".into());
        }
        for (name, speed) in [
            ("input.mouse", raw.input.mouse.accel_speed),
            ("input.touchpad", raw.input.touchpad.accel_speed),
        ] {
            if !(-1.0..=1.0).contains(&speed) {
                return Err(format!("{name}.accel_speed must be between -1.0 and 1.0"));
            }
        }
        if !(8..=256).contains(&raw.cursor.size) {
            return Err("cursor.size must be between 8 and 256".into());
        }
        // a drag modifier on top of mod and if mod already is that key shift stands in
        let with_mod = |extra: &str, what: &str| -> Result<Mods, String> {
            let mut mods = mod_mods;
            let before = mods;
            mods.add(extra).map_err(|e| format!("{what}: {e}"))?;
            if mods == before {
                mods.shift = true;
            }
            Ok(mods)
        };
        let pan_mods = with_mod(&view.pan_modifier, "view.pan_modifier")?;
        let workspaces = Workspaces {
            gap: raw.workspaces.gap.max(0),
            drag_mods: with_mod(&raw.workspaces.drag_modifier, "workspaces.drag_modifier")?,
            drag_modifier: raw.workspaces.drag_modifier.clone(),
        };
        if workspaces.drag_mods == pan_mods {
            return Err("workspaces.drag_modifier and view.pan_modifier must differ".into());
        }
        let tiling = raw.tiling;
        if !(0.1..=0.9).contains(&tiling.split_ratio) {
            return Err("tiling.split_ratio must be between 0.1 and 0.9".into());
        }

        Ok(Self {
            step: raw.step.max(1),
            mod_mods,
            bindings,
            binding_list,
            mod_key: mod_key.clone(),
            canvas,
            tiling: Tiling {
                gaps_inner: tiling.gaps_inner.max(0),
                gaps_outer: tiling.gaps_outer.max(0),
                ..tiling
            },
            workspaces,
            pan_mods,
            placement: raw.placement,
            snap: Snap {
                gap: raw.snap.gap.max(0),
                threshold: raw.snap.threshold.max(0),
                ..raw.snap
            },
            view,
            screenshot: raw.screenshot,
            autostart: raw.autostart,
            keep_running: raw.keep_running,
            xwayland: raw.xwayland,
            input: raw.input,
            rules: raw.rules,
            animations: raw.animations,
            collapse: raw.collapse,
            session: raw.session,
            theme: Theme {
                menu_background: rgba_bytes(&raw.theme.menu_background, "theme.menu_background")?,
                menu_text: rgba_bytes(&raw.theme.menu_text, "theme.menu_text")?,
                menu_hover: rgba_bytes(&raw.theme.menu_hover, "theme.menu_hover")?,
                menu_disabled: rgba_bytes(&raw.theme.menu_disabled, "theme.menu_disabled")?,
                menu_edge: rgba_bytes(&raw.theme.menu_edge, "theme.menu_edge")?,
            },
            monitors: {
                for monitor in &raw.monitors {
                    if let Some(mode) = &monitor.mode {
                        parse_mode(mode).map_err(|e| format!("monitor {}: {e}", monitor.name))?;
                    }
                    if monitor.scale.is_some_and(|s| !(0.5..=4.0).contains(&s)) {
                        return Err(format!("monitor {}: scale must be 0.5 to 4", monitor.name));
                    }
                }
                raw.monitors
            },
            idle: raw.idle,
            night_light: {
                let n = &raw.night_light;
                for (name, value) in [("from", &n.from), ("until", &n.until)] {
                    if !value.trim().is_empty() && hhmm(value).is_none() {
                        return Err(format!("night_light.{name} should be hh:mm like 20:00 or empty not '{value}'"));
                    }
                }
                NightLight {
                    temperature: n.temperature.clamp(1000, 6500),
                    ..raw.night_light
                }
            },
            anti_flashbang: AntiFlashbang {
                max_brightness: raw.anti_flashbang.max_brightness.clamp(0.05, 1.0),
                ..raw.anti_flashbang
            },
            cursor: raw.cursor,
            border: Border {
                width: raw.border.width.max(0),
                focused: parse_color(&raw.border.focused)
                    .map_err(|e| format!("border.focused: {e}"))?,
                unfocused: parse_color(&raw.border.unfocused)
                    .map_err(|e| format!("border.unfocused: {e}"))?,
            },
            decorations: Decorations {
                corner_radius: raw.decorations.corner_radius.clamp(0, 100),
                shadow: raw.decorations.shadow,
                shadow_on_tiles: raw.decorations.shadow_on_tiles,
                shadow_size: raw.decorations.shadow_size.clamp(0, 200),
                shadow_offset: raw.decorations.shadow_offset,
                shadow_color: parse_color(&raw.decorations.shadow_color)
                    .map_err(|e| format!("decorations.shadow_color: {e}"))?,
                titlebar: raw.decorations.titlebar,
                titlebar_height: raw.decorations.titlebar_height.clamp(12, 64),
                titlebar_focused: rgba_bytes(
                    &raw.decorations.titlebar_focused,
                    "decorations.titlebar_focused",
                )?,
                titlebar_unfocused: rgba_bytes(
                    &raw.decorations.titlebar_unfocused,
                    "decorations.titlebar_unfocused",
                )?,
                titlebar_text: rgba_bytes(&raw.decorations.titlebar_text, "decorations.titlebar_text")?,
            },
            output_mode: parse_mode(&raw.output.mode)?,
            warnings,
        })
    }

    /// the mode set for the monitor called name from its own entry or the default
    pub fn output_mode(&self, name: &str) -> Option<(u16, u16, u32)> {
        self.monitor(name)
            .and_then(|m| m.mode.as_deref())
            .and_then(|mode| parse_mode(mode).ok())
            .flatten()
            .or(self.output_mode)
    }

    pub fn monitor(&self, name: &str) -> Option<&MonitorConfig> {
        self.monitors.iter().find(|m| m.name == name)
    }

    /// every rule matching a window merged w later ones winning
    pub fn rule_for(&self, app_id: &str, title: &str) -> Rule {
        let mut merged = Rule::default();
        for rule in self.rules.iter().filter(|r| r.matches(app_id, title)) {
            merged.float = rule.float.or(merged.float);
            merged.size = rule.size.or(merged.size);
            merged.hide_from_screencast = rule.hide_from_screencast.or(merged.hide_from_screencast);
        }
        merged
    }

    /// whether an app is one of the tile_apps like a terminal
    pub fn tiles_app(&self, app_id: &str) -> bool {
        self.placement.tile_apps.iter().any(|p| glob(p, app_id))
    }

    pub fn lookup(&self, mods: Mods, key: Keysym) -> Option<&Action> {
        self.bindings.get(&KeyCombo { mods, key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::input::keyboard::keysyms;

    fn combo(mods: Mods, raw: u32) -> KeyCombo {
        KeyCombo {
            mods,
            key: Keysym::new(raw),
        }
    }

    const SUPER: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
        logo: true,
    };

    #[test]
    fn defaults_parse_without_warnings() {
        let config = Config::from_toml("", false).unwrap();
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(
            config.lookup(SUPER, Keysym::new(keysyms::KEY_t)),
            Some(&Action::Exec("kitty".into()))
        );
        assert_eq!(
            config.lookup(SUPER, Keysym::new(keysyms::KEY_q)),
            Some(&Action::CloseWindow)
        );
    }

    #[test]
    fn a_bad_color_push_changes_nothing() {
        let mut config = Config::from_toml("", false).unwrap();
        let before = config.border.focused;
        let push = |v: serde_json::Value| v.as_object().unwrap().clone();
        let bad = push(serde_json::json!({ "border.focused": "#ff0000", "nope": "#00ff00" }));
        assert!(config.apply_colors(&bad).is_err());
        assert_eq!(config.border.focused, before);
        let good = push(serde_json::json!({ "border.focused": "#ff0000", "theme.menu_text": "#00ff00" }));
        assert!(config.apply_colors(&good).is_ok());
        assert_eq!(config.border.focused, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(config.theme.menu_text, [0, 255, 0, 255]);
    }

    #[test]
    fn hjkl_is_left_up_down_right_and_mod_shift_m_quits() {
        let config = Config::from_toml("", false).unwrap();
        let focus = |key| config.lookup(SUPER, Keysym::new(key)).cloned();
        assert_eq!(focus(keysyms::KEY_h), Some(Action::Focus(Direction::Left)));
        assert_eq!(focus(keysyms::KEY_j), Some(Action::Focus(Direction::Down)));
        assert_eq!(focus(keysyms::KEY_k), Some(Action::Focus(Direction::Up)));
        assert_eq!(focus(keysyms::KEY_l), Some(Action::Focus(Direction::Right)));
        assert_eq!(focus(keysyms::KEY_m), None);
        let shift = Mods {
            shift: true,
            ..SUPER
        };
        assert_eq!(
            config.lookup(shift, Keysym::new(keysyms::KEY_m)),
            Some(&Action::Quit)
        );
    }

    #[test]
    fn arrows_and_vim_keys_both_focus() {
        let config = Config::from_toml("", false).unwrap();
        for key in [keysyms::KEY_h, keysyms::KEY_Left] {
            assert_eq!(
                config.bindings.get(&combo(SUPER, key)),
                Some(&Action::Focus(Direction::Left))
            );
        }
    }

    #[test]
    fn nested_mod_is_alt_and_literal_alt_tab_wins() {
        let config = Config::from_toml("", true).unwrap();
        let alt = Mods {
            alt: true,
            ..Mods::default()
        };
        assert_eq!(
            config.lookup(alt, Keysym::new(keysyms::KEY_t)),
            Some(&Action::Exec("kitty".into()))
        );
        assert_eq!(
            config.lookup(alt, Keysym::new(keysyms::KEY_Tab)),
            Some(&Action::CycleWindows)
        );
        assert!(config.warnings.iter().any(|w| w.contains("mod+tab")));
        // mod+alt+h folds into mod+h and the plainer focus bind keeps it
        assert_eq!(
            config.lookup(alt, Keysym::new(keysyms::KEY_h)),
            Some(&Action::Focus(Direction::Left))
        );
    }

    #[test]
    fn user_entries_override_and_unbind() {
        let config = Config::from_toml(
            "[keybindings]\n\"mod+t\" = \"exec foot\"\n\"mod+q\" = \"none\"\n",
            false,
        )
        .unwrap();
        assert_eq!(
            config.lookup(SUPER, Keysym::new(keysyms::KEY_t)),
            Some(&Action::Exec("foot".into()))
        );
        assert_eq!(config.lookup(SUPER, Keysym::new(keysyms::KEY_q)), None);
    }

    #[test]
    fn unbinding_works_whatever_the_spelling() {
        let config = Config::from_toml(
            "[keybindings]\n\"Mod+Q\" = \"none\"\n\"super+T\" = \"exec foot\"\n",
            false,
        )
        .unwrap();
        assert_eq!(config.lookup(SUPER, Keysym::new(keysyms::KEY_q)), None);
        assert_eq!(
            config.lookup(SUPER, Keysym::new(keysyms::KEY_t)),
            Some(&Action::Exec("foot".into()))
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn sections_merge_setting_by_setting() {
        let config = Config::from_toml(
            "[canvas]\nsize = [8000, 6000]\n[tiling]\ngaps_inner = 20\n",
            false,
        )
        .unwrap();
        assert_eq!(config.canvas.size, CanvasSize::Limited(8000, 6000));
        assert_eq!(config.tiling.gaps_inner, 20);
        // untouched settings in the same sections keep their defaults
        assert_eq!(config.tiling.new_tile_position, NewTilePosition::Main);
        assert_eq!(config.canvas.background, parse_color("#141418").unwrap());
    }

    #[test]
    fn bad_settings_are_errors() {
        assert!(Config::from_toml("[canvas]\nsize = \"huge\"\n", false).is_err());
        assert!(Config::from_toml("[canvas]\nbackground = \"red\"\n", false).is_err());
        assert!(Config::from_toml("[tiling]\nsplit_ratio = 2.0\n", false).is_err());
        assert!(Config::from_toml("[view]\nzoom_step = 0.5\n", false).is_err());
        assert!(
            Config::from_toml("[snap]\nmagnet = true\n", false).is_err(),
            "unknown setting"
        );
    }

    #[test]
    fn globs_match_like_shell_patterns() {
        assert!(glob("*", ""));
        assert!(glob("firefox", "firefox"));
        assert!(!glob("firefox", "firefoxy"));
        assert!(glob(
            "*Picture-in-Picture*",
            "YouTube - Picture-in-Picture - Firefox"
        ));
        assert!(glob("org.gnome.?alculator", "org.gnome.Calculator"));
        assert!(!glob("steam*", "Steam"), "patterns are case-sensitive");
    }

    #[test]
    fn later_rules_win_and_unmatched_rules_do_nothing() {
        let config = Config::from_toml(
            "[[rules]]\napp_id = \"pavucontrol\"\nfloat = true\nsize = [800, 500]\n\
             [[rules]]\napp_id = \"pavu*\"\nsize = [600, 400]\n\
             [[rules]]\ntitle = \"*secret*\"\nhide_from_screencast = true\n",
            false,
        )
        .unwrap();
        let rule = config.rule_for("pavucontrol", "Volume Control");
        assert_eq!(rule.float, Some(true));
        assert_eq!(rule.size, Some([600, 400]));
        assert_eq!(rule.hide_from_screencast, None);
        assert_eq!(
            config
                .rule_for("kitty", "my secret notes")
                .hide_from_screencast,
            Some(true)
        );
    }

    #[test]
    fn colours_parse_with_and_without_alpha() {
        assert_eq!(parse_color("#ff0000").unwrap(), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(parse_color("#00000080").unwrap()[3], 128.0 / 255.0);
        assert!(parse_color("#12345").is_err());
    }

    #[test]
    fn translucent_colours_are_premultiplied() {
        let [r, g, b, a] = parse_color("#ffffff40").unwrap();
        assert_eq!(a, 64.0 / 255.0);
        assert_eq!([r, g, b], [a, a, a]);
    }

    #[test]
    fn bad_entries_warn_instead_of_failing() {
        let config = Config::from_toml(
            "[keybindings]\n\"mod+nosuchkey\" = \"quit\"\n\"mod+x\" = \"fly\"\n",
            false,
        )
        .unwrap();
        assert_eq!(config.warnings.len(), 2, "{:?}", config.warnings);
    }
}
