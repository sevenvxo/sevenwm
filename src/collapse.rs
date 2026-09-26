//! mod+n folds a window into a small marker and clicking the marker brings it back

use std::cell::Cell;
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::layout::Rect;
use crate::state::Seven;

/// marker size on screen which stays the same at any zoom
pub const MARKER_W: i32 = 220;
pub const MARKER_H: i32 = 36;
/// how far a press can wander and still count as a click
const CLICK_SLOP: f64 = 5.0;

pub struct Collapsed {
    pub window: Window,
    /// where the window was
    pub rect: Rect,
    /// the workspace it was tiled in by number
    pub workspace: Option<u32>,
    /// the uhhh marker middle on the canvas
    pub anchor: Point<f64, Logical>,
    pub image: MemoryRenderBuffer,
}

/// when a window last had focus for auto collapse
pub struct LastFocused(pub Cell<Instant>);

pub fn touch(window: &Window) {
    window
        .user_data()
        .get_or_insert(|| LastFocused(Cell::new(Instant::now())))
        .0
        .set(Instant::now());
}

fn last_focused(window: &Window) -> Instant {
    window
        .user_data()
        .get_or_insert(|| LastFocused(Cell::new(Instant::now())))
        .0
        .get()
}

impl Seven {
    pub fn is_collapsed(&self, window: &Window) -> bool {
        self.collapsed.iter().any(|c| c.window == *window)
    }

    /// the most recently focused window thats not collapsed
    pub fn most_recent_window(&self) -> Option<Window> {
        self.history.iter().find(|w| !self.is_collapsed(w)).cloned()
    }

    /// a markers rect on the active monitor
    pub fn marker_rect(&self, anchor: Point<f64, Logical>) -> Rectangle<f64, Logical> {
        let at = self.view.to_screen(anchor);
        Rectangle::new(
            Point::from((at.x - MARKER_W as f64 / 2.0, at.y - MARKER_H as f64 / 2.0)),
            Size::from((MARKER_W as f64, MARKER_H as f64)),
        )
    }

    /// the marker under a screen point topmost first
    pub fn marker_at(&self, screen: Point<f64, Logical>) -> Option<usize> {
        (0..self.collapsed.len())
            .rev()
            .find(|&i| self.marker_rect(self.collapsed[i].anchor).contains(screen))
    }

    /// mod+n collapses the focused window or restores the marker under the pointer
    pub fn toggle_collapse(&mut self) {
        if let Some(i) = self.marker_at(self.pointer_screen) {
            let window = self.collapsed[i].window.clone();
            self.restore(&window);
            return;
        }
        if let Some(window) = self.focused_window() {
            self.collapse(&window);
        }
    }

    pub fn collapse(&mut self, window: &Window) {
        if self.is_collapsed(window) {
            return;
        }
        let Some(rect) = self.frame(window) else {
            return;
        };
        if self.is_fullscreen(window) {
            self.unfullscreen(window);
        }
        let rect = self.frame(window).unwrap_or(rect);
        let workspace = self.ws_of(window).map(|i| self.workspaces[i].number);
        let anchor = match self.ws_of(window) {
            // tiles wait in a row right below their workspace
            Some(i) => {
                let ws = self.workspaces[i].rect;
                let slot = |n: usize| -> Point<f64, Logical> {
                    Point::from((
                        ws.loc.x as f64 + (MARKER_W as f64 + 12.0) * (n as f64 + 0.5),
                        (ws.loc.y + ws.size.h) as f64 + MARKER_H as f64,
                    ))
                };
                // the first spot in the row w no marker
                let taken = |p: Point<f64, Logical>| {
                    self.collapsed.iter().any(|c| {
                        (c.anchor.x - p.x).abs() < MARKER_W as f64
                            && (c.anchor.y - p.y).abs() < MARKER_H as f64
                    })
                };
                let free = (0..).find(|&n| !taken(slot(n))).unwrap_or(0);
                slot(free)
            }
            None => Point::from((
                rect.loc.x as f64 + rect.size.w as f64 / 2.0,
                rect.loc.y as f64 + rect.size.h as f64 / 2.0,
            )),
        };
        self.untile(window);
        self.space.unmap_elem(window);
        window.set_activated(false);
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_pending_configure();
        }
        let image = self.draw_marker(window);
        self.collapsed.push(Collapsed {
            window: window.clone(),
            rect,
            workspace,
            anchor,
            image,
        });
        // give the keyboard to the most recent window still out
        if self.focused_window().is_none_or(|w| w == *window) {
            let next = self.most_recent_window();
            self.focus(next.as_ref());
        }
    }

    /// bring a collapsed window back where it was and focus it
    pub fn restore(&mut self, window: &Window) {
        let Some(i) = self.collapsed.iter().position(|c| c.window == *window) else {
            return;
        };
        let entry = self.collapsed.remove(i);
        self.place_frame(window, entry.rect.loc, true);
        match entry.workspace.and_then(|n| self.ws_index(n)) {
            Some(ws) => self.tile_into(window, ws),
            None => self.resize_window(window, entry.rect),
        }
        self.focus(Some(window));
        self.bring_into_view(window);
    }

    /// the marker is the window title on a small pill
    fn draw_marker(&mut self, window: &Window) -> MemoryRenderBuffer {
        let (title, app_id) = crate::screencast::title_and_app_id(window);
        let mut label = if title.is_empty() { app_id } else { title };
        if label.chars().count() > 24 {
            label = label.chars().take(23).collect::<String>() + "\u{2026}";
        }
        // drawn at the monitor scale rounded up so its prolly sharp
        let scale = self.ui_scale();
        let sf = scale as f32;
        let (w, h) = ((MARKER_W * scale) as usize, (MARKER_H * scale) as usize);
        let theme = &self.config.theme;
        let (background, text, edge) = (theme.menu_background, theme.menu_text, theme.menu_edge);
        let radius = h as f64 / 2.0;
        let mut pixels = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                // a pill w round ends and a one pixel edge
                let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
                let cx = px.clamp(radius, w as f64 - radius);
                let d = ((px - cx).powi(2) + (py - radius).powi(2)).sqrt();
                let coverage = (radius - d + 0.5).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                let color = if d > radius - 1.5 * scale as f64 { edge } else { background };
                let a = color[3] as f64 / 255.0 * coverage;
                // premultiplied bc thats how the renderer blends
                let i = (y * w + x) * 4;
                for c in 0..3 {
                    pixels[i + c] = (color[c] as f64 * a).round() as u8;
                }
                pixels[i + 3] = (a * 255.0).round() as u8;
            }
        }
        if self.font.is_none() {
            self.font = crate::text::Font::system();
        }
        if let Some(font) = &mut self.font {
            let size = 14.0 * sf;
            let x = ((w as f32 - font.width(&label, size)) / 2.0).max(10.0 * sf);
            font.draw(&mut pixels, w, x, h as f32 * 0.66, &label, size, text);
        }
        MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Abgr8888,
            (MARKER_W * scale, MARKER_H * scale),
            scale,
            Transform::Normal,
            None,
        )
    }

    /// a press on a marker where a click restores it and a drag moves it
    pub fn start_marker_drag(&mut self, i: usize, button: u32, serial: smithay::utils::Serial) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let grab = MarkerDrag {
            start_data: GrabStartData {
                focus: None,
                button,
                location: pointer.current_location(),
            },
            window: self.collapsed[i].window.clone(),
            start_screen: self.pointer_screen,
            initial: self.collapsed[i].anchor,
            moved: 0.0,
        };
        pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Clear);
    }

    /// once a sec collapse floating windows left off screen and unfocused for too long
    pub fn auto_collapse(&mut self) {
        let minutes = self.config.collapse.auto_after_minutes;
        if minutes == 0 || self.is_locked() {
            return;
        }
        let limit = Duration::from_secs(minutes * 60);
        let seen = self.visible_on_any_monitor();
        let focused = self.focused_window();
        let stale: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                Some(*w) != focused.as_ref()
                    && !self.is_tiled(w)
                    && !self.is_fullscreen(w)
                    && self.dragging.as_ref() != Some(*w)
                    && last_focused(w).elapsed() >= limit
                    && self
                        .space
                        .element_geometry(w)
                        .is_some_and(|r| !seen.iter().any(|v| v.overlaps(r.to_f64())))
            })
            .cloned()
            .collect();
        for window in stale {
            tracing::info!("collapsing a window left alone for {minutes} min");
            self.collapse(&window);
        }
    }

    /// what each monitor shows of the canvas
    pub fn visible_on_any_monitor(&self) -> Vec<Rectangle<f64, Logical>> {
        let mut seen = vec![self.view.visible(self.screen_size())];
        seen.extend(self.monitors.iter().map(|m| {
            m.view
                .visible(crate::monitors::size_of(&m.output))
        }));
        seen
    }
}

/// dragging a marker or clicking it
struct MarkerDrag {
    start_data: GrabStartData<Seven>,
    window: Window,
    start_screen: Point<f64, Logical>,
    initial: Point<f64, Logical>,
    moved: f64,
}

impl PointerGrab<Seven> for MarkerDrag {
    fn motion(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let d = data.pointer_screen - self.start_screen;
        self.moved = self.moved.max((d.x * d.x + d.y * d.y).sqrt());
        if self.moved < CLICK_SLOP {
            return;
        }
        let zoom = data.view.zoom;
        if let Some(entry) = data.collapsed.iter_mut().find(|c| c.window == self.window) {
            entry.anchor = self.initial + Point::from((d.x / zoom, d.y / zoom));
        }
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

    crate::grabs::forward_gestures!();

    fn start_data(&self) -> &GrabStartData<Seven> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut Seven) {
        // cut short by the lock so leave it
        if data.is_locked() {
            return;
        }
        if self.moved < CLICK_SLOP {
            data.restore(&self.window);
        } else if let Some(entry) = data.collapsed.iter_mut().find(|c| c.window == self.window) {
            // a dragged tile marker left the workspace so it comes back floating there
            if entry.workspace.take().is_some() {
                entry.rect.loc = Point::from((
                    (entry.anchor.x - entry.rect.size.w as f64 / 2.0) as i32,
                    (entry.anchor.y - entry.rect.size.h as f64 / 2.0) as i32,
                ));
            } else {
                let shift = (entry.anchor - self.initial).to_i32_round();
                entry.rect.loc += shift;
            }
        }
    }
}
