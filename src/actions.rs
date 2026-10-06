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
/// env var that marks programs started by exec-outside
pub const OUTSIDE_ENV: &str = "SEVENWM_OPEN_OUTSIDE";

impl Seven {
    pub fn run_action(&mut self, action: Action) {
        self.damage();
        match action {
            Action::Exec(command) => self.spawn(&command),
            Action::ExecOutside(command) => {
                // the tag rides along in its env so only a window from what it started gets picked
                let tag = format!("{}-{}", std::process::id(), self.start_time.elapsed().as_nanos());
                self.open_outside.push((Instant::now() + OPEN_OUTSIDE_WAIT, tag.clone()));
                self.spawn_with(&command, &[(OUTSIDE_ENV, &tag)]);
            }
            Action::CloseWindow => {
                if let Some(window) = self.focused_window()
                    && let Some(toplevel) = window.toplevel()
                {
                    toplevel.send_close();
                    self.closed_by_key = self.window_centre(&window).map(|c| (window.clone(), c));
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
            // the far edge comes in so shrink left pulls the right side over to the left
            Action::Shrink(dir) => self.resize_focused(dir.opposite(), -self.config.step),
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
            Action::GoToOrigin => self.go_to_origin(),
            Action::WindowToOrigin => self.window_to_origin(),
            Action::WorkspaceToOrigin => self.workspace_to_origin(),
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
                if let Some(colors) = &self.color_override {
                    // checked when it came in so this cant fail
                    let _ = self.config.apply_colors(colors);
                }
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
        // w focus_leaves_workspace off a tile only moves between tiles of its own workspace
        let stay_in = current
            .as_ref()
            .filter(|_| !self.config.view.focus_leaves_workspace)
            .and_then(|w| self.ws_of(w));
        let (windows, rects): (Vec<Window>, Vec<_>) = self
            .space
            .elements()
            .filter(|w| Some(*w) != current.as_ref())
            .filter(|w| stay_in.is_none() || self.ws_of(w) == stay_in)
            .filter_map(|w| Some((w.clone(), self.frame(w)?)))
            .unzip();
        let target = crate::layout::nearest(from, dir, &rects).map(|i| windows[i].clone());
        if let Some(window) = target {
            self.focus(Some(&window));
            self.bring_into_view(&window);
        }
    }

    /// the window whose middle is closest to a canvas point
    pub fn nearest_window(&self, from: Point<f64, Logical>) -> Option<Window> {
        self.space
            .elements()
            .filter_map(|w| {
                let c = self.window_centre(w)?;
                Some((w, (c.x - from.x).powi(2) + (c.y - from.y).powi(2)))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(w, _)| w.clone())
    }

    pub fn window_centre(&self, window: &Window) -> Option<Point<f64, Logical>> {
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
        // moved by hand so its not maximized anymore and unmaximize wont jump it back later
        self.drop_maximized(&window);
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

    /// like hyprland the border goes the way of the arrow so its own edge that way or else the one behind it
    fn move_tile_border(&mut self, window: &Window, dir: Direction) {
        let step = self.config.step;
        if !self.resize_tile(window, dir, step) {
            self.resize_tile(window, dir.opposite(), -step);
        }
    }

    /// move a tiles edge out by amount pixels by shifting the split that makes it and says if it had that edge
    fn resize_tile(&mut self, window: &Window, dir: Direction, amount: i32) -> bool {
        let Some(ws) = self.ws_of(window) else {
            return false;
        };
        let tiled = &self.workspaces[ws].tiled;
        let Some(i) = tiled.iter().position(|(w, _)| w == window) else {
            return false;
        };
        let ratios: Vec<f64> = tiled.iter().map(|(_, ratio)| *ratio).collect();
        let gap = self.config.tiling.gaps_inner;
        let Some((split, span)) = layout::dwindle_edge(self.tile_area(ws), &ratios, gap, i, dir)
        else {
            return false;
        };
        let ratio = &mut self.workspaces[ws].tiled[split].1;
        *ratio = (*ratio + amount as f64 / span).clamp(0.1, 0.9);
        self.retile();
        true
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
            // grow and shrink both move the border the way of the arrow in a workspace
            let dir = if amount < 0 { dir.opposite() } else { dir };
            self.move_tile_border(&window, dir);
            return;
        }
        let Some(Rectangle { mut loc, size }) = self.frame(&window) else {
            return;
        };
        // it hasnt drawn yet so theres no size to grow from
        if size.w <= 0 || size.h <= 0 {
            return;
        }
        self.drop_maximized(&window);
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
        // a rect w no size is from a window that hadnt drawn yet so the app picks its own
        let sized = rect.size.w > 0 && rect.size.h > 0;
        let rect = self.content_of(window, rect);
        tracing::debug!(?rect, "resize window");
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| state.size = sized.then_some(rect.size));
            // before its first configure a window gets its size w that configure instead
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
        // a space location is where the windows visible part starts and a mapped window keeps its spot in the stack
        if self.space.element_location(window).is_some() {
            self.space.relocate_element(window, rect.loc);
        } else {
            self.space.map_element(window.clone(), rect.loc, false);
        }
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
        let now = before;
        // a maximized tile stays maximized under the fullscreen and a floating one gets maximized again after
        let was_maximized = !self.is_tiled(window) && self.is_maximized(window);
        let before = self.unmaximized_rect(window).filter(|_| was_maximized).unwrap_or(before);
        // keep who maximize already pushed so they can still go back later
        let mut pushed = window.user_data().get::<Pushed>().map(|p| p.0.take()).unwrap_or_default();
        if was_maximized {
            // no configure of its own bc a browser told its unmaximized right before fullscreen thinks its video left fullscreen
            if let Some(m) = window.user_data().get::<Maximized>() {
                m.0.take();
            }
            toplevel.with_pending_state(|state| state.states.unset(xdg_toplevel::State::Maximized));
        }
        window
            .user_data()
            .get_or_insert(MaximizedBeforeFullscreen::default)
            .0
            .set(was_maximized);
        // fill a screen sized area at the tiles workspace or grow away from the edge the floating window is snapped to and fly there
        let size = self.screen_size();
        // another monitor already showing the tiles workspace is the one it fills so this ones view stays put
        let shown_on = self.ws_of(window).and_then(|i| {
            let at = (self.workspaces[i].rect.loc.to_f64(), 1.0);
            self.monitors
                .iter()
                .find(|m| m.view.destination() == at)
                .filter(|_| self.view.destination() != at)
                .map(|m| crate::monitors::size_of(&m.output))
        });
        let screen = match self.ws_of(window) {
            Some(i) => Rectangle::new(self.workspaces[i].rect.loc, shown_on.unwrap_or(size)),
            None => {
                let screen = self.spot_from(now, size);
                for (other, from, to) in self.push_snapped(window, now, screen) {
                    self.place_frame(&other, to, false);
                    match pushed.iter_mut().find(|(w, _, _)| *w == other) {
                        Some(p) => p.2 = to,
                        None => pushed.push((other, from, to)),
                    }
                }
                screen
            }
        };
        *window.user_data().get_or_insert(Pushed::default).0.borrow_mut() = pushed;
        let view = self.view.destination();
        window
            .user_data()
            .get_or_insert(ViewBeforeFullscreen::default)
            .0
            .set(shown_on.is_none().then_some(view));
        self.fullscreen.retain(|(w, _)| w != window);
        self.fullscreen.push((window.clone(), before));
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = Some(screen.size);
        });
        toplevel.send_pending_configure();
        // it keeps its spot in the stack so whatever floats over it still does
        self.space.relocate_element(window, screen.loc);
        self.cycle = None;
        self.focus_hovered(window);
        self.restack();
        if shown_on.is_none() {
            let duration = std::time::Duration::from_millis(self.config.view.fly_duration_ms);
            self.view.fly_to(screen.loc.to_f64(), 1.0, duration);
        }
    }

    pub fn unfullscreen(&mut self, window: &Window) {
        let Some(i) = self.fullscreen.iter().position(|(w, _)| w == window) else {
            return;
        };
        self.view_back_from_fullscreen(window);
        let (_, before) = self.fullscreen.remove(i);
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.fullscreen_output = None;
            });
        }
        let remaximize = window
            .user_data()
            .get::<MaximizedBeforeFullscreen>()
            .is_some_and(|m| m.0.take());
        if self.is_tiled(window) {
            self.retile();
        } else {
            self.pull_back_pushed(window);
            if remaximize {
                self.maximize_floating(window, before, None);
            } else {
                self.resize_window(window, before);
            }
        }
    }

    /// leave fullscreen without flying the view back
    pub fn unfullscreen_in_place(&mut self, window: &Window) {
        if let Some(v) = window.user_data().get::<ViewBeforeFullscreen>() {
            v.0.take();
        }
        self.unfullscreen(window);
    }

    /// fly back to where the view was before the window went fullscreen but only if ur still looking at it
    pub fn view_back_from_fullscreen(&mut self, window: &Window) {
        let before = window
            .user_data()
            .get::<ViewBeforeFullscreen>()
            .and_then(|v| v.0.take());
        let looking = self
            .space
            .element_location(window)
            .is_some_and(|loc| self.view.destination() == (loc.to_f64(), 1.0));
        if let Some((camera, zoom)) = before
            && looking
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

    /// mod+shift+f maximizes the window or puts it back and shows it at 100% zoom
    pub fn toggle_maximize(&mut self, window: &Window) {
        if self.is_maximized(window) {
            self.unmaximize(window);
            return;
        }
        self.maximize(window);
        if !self.is_maximized(window) {
            return;
        }
        self.overview = None;
        let duration = Duration::from_millis(self.config.view.fly_duration_ms);
        if let Some(i) = self.ws_of(window) {
            self.fly_to_workspace(i);
        } else if let Some(rect) = self.maximized_rect(window) {
            self.view.fly_to(self.camera_for(rect, 1.0), 1.0, duration);
        }
    }

    /// floating windows fill the screen minus panels and tiles fill their workspace
    pub fn maximize(&mut self, window: &Window) {
        self.maximize_into(window, None);
    }

    /// maximize but a floating window can be told exactly where to fill
    pub fn maximize_into(&mut self, window: &Window, area: Option<Rectangle<i32, Logical>>) {
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
        self.maximize_floating(window, before, area);
        self.focus(Some(window));
    }

    /// fill area or the spot around before and remember before as where it goes back to
    fn maximize_floating(
        &mut self,
        window: &Window,
        before: Rectangle<i32, Logical>,
        area: Option<Rectangle<i32, Logical>>,
    ) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let area = area.unwrap_or_else(|| self.maximize_spot(before));
        let pushed = self.push_snapped(window, before, area);
        for (other, _, to) in &pushed {
            self.place_frame(other, *to, false);
        }
        window
            .user_data()
            .get_or_insert(Maximized::default)
            .0
            .set(Some(before));
        *window.user_data().get_or_insert(Pushed::default).0.borrow_mut() = pushed;
        toplevel.with_pending_state(|state| state.states.set(xdg_toplevel::State::Maximized));
        self.resize_window(window, area);
    }

    pub fn unmaximize(&mut self, window: &Window) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let before = window.user_data().get::<Maximized>().and_then(|m| m.0.take());
        if let Some(m) = window.user_data().get::<MaximizedBeforeFullscreen>() {
            m.0.set(false);
        }
        toplevel.with_pending_state(|state| state.states.unset(xdg_toplevel::State::Maximized));
        match before {
            // a tile goes back into its layout
            Some(_) if !self.is_fullscreen(window) && let Some(i) = self.ws_of(window) => {
                self.retile_ws(i);
                self.restack();
            }
            Some(rect) if !self.is_fullscreen(window) => {
                self.resize_window(window, rect);
                self.pull_back_pushed(window);
            }
            _ => {
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
        }
    }

    /// forget a window is maximized but keep its size
    pub fn drop_maximized(&mut self, window: &Window) {
        if let Some(pushed) = window.user_data().get::<Pushed>() {
            pushed.0.take();
        }
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

    /// the rect a floating maximized window is going to fill
    fn maximized_rect(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let before = self.unmaximized_rect(window)?;
        Some(self.maximize_spot(before))
    }

    /// a screen sized rect minus panels placed by spot_from
    fn maximize_spot(&self, before: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        self.spot_from(before, self.maximize_size())
    }

    /// how big a floating maximized window is so the screen minus panels and outer gaps
    pub fn maximize_size(&self) -> Size<i32, Logical> {
        let zone = self.usable_screen();
        let gap = self.config.tiling.gaps_outer;
        Size::from((
            (zone.size.w as i32 - 2 * gap).max(1),
            (zone.size.h as i32 - 2 * gap).max(1),
        ))
    }

    /// a rect of this size that grows away from a workspace edge the window was snapped to or else from its middle
    fn spot_from(&self, before: Rectangle<i32, Logical>, size: Size<i32, Logical>) -> Rectangle<i32, Logical> {
        let snap_gap = self.config.snap.gap;
        let areas = self.ws_areas();
        let stuck = |side: Direction| areas.iter().any(|a| layout::beside(before, *a, side, snap_gap));
        let axis = |pos: i32, len: i32, new_len: i32, low: bool, high: bool| match (low, high) {
            (true, false) => pos,
            (false, true) => pos + len - new_len,
            _ => pos + (len - new_len) / 2,
        };
        let loc = Point::from((
            axis(before.loc.x, before.size.w, size.w, stuck(Direction::Left), stuck(Direction::Right)),
            axis(before.loc.y, before.size.h, size.h, stuck(Direction::Up), stuck(Direction::Down)),
        ));
        let mut rect = Rectangle::new(loc, size);
        if let Some(bounds) = self.bounds() {
            rect.loc = layout::clamp_into(rect, bounds);
        }
        rect
    }

    /// floating windows snapped to the window or to those get pushed out as far as it grows so they stay snapped
    fn push_snapped(
        &self,
        window: &Window,
        before: Rectangle<i32, Logical>,
        after: Rectangle<i32, Logical>,
    ) -> Vec<(Window, Point<i32, Logical>, Point<i32, Logical>)> {
        let gap = self.config.snap.gap;
        let others: Vec<(Window, Rectangle<i32, Logical>)> = self
            .space
            .elements()
            .filter(|w| *w != window && !self.is_tiled(w) && !self.is_fullscreen(w))
            .filter_map(|w| Some((w.clone(), self.frame(w)?)))
            .collect();
        let grow = [
            (Direction::Left, before.loc.x - after.loc.x),
            (Direction::Right, (after.loc.x + after.size.w) - (before.loc.x + before.size.w)),
            (Direction::Up, before.loc.y - after.loc.y),
            (Direction::Down, (after.loc.y + after.size.h) - (before.loc.y + before.size.h)),
        ];
        let mut pushed: Vec<(Window, Point<i32, Logical>, Point<i32, Logical>)> = Vec::new();
        for (side, by) in grow {
            if by <= 0 {
                continue;
            }
            let (dx, dy) = side.delta();
            // spreads thru chains of windows snapped one after another
            let mut edge = vec![before];
            while let Some(from) = edge.pop() {
                for (other, rect) in &others {
                    if pushed.iter().any(|(w, _, _)| w == other) || !layout::beside(from, *rect, side, gap) {
                        continue;
                    }
                    let to = rect.loc + Point::from((dx * by, dy * by));
                    pushed.push((other.clone(), rect.loc, to));
                    edge.push(*rect);
                }
            }
        }
        pushed
    }

    /// windows pushed out by maximize go back if nobody moved them since
    pub fn pull_back_pushed(&mut self, window: &Window) {
        let Some(pushed) = window.user_data().get::<Pushed>().map(|p| p.0.take()) else {
            return;
        };
        for (other, from, to) in pushed {
            if smithay::utils::IsAlive::alive(&other) && !self.is_tiled(&other) && self.frame(&other).is_some_and(|r| r.loc == to) {
                self.place_frame(&other, from, false);
            }
        }
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
        // a floating window picked up by the move starts to wobble from where u grabbed it
        let a = &self.config.animations;
        if a.enabled && a.wobbly && matches!(kind, DragKind::Move) && !lift_tile {
            let at = pointer.current_location();
            let uv = [
                (at.x - initial.loc.x as f64) / initial.size.w.max(1) as f64,
                (at.y - initial.loc.y as f64) / initial.size.h.max(1) as f64,
            ];
            crate::wobbly::grab(&window, uv);
        }
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

    /// whether the window is an x11 one coming thru xwayland-satellite
    pub fn is_satellite(&self, window: &Window) -> bool {
        let Some(satellite) = self.xwayland.as_ref().map(|c| c.id() as i32) else {
            return false;
        };
        window
            .toplevel()
            .and_then(|t| t.wl_surface().client())
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .is_some_and(|creds| creds.pid == satellite)
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
        // a program that also draws panels like the shell would lose all of them and not get this window back
        let client = window.toplevel().and_then(|t| t.wl_surface().client());
        let draws_panels = self.outputs().any(|o| {
            smithay::desktop::layer_map_for_output(o)
                .layers()
                .any(|l| l.wl_surface().client() == client)
        });
        if draws_panels {
            self.notify("This window belongs to the shell so mod+r would restart the whole shell and not bring it back");
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

/// windows maximize pushed out of the way w where they were and where they went
#[derive(Default)]
struct Pushed(std::cell::RefCell<Vec<(Window, Point<i32, Logical>, Point<i32, Logical>)>>);

/// a floating window that was maximized when it went fullscreen
#[derive(Default)]
struct MaximizedBeforeFullscreen(std::cell::Cell<bool>);

/// a window that asked for fullscreen before it was placed
#[derive(Default)]
pub struct WantsFullscreen;

/// a maximized window and the frame it goes back to
#[derive(Default)]
pub struct Maximized(pub std::cell::Cell<Option<Rectangle<i32, Logical>>>);

/// a windows client process id looked up once
struct WindowPid(Option<i32>);
