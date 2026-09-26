//! optional title bars drawn by sevenwm for apps that let us and a frame is content plus bar

use std::cell::RefCell;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

use crate::layout::Rect;
use crate::state::Seven;

/// a title bar button right to left
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Close,
    Collapse,
    Float,
}

const BUTTONS: [Button; 3] = [Button::Close, Button::Collapse, Button::Float];

/// a drawn bar and everything it was drawn from
type DrawnKey = (String, bool, i32, bool, i32, [[u8; 4]; 3], i32, i32);

struct Drawn {
    key: DrawnKey,
    image: MemoryRenderBuffer,
}

impl Seven {
    /// the decoration mode sevenwm uhhh wants
    fn wanted_mode(&self) -> Mode {
        if self.config.decorations.titlebar {
            Mode::ServerSide
        } else {
            Mode::ClientSide
        }
    }

    /// whether the window gets a sevenwm title bar
    pub fn has_titlebar(&self, window: &Window) -> bool {
        self.config.decorations.titlebar
            && !self.is_fullscreen(window)
            && window.toplevel().is_some_and(|t| {
                t.with_pending_state(|s| s.decoration_mode) == Some(Mode::ServerSide)
            })
    }

    pub fn titlebar_height(&self, window: &Window) -> i32 {
        if self.has_titlebar(window) {
            self.config.decorations.titlebar_height
        } else {
            0
        }
    }

    /// the window frame on the canvas w its content and bar
    pub fn frame(&self, window: &Window) -> Option<Rect> {
        let geo = self.space.element_geometry(window)?;
        let h = self.titlebar_height(window);
        Some(Rectangle::new(
            geo.loc - Point::from((0, h)),
            Size::from((geo.size.w, geo.size.h + h)),
        ))
    }

    /// put the frames top left at loc
    pub fn place_frame(&mut self, window: &Window, loc: Point<i32, Logical>, activate: bool) {
        let h = self.titlebar_height(window);
        self.space
            .map_element(window.clone(), loc + Point::from((0, h)), activate);
    }

    /// the content rect inside a frame rect
    pub fn content_of(&self, window: &Window, frame: Rect) -> Rect {
        let h = self.titlebar_height(window);
        Rectangle::new(
            frame.loc + Point::from((0, h)),
            Size::from((frame.size.w, (frame.size.h - h).max(1))),
        )
    }

    /// tell every window which decorations to draw
    pub fn apply_decoration_mode(&mut self) {
        let mode = self.wanted_mode();
        let windows: Vec<Window> = self
            .space
            .elements()
            .cloned()
            .chain(self.collapsed.iter().map(|c| c.window.clone()))
            .collect();
        for window in windows {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let frame = self.frame(&window);
            let changed = toplevel.with_pending_state(|s| {
                let changed = s.decoration_mode.is_some() && s.decoration_mode != Some(mode);
                if changed {
                    s.decoration_mode = Some(mode);
                }
                changed
            });
            if changed {
                // keep the frame where it was and the content moves under the bar
                if let Some(frame) = frame
                    && self.space.element_geometry(&window).is_some()
                    && !self.is_tiled(&window)
                    && !self.is_fullscreen(&window)
                {
                    self.resize_window(&window, frame);
                }
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
        }
        self.retile();
    }

    /// the bar rect on the active screen if the window has one
    pub fn titlebar_rect(&self, window: &Window, offset: Point<f64, Logical>) -> Option<Rectangle<f64, Logical>> {
        if !self.has_titlebar(window) {
            return None;
        }
        let frame = self.frame(window)?;
        let zoom = self.view.zoom;
        let h = self.config.decorations.titlebar_height as f64;
        Some(Rectangle::new(
            self.view.to_screen(frame.loc.to_f64() + offset),
            Size::from((frame.size.w as f64 * zoom, h * zoom)),
        ))
    }

    /// the window whose bar is under a point and which button unless something covers it
    pub fn titlebar_at(&self, screen: Point<f64, Logical>) -> Option<(Window, Option<Button>)> {
        let canvas = self.view.to_canvas(screen);
        for window in self.space.elements().rev() {
            if let Some(bar) = self.titlebar_rect(window, Point::default())
                && bar.contains(screen)
            {
                let size = bar.size.h;
                let from_right = bar.loc.x + bar.size.w - screen.x;
                let button = BUTTONS.get((from_right / size) as usize).copied();
                return Some((window.clone(), button));
            }
            // this windows content is on top here
            if self
                .space
                .element_geometry(window)
                .is_some_and(|g| g.to_f64().contains(canvas))
            {
                return None;
            }
        }
        None
    }

    /// a press on a bar where a button acts and anywhere else starts a move
    pub fn press_titlebar(
        &mut self,
        window: Window,
        button: Option<Button>,
        pointer_button: u32,
        serial: smithay::utils::Serial,
    ) {
        match button {
            Some(Button::Close) => {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
            }
            Some(Button::Collapse) => self.collapse(&window),
            Some(Button::Float) => self.toggle_floating(&window),
            None => {
                self.focus(Some(&window));
                self.start_drag(window, crate::grabs::DragKind::Move, pointer_button, serial);
            }
        }
    }

    /// the bar image at zoom 1 w round top corners the title and buttons
    pub fn titlebar_image(&mut self, window: &Window, focused: bool) -> Option<MemoryRenderBuffer> {
        let frame = self.frame(window)?;
        let (title, app_id) = crate::screencast::title_and_app_id(window);
        let title = if title.is_empty() { app_id } else { title };
        let tiled = self.is_tiled(window);
        let deco = &self.config.decorations;
        let key = (
            title.clone(),
            focused,
            frame.size.w,
            tiled,
            deco.titlebar_height,
            [deco.titlebar_focused, deco.titlebar_unfocused, deco.titlebar_text],
            deco.corner_radius,
            self.ui_scale(),
        );
        let cache = window
            .user_data()
            .get_or_insert(|| RefCell::new(None::<Drawn>));
        if let Some(drawn) = cache.borrow().as_ref()
            && drawn.key == key
        {
            return Some(drawn.image.clone());
        }

        let deco = &self.config.decorations;
        // drawn at the monitor scale rounded up so its sharp
        let scale = self.ui_scale();
        let sf = scale as f32;
        let (w, h) = (
            (frame.size.w.max(1) * scale) as usize,
            (deco.titlebar_height.max(1) * scale) as usize,
        );
        let background = if focused {
            deco.titlebar_focused
        } else {
            deco.titlebar_unfocused
        };
        let text = deco.titlebar_text;
        // never wider than half the bar bc a window that hasnt sized itself can be a pixel wide
        let radius = (deco.corner_radius as f64 * scale as f64)
            .min(h as f64)
            .min(w as f64 / 2.0);
        let mut pixels = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                // round top corners w antialiasing and a square bottom
                let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
                let cx = px.clamp(radius, w as f64 - radius);
                let coverage = if py < radius && (px < radius || px > w as f64 - radius) {
                    let d = ((px - cx).powi(2) + (py - radius).powi(2)).sqrt();
                    (radius - d + 0.5).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let a = background[3] as f64 / 255.0 * coverage;
                let i = (y * w + x) * 4;
                for c in 0..3 {
                    pixels[i + c] = (background[c] as f64 * a).round() as u8;
                }
                pixels[i + 3] = (a * 255.0).round() as u8;
            }
        }
        // buttons from the right are close then collapse then float or tile
        let hf = h as f64;
        let arm = hf * 0.16;
        for (i, button) in BUTTONS.iter().enumerate() {
            let (cx, cy) = (w as f64 - (i as f64 + 0.5) * hf, hf / 2.0);
            let mut line = |a: (f64, f64), b: (f64, f64)| {
                stroke(&mut pixels, w, h, (cx + a.0, cy + a.1), (cx + b.0, cy + b.1), text);
            };
            match button {
                Button::Close => {
                    line((-arm, -arm), (arm, arm));
                    line((-arm, arm), (arm, -arm));
                }
                Button::Collapse => line((-arm, arm * 0.6), (arm, arm * 0.6)),
                Button::Float => {
                    line((-arm, -arm), (arm, -arm));
                    line((arm, -arm), (arm, arm));
                    line((arm, arm), (-arm, arm));
                    line((-arm, arm), (-arm, -arm));
                    if tiled {
                        line((-arm, -arm * 0.4), (arm, -arm * 0.4));
                    }
                }
            }
        }
        if self.font.is_none() {
            self.font = crate::text::Font::system();
        }
        if let Some(font) = &mut self.font {
            let size = (h as f32 * 0.5).max(8.0);
            let baseline = h as f32 * 0.68;
            // the title centered in the space the buttons leave and cut to fit
            let room = w as f32 - 3.0 * h as f32 - 24.0 * sf;
            let mut chars: Vec<char> = title.chars().collect();
            let mut label = title;
            while font.width(&label, size) > room && !chars.is_empty() {
                chars.pop();
                label = chars.iter().collect::<String>() + "\u{2026}";
            }
            if font.width(&label, size) > room {
                label.clear();
            }
            let x = ((w as f32 - 3.0 * h as f32 - font.width(&label, size)) / 2.0).max(12.0 * sf);
            font.draw(&mut pixels, w, x, baseline, &label, size, text);
        }
        let image = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Abgr8888,
            (w as i32, h as i32),
            scale,
            Transform::Normal,
            None,
        );
        *cache.borrow_mut() = Some(Drawn {
            key,
            image: image.clone(),
        });
        Some(image)
    }
}

/// a 1.5 px antialiased line in straight rgba color blended over premultiplied pixels
fn stroke(pixels: &mut [u8], w: usize, h: usize, a: (f64, f64), b: (f64, f64), color: [u8; 4]) {
    let half = 0.75;
    let (x0, x1) = (a.0.min(b.0) - 2.0, a.0.max(b.0) + 2.0);
    let (y0, y1) = (a.1.min(b.1) - 2.0, a.1.max(b.1) + 2.0);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = (dx * dx + dy * dy).max(1e-9);
    for y in (y0.max(0.0) as usize)..(y1.min(h as f64) as usize) {
        for x in (x0.max(0.0) as usize)..(x1.min(w as f64) as usize) {
            let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
            let t = (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0);
            let d = ((px - a.0 - t * dx).powi(2) + (py - a.1 - t * dy).powi(2)).sqrt();
            let coverage = (half + 0.5 - d).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let alpha = color[3] as f64 / 255.0 * coverage;
            let i = (y * w + x) * 4;
            for c in 0..3 {
                let over = color[c] as f64 * alpha;
                pixels[i + c] = (over + pixels[i + c] as f64 * (1.0 - alpha)).round() as u8;
            }
            pixels[i + 3] = (alpha * 255.0 + pixels[i + 3] as f64 * (1.0 - alpha)).round() as u8;
        }
    }
}

impl XdgDecorationHandler for Seven {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        let mode = self.wanted_mode();
        toplevel.with_pending_state(|s| s.decoration_mode = Some(mode));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    /// sevenwm decides no matter what the app wants
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        self.new_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.new_decoration(toplevel);
    }
}
