//! sevenwms own menus drawn as one small image
//! the window menu on mod+right click w close hide always on top volume and tile or float
//! the workspace menu on mod+ctrl+right click to renumber make or remove one

use std::cell::Cell;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Point, Rectangle, Size, Transform};

use crate::audio::{self, Streams};
use crate::state::Seven;
use crate::text::Font;
use crate::workspaces::MAX_NUMBER;

const WIDTH: i32 = 250;
const ITEM_HEIGHT: i32 = 30;
const PADDING: i32 = 6;
const FONT_SIZE: f32 = 15.0;
const VOLUME_STEP: u32 = 5;
/// workspace numbers per row of the number picker
const PER_ROW: u32 = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    Close,
    HideFromScreencast,
    AlwaysOnTop,
    Volume,
    ReturnToRegion,
    Float,
    Collapse,
    /// opens the tiles workspace menu beside this one
    WorkspaceMenu,
    /// the workspace heading
    Heading,
    /// a row of workspace numbers starting at this one
    Numbers(u32),
    NewWorkspace,
    RemoveWorkspace,
}

const WINDOW_ITEMS: [Item; 8] = [
    Item::Close,
    Item::HideFromScreencast,
    Item::AlwaysOnTop,
    Item::Volume,
    Item::ReturnToRegion,
    Item::Float,
    Item::Collapse,
    Item::WorkspaceMenu,
];

const WORKSPACE_ITEMS: [Item; 5] = [
    Item::Heading,
    Item::Numbers(1),
    Item::Numbers(1 + PER_ROW),
    Item::NewWorkspace,
    Item::RemoveWorkspace,
];

/// what a menu is for
#[derive(Clone, PartialEq)]
pub enum Target {
    Window(Window),
    /// a workspace by number
    Workspace(u32),
}

/// marks a window hidden from screen captures
#[derive(Default)]
pub struct HiddenFromCapture(pub Cell<bool>);

pub fn hidden_from_capture(window: &Window) -> bool {
    window
        .user_data()
        .get::<HiddenFromCapture>()
        .is_some_and(|h| h.0.get())
}

/// marks a window that stays above the others and cant be tiled
#[derive(Default)]
pub struct AlwaysOnTop(pub Cell<bool>);

pub fn always_on_top(window: &Window) -> bool {
    window
        .user_data()
        .get::<AlwaysOnTop>()
        .is_some_and(|t| t.0.get())
}

pub struct Menu {
    pub target: Target,
    /// top left on screen on the monitor it opened on
    pub pos: Point<i32, Logical>,
    pub output: String,
    /// the row under the pointer and the cell in it
    hover: Option<(usize, u32)>,
    streams: Streams,
    /// the drawn menu rebuilt when it changes
    pub image: MemoryRenderBuffer,
}

impl Menu {
    pub fn window(&self) -> Option<&Window> {
        match &self.target {
            Target::Window(window) => Some(window),
            Target::Workspace(_) => None,
        }
    }

    fn items(&self) -> &'static [Item] {
        match self.target {
            Target::Window(_) => &WINDOW_ITEMS,
            Target::Workspace(_) => &WORKSPACE_ITEMS,
        }
    }

    fn size(&self) -> Size<i32, Logical> {
        Size::from((WIDTH, self.items().len() as i32 * ITEM_HEIGHT + 2 * PADDING))
    }
}

/// which cell of a number row x lands in
fn cell_at(x: f64) -> u32 {
    ((x - 2.0).max(0.0) / ((WIDTH - 4) as f64 / PER_ROW as f64)).min(PER_ROW as f64 - 1.0) as u32
}

impl Seven {
    /// open a menu at the pointer
    pub fn open_menu(&mut self, target: Target) {
        let streams = match &target {
            Target::Window(window) if !window.alive() => return,
            Target::Window(window) => self.window_streams(window),
            Target::Workspace(_) => Streams {
                indices: Vec::new(),
                percent: None,
            },
        };
        let mut menu = Menu {
            target,
            pos: Point::default(),
            output: self.output.as_ref().map(|o| o.name()).unwrap_or_default(),
            hover: None,
            streams,
            image: MemoryRenderBuffer::new(Fourcc::Abgr8888, (1, 1), 1, Transform::Normal, None),
        };
        // at the pointer but kept on screen
        let screen = self.screen_size();
        let size = menu.size();
        let at = self.pointer_screen.to_i32_round::<i32>();
        menu.pos = Point::from((
            at.x.min(screen.w - size.w).max(0),
            at.y.min(screen.h - size.h).max(0),
        ));
        self.draw_menu(&mut menu);
        self.submenu = None;
        let already_open = self.menu.replace(menu).is_some();
        if already_open {
            // switching menus so the grab stays
            return;
        }

        let pointer = self.seat.get_pointer().expect("the seat has a pointer");
        let grab = MenuGrab {
            start_data: GrabStartData {
                focus: None,
                button: 0,
                location: pointer.current_location(),
            },
        };
        pointer.set_grab(
            self,
            grab,
            smithay::utils::SERIAL_COUNTER.next_serial(),
            smithay::input::pointer::Focus::Clear,
        );
    }

    /// the audio streams a windows app is playing
    fn window_streams(&self, window: &Window) -> Streams {
        let pid = window
            .toplevel()
            .and_then(|t| t.wl_surface().client())
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .map(|creds| creds.pid);
        // x11 apps all share xwayland-satellite so find their audio by name
        let from_xwayland = pid.is_some() && pid == self.xwayland.as_ref().map(|c| c.id() as i32);
        let app_id = crate::screencast::title_and_app_id(window).1;
        let owner = if from_xwayland {
            audio::Owner::Name(app_id)
        } else {
            pid.map_or(audio::Owner::Name(app_id), audio::Owner::Process)
        };
        audio::streams_for(owner)
    }

    /// close the menu and give the pointer back
    pub fn close_menu(&mut self) {
        self.submenu = None;
        if self.menu.take().is_none() {
            return;
        }
        let pointer = self.seat.get_pointer().expect("the seat has a pointer");
        let time = smithay::backend::input::InputTime::now();
        pointer.unset_grab(self, smithay::utils::SERIAL_COUNTER.next_serial(), time);
        self.refresh_pointer();
    }

    /// which menu is under a point and the row and cell there
    fn menu_item_at(&self, screen: Point<f64, Logical>) -> Option<(bool, usize, u32)> {
        if let Some((row, cell)) = self.submenu.as_ref().and_then(|m| self.item_in(m, screen)) {
            return Some((true, row, cell));
        }
        let (row, cell) = self.menu.as_ref().and_then(|m| self.item_in(m, screen))?;
        Some((false, row, cell))
    }

    /// the row and cell of menu under a point
    fn item_in(&self, menu: &Menu, screen: Point<f64, Logical>) -> Option<(usize, u32)> {
        // the pointer is on another monitor
        if self.output.as_ref().map(|o| o.name()).as_deref() != Some(menu.output.as_str()) {
            return None;
        }
        let local = screen - menu.pos.to_f64();
        if local.x < 0.0 || local.x >= WIDTH as f64 {
            return None;
        }
        let row = (local.y - PADDING as f64) / ITEM_HEIGHT as f64;
        let row = (row >= 0.0 && (row as usize) < menu.items().len()).then_some(row as usize)?;
        let cell = match menu.items()[row] {
            Item::Numbers(_) => cell_at(local.x),
            _ => 0,
        };
        Some((row, cell))
    }

    fn menu_hover(&mut self) {
        let hit = self.menu_item_at(self.pointer_screen);
        // in the submenu its row in the menu stays lit
        let main = match hit {
            Some((false, row, cell)) => Some((row, cell)),
            Some((true, ..)) => self.menu.as_ref().and_then(|m| m.hover),
            None => None,
        };
        let sub = match hit {
            Some((true, row, cell)) => Some((row, cell)),
            _ => None,
        };
        self.set_menu_hover(false, main);
        self.set_menu_hover(true, sub);
        // hovering workspace opens its menu and another row closes it
        if let Some((false, row, _)) = hit {
            let number = self
                .menu
                .as_ref()
                .filter(|m| {
                    m.items()[row] == Item::WorkspaceMenu
                        && self.menu_item_enabled(Item::WorkspaceMenu, m)
                })
                .and_then(|m| self.menu_workspace(m))
                .map(|i| self.workspaces[i].number);
            match number {
                Some(number) => self.open_submenu(number, row),
                None => self.submenu = None,
            }
        }
    }

    fn take_menu(&mut self, sub: bool) -> Option<Menu> {
        if sub { self.submenu.take() } else { self.menu.take() }
    }

    fn put_menu(&mut self, sub: bool, menu: Menu) {
        if sub {
            self.submenu = Some(menu);
        } else {
            self.menu = Some(menu);
        }
    }

    fn set_menu_hover(&mut self, sub: bool, hover: Option<(usize, u32)>) {
        if let Some(mut menu) = self.take_menu(sub) {
            if menu.hover != hover {
                menu.hover = hover;
                self.draw_menu(&mut menu);
            }
            self.put_menu(sub, menu);
        }
    }

    /// workspace numbers menu next to row on the right or left if no room
    fn open_submenu(&mut self, number: u32, row: usize) {
        let target = Target::Workspace(number);
        if self.submenu.as_ref().is_some_and(|m| m.target == target) {
            return;
        }
        let Some(main) = self.menu.as_ref() else {
            return;
        };
        let mut menu = Menu {
            target,
            pos: Point::default(),
            output: main.output.clone(),
            hover: None,
            streams: Streams {
                indices: Vec::new(),
                percent: None,
            },
            image: MemoryRenderBuffer::new(Fourcc::Abgr8888, (1, 1), 1, Transform::Normal, None),
        };
        let screen = self.screen_size();
        let size = menu.size();
        let right = main.pos.x + WIDTH - 2;
        let x = if right + size.w <= screen.w {
            right
        } else {
            (main.pos.x - size.w + 2).max(0)
        };
        let y = main.pos.y + row as i32 * ITEM_HEIGHT;
        menu.pos = Point::from((x, y.min(screen.h - size.h).max(0)));
        self.draw_menu(&mut menu);
        self.submenu = Some(menu);
    }

    /// a click that returns whether the menu should close
    fn menu_click(&mut self) -> bool {
        let Some((sub, index, cell)) = self.menu_item_at(self.pointer_screen) else {
            // outside the menus so js uhhh close them
            return true;
        };
        let menu = if sub { self.submenu.as_ref() } else { self.menu.as_ref() };
        let Some(menu) = menu else {
            return true;
        };
        let item = menu.items()[index];
        if !self.menu_item_enabled(item, menu) {
            return false;
        }
        let target = menu.target.clone();
        let workspace = self.menu_workspace(menu);
        match (item, target) {
            (Item::Close, Target::Window(window)) => {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
            }
            (Item::HideFromScreencast, Target::Window(window)) => {
                let flag = window.user_data().get_or_insert(HiddenFromCapture::default);
                flag.0.set(!flag.0.get());
            }
            (Item::AlwaysOnTop, Target::Window(window)) => {
                let flag = window.user_data().get_or_insert(AlwaysOnTop::default);
                flag.0.set(!flag.0.get());
                // a tile cant stay on top so it floats where it is
                self.untile(&window);
                self.restack();
            }
            (Item::Volume, _) => {
                // left half turns it down and right half up and the menu stays open
                let local_x = self.pointer_screen.x - menu.pos.x as f64;
                let up = local_x >= WIDTH as f64 / 2.0;
                self.menu_adjust_volume(if up { 1 } else { -1 });
                return false;
            }
            (Item::ReturnToRegion, Target::Window(window)) => {
                if !self.is_tiled(&window) {
                    self.toggle_tiling(&window);
                }
            }
            (Item::Float, Target::Window(window)) => {
                if self.is_tiled(&window) {
                    self.toggle_floating(&window);
                }
            }
            (Item::Collapse, Target::Window(window)) => self.collapse(&window),
            (Item::WorkspaceMenu, _) => {
                if let Some(i) = workspace {
                    let number = self.workspaces[i].number;
                    self.open_submenu(number, index);
                }
                return false;
            }
            (Item::Numbers(first), _) => {
                if let Some(i) = workspace {
                    self.renumber_workspace(i, first + cell);
                    let number = self.workspaces[i].number;
                    // stay open on the renumbered workspace and update the menu next to it
                    if let Some(mut menu) = self.take_menu(sub) {
                        menu.target = Target::Workspace(number);
                        self.draw_menu(&mut menu);
                        self.put_menu(sub, menu);
                    }
                    if sub && let Some(mut menu) = self.menu.take() {
                        self.draw_menu(&mut menu);
                        self.menu = Some(menu);
                    }
                }
                return false;
            }
            (Item::NewWorkspace, _) => self.new_workspace(),
            (Item::RemoveWorkspace, _) => {
                if let Some(i) = workspace {
                    self.remove_workspace(i);
                }
            }
            _ => {}
        }
        true
    }

    /// the workspace a menu is about
    fn menu_workspace(&self, menu: &Menu) -> Option<usize> {
        match &menu.target {
            Target::Window(window) => self.ws_of(window),
            Target::Workspace(number) => self.ws_index(*number),
        }
    }

    /// scrolling over the volume row turns it up or down
    fn menu_scroll(&mut self, frame: &AxisFrame) {
        let over_volume = self.menu.as_ref().is_some_and(|menu| {
            self.menu_item_at(self.pointer_screen)
                .is_some_and(|(sub, i, _)| !sub && menu.items()[i] == Item::Volume)
        });
        if !over_volume {
            return;
        }
        let notches = frame
            .v120
            .map(|(_, v)| v as f64 / 120.0)
            .unwrap_or(frame.axis.1 / 15.0);
        if notches != 0.0 {
            // scrolling up turns it up
            self.menu_adjust_volume(if notches < 0.0 { 1 } else { -1 });
        }
    }

    fn menu_adjust_volume(&mut self, direction: i32) {
        let Some(mut menu) = self.menu.take() else {
            return;
        };
        if let Some(percent) = menu.streams.percent {
            let stepped = (percent as i32 + direction * VOLUME_STEP as i32).clamp(0, 150) as u32;
            audio::set_volume(&menu.streams, stepped);
            menu.streams.percent = Some(stepped);
            self.draw_menu(&mut menu);
        }
        self.menu = Some(menu);
    }

    fn menu_item_enabled(&self, item: Item, menu: &Menu) -> bool {
        let tiled = menu.window().is_some_and(|w| self.is_tiled(w));
        let on_top = menu.window().is_some_and(always_on_top);
        match item {
            Item::Volume => menu.streams.percent.is_some(),
            Item::ReturnToRegion => !tiled && !on_top,
            Item::Float | Item::WorkspaceMenu => tiled,
            Item::Heading => false,
            Item::RemoveWorkspace
            | Item::Close
            | Item::HideFromScreencast
            | Item::AlwaysOnTop
            | Item::Collapse
            | Item::Numbers(_)
            | Item::NewWorkspace => true,
        }
    }

    fn menu_label(&self, item: Item, menu: &Menu) -> (String, Option<String>) {
        let window_hidden = menu.window().is_some_and(hidden_from_capture);
        let on_top = menu.window().is_some_and(always_on_top);
        let number = self
            .menu_workspace(menu)
            .map(|i| self.workspaces[i].number);
        match item {
            Item::Close => ("Close window".into(), None),
            Item::HideFromScreencast => (
                "Hide from screencast".into(),
                Some(if window_hidden { "on" } else { "off" }.into()),
            ),
            Item::AlwaysOnTop => (
                "Always on top".into(),
                Some(if on_top { "on" } else { "off" }.into()),
            ),
            Item::Volume => match menu.streams.percent {
                Some(percent) => ("Volume   \u{2212}".into(), Some(format!("{percent}%   +"))),
                None => ("Volume".into(), Some("no audio".into())),
            },
            Item::ReturnToRegion => ("Tile in workspace".into(), None),
            Item::Float => ("Float".into(), None),
            Item::Collapse => ("Collapse".into(), Some("mod+n".into())),
            Item::WorkspaceMenu => match number {
                Some(n) => (format!("Workspace {n}"), Some("\u{203a}".into())),
                None => ("Workspace".into(), Some("\u{203a}".into())),
            },
            Item::Heading => match number {
                Some(n) => (format!("Workspace {n}: pick a number"), None),
                None => ("Workspace".into(), None),
            },
            Item::Numbers(_) => (String::new(), None),
            Item::NewWorkspace => ("New workspace".into(), None),
            Item::RemoveWorkspace => ("Remove workspace".into(), None),
        }
    }

    /// does the paint thing for the menu into its image
    fn draw_menu(&mut self, menu: &mut Menu) {
        let size = menu.size();
        // drawn at the monitor scale rounded up so its sharp
        let scale = self.ui_scale();
        let (s, sf) = (scale, scale as f32);
        let (w, h) = ((size.w * s) as usize, (size.h * s) as usize);
        let mut pixels = vec![0u8; w * h * 4];
        let fill = |pixels: &mut [u8], rect: Rectangle<i32, Logical>, color: [u8; 4]| {
            let (x0, y0) = (rect.loc.x * s, rect.loc.y * s);
            let (x1, y1) = (x0 + rect.size.w * s, y0 + rect.size.h * s);
            for y in y0.max(0)..y1.min(h as i32) {
                for x in x0.max(0)..x1.min(w as i32) {
                    let i = (y as usize * w + x as usize) * 4;
                    pixels[i..i + 4].copy_from_slice(&color);
                }
            }
        };
        let theme = &self.config.theme;
        let (edge, background, hover_color) =
            (theme.menu_edge, theme.menu_background, theme.menu_hover);
        let (text, disabled) = (theme.menu_text, theme.menu_disabled);
        fill(&mut pixels, Rectangle::from_size(size), edge);
        fill(
            &mut pixels,
            Rectangle::new(Point::from((1, 1)), Size::from((size.w - 2, size.h - 2))),
            background,
        );
        let items = menu.items();
        let cell_w = (WIDTH - 4) / PER_ROW as i32;
        let current = self
            .menu_workspace(menu)
            .map(|i| self.workspaces[i].number);
        let row_top = |row: usize| PADDING + row as i32 * ITEM_HEIGHT;
        if let Some((row, cell)) = menu.hover
            && self.menu_item_enabled(items[row], menu)
        {
            let rect = match items[row] {
                Item::Numbers(_) => Rectangle::new(
                    Point::from((2 + cell as i32 * cell_w, row_top(row))),
                    Size::from((cell_w, ITEM_HEIGHT)),
                ),
                _ => Rectangle::new(
                    Point::from((2, row_top(row))),
                    Size::from((size.w - 4, ITEM_HEIGHT)),
                ),
            };
            fill(&mut pixels, rect, hover_color);
        }
        // the workspaces own number gets an outline
        for (row, item) in items.iter().enumerate() {
            if let (Item::Numbers(first), Some(current)) = (*item, current)
                && (first..first + PER_ROW).contains(&current)
            {
                let x = 2 + (current - first) as i32 * cell_w;
                let r = Rectangle::new(
                    Point::from((x + 3, row_top(row) + 3)),
                    Size::from((cell_w - 6, ITEM_HEIGHT - 6)),
                );
                fill(&mut pixels, r, text);
                fill(
                    &mut pixels,
                    Rectangle::new(r.loc + Point::from((2, 2)), r.size - Size::from((4, 4))),
                    background,
                );
            }
        }

        if self.font.is_none() {
            self.font = Font::system();
        }
        let labels: Vec<_> = items
            .iter()
            .map(|&item| {
                (
                    item,
                    self.menu_label(item, menu),
                    self.menu_item_enabled(item, menu),
                )
            })
            .collect();
        let taken: Vec<u32> = self.workspaces.iter().map(|w| w.number).collect();
        if let Some(font) = &mut self.font {
            for (row, (item, (label, value), enabled)) in labels.into_iter().enumerate() {
                let color = if enabled { text } else { disabled };
                let baseline = row_top(row) as f32 + ITEM_HEIGHT as f32 * 0.68;
                if let Item::Numbers(first) = item {
                    for n in first..(first + PER_ROW).min(MAX_NUMBER + 1) {
                        let label = n.to_string();
                        // numbers another workspace has are dimmer bc picking one swaps
                        let color = if taken.contains(&n) && Some(n) != current {
                            disabled
                        } else {
                            text
                        };
                        let cx = 2.0 + ((n - first) as f32 + 0.5) * cell_w as f32;
                        let x = cx - font.width(&label, FONT_SIZE) / 2.0;
                        font.draw(&mut pixels, w, x * sf, baseline * sf, &label, FONT_SIZE * sf, color);
                    }
                    continue;
                }
                font.draw(&mut pixels, w, 12.0 * sf, baseline * sf, &label, FONT_SIZE * sf, color);
                if let Some(value) = value {
                    let x = WIDTH as f32 - 12.0 - font.width(&value, FONT_SIZE);
                    font.draw(&mut pixels, w, x * sf, baseline * sf, &value, FONT_SIZE * sf, color);
                }
            }
        }
        menu.image = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Abgr8888,
            (size.w * s, size.h * s),
            s,
            Transform::Normal,
            None,
        );
    }
}

/// holds the pointer while the menu is open
struct MenuGrab {
    start_data: GrabStartData<Seven>,
}

impl PointerGrab<Seven> for MenuGrab {
    fn motion(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        data.menu_hover();
    }

    fn relative_motion(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        event: &ButtonEvent,
    ) {
        if event.state != smithay::backend::input::ButtonState::Pressed {
            return;
        }
        if data.menu_click() {
            data.menu = None;
            data.submenu = None;
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut Seven,
        _handle: &mut PointerInnerHandle<'_, Seven>,
        details: AxisFrame,
    ) {
        data.menu_scroll(&details);
    }

    fn frame(&mut self, data: &mut Seven, handle: &mut PointerInnerHandle<'_, Seven>) {
        handle.frame(data);
    }

    crate::grabs::swallow_gestures!();

    fn start_data(&self) -> &GrabStartData<Seven> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut Seven) {
        data.menu = None;
        data.submenu = None;
    }
}
