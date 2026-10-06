//! moving and resizing a window by dragging and the grab owns the pointer till the button goes up

use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Point, Rectangle, Size};

use crate::layout;
use crate::state::Seven;

/// how far a press can wander and still count as a click
const CLICK_SLOP: f64 = 5.0;

/// smallest size a drag can shrink a window to
const MIN_SIZE: i32 = 100;

/// which edges a resize drags while the other ones stay put
#[derive(Clone, Copy, Debug, Default)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl From<xdg_toplevel::ResizeEdge> for Edges {
    fn from(edge: xdg_toplevel::ResizeEdge) -> Self {
        use xdg_toplevel::ResizeEdge as E;
        Self {
            left: matches!(edge, E::Left | E::TopLeft | E::BottomLeft),
            right: matches!(edge, E::Right | E::TopRight | E::BottomRight),
            top: matches!(edge, E::Top | E::TopLeft | E::TopRight),
            bottom: matches!(edge, E::Bottom | E::BottomLeft | E::BottomRight),
        }
    }
}

impl Edges {
    /// the corner nearest point so grabbing near a corner uhhh drags that corner
    pub fn nearest_corner(rect: Rectangle<i32, Logical>, point: Point<f64, Logical>) -> Self {
        let centre_x = rect.loc.x as f64 + rect.size.w as f64 / 2.0;
        let centre_y = rect.loc.y as f64 + rect.size.h as f64 / 2.0;
        Self {
            left: point.x < centre_x,
            right: point.x >= centre_x,
            top: point.y < centre_y,
            bottom: point.y >= centre_y,
        }
    }
}

pub enum DragKind {
    Move,
    Resize(Edges),
}

pub struct WindowDrag {
    pub start_data: GrabStartData<Seven>,
    pub window: Window,
    /// the window rect when the drag started
    pub initial: Rectangle<i32, Logical>,
    pub kind: DragKind,
    /// the button whose release ends the drag
    pub button: u32,
    /// let go without really moving means it was a click so open the window menu
    pub menu_on_click: bool,
    /// a tile that stays in the workspace till the drag maybe moves past a click
    pub lift_tile: bool,
    /// the furthest the pointer got from where the drag started
    pub moved: f64,
}

impl WindowDrag {
    fn still_draggable(&self, state: &Seven) -> bool {
        self.window.alive() && !state.is_collapsed(&self.window) && !state.is_fullscreen(&self.window)
    }

    fn apply(&self, state: &mut Seven, pointer: Point<f64, Logical>) {
        // closed collapsed or fullscreened mid drag so moving it would put it back on the canvas
        if !self.still_draggable(state) {
            return;
        }
        let delta = (pointer - self.start_data.location).to_i32_round::<i32>();
        match self.kind {
            DragKind::Move => {
                let mut rect = Rectangle::new(self.initial.loc + delta, self.initial.size);
                // an always on top window cant tile so it js floats over the workspace
                state.drop_target = state
                    .workspace_at(pointer)
                    .filter(|_| !crate::menu::always_on_top(&self.window))
                    .map(|i| state.workspaces[i].number);
                // over a workspace the window is about to tile so no snapping
                if state.config.snap.enabled && state.drop_target.is_none() {
                    let mut targets = state.floating_rects(Some(&self.window));
                    targets.extend(state.ws_areas());
                    rect.loc = layout::snap(
                        rect,
                        &targets,
                        state.config.snap.gap,
                        state.config.snap.threshold,
                    );
                }
                if let Some(bounds) = state.bounds() {
                    rect.loc = layout::clamp_into(rect, bounds);
                }
                state.place_frame(&self.window, rect.loc, true);
            }
            DragKind::Resize(edges) => {
                let Rectangle { loc, size } = self.initial;
                let dw = if edges.left {
                    -delta.x
                } else if edges.right {
                    delta.x
                } else {
                    0
                };
                let dh = if edges.top {
                    -delta.y
                } else if edges.bottom {
                    delta.y
                } else {
                    0
                };
                let new_size =
                    Size::from(((size.w + dw).max(MIN_SIZE), (size.h + dh).max(MIN_SIZE)));
                // dragging a left or top edge moves the window so the other edge stays
                let new_loc = Point::from((
                    if edges.left {
                        loc.x + size.w - new_size.w
                    } else {
                        loc.x
                    },
                    if edges.top {
                        loc.y + size.h - new_size.h
                    } else {
                        loc.y
                    },
                ));
                let mut rect = Rectangle::new(new_loc, new_size);
                if let Some(bounds) = state.bounds() {
                    rect = rect.intersection(bounds).unwrap_or(rect);
                }
                state.resize_window(&self.window, rect);
            }
        }
    }
}

/// every gesture handler a grab needs each made by how
macro_rules! gesture_handlers {
    ($how:ident) => {
        crate::grabs::$how!(gesture_swipe_begin, GestureSwipeBeginEvent);
        crate::grabs::$how!(gesture_swipe_update, GestureSwipeUpdateEvent);
        crate::grabs::$how!(gesture_swipe_end, GestureSwipeEndEvent);
        crate::grabs::$how!(gesture_pinch_begin, GesturePinchBeginEvent);
        crate::grabs::$how!(gesture_pinch_update, GesturePinchUpdateEvent);
        crate::grabs::$how!(gesture_pinch_end, GesturePinchEndEvent);
        crate::grabs::$how!(gesture_hold_begin, GestureHoldBeginEvent);
        crate::grabs::$how!(gesture_hold_end, GestureHoldEndEvent);
    };
}

macro_rules! forward_gesture {
    ($name:ident, $event:ident) => {
        fn $name(
            &mut self,
            data: &mut Seven,
            handle: &mut PointerInnerHandle<'_, Seven>,
            event: &smithay::input::pointer::$event,
        ) {
            handle.$name(data, event);
        }
    };
}

macro_rules! swallow_gesture {
    ($name:ident, $event:ident) => {
        fn $name(
            &mut self,
            _: &mut Seven,
            _: &mut PointerInnerHandle<'_, Seven>,
            _: &smithay::input::pointer::$event,
        ) {
        }
    };
}

/// every gesture passes straight thru a grab
macro_rules! forward_gestures {
    () => {
        crate::grabs::gesture_handlers!(forward_gesture);
    };
}

/// every gesture stops at the grab and never reaches a client
macro_rules! swallow_gestures {
    () => {
        crate::grabs::gesture_handlers!(swallow_gesture);
    };
}
pub(crate) use {forward_gesture, forward_gestures, gesture_handlers, swallow_gesture, swallow_gestures};

impl PointerGrab<Seven> for WindowDrag {
    fn motion(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // no client gets pointer focus mid drag
        handle.motion(data, None, event);
        // in screen pixels so a click is a click at any zoom
        let d = event.location - self.start_data.location;
        self.moved = self
            .moved
            .max((d.x * d.x + d.y * d.y).sqrt() * data.view.zoom);
        if (self.menu_on_click || self.lift_tile) && self.moved < CLICK_SLOP {
            return;
        }
        if std::mem::take(&mut self.lift_tile) && self.still_draggable(data) {
            data.untile(&self.window);
            data.space.raise_element(&self.window, true);
            data.restack();
        }
        self.apply(data, event.location);
    }

    fn relative_motion(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&self.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut Seven, handle: &mut PointerInnerHandle<'_, Seven>) {
        handle.frame(data);
    }

    forward_gestures!();

    fn start_data(&self) -> &GrabStartData<Seven> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut Seven) {
        data.dragging = None;
        crate::wobbly::release(&self.window);
        match self.kind {
            DragKind::Move => {
                if let Some(number) = data.drop_target.take()
                    && self.still_draggable(data)
                    && let Some(i) = data.ws_index(number)
                {
                    let at = data.view.to_canvas(data.pointer_screen);
                    data.tile_sized(&self.window, i, Some(at));
                    data.focus(Some(&self.window));
                }
                // in the overview a click without really dragging picks the window
                if self.moved < CLICK_SLOP && data.overview.is_some() && self.window.alive() {
                    data.leave_overview_to(&self.window);
                }
            }
            DragKind::Resize(_) => {
                if self.menu_on_click && self.moved < CLICK_SLOP {
                    data.pending_menu = Some(self.window.clone());
                }
                if let Some(toplevel) = self.window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.states.unset(xdg_toplevel::State::Resizing);
                    });
                    toplevel.send_pending_configure();
                }
            }
        }
    }
}

/// dragging the canvas itself so the view follows the pointer
pub struct PanGrab {
    pub start_data: GrabStartData<Seven>,
    /// the canvas point u grabbed which stays under the pointer even while zooming
    pub anchor: Point<f64, Logical>,
    /// the monitor being panned and the pan stops while the pointer is on another one
    pub output: Option<String>,
}

impl PointerGrab<Seven> for PanGrab {
    fn motion(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        if data.output.as_ref().map(|o| o.name()) != self.output {
            handle.motion(data, None, event);
            return;
        }
        // use the screen position bc the canvas one smithay gives shifts w the camera
        let zoom = data.view.zoom;
        let screen = data.pointer_screen;
        let camera = self.anchor - Point::from((screen.x / zoom, screen.y / zoom));
        data.view.set(camera, zoom);
        let location = data.view.to_canvas(data.pointer_screen);
        handle.motion(data, None, &MotionEvent { location, ..*event });
    }

    fn relative_motion(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut Seven, handle: &mut PointerInnerHandle<'_, Seven>) {
        handle.frame(data);
    }

    forward_gestures!();

    fn start_data(&self) -> &GrabStartData<Seven> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut Seven) {}
}
