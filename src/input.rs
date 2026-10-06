use smithay::backend::input::PointerMotionEvent;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    InputTime, KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
};
use std::time::Duration;

use smithay::backend::session::Session;
use smithay::input::keyboard::{FilterResult, Keysym};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent, RelativeMotionEvent,
};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::RegionAttributes;
use smithay::wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint};

use smithay::wayland::shell::wlr_layer::Layer;

use crate::config::{Action, Mods};
use crate::grabs::{DragKind, Edges, PanGrab};
use crate::state::Seven;

/// linux input event codes
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

impl Seven {
    /// run action over and over while the key stays held at the keyboard repeat rate
    fn start_key_repeat(&mut self, code: u32, action: Action) {
        self.stop_key_repeat();
        let kb = &self.config.input.keyboard;
        if kb.repeat_rate <= 0 {
            return;
        }
        let delay = Duration::from_millis(kb.repeat_delay.max(0) as u64);
        let interval = Duration::from_secs_f64(1.0 / kb.repeat_rate as f64);
        let timer = Timer::from_duration(delay);
        match self.loop_handle.insert_source(timer, move |_, _, state| {
            // only media keys work behind the lock
            if !state.is_locked() || matches!(action, Action::Exec(_)) {
                state.run_action(action.clone());
            }
            TimeoutAction::ToDuration(interval)
        }) {
            Ok(token) => self.key_repeat = Some((code, token)),
            Err(err) => tracing::warn!("key repeat timer: {err}"),
        }
    }

    pub fn stop_key_repeat(&mut self) {
        if let Some((_, token)) = self.key_repeat.take() {
            self.loop_handle.remove(token);
        }
    }

    pub fn handle_input<I: InputBackend>(&mut self, event: InputEvent<I>) {
        self.activity();
        self.damage();
        match event {
            InputEvent::Keyboard { event } => {
                let keyboard = self.seat.get_keyboard().expect("the seat has a keyboard");
                let code = event.key_code().raw();
                let pressed = event.state() == KeyState::Pressed;
                // an intercept w none eats a key without running anything
                let action = keyboard.input::<Option<Action>, _>(
                    self,
                    event.key_code(),
                    event.state(),
                    SERIAL_COUNTER.next_serial(),
                    event.time(),
                    |state, modifiers, keysym| {
                        // letting go of alt ends an alt-tab cycle
                        if !modifiers.alt {
                            state.end_cycle();
                        }
                        if !pressed {
                            if state.key_repeat.as_ref().is_some_and(|(c, _)| *c == code) {
                                state.stop_key_repeat();
                            }
                            // the release of a key that ran a bind is ours too
                            return if state.suppressed_keys.remove(&code) {
                                FilterResult::Intercept(None)
                            } else {
                                FilterResult::Forward
                            };
                        }
                        // our own timer does the repeat thing for a held bind so drop the hosts repeats and any other key stops it
                        if state.key_repeat.as_ref().is_some_and(|(c, _)| *c == code) {
                            return FilterResult::Intercept(None);
                        }
                        state.stop_key_repeat();
                        let Some(key) = keysym.raw_latin_sym_or_raw_current_sym() else {
                            return FilterResult::Forward;
                        };
                        // quit and vt switching always work but not while locked
                        if modifiers.ctrl
                            && modifiers.alt
                            && key == Keysym::BackSpace
                            && !state.is_locked()
                        {
                            tracing::warn!("ctrl+alt+backspace: quitting");
                            state.suppressed_keys.insert(code);
                            return FilterResult::Intercept(Some(Action::Quit));
                        }
                        let vt = keysym.modified_sym().raw();
                        if (Keysym::XF86_Switch_VT_1.raw()..=Keysym::XF86_Switch_VT_12.raw())
                            .contains(&vt)
                            && let Some(session) = &mut state.session
                        {
                            let n = (vt - Keysym::XF86_Switch_VT_1.raw() + 1) as i32;
                            if let Err(err) = session.change_vt(n) {
                                tracing::warn!("switching to VT {n}: {err}");
                            }
                            state.suppressed_keys.insert(code);
                            return FilterResult::Intercept(None);
                        }
                        // media keys work behind the lock and repeat while held
                        let media = (0x1008_FF01..=0x1008_FFFF).contains(&key.raw());
                        if state.is_locked() {
                            // while locked every other key goes to the lock screen
                            let binding = state.config.lookup(Mods::from_state(modifiers), key);
                            return match binding {
                                Some(action @ Action::Exec(_)) if media => {
                                    let action = action.clone();
                                    if state.suppressed_keys.insert(code) {
                                        state.start_key_repeat(code, action.clone());
                                    }
                                    FilterResult::Intercept(Some(action))
                                }
                                _ => FilterResult::Forward,
                            };
                        }
                        // escape closes the window menu
                        if key == Keysym::Escape && state.menu.is_some() {
                            state.suppressed_keys.insert(code);
                            return FilterResult::Intercept(Some(Action::CloseMenu));
                        }
                        // escape leaves the overview
                        if key == Keysym::Escape && state.overview.is_some() {
                            state.suppressed_keys.insert(code);
                            return FilterResult::Intercept(Some(Action::Overview));
                        }
                        match state.config.lookup(Mods::from_state(modifiers), key) {
                            Some(action) => {
                                let action = action.clone();
                                // a held key repeats as more presses and only some binds should act on those
                                let repeat = !state.suppressed_keys.insert(code);
                                if repeat && !action.repeats() && !media {
                                    FilterResult::Intercept(None)
                                } else {
                                    if !repeat && (action.repeats() || media) {
                                        state.start_key_repeat(code, action.clone());
                                    }
                                    FilterResult::Intercept(Some(action))
                                }
                            }
                            None => FilterResult::Forward,
                        }
                    },
                );
                if let Some(Some(action)) = action {
                    self.run_action(action);
                }
            }
            InputEvent::PointerMotion { event } => {
                self.relative_motion::<I>(&event);
                self.focus_under_pointer();
            }
            InputEvent::PointerMotionAbsolute { event } => {
                // nested theres one monitor and the host says where on it
                let screen = self.screen_size();
                let local = event.position_transformed(screen);
                self.set_pointer_global(local + self.active_pos.to_f64());
                self.pointer_moved(event.time());
                self.focus_under_pointer();
            }
            InputEvent::PointerButton { event } => {
                let pointer = self.seat.get_pointer().expect("the seat has a pointer");
                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();
                if event.state() == ButtonState::Pressed
                    && !pointer.is_grabbed()
                    && !self.is_locked()
                    && self.press_starts_something(button, serial)
                {
                    // the press turned into a drag or pan so the client never sees it
                    return;
                }
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: event.state(),
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(self);
                // a mod right click that didnt turn into a resize
                if let Some(window) = self.pending_menu.take() {
                    self.open_menu(crate::menu::Target::Window(window));
                }
            }
            InputEvent::PointerAxis { event } => {
                if self.held_mods().contains(self.config.mod_mods) && !self.is_locked() {
                    self.zoom_by_scroll::<I>(&event);
                    return;
                }
                let pointer = self.seat.get_pointer().expect("the seat has a pointer");
                pointer.axis(self, axis_frame::<I>(&event));
                pointer.frame(self);
            }
            _ => {}
        }
    }
}

impl Seven {
    pub fn held_mods(&self) -> Mods {
        self.seat
            .get_keyboard()
            .map(|k| Mods::from_state(&k.modifier_state()))
            .unwrap_or_default()
    }

    /// tell smithays pointer where it is now thru the camera
    fn pointer_moved(&mut self, time: InputTime) {
        let location = self.view.to_canvas(self.pointer_screen);
        let under = self.surface_under(self.pointer_screen);
        // a games pointer lock kicks in once the pointer is over it
        if let Some((surface, _)) = &under {
            let pointer = self.seat.get_pointer().expect("the seat has a pointer");
            with_pointer_constraint(surface, &pointer, |constraint| {
                if let Some(constraint) = constraint
                    && !constraint.is_active()
                {
                    constraint.activate();
                }
            });
        }
        let pointer = self.seat.get_pointer().expect("the seat has a pointer");
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    /// a mouse moved so send the raw move to the app then move the pointer unless a game locked it prolly
    fn relative_motion<I: InputBackend>(&mut self, event: &I::PointerMotionEvent) {
        let pointer = self.seat.get_pointer().expect("the seat has a pointer");
        let under = self.surface_under(self.pointer_screen);
        pointer.relative_motion(
            self,
            under.clone(),
            &RelativeMotionEvent {
                delta: event.delta(),
                delta_unaccel: event.delta_unaccel(),
                time: event.time(),
            },
        );

        let mut locked = false;
        let mut confined: Option<(WlSurface, Point<f64, Logical>, Option<RegionAttributes>)> = None;
        if let Some((surface, origin)) = &under {
            with_pointer_constraint(surface, &pointer, |constraint| {
                match constraint.as_deref() {
                    Some(PointerConstraint::Locked(_))
                        if constraint.as_ref().is_some_and(|c| c.is_active()) =>
                    {
                        locked = true;
                    }
                    Some(PointerConstraint::Confined(c))
                        if constraint.as_ref().is_some_and(|c| c.is_active()) =>
                    {
                        confined = Some((surface.clone(), *origin, c.region().cloned()));
                    }
                    _ => {}
                }
            });
        }
        if locked {
            pointer.frame(self);
            return;
        }

        let global = self.clamp_to_monitors(self.pointer_global + event.delta());
        let moved = global - self.active_pos.to_f64();
        if let Some((surface, origin, region)) = confined {
            // stay on the confining surface and inside its region
            let canvas = self.view.to_canvas(moved);
            let local = canvas - origin;
            let still_over = self
                .surface_under(moved)
                .is_some_and(|(hit, _)| hit == surface);
            let in_region = region.is_none_or(|r| r.contains(local.to_i32_round()));
            if !still_over || !in_region {
                pointer.frame(self);
                return;
            }
        }
        self.set_pointer_global(global);
        self.pointer_moved(event.time());
    }

    /// the view moved under a still pointer so aim it at whats there now
    pub fn refresh_pointer(&mut self) {
        self.pointer_moved(InputTime::now());
    }

    /// only real mouse moves call this so a flying view never steals focus from a key pick
    fn focus_under_pointer(&mut self) {
        if !self.config.view.focus_follows_mouse
            || self.is_locked()
            || self.cycle.is_some()
            || self.overview.is_some()
            || self.menu.is_some()
            || self.exclusive_layer().is_some()
        {
            return;
        }
        let pointer = self.seat.get_pointer().expect("the seat has a pointer");
        if pointer.is_grabbed() {
            return;
        }
        // a panel or launcher holding the keys keeps them
        let keyboard = self.seat.get_keyboard().expect("the seat has a keyboard");
        let focused = self.focused_window();
        if keyboard.current_focus().is_some() && focused.is_none() {
            return;
        }
        let Some(window) = self.window_under(self.pointer_screen) else {
            return;
        };
        if focused.as_ref() == Some(&window) || self.is_collapsed(&window) {
            return;
        }
        self.focus_hovered(&window);
    }

    /// move the mouse to a newly focused window once the view knows where its going
    pub fn queue_warp(&mut self, window: &Window) {
        // only tiles pull the mouse so floating windows on the canvas leave it alone
        if !self.config.view.mouse_follows_focus || !self.is_tiled(window) {
            return;
        }
        let first = self.warp_pending.is_none();
        self.warp_pending = Some((window.clone(), 30));
        // idle runs after this event so bring_into_view has already picked the camera
        if first {
            self.loop_handle.insert_idle(|state| state.warp_to_focus());
        }
    }

    fn warp_to_focus(&mut self) {
        let Some((window, tries)) = self.warp_pending.take() else {
            return;
        };
        let pointer = self.seat.get_pointer().expect("the seat has a pointer");
        if self.is_locked() || pointer.is_grabbed() || self.focused_window().as_ref() != Some(&window) {
            return;
        }
        let Some(frame) = self.frame(&window) else {
            return;
        };
        // a new window has no size till its first commit so check back in a bit
        if frame.size.w <= 0 || frame.size.h <= 0 {
            if tries > 0 {
                self.warp_pending = Some((window, tries - 1));
                let _ = self.loop_handle.insert_source(
                    Timer::from_duration(Duration::from_millis(16)),
                    |_, _, state| {
                        state.warp_to_focus();
                        TimeoutAction::Drop
                    },
                );
            }
            return;
        }
        let frame = frame.to_f64();
        let to_canvas = |screen: Point<f64, Logical>, (camera, zoom): (Point<f64, Logical>, f64)| {
            Point::from((screen.x / zoom + camera.x, screen.y / zoom + camera.y))
        };
        // the mouse is already on it like after a click so leave it be
        if frame.contains(to_canvas(self.pointer_screen, self.view.destination())) {
            return;
        }
        let centre = frame.loc + Point::from((frame.size.w / 2.0, frame.size.h / 2.0));
        // the monitor that ends up showing it which is this one unless another already does
        let mut screens = vec![(
            self.active_pos.to_f64(),
            self.view.destination(),
            self.screen_size(),
        )];
        screens.extend(self.monitors.iter().map(|m| {
            (m.pos.to_f64(), m.view.destination(), crate::monitors::size_of(&m.output))
        }));
        let (pos, (camera, zoom), _) = screens
            .iter()
            .copied()
            .find(|(_, (camera, zoom), size)| {
                let visible = Rectangle::new(
                    *camera,
                    Size::from((size.w as f64 / zoom, size.h as f64 / zoom)),
                );
                visible.contains(centre)
            })
            .unwrap_or(screens[0]);
        let screen = Point::from(((centre.x - camera.x) * zoom, (centre.y - camera.y) * zoom));
        self.set_pointer_global(pos + screen);
        self.refresh_pointer();
    }

    /// handle focus for a press and start a pan or drag if it asks for one
    fn press_starts_something(&mut self, button: u32, serial: smithay::utils::Serial) -> bool {
        let screen = self.pointer_screen;
        let canvas = self.view.to_canvas(screen);
        let mods = self.held_mods();
        let mod_held = mods.contains(self.config.mod_mods);

        // mod+alt+drag on a workspace moves it and its tiles
        if button == BTN_LEFT
            && mods.contains(self.config.workspaces.drag_mods)
            && let Some(i) = self.workspace_at(canvas)
        {
            self.start_workspace_drag(i, button, serial);
            return true;
        }
        // mod+ctrl+drag pans from anywhere even over a window
        if button == BTN_LEFT && mods.contains(self.config.pan_mods) {
            self.start_pan(button, serial);
            return true;
        }

        let layer = self
            .layer_surface_under(
                screen,
                &[Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background],
            )
            .map(|(layer, ..)| layer);

        let over_panel = self
            .layer_surface_under(screen, &[Layer::Overlay, Layer::Top])
            .is_some();
        // a layer that owns presses here like a panel but a wallpaper cant stop panning
        let layer_owns_press =
            over_panel || layer.as_ref().is_some_and(crate::layers::wants_keyboard);
        // a title bar for its buttons or dragging the window
        if button == BTN_LEFT
            && !over_panel
            && self.overview.is_none()
            && let Some((window, bar_button)) = self.titlebar_at(screen)
        {
            self.press_titlebar(window, bar_button, button, serial);
            return true;
        }

        // a collapsed window marker where a click restores it and a drag moves it
        if button == BTN_LEFT
            && !over_panel
            && self.overview.is_none()
            && let Some(i) = self.marker_at(screen)
        {
            self.start_marker_drag(i, button, serial);
            return true;
        }
        let window = self.window_under(screen);

        // in the overview a plain drag moves a window and a click picks it instead of using it
        if self.overview.is_some()
            && button == BTN_LEFT
            && !mod_held
            && let Some(window) = &window
        {
            if self.is_fullscreen(window) {
                self.leave_overview_to(window);
            } else {
                self.start_drag(window.clone(), DragKind::Move, button, serial);
            }
            return true;
        }

        // click to focus but a panel that doesnt take keys leaves focus alone
        let top_layer = if over_panel { layer.as_ref() } else { None };
        match (top_layer, &layer) {
            (_, Some(layer)) if crate::layers::wants_keyboard(layer) && (over_panel || window.is_none()) => {
                self.focus_layer(layer)
            }
            (Some(_), _) => {}
            _ => self.focus(window.as_ref()),
        }

        // mod+right click opens the window menu and mod+ctrl+right click on empty space opens the workspace menu
        if mod_held
            && button == BTN_RIGHT
            && let Some(window) = &window
            && self.is_tiled(window)
        {
            self.open_menu(crate::menu::Target::Window(window.clone()));
            return true;
        }
        if button == BTN_RIGHT
            && mods.contains(self.config.workspaces.drag_mods)
            && window.is_none()
            && !layer_owns_press
            && let Some(i) = self.workspace_at(canvas)
        {
            let number = self.workspaces[i].number;
            self.open_menu(crate::menu::Target::Workspace(number));
            return true;
        }
        // mod+left drag moves and mod+right drag resizes from the nearest corner
        if mod_held && let Some(window) = window {
            let kind = match button {
                BTN_LEFT => Some(DragKind::Move),
                BTN_RIGHT => self
                    .frame(&window)
                    .map(|rect| DragKind::Resize(Edges::nearest_corner(rect, canvas))),
                _ => None,
            };
            if let Some(kind) = kind {
                self.start_drag(window, kind, button, serial);
                return true;
            }
            return false;
        }

        // a plain drag on empty canvas pans
        if button == BTN_LEFT
            && window.is_none()
            && !layer_owns_press
            && self.config.view.drag_empty_canvas_pans
        {
            self.start_pan(button, serial);
            return true;
        }
        false
    }

    fn start_pan(&mut self, button: u32, serial: smithay::utils::Serial) {
        let pointer = self.seat.get_pointer().expect("the seat has a pointer");
        let grab = PanGrab {
            start_data: GrabStartData {
                focus: None,
                button,
                location: self.view.to_canvas(self.pointer_screen),
            },
            anchor: self.view.to_canvas(self.pointer_screen),
            output: self.output.as_ref().map(|o| o.name()),
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    /// mod+scroll zooms around the cursor one step per notch
    fn zoom_by_scroll<I: InputBackend>(&mut self, event: &I::PointerAxisEvent) {
        let notches = event
            .amount_v120(Axis::Vertical)
            .map(|v| v / 120.0)
            .or_else(|| event.amount(Axis::Vertical).map(|px| px / 15.0))
            .unwrap_or(0.0);
        if notches == 0.0 {
            return;
        }
        let view = &self.config.view;
        // scrolling up zooms in
        let factor = view.zoom_step.powf(-notches);
        let (min, max) = (view.zoom_min, view.zoom_max);
        self.view.zoom_at(self.pointer_screen, factor, min, max);
        self.refresh_pointer();
    }
}

/// turn one scroll event into the frame clients expect
fn axis_frame<I: InputBackend>(event: &I::PointerAxisEvent) -> AxisFrame {
    let source = event.source();
    let mut frame = AxisFrame::new(event.time()).source(source);
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let v120 = event.amount_v120(axis);
        // wheels without smooth scrolling only send v120 clicks of 120 per notch and 15 px each
        let amount = event
            .amount(axis)
            .unwrap_or_else(|| v120.unwrap_or(0.0) * 15.0 / 120.0);
        if amount != 0.0 {
            frame = frame.value(axis, amount);
            if let Some(v120) = v120 {
                frame = frame.v120(axis, v120 as i32);
            }
        }
        if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
            frame = frame.stop(axis);
        }
    }
    frame
}
