//! what each keybind does

use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::input::pointer::Focus;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::config::{Action, Config, Direction};
use crate::grabs::{DragKind, WindowDrag};
use crate::layout;
use crate::state::Seven;

/// smallest size a keyboard resize can prolly shrink a window to
const MIN_SIZE: i32 = 100;

/// where the view was before fullscreen so we can go back
#[derive(Default)]
struct ViewBeforeFullscreen(std::cell::Cell<Option<(Point<f64, Logical>, f64)>>);

/// how long exec-outside waits for its window to show up
const OPEN_OUTSIDE_WAIT: Duration = Duration::from_secs(15);

impl Seven {
    pub fn run_action(&mut self, action: Action) {
        self.damage();
        match action {
            Action::Exec(command) => self.spawn(&command),
            Action::ExecOutside(command) => {
                self.open_outside.push(Instant::now() + OPEN_OUTSIDE_WAIT);
                self.spawn(&command);
            }
            Action::CloseWindow => {
                if let Some(toplevel) = self.focused_window().and_then(|w| w.toplevel().cloned()) {
                    toplevel.send_close();
                }
            }
            Action::Quit => self.loop_signal.stop(),
            Action::CycleWindows => self.cycle_windows(),
            Action::ToggleMaximize => {
                if let Some(window) = self.focused_window() {
                    self.toggle_maximize(&window);
                }
            }
            Action::ToggleFullscreen => {
                if let Some(window) = self.focused_window() {
                    self.toggle_fullscreen(&window);
                }
            }
            Action::Focus(dir) => self.focus_direction(dir),
            Action::Move(dir) => self.nudge_focused(dir),
            Action::Grow(dir) => self.resize_focused(dir, self.config.step),
            Action::Shrink(dir) => self.resize_focused(dir, -self.config.step),
            Action::RelaunchWindow => self.relaunch_focused(),
            Action::ReloadConfig => self.reload_config(),
            Action::Home => self.fly_home(),
            Action::ToggleFloating => {
                if let Some(window) = self.focused_window() {
                    self.toggle_floating(&window);
                }
            }
            Action::ToggleTiling => {
                if let Some(window) = self.focused_window() {
                    self.toggle_tiling(&window);
                }
            }
            Action::Overview => self.toggle_overview(),
            Action::ZoomIn => self.zoom_centre(self.config.view.zoom_step),
            Action::ZoomOut => self.zoom_centre(1.0 / self.config.view.zoom_step),
            Action::CloseMenu => self.close_menu(),
            Action::Screenshot => self.screenshot(),
            Action::Workspace(n) => self.go_to_workspace(n),
            Action::MoveToWorkspace(n) => self.move_to_workspace(n),
            Action::NewWorkspace => self.new_workspace(),
            Action::RemoveWorkspace => self.destroy_workspace(),
            Action::ToggleCollapse => self.toggle_collapse(),
            Action::CenterWindow => {
                if let Some(window) = self.focused_window() {
                    self.center_window(&window);
                }
            }
        }
    }

    pub fn focused_window(&self) -> Option<Window> {
        let surface = self.seat.get_keyboard()?.current_focus()?;
        self.window_for_surface(&surface)
    }

    pub fn reload_config(&mut self) {
        match Config::load(self.nested) {
            Ok(config) => {
                for warning in &config.warnings {
                    tracing::warn!("config: {warning}");
                }
                let titlebar_was = self.config.decorations.titlebar;
                let ratio_was = self.config.tiling.split_ratio;
                self.config = config;
                if self.greeter.is_some() {
                    self.config.restrict_for_greeter();
                }
                if self.config.decorations.titlebar != titlebar_was {
                    self.apply_decoration_mode();
                }
                if self.cursors.config != self.config.cursor {
                    self.cursors = crate::cursor::Cursors::new(&self.config.cursor);
                }
                // a new split ratio resets the splits but other changes keep tiles as they were
                let ratio = self.config.tiling.split_ratio;
                if ratio != ratio_was {
                    for ws in &mut self.workspaces {
                        for (_, r) in &mut ws.tiled {
                            *r = ratio;
                        }
                    }
                }
                if let Some(mut apply_modes) = self.mode_hook.take() {
                    apply_modes(self);
                    self.mode_hook = Some(apply_modes);
                }
                self.arrange_monitors();
                self.apply_keyboard_settings();
                self.reconfigure_input_devices();
                self.apply_keep_running();
                tracing::info!("config reloaded");
            }
            Err(err) => {
                tracing::warn!("config not reloaded, keeping the old one: {err}");
                // say it where u can see it or saved settings js silently dont apply
                self.notify_error("Config not applied", &err);
            }
        }
    }

    /// step thru windows most recent first but only reorder when the cycle ends so taps go further back
    fn cycle_windows(&mut self) {
        if self.history.len() < 2 {
            return;
        }
        // collapsed windows stay folded while cycling
        let mut next = self.cycle.unwrap_or(0);
        for _ in 0..self.history.len() {
            next = (next + 1) % self.history.len();
            if !self.is_collapsed(&self.history[next]) {
                break;
            }
        }
        if self.is_collapsed(&self.history[next]) {
            return;
        }
        self.cycle = Some(next);
        let window = self.history[next].clone();
        self.focus_without_history(Some(&window));
        self.bring_into_view(&window);
    }

    /// end an alt-tab cycle and make the landed window most recent
    pub fn end_cycle(&mut self) {
        if let Some(i) = self.cycle.take() {
            let window = self.history.remove(i);
            self.history.insert(0, window);
        }
    }

    /// focus the nearest window that way starting from the focused one or the middle of the screen
    fn focus_direction(&mut self, dir: Direction) {
        let (camera, zoom) = self.view.destination();
        let screen = self.screen_size();
        let visible = Rectangle::new(
            camera,
            Size::from((screen.w as f64 / zoom, screen.h as f64 / zoom)),
        );
        let current = self.focused_window().filter(|w| {
            self.space
                .element_geometry(w)
                .is_some_and(|rect| visible.overlaps(rect.to_f64()))
        });
        let from = match &current {
            Some(window) => match self.window_centre(window) {
                Some(centre) => centre,
                None => return,
            },
            None => Point::from((
                visible.loc.x + visible.size.w / 2.0,
                visible.loc.y + visible.size.h / 2.0,
            )),
        };
        let (windows, rects): (Vec<Window>, Vec<_>) = self
            .space
            .elements()
            .filter(|w| Some(*w) != current.as_ref())
            .filter_map(|w| Some((w.clone(), self.frame(w)?)))
            .unzip();
        let target = crate::layout::nearest(from, dir, &rects).map(|i| windows[i].clone());
        if let Some(window) = target {
            self.focus(Some(&window));
            self.bring_into_view(&window);
        }
    }

    fn window_centre(&self, window: &Window) -> Option<Point<f64, Logical>> {
        let geo = self.space.element_geometry(window)?;
        Some(Point::from((
            geo.loc.x as f64 + geo.size.w as f64 / 2.0,
            geo.loc.y as f64 + geo.size.h as f64 / 2.0,
        )))
    }

    fn nudge_focused(&mut self, dir: Direction) {
        let Some(window) = self.focused_window() else {
            return;
        };
        if self.is_fullscreen(&window) {
            return;
        }
        if self.is_tiled(&window) {
            self.swap_tile(&window, dir);
            return;
        }
        let Some(rect) = self.frame(&window) else {
            return;
        };
        let (dx, dy) = dir.delta();
        let step = self.config.step;
        let mut moved = Rectangle::new(rect.loc + Point::from((dx * step, dy * step)), rect.size);
        if let Some(bounds) = self.bounds() {
            moved.loc = crate::layout::clamp_into(moved, bounds);
        }
        self.place_frame(&window, moved.loc, true);
    }

    /// swap a tile w the nearest tile that way
    fn swap_tile(&mut self, window: &Window, dir: Direction) {
        let Some(from) = self.window_centre(window) else {
            return;
        };
        let Some(ws) = self.ws_of(window) else {
            return;
        };
        let (dx, dy) = dir.delta();
        let target = self.workspaces[ws]
            .tiled
            .iter()
            .map(|(w, _)| w)
            .filter(|w| *w != window)
            .filter_map(|w| {
                let to = self.window_centre(w)?;
                let along = (to.x - from.x) * dx as f64 + (to.y - from.y) * dy as f64;
                let across = ((to.x - from.x) * dy as f64 - (to.y - from.y) * dx as f64).abs();
                (along > 0.0).then_some((along + 2.0 * across, w.clone()))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, w)| w);
        let Some(target) = target else {
            return;
        };
        let tiled = &mut self.workspaces[ws].tiled;
        let i = tiled.iter().position(|(w, _)| w == window);
        let j = tiled.iter().position(|(w, _)| *w == target);
        if let (Some(i), Some(j)) = (i, j) {
            // swap the windows not the splits so the layout keeps its shape
            let (a, b) = (tiled[i].0.clone(), tiled[j].0.clone());
            tiled[i].0 = b;
            tiled[j].0 = a;
            self.retile();
        }
    }

    /// move a tiles edge out by amount pixels by shifting the split that makes it
    fn resize_tile(&mut self, window: &Window, dir: Direction, amount: i32) {
        let Some(ws) = self.ws_of(window) else {
            return;
        };
        let tiled = &self.workspaces[ws].tiled;
        let Some(i) = tiled.iter().position(|(w, _)| w == window) else {
            return;
        };
        let ratios: Vec<f64> = tiled.iter().map(|(_, ratio)| *ratio).collect();
        let gap = self.config.tiling.gaps_inner;
        let Some((split, span)) = layout::dwindle_edge(self.tile_area(ws), &ratios, gap, i, dir)
        else {
            return;
        };
        let ratio = &mut self.workspaces[ws].tiled[split].1;
        *ratio = (*ratio + amount as f64 / span).clamp(0.1, 0.9);
        self.retile();
    }

    /// move the windows edge out by amount and keep the other edge still
    fn resize_focused(&mut self, dir: Direction, amount: i32) {
        let Some(window) = self.focused_window() else {
            return;
        };
        if self.is_fullscreen(&window) {
            return;
        }
        if self.is_tiled(&window) {
            self.resize_tile(&window, dir, amount);
            return;
        }
        let Some(Rectangle { mut loc, size }) = self.frame(&window) else {
            return;
        };
        let mut new_size = size;
        match dir {
            Direction::Left | Direction::Right => new_size.w = (size.w + amount).max(MIN_SIZE),
            Direction::Up | Direction::Down => new_size.h = (size.h + amount).max(MIN_SIZE),
        }
        match dir {
            Direction::Left => loc.x -= new_size.w - size.w,
            Direction::Up => loc.y -= new_size.h - size.h,
            Direction::Right | Direction::Down => {}
        }
        let mut rect = Rectangle::new(loc, new_size);
        if let Some(bounds) = self.bounds() {
            rect = rect.intersection(bounds).unwrap_or(rect);
        }
        self.resize_window(&window, rect);
    }

    /// fit the windows frame to rect
    pub fn resize_window(&mut self, window: &Window, rect: Rectangle<i32, Logical>) {
        let rect = self.content_of(window, rect);
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| state.size = Some(rect.size));
            // before its first configure a window gets its size w that configure instead
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
        // a space location is where the windows visible part starts
        self.space.map_element(window.clone(), rect.loc, false);
    }

    pub fn is_fullscreen(&self, window: &Window) -> bool {
        self.fullscreen.iter().any(|(w, _)| w == window)
    }

    /// does the fill the screen thing or puts it back
    pub fn toggle_fullscreen(&mut self, window: &Window) {
        if self.is_fullscreen(window) {
            self.unfullscreen(window);
        } else {
            self.fullscreen(window);
        }
    }

    pub fn fullscreen(&mut self, window: &Window) {
        // it asked before its first frame so go fullscreen once its placed
        if self.unplaced.contains(window) {
            window.user_data().insert_if_missing(WantsFullscreen::default);
            return;
        }
        let (Some(toplevel), Some(before)) =
            (window.toplevel(), self.frame(window))
        else {
            return;
        };
        let before = self.unmaximized_rect(window).unwrap_or(before);
        self.drop_maximized(window);
        // fill a screen sized area at the tiles workspace or around the floating window and fly there
        let size = self.screen_size();
        let loc = match self.ws_of(window) {
            Some(i) => self.workspaces[i].rect.loc,
            None => {
                let loc = before.loc + Point::from(((before.size.w - size.w) / 2, (before.size.h - size.h) / 2));
                match self.bounds() {
                    Some(bounds) => layout::clamp_into(Rectangle::new(loc, size), bounds),
                    None => loc,
                }
            }
        };
        let screen = Rectangle::new(loc, size);
        let view = self.view.destination();
        window
            .user_data()
            .get_or_insert(ViewBeforeFullscreen::default)
            .0
            .set(Some(view));
        self.fullscreen.retain(|(w, _)| w != window);
        self.fullscreen.push((window.clone(), before));
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = Some(screen.size);
        });
        toplevel.send_pending_configure();
        self.space.map_element(window.clone(), screen.loc, true);
        self.focus(Some(window));
        let duration = std::time::Duration::from_millis(self.config.view.fly_duration_ms);
        self.view.fly_to(screen.loc.to_f64(), 1.0, duration);
    }

    pub fn unfullscreen(&mut self, window: &Window) {
        let Some(i) = self.fullscreen.iter().position(|(w, _)| w == window) else {
            return;
        };
        let (_, before) = self.fullscreen.remove(i);
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.fullscreen_output = None;
            });
        }
        if self.is_tiled(window) {
            self.retile();
        } else {
            self.resize_window(window, before);
        }
        if let Some((camera, zoom)) = window
            .user_data()
            .get::<ViewBeforeFullscreen>()
            .and_then(|v| v.0.take())
        {
            let duration = Duration::from_millis(self.config.view.fly_duration_ms);
            self.view.fly_to(camera, zoom, duration);
        }
    }

    pub fn is_maximized(&self, window: &Window) -> bool {
        window
            .user_data()
            .get::<Maximized>()
            .is_some_and(|m| m.0.get().is_some())
    }

    /// the rect a maximized window goes back to
    fn unmaximized_rect(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        window.user_data().get::<Maximized>()?.0.get()
    }

    /// mod+shift+f maximizes the window or puts it back
    pub fn toggle_maximize(&mut self, window: &Window) {
        if self.is_maximized(window) {
            self.unmaximize(window);
        } else {
            self.maximize(window);
        }
    }

    /// floating windows fill the screen minus panels and tiles fill their workspace
    pub fn maximize(&mut self, window: &Window) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        if let Some(i) = self.ws_of(window)
            && !self.is_fullscreen(window)
            && !self.is_maximized(window)
            && let Some(before) = self.frame(window)
        {
            window
                .user_data()
                .get_or_insert(Maximized::default)
                .0
                .set(Some(before));
            toplevel.with_pending_state(|state| state.states.set(xdg_toplevel::State::Maximized));
            self.retile_ws(i);
            self.restack();
            self.focus(Some(window));
            return;
        }
        if self.is_tiled(window) || self.is_fullscreen(window) || self.is_maximized(window) {
            // the protocol wants an answer either way
            if toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
            return;
        }
        let Some(before) = self.frame(window) else {
            return;
        };
        let area = self.maximize_area();
        window
            .user_data()
            .get_or_insert(Maximized::default)
            .0
            .set(Some(before));
        toplevel.with_pending_state(|state| state.states.set(xdg_toplevel::State::Maximized));
        self.resize_window(window, area);
        self.focus(Some(window));
    }

    pub fn unmaximize(&mut self, window: &Window) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let before = window.user_data().get::<Maximized>().and_then(|m| m.0.take());
        toplevel.with_pending_state(|state| state.states.unset(xdg_toplevel::State::Maximized));
        match before {
            // a tile goes back into its layout
            Some(_) if !self.is_fullscreen(window) && let Some(i) = self.ws_of(window) => {
                self.retile_ws(i);
                self.restack();
            }
            Some(rect) if !self.is_fullscreen(window) => self.resize_window(window, rect),
            _ => {
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
        }
    }

    /// forget a window is maximized but keep its size
    pub fn drop_maximized(&mut self, window: &Window) {
        if let Some(m) = window.user_data().get::<Maximized>()
            && m.0.take().is_some()
            && let Some(toplevel) = window.toplevel()
        {
            toplevel.with_pending_state(|state| state.states.unset(xdg_toplevel::State::Maximized));
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
    }

    /// what a maximized window fills which is the visible canvas minus panels and gap
    fn maximize_area(&self) -> Rectangle<i32, Logical> {
        let screen = self.screen_size();
        let zone = self.output.as_ref().map_or(Rectangle::from_size(screen), |o| {
            smithay::desktop::layer_map_for_output(o).non_exclusive_zone()
        });
        let zoom = self.view.zoom;
        let loc = self.view.to_canvas(zone.loc.to_f64());
        let area = Rectangle::new(
            loc.to_i32_round(),
            Size::from((
                (zone.size.w as f64 / zoom).round() as i32,
                (zone.size.h as f64 / zoom).round() as i32,
            )),
        );
        crate::layout::inset(area, self.config.tiling.gaps_outer)
    }

    /// start dragging a window to move or resize it
    pub fn start_drag(&mut self, window: Window, kind: DragKind, button: u32, serial: Serial) {
        if self.is_fullscreen(&window) {
            return;
        }
        // picking up a tile floats it only once the drag really moves so a click leaves it tiled prolly
        let lift_tile = self.is_tiled(&window);
        // a tiles size belongs to the layout
        if lift_tile && matches!(kind, DragKind::Resize(_)) {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(initial) = self.frame(&window) else {
            return;
        };
        self.drop_maximized(&window);
        let start_data = smithay::input::pointer::GrabStartData {
            focus: None,
            button,
            location: pointer.current_location(),
        };
        if let (DragKind::Resize(_), Some(toplevel)) = (&kind, window.toplevel()) {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });
            toplevel.send_pending_configure();
        }
        let menu_on_click = matches!(kind, DragKind::Resize(_));
        let grab_window = window.clone();
        let grab = WindowDrag {
            start_data,
            window,
            initial,
            kind,
            button,
            menu_on_click,
            lift_tile,
            moved: 0.0,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
        self.dragging = Some(grab_window);
    }

    /// the process that owns the window and none for x11 windows
    pub fn window_pid(&self, window: &Window) -> Option<i32> {
        // ask once per window bc a clients process never changes
        let pid = *window
            .user_data()
            .get_or_insert(|| {
                WindowPid(
                    window
                        .toplevel()
                        .and_then(|t| t.wl_surface().client())
                        .and_then(|c| c.get_credentials(&self.display_handle).ok())
                        .map(|creds| creds.pid),
                )
            })
            .0
            .as_ref()?;
        let satellite = self.xwayland.as_ref().is_some_and(|c| c.id() as i32 == pid);
        (!satellite).then_some(pid)
    }

    /// kill the app and start it again w the same command and folder
    fn relaunch_focused(&mut self) {
        let Some(window) = self.focused_window() else {
            return;
        };
        let Some(pid) = window
            .toplevel()
            .and_then(|t| t.wl_surface().client())
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .map(|creds| creds.pid)
        else {
            tracing::warn!("relaunch: can't tell which process owns the window");
            return;
        };
        // every x11 app shares xwayland-satellites process so killing it would kill them all
        if self.xwayland.as_ref().is_some_and(|c| c.id() as i32 == pid) {
            tracing::warn!("relaunch: X11 apps can't be relaunched one at a time");
            return;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            tracing::warn!("relaunch: can't read process {pid}'s command line");
            return;
        };
        let argv: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();

        tracing::info!("relaunching {program} (pid {pid})");
        // safety plain syscall and a stale pid js fails w ESRCH
        unsafe { libc::kill(pid, libc::SIGTERM) };

        let mut command = std::process::Command::new(program);
        command.args(args);
        self.session_env(&mut command);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        // start it once the old one is gone or give up after a few secs and start it anyway
        let program = program.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut command = Some(command);
        let _ = self.loop_handle.insert_source(
            Timer::from_duration(std::time::Duration::from_millis(50)),
            move |_, _, state| {
                // a zombie counts as gone
                let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|stat| {
                        stat.rsplit_once(')')
                            .and_then(|(_, rest)| rest.split_whitespace().next())
                            .map(|state| state != "Z")
                    })
                    .unwrap_or(false);
                if alive && std::time::Instant::now() < deadline {
                    return TimeoutAction::ToDuration(std::time::Duration::from_millis(50));
                }
                if let Some(mut command) = command.take() {
                    match command.spawn() {
                        Ok(child) => state.watch_child(&program, child),
                        Err(err) => tracing::warn!("relaunch: failed to start {program}: {err}"),
                    }
                }
                TimeoutAction::Drop
            },
        );
    }
}

/// a window that asked for fullscreen before it was placed
#[derive(Default)]
pub struct WantsFullscreen;

/// a maximized window and the frame it goes back to
#[derive(Default)]
pub struct Maximized(pub std::cell::Cell<Option<Rectangle<i32, Logical>>>);

/// a windows client process id looked up once
struct WindowPid(Option<i32>);
