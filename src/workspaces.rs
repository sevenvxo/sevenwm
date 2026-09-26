//! workspaces are numbered tiled areas on the canvas that u can make remove drag and jump to

use std::time::Duration;

use smithay::backend::renderer::element::Id;
use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::layout::{self, Rect};
use crate::state::Seven;

/// highest workspace number and mod+0 is 10
pub const MAX_NUMBER: u32 = 10;

pub struct Workspace {
    pub number: u32,
    /// where it is on the canvas
    pub rect: Rect,
    /// the monitor its sized to which it follows while connected
    pub output: String,
    /// its tiles main first each w the share of space its split takes
    pub tiled: Vec<(Window, f64)>,
    /// stable ids for its highlight and outline
    pub ids: [Id; 5],
}

impl Workspace {
    pub fn new(number: u32, rect: Rect, output: String) -> Self {
        Self {
            number,
            rect,
            output,
            tiled: Vec::new(),
            ids: std::array::from_fn(|_| Id::new()),
        }
    }

    pub fn centre(&self) -> Point<f64, Logical> {
        centre(self.rect)
    }
}

fn centre(rect: Rect) -> Point<f64, Logical> {
    Point::from((
        rect.loc.x as f64 + rect.size.w as f64 / 2.0,
        rect.loc.y as f64 + rect.size.h as f64 / 2.0,
    ))
}

impl Seven {
    pub fn ws_index(&self, number: u32) -> Option<usize> {
        self.workspaces.iter().position(|w| w.number == number)
    }

    /// the active monitors home workspace
    pub fn home_index(&self) -> Option<usize> {
        self.ws_index(self.home)
            .or_else(|| (!self.workspaces.is_empty()).then_some(0))
    }

    /// the workspace that has the window as a tile
    pub fn ws_of(&self, window: &Window) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|ws| ws.tiled.iter().any(|(w, _)| w == window))
    }

    /// the workspace under a canvas point
    pub fn workspace_at(&self, canvas: Point<f64, Logical>) -> Option<usize> {
        self.ws_areas()
            .iter()
            .position(|area| area.to_f64().contains(canvas))
    }

    /// the workspace the view is in which is the one under the middle of the screen
    pub fn current_workspace(&self) -> Option<usize> {
        let (camera, zoom) = self.view.destination();
        let screen = self.screen_size();
        let middle = Point::from((
            camera.x + screen.w as f64 / 2.0 / zoom,
            camera.y + screen.h as f64 / 2.0 / zoom,
        ));
        self.workspace_at(middle)
    }

    /// looking at a workspace makes it the monitors home
    pub fn update_home(&mut self) {
        if let Some(i) = self.current_workspace() {
            self.home = self.workspaces[i].number;
        }
    }

    /// the lowest number no workspace has
    fn free_number(&self) -> Option<u32> {
        (1..=MAX_NUMBER).find(|n| self.ws_index(*n).is_none())
    }

    /// a monitor showed up so give it a home by reusing one sized to it or making a new one nearby
    pub fn home_for_monitor(&mut self, name: &str, pos: Point<i32, Logical>) -> u32 {
        if let Some(ws) = self.workspaces.iter().find(|w| w.output == name) {
            return ws.number;
        }
        let size = self.screen_size();
        let number = self.free_number().unwrap_or(MAX_NUMBER + self.workspaces.len() as u32);
        let obstacles: Vec<Rect> = self.workspaces.iter().map(|w| w.rect).collect();
        let loc = layout::free_spot(
            size,
            centre(Rectangle::new(pos, size)),
            &obstacles,
            self.config.workspaces.gap,
            None,
        )
        .unwrap_or(pos);
        self.workspaces
            .push(Workspace::new(number, Rectangle::new(loc, size), name.to_string()));
        number
    }

    /// does the size thing for every workspace and its monitor
    pub fn resize_workspaces(&mut self) {
        let sizes: Vec<(String, Size<i32, Logical>)> = self
            .outputs()
            .iter()
            .map(|o| (o.name(), crate::monitors::size_of(o)))
            .collect();
        for ws in &mut self.workspaces {
            if let Some((_, size)) = sizes.iter().find(|(name, _)| *name == ws.output) {
                ws.rect.size = *size;
            }
        }
        self.retile();
    }

    /// fly to workspace i at zoom 1 and make it home
    pub fn fly_to_workspace(&mut self, i: usize) {
        let Some(ws) = self.workspaces.get(i) else {
            return;
        };
        self.home = ws.number;
        let duration = Duration::from_millis(self.config.view.fly_duration_ms);
        self.view.fly_to(ws.rect.loc.to_f64(), 1.0, duration);
    }

    /// mod+number goes to that workspace and focuses its most recent tile
    pub fn go_to_workspace(&mut self, number: u32) {
        let Some(i) = self.ws_index(number) else {
            self.notify(&format!("There's no workspace {number}"));
            return;
        };
        self.overview = None;
        self.fly_to_workspace(i);
        let recent = self
            .history
            .iter()
            .find(|w| self.workspaces[i].tiled.iter().any(|(t, _)| t == *w))
            .cloned();
        if let Some(window) = recent {
            self.focus(Some(&window));
        }
    }

    /// mod+shift+number moves the focused window into that workspace
    pub fn move_to_workspace(&mut self, number: u32) {
        let Some(window) = self.focused_window() else {
            return;
        };
        let Some(i) = self.ws_index(number) else {
            self.notify(&format!("There's no workspace {number}"));
            return;
        };
        if self.ws_of(&window) == Some(i) {
            return;
        }
        if self.is_fullscreen(&window) {
            self.unfullscreen(&window);
        }
        self.untile(&window);
        self.tile_into(&window, i);
        self.fly_to_workspace(i);
        self.focus(Some(&window));
    }

    /// mod+shift+plus makes a new empty workspace next to the one ur in and goes there
    pub fn new_workspace(&mut self) {
        let Some(number) = self.free_number() else {
            self.notify(&format!("All {MAX_NUMBER} workspaces are in use"));
            return;
        };
        let size = self.screen_size();
        // aim just right of the one ur in and take the nearest free spot
        let gap = self.config.workspaces.gap as f64;
        let near = match self.current_workspace() {
            Some(i) => {
                let ws = &self.workspaces[i];
                ws.centre() + Point::from((ws.rect.size.w as f64 / 2.0 + gap + size.w as f64 / 2.0, 0.0))
            }
            None => {
                let visible = self.view.visible(size);
                centre(visible.to_i32_round())
            }
        };
        let obstacles: Vec<Rect> = self.workspaces.iter().map(|w| w.rect).collect();
        let Some(loc) = layout::free_spot(
            size,
            near,
            &obstacles,
            self.config.workspaces.gap,
            self.bounds(),
        ) else {
            self.notify("No room on the canvas for another workspace");
            return;
        };
        let output = self.output.as_ref().map(|o| o.name()).unwrap_or_default();
        self.workspaces
            .push(Workspace::new(number, Rectangle::new(loc, size), output));
        self.fly_to_workspace(self.workspaces.len() - 1);
        tracing::info!("workspace {number} made at {loc:?}");
    }

    /// mod+shift+minus removes the workspace ur in and floats its tiles but the last one stays
    pub fn destroy_workspace(&mut self) {
        let Some(i) = self.current_workspace() else {
            self.notify("Go into a workspace to remove it");
            return;
        };
        self.remove_workspace(i);
    }

    pub fn remove_workspace(&mut self, i: usize) {
        if self.workspaces.len() <= 1 {
            self.notify("The last workspace can't be removed");
            return;
        }
        let ws = self.workspaces.remove(i);
        for (window, _) in &ws.tiled {
            if self.is_fullscreen(window) {
                self.unfullscreen(window);
            }
            self.place_beside(window, ws.rect);
        }
        // monitors that called it home move to the nearest one left
        let nearest = |from: Point<f64, Logical>, all: &[Workspace]| {
            all.iter()
                .min_by(|a, b| {
                    let d = |w: &Workspace| {
                        let c = w.centre();
                        (c.x - from.x).powi(2) + (c.y - from.y).powi(2)
                    };
                    d(a).total_cmp(&d(b))
                })
                .map(|w| w.number)
        };
        let replacement = nearest(ws.centre(), &self.workspaces).unwrap_or(1);
        if self.home == ws.number {
            self.home = replacement;
        }
        for monitor in &mut self.monitors {
            if monitor.home == ws.number {
                monitor.home = replacement;
            }
        }
        // its collapsed tiles come back floating where they were
        for entry in self.collapsed.iter_mut().filter(|c| c.workspace == Some(ws.number)) {
            entry.workspace = None;
        }
        tracing::info!("workspace {} removed", ws.number);
    }

    /// give workspace i this number and swap w whoever had it
    pub fn renumber_workspace(&mut self, i: usize, number: u32) {
        let Some(old) = self.workspaces.get(i).map(|w| w.number) else {
            return;
        };
        if let Some(j) = self.ws_index(number) {
            self.workspaces[j].number = old;
        }
        self.workspaces[i].number = number;
        // homes follow the workspace not the number
        let swap = |home: &mut u32| {
            if *home == old {
                *home = number;
            } else if *home == number {
                *home = old;
            }
        };
        swap(&mut self.home);
        for monitor in &mut self.monitors {
            swap(&mut monitor.home);
        }
        // collapsed tiles too
        for entry in &mut self.collapsed {
            if let Some(home) = &mut entry.workspace {
                swap(home);
            }
        }
    }

    /// move workspace i to loc and take its uhhhh tiles along
    pub fn move_workspace(&mut self, i: usize, loc: Point<i32, Logical>) {
        let Some(ws) = self.workspaces.get_mut(i) else {
            return;
        };
        let delta = loc - ws.rect.loc;
        if delta == Point::default() {
            return;
        }
        let old = ws.rect;
        ws.rect.loc = loc;
        let windows: Vec<Window> = ws.tiled.iter().map(|(w, _)| w.clone()).collect();
        for window in windows {
            if let Some(at) = self.space.element_location(&window) {
                self.space.map_element(window, at + delta, false);
            }
        }
        // a fullscreen window on it goes along too
        let fullscreen: Vec<Window> = self
            .fullscreen
            .iter()
            .map(|(w, _)| w.clone())
            .filter(|w| !self.is_tiled(w) && self.space.element_location(w) == Some(old.loc))
            .collect();
        for window in fullscreen {
            self.space.map_element(window, loc, false);
        }
        // and its collapsed tile markers below it
        let number = self.workspaces[i].number;
        for entry in self.collapsed.iter_mut().filter(|c| c.workspace == Some(number)) {
            entry.anchor += delta.to_f64();
            entry.rect.loc += delta;
        }
    }

    /// show a short notification
    pub fn notify(&self, text: &str) {
        tracing::info!("{text}");
        if let Ok(child) = std::process::Command::new("notify-send")
            .args(["-a", "sevenwm", "-t", "2500", text])
            .spawn()
        {
            self.watch_child("notify-send", child);
        }
    }

    /// a notification that stays till u dismiss it
    pub fn notify_error(&self, summary: &str, body: &str) {
        tracing::warn!("{summary}: {body}");
        if let Ok(child) = std::process::Command::new("notify-send")
            .args(["-a", "sevenwm", "-u", "critical", summary, body])
            .spawn()
        {
            self.watch_child("notify-send", child);
        }
    }

    /// mod+ctrl+drag on a workspace picks it up
    pub fn start_workspace_drag(&mut self, i: usize, button: u32, serial: smithay::utils::Serial) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let number = self.workspaces[i].number;
        let grab = WorkspaceDrag {
            start_data: GrabStartData {
                focus: None,
                button,
                location: pointer.current_location(),
            },
            number,
            initial: self.workspaces[i].rect.loc,
        };
        self.dragging_workspace = Some(number);
        pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Clear);
    }
}

/// dragging a workspace moves its tiles too and snaps to other workspaces
pub struct WorkspaceDrag {
    start_data: GrabStartData<Seven>,
    number: u32,
    initial: Point<i32, Logical>,
}

impl WorkspaceDrag {
    fn others(&self, state: &Seven) -> Vec<Rect> {
        state
            .workspaces
            .iter()
            .filter(|w| w.number != self.number)
            .map(|w| w.rect)
            .collect()
    }
}

impl PointerGrab<Seven> for WorkspaceDrag {
    fn motion(
        &mut self,
        data: &mut Seven,
        handle: &mut PointerInnerHandle<'_, Seven>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let Some(i) = data.ws_index(self.number) else {
            return;
        };
        let delta = (event.location - self.start_data.location).to_i32_round::<i32>();
        let mut rect = Rectangle::new(self.initial + delta, data.workspaces[i].rect.size);
        if data.config.snap.enabled {
            rect.loc = layout::snap(
                rect,
                &self.others(data),
                data.config.workspaces.gap,
                data.config.snap.threshold,
            );
        }
        if let Some(bounds) = data.bounds() {
            rect.loc = layout::clamp_into(rect, bounds);
        }
        data.move_workspace(i, rect.loc);
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
        data.dragging_workspace = None;
        let Some(i) = data.ws_index(self.number) else {
            return;
        };
        // workspaces never overlap so dropped on another it moves clear
        let rect = data.workspaces[i].rect;
        let others = self.others(data);
        if others.iter().any(|o| o.overlaps(rect)) {
            let gap = data.config.workspaces.gap;
            if let Some(loc) = layout::free_spot(rect.size, centre(rect), &others, gap, data.bounds())
            {
                data.move_workspace(i, loc);
            }
        }
        data.retile();
    }
}
