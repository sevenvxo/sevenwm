//! tiling inside workspaces and moving windows between them and the canvas

use std::time::{Duration, Instant};

use smithay::desktop::{Window, layer_map_for_output};
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::config::{CanvasSize, NewTilePosition, NewWindows};
use crate::layout::{self, Rect};
use crate::state::Seven;

/// where a new floating window was meant to go till it shows the size it picked
#[derive(Clone, Copy)]
pub struct Settle {
    /// its parents middle for a dialog
    parent: Option<Point<f64, Logical>>,
    view_centre: Point<f64, Logical>,
    viewed: bool,
}

fn rect_centre(rect: Rect) -> Point<f64, Logical> {
    Point::from((
        rect.loc.x as f64 + rect.size.w as f64 / 2.0,
        rect.loc.y as f64 + rect.size.h as f64 / 2.0,
    ))
}

impl Seven {
    /// the screen size which is also the workspace size
    pub fn screen_size(&self) -> Size<i32, Logical> {
        self.output
            .as_ref()
            .and_then(|o| {
                o.current_mode()
                    .map(|m| (m, o.current_scale().fractional_scale()))
            })
            .map_or(Size::from((1280, 720)), |(mode, scale)| {
                mode.size.to_f64().to_logical(scale).to_i32_round()
            })
    }

    /// the active monitors home workspace rect
    pub fn region(&self) -> Rect {
        match self.home_index() {
            Some(i) => self.workspaces[i].rect,
            None => Rectangle::new(self.active_pos, self.screen_size()),
        }
    }

    /// where workspace is tiles go which is its area minus the outer gap
    pub fn tile_area(&self, i: usize) -> Rect {
        layout::inset(self.ws_area(i), self.config.tiling.gaps_outer)
    }

    /// workspace i as u see it w the bars strip cut off while the stored rect stays screen sized
    pub fn ws_area(&self, i: usize) -> Rect {
        let region = self.workspaces[i].rect;
        // panels reserve edges of the screen so the same edges of the workspace
        let output = self
            .outputs()
            .into_iter()
            .find(|o| o.name() == self.workspaces[i].output)
            .or_else(|| self.output.clone());
        let screen = output
            .as_ref()
            .map_or_else(|| self.screen_size(), crate::monitors::size_of);
        let zone = output.as_ref().map_or(Rectangle::from_size(screen), |o| {
            layer_map_for_output(o).non_exclusive_zone()
        });
        let right = screen.w - zone.loc.x - zone.size.w;
        let bottom = screen.h - zone.loc.y - zone.size.h;
        Rectangle::new(
            region.loc + zone.loc,
            Size::from((
                (region.size.w - zone.loc.x - right).max(1),
                (region.size.h - zone.loc.y - bottom).max(1),
            )),
        )
    }

    /// the part of the active screen the panels leave free in screen coords
    pub fn usable_screen(&self) -> Rectangle<f64, Logical> {
        let screen = self.screen_size();
        self.output
            .as_ref()
            .map_or(Rectangle::from_size(screen), |o| {
                layer_map_for_output(o).non_exclusive_zone()
            })
            .to_f64()
    }

    /// the canvas u can see around the panels from a camera at a zoom
    pub fn usable_view(&self, camera: Point<f64, Logical>, zoom: f64) -> Rectangle<f64, Logical> {
        let zone = self.usable_screen();
        Rectangle::new(
            Point::from((camera.x + zone.loc.x / zoom, camera.y + zone.loc.y / zoom)),
            Size::from((zone.size.w / zoom, zone.size.h / zoom)),
        )
    }

    /// the camera that puts rect in the middle under the bar or its top left in view if its too big
    pub fn camera_for(&self, rect: Rect, zoom: f64) -> Point<f64, Logical> {
        let zone = self.usable_screen();
        let axis = |pos: i32, len: i32, zone_pos: f64, zone_len: f64| {
            let span = zone_len / zoom;
            let start = if len as f64 > span {
                pos as f64
            } else {
                pos as f64 + (len as f64 - span) / 2.0
            };
            start - zone_pos / zoom
        };
        Point::from((
            axis(rect.loc.x, rect.size.w, zone.loc.x, zone.size.w),
            axis(rect.loc.y, rect.size.h, zone.loc.y, zone.size.h),
        ))
    }

    /// every workspace area
    pub fn ws_areas(&self) -> Vec<Rect> {
        (0..self.workspaces.len()).map(|i| self.ws_area(i)).collect()
    }

    /// the canvas edge if it has one which windows cant cross
    pub fn bounds(&self) -> Option<Rect> {
        let CanvasSize::Limited(w, h) = self.config.canvas.size else {
            return None;
        };
        // sized from the biggest monitor so it doesnt shift between monitors
        let screen = self
            .outputs()
            .iter()
            .map(crate::monitors::size_of)
            .fold(self.screen_size(), |a, b| Size::from((a.w.max(b.w), a.h.max(b.h))));
        let (w, h) = (w.max(screen.w), h.max(screen.h));
        Some(Rectangle::new(
            Point::from((screen.w / 2 - w / 2, screen.h / 2 - h / 2)),
            Size::from((w, h)),
        ))
    }

    /// whether the window is tiled in any workspace
    pub fn is_tiled(&self, window: &Window) -> bool {
        self.ws_of(window).is_some()
    }

    /// lay out every workspaces tiles again
    pub fn retile(&mut self) {
        for i in 0..self.workspaces.len() {
            self.retile_ws(i);
        }
        self.restack();
    }

    pub fn retile_ws(&mut self, i: usize) {
        let ws = &self.workspaces[i];
        let ratios: Vec<f64> = ws.tiled.iter().map(|(_, ratio)| *ratio).collect();
        let windows: Vec<Window> = ws.tiled.iter().map(|(w, _)| w.clone()).collect();
        let rects = layout::dwindle(self.tile_area(i), &ratios, self.config.tiling.gaps_inner);
        tracing::debug!("retile {}: {rects:?}", ws.number);
        let area = self.tile_area(i);
        for (window, rect) in windows.iter().zip(rects) {
            // a maximized tile covers the whole workspace
            let rect = if self.is_maximized(window) { area } else { rect };
            if !self.is_fullscreen(window) {
                self.resize_window(window, rect);
            }
        }
    }

    /// add the window to workspace i and retile
    pub fn tile_into(&mut self, window: &Window, i: usize) {
        if self.is_tiled(window) {
            return;
        }
        self.drop_maximized(window);
        let entry = (window.clone(), self.config.tiling.split_ratio);
        let tiled = &mut self.workspaces[i].tiled;
        match self.config.tiling.new_tile_position {
            NewTilePosition::Main => tiled.insert(0, entry),
            NewTilePosition::End => tiled.push(entry),
        }
        self.retile_ws(i);
        self.restack();
    }

    /// add the window next to the tile under at on the side its dropped sized to about its own width or height
    pub fn tile_sized(&mut self, window: &Window, i: usize, at: Option<Point<f64, Logical>>) {
        if self.is_tiled(window) {
            return;
        }
        let Some(size) = self.frame(window).map(|r| r.size) else {
            return self.tile_into(window, i);
        };
        self.drop_maximized(window);
        let area = self.tile_area(i);
        let gap = self.config.tiling.gaps_inner;
        let mut ratios: Vec<f64> = self.workspaces[i].tiled.iter().map(|(_, r)| *r).collect();
        let rects = layout::dwindle(area, &ratios, gap);
        let under = at.and_then(|p| {
            let k = rects.iter().position(|r| r.to_f64().contains(p))?;
            let r = rects[k].to_f64();
            let across = layout::dwindle_remaining(area, &ratios, gap, k);
            let second_half = if across.size.w >= across.size.h {
                p.x > r.loc.x + r.size.w / 2.0
            } else {
                p.y > r.loc.y + r.size.h / 2.0
            };
            Some(k + second_half as usize)
        });
        let index = under.unwrap_or(match self.config.tiling.new_tile_position {
            NewTilePosition::Main => 0,
            NewTilePosition::End => ratios.len(),
        });
        ratios.insert(index, self.config.tiling.split_ratio);
        // its own cut sizes it but the last tile has none so the cut before it takes the rest
        if ratios.len() > 1 {
            let cut = if index + 1 < ratios.len() { index } else { index - 1 };
            let across = layout::dwindle_remaining(area, &ratios, gap, cut);
            let share = layout::ratio_for(across, size, gap);
            ratios[cut] = if cut == index { share } else { 1.0 - share };
        }
        let tiled = &mut self.workspaces[i].tiled;
        tiled.insert(index, (window.clone(), self.config.tiling.split_ratio));
        for ((_, ratio), new) in tiled.iter_mut().zip(ratios) {
            *ratio = new;
        }
        self.retile_ws(i);
        self.restack();
    }

    /// take the window out of its workspace and retile
    pub fn untile(&mut self, window: &Window) {
        let Some(i) = self.ws_of(window) else {
            return;
        };
        self.drop_maximized(window);
        self.workspaces[i].tiled.retain(|(w, _)| w != window);
        self.retile_ws(i);
        self.restack();
    }

    /// floating windows stay above tiled ones
    pub fn restack(&mut self) {
        // maximized tiles over other tiles and floating windows over both
        let maximized: Vec<Window> = self
            .space
            .elements()
            .filter(|w| self.is_tiled(w) && self.is_maximized(w))
            .cloned()
            .collect();
        for window in maximized {
            self.space.raise_element(&window, false);
        }
        let floating: Vec<Window> = self
            .space
            .elements()
            .filter(|w| !self.is_tiled(w))
            .cloned()
            .collect();
        for window in floating {
            self.space.raise_element(&window, false);
        }
    }

    /// mod+ctrl+space moves a canvas window into the workspace or a tile out beside it
    pub fn toggle_tiling(&mut self, window: &Window) {
        if self.is_tiled(window) {
            let from = self.ws_of(window).map(|i| self.workspaces[i].rect);
            self.untile(window);
            if let Some(from) = from {
                self.place_beside(window, from);
            }
            // the workspace keeps the keyboard so give it to its next tile maybe
            let next = self
                .history
                .iter()
                .find(|w| from.is_some() && self.ws_of(w).map(|i| self.workspaces[i].rect) == from)
                .cloned();
            if let Some(next) = next {
                self.focus(Some(&next));
            }
        } else {
            self.bring_into_region(window);
        }
    }

    /// mod+space floats a tile where it is or tiles a floating window
    pub fn toggle_floating(&mut self, window: &Window) {
        if self.is_tiled(window) {
            self.untile(window);
            self.space.raise_element(window, true);
        } else {
            self.bring_into_region(window);
        }
    }

    fn bring_into_region(&mut self, window: &Window) {
        if self.is_fullscreen(window) {
            self.unfullscreen(window);
        }
        let Some(i) = self.home_index() else {
            return;
        };
        self.tile_sized(window, i, None);
        self.focus(Some(window));
        self.fly_to_workspace(i);
    }

    /// move a window that js left a workspace to the nearest free spot outside
    pub fn place_beside(&mut self, window: &Window, from: Rect) {
        let Some(rect) = self.frame(window) else {
            return;
        };
        let centre = Point::from((
            rect.loc.x as f64 + rect.size.w as f64 / 2.0,
            rect.loc.y as f64 + rect.size.h as f64 / 2.0,
        ));
        let mut obstacles = self.floating_rects(Some(window));
        obstacles.extend(self.ws_areas());
        obstacles.push(from);
        let gap = self.config.snap.gap;
        if let Some(loc) = layout::free_spot(rect.size, centre, &obstacles, gap, self.bounds()) {
            self.place_frame(window, loc, false);
        }
    }

    /// floating window rects w one left out optionally
    pub fn floating_rects(&self, except: Option<&Window>) -> Vec<Rect> {
        self.space
            .elements()
            .filter(|w| Some(*w) != except && !self.is_tiled(w))
            .filter_map(|w| self.frame(w))
            .collect()
    }

    /// put a new window where it goes tiled if ur looking at a workspace or floating where ur looking
    pub fn place_new_window(&mut self, window: Window) {
        let visible = self.usable_view(self.view.camera, self.view.zoom);
        let view_centre = Point::from((
            visible.loc.x + visible.size.w / 2.0,
            visible.loc.y + visible.size.h / 2.0,
        ));
        let (title, app_id) = crate::screencast::title_and_app_id(&window);
        let rule = self.config.rule_for(&app_id, &title);
        if rule.hide_from_screencast == Some(true) {
            window
                .user_data()
                .get_or_insert(crate::menu::HiddenFromCapture::default)
                .0
                .set(true);
        }
        let viewed = self.workspace_at(view_centre);
        if self.place_remembered(&window) {
            return;
        }
        // dialogs always float over their parent
        let parent = window
            .toplevel()
            .and_then(|t| t.parent())
            .and_then(|p| self.window_for_surface(&p))
            .and_then(|p| self.frame(&p));
        let fixed = window.toplevel().and_then(|t| {
            let (min, max) = smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                let mut cached = states
                    .cached_state
                    .get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
                let current = cached.current();
                (current.min_size, current.max_size)
            });
            (min.w > 0 && min.h > 0 && min == max).then_some(min)
        });
        let dialog = parent.is_some() || fixed.is_some();
        // the window an exec-outside launch was waiting for so float it beside the workspace
        let now = Instant::now();
        self.open_outside.retain(|(deadline, _)| *deadline > now);
        let tag = (!dialog && !self.open_outside.is_empty())
            .then(|| self.window_pid(&window))
            .flatten()
            .and_then(outside_tag);
        let waiting = tag.and_then(|tag| self.open_outside.iter().position(|(_, t)| *t == tag));
        let outside = waiting.is_some();
        if let Some(i) = waiting {
            self.open_outside.remove(i);
        }
        let viewed = viewed.filter(|_| !outside);
        let tile = match (rule.float, self.config.placement.new_windows) {
            _ if outside => false,
            (Some(float), _) => !float,
            _ if dialog => false,
            (None, NewWindows::AlwaysTile) => true,
            (None, NewWindows::AlwaysFloat) => false,
            (None, NewWindows::ByView) => viewed.is_some(),
        };
        if let Some(i) = viewed.or_else(|| self.home_index()).filter(|_| tile) {
            self.space
                .map_element(window.clone(), self.workspaces[i].rect.loc, false);
            self.tile_into(&window, i);
            self.focus(Some(&window));
            if viewed.is_none() {
                self.fly_to_workspace(i);
            }
            return;
        }

        // a rule size goes to the app or else the window gets placed again once it picks a size
        let ruled = rule.size.map(|[w, h]| Size::from((w.max(1), h.max(1))));
        let [w, h] = self.config.placement.float_size;
        let guess = ruled
            .or(fixed)
            .unwrap_or_else(|| Size::from((w.max(1), h.max(1))));
        let settle = Settle {
            parent: parent.map(rect_centre),
            view_centre,
            viewed: viewed.is_some(),
        };
        let loc = self.float_spot(guess, &settle, Some(&window));
        match ruled {
            Some(size) => self.resize_window(&window, Rectangle::new(loc, size)),
            None => {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| state.size = None);
                }
                self.place_frame(&window, loc, false);
                window.user_data().insert_if_missing(|| std::cell::Cell::new(Some(settle)));
            }
        }
        self.focus(Some(&window));
        // no free spot in view so follow it
        self.bring_into_view(&window);
    }

    /// where a floating window goes like over its parent or a free spot in view or anywhere nearest
    fn float_spot(&self, size: Size<i32, Logical>, settle: &Settle, except: Option<&Window>) -> Point<i32, Logical> {
        let centred_on = |c: Point<f64, Logical>| {
            Point::from((
                (c.x - size.w as f64 / 2.0) as i32,
                (c.y - size.h as f64 / 2.0) as i32,
            ))
        };
        if let Some(parent) = settle.parent {
            let loc = centred_on(parent);
            return match self.bounds() {
                Some(bounds) => layout::clamp_into(Rectangle::new(loc, size), bounds),
                None => loc,
            };
        }
        let mut obstacles = self.floating_rects(except);
        // on the canvas stay clear of workspaces but in one a floating window floats over the tiles
        if !settle.viewed {
            obstacles.extend(self.ws_areas());
        }
        let visible = self.usable_view(self.view.camera, self.view.zoom);
        let gap = self.config.snap.gap;
        let in_view = layout::inset(visible.to_i32_round(), gap);
        let in_view = match self.bounds() {
            Some(bounds) => in_view.intersection(bounds).unwrap_or(in_view),
            None => in_view,
        };
        let centre = settle.view_centre;
        layout::free_spot(size, centre, &obstacles, gap, Some(in_view))
            .or_else(|| layout::free_spot(size, centre, &obstacles, gap, self.bounds()))
            .unwrap_or_else(|| centred_on(centre))
    }

    /// a new floating windows first commit w a size so place it again for that size
    pub fn settle_new_window(&mut self, window: &Window) {
        let Some(slot) = window.user_data().get::<std::cell::Cell<Option<Settle>>>() else {
            return;
        };
        if window.geometry().size.w <= 0 || window.geometry().size.h <= 0 {
            return;
        }
        let Some(settle) = slot.take() else {
            return;
        };
        if self.is_tiled(window) || self.is_fullscreen(window) || self.is_maximized(window) {
            return;
        }
        let Some(frame) = self.frame(window) else {
            return;
        };
        let loc = self.float_spot(frame.size, &settle, Some(window));
        self.place_frame(window, loc, false);
        self.bring_into_view(window);
    }

    /// fly the view back home at zoom 1
    pub fn fly_home(&mut self) {
        if let Some(i) = self.home_index() {
            self.fly_to_workspace(i);
        }
    }

    /// if the window isnt fully in view fly to it and keep the zoom
    pub fn bring_into_view(&mut self, window: &Window) {
        if !self.config.view.focus_follows_view {
            return;
        }
        if let Some(i) = self.ws_of(window) {
            if self.view.destination() != (self.workspaces[i].rect.loc.to_f64(), 1.0) {
                self.fly_to_workspace(i);
            }
            return;
        }
        let Some(rect) = self.frame(window) else {
            return;
        };
        if self.is_fullscreen(window) {
            let duration = Duration::from_millis(self.config.view.fly_duration_ms);
            self.view.fly_to(rect.loc.to_f64(), 1.0, duration);
            return;
        }
        // already fully in view on another monitor so thats enough
        let others: Vec<_> = self.visible_on_any_monitor().into_iter().skip(1).collect();
        if others.iter().any(|v| v.contains_rect(rect.to_f64())) {
            return;
        }
        let (camera, zoom) = self.view.destination();
        // the bar covers part of the screen so only count whats under it
        if self.usable_view(camera, zoom).contains_rect(rect.to_f64()) {
            return;
        }
        let camera = self.camera_for(rect, zoom);
        let duration = Duration::from_millis(self.config.view.fly_duration_ms);
        self.view.fly_to(camera, zoom, duration);
    }

    /// fly to the window at 100% zoom even if its in view
    pub fn center_window(&mut self, window: &Window) {
        self.overview = None;
        if let Some(i) = self.ws_of(window) {
            self.fly_to_workspace(i);
            return;
        }
        let Some(rect) = self.frame(window) else {
            return;
        };
        if self.is_fullscreen(window) {
            self.fly(rect.loc.to_f64(), 1.0);
            return;
        }
        self.fly(self.camera_for(rect, 1.0), 1.0);
    }
}

impl Seven {
    /// mod+tab zooms out to fit everything or goes back to the view from before
    pub fn toggle_overview(&mut self) {
        match self.overview.take() {
            Some((camera, zoom)) => self.fly(camera, zoom),
            None => self.enter_overview(),
        }
    }

    fn enter_overview(&mut self) {
        let everything = self
            .space
            .elements()
            .filter_map(|w| self.frame(w))
            .chain(self.ws_areas())
            .fold(self.region(), |all, rect| all.merge(rect));
        let screen = self.screen_size();
        let margin = 0.05;
        let zoom = (screen.w as f64 / everything.size.w as f64)
            .min(screen.h as f64 / everything.size.h as f64)
            * (1.0 - 2.0 * margin);
        let zoom = zoom.clamp(self.config.view.zoom_min, 1.0);
        let camera = Point::from((
            everything.loc.x as f64 + everything.size.w as f64 / 2.0 - screen.w as f64 / zoom / 2.0,
            everything.loc.y as f64 + everything.size.h as f64 / 2.0 - screen.h as f64 / zoom / 2.0,
        ));
        self.overview = Some(self.view.destination());
        self.fly(camera, zoom);
    }

    /// leave the overview onto the window and fly to it at zoom 1
    pub fn leave_overview_to(&mut self, window: &Window) {
        self.overview = None;
        self.focus(Some(window));
        if let Some(i) = self.ws_of(window) {
            self.fly_to_workspace(i);
            return;
        }
        let Some(rect) = self.frame(window) else {
            return;
        };
        self.fly(self.camera_for(rect, 1.0), 1.0);
    }

    /// mod+plus and mod+minus zoom around the middle of the screen and held keys keep zooming smooth
    pub fn zoom_centre(&mut self, factor: f64) {
        let (camera, zoom) = self.view.destination();
        let screen = self.screen_size();
        let (half_w, half_h) = (screen.w as f64 / 2.0, screen.h as f64 / 2.0);
        let (min, max) = (self.config.view.zoom_min, self.config.view.zoom_max);
        let new_zoom = (zoom * factor).clamp(min, max);
        let camera = Point::from((
            camera.x + half_w / zoom - half_w / new_zoom,
            camera.y + half_h / zoom - half_h / new_zoom,
        ));
        self.fly(camera, new_zoom);
    }

    fn fly(&mut self, camera: Point<f64, Logical>, zoom: f64) {
        let duration = Duration::from_millis(self.config.view.fly_duration_ms);
        self.view.fly_to(camera, zoom, duration);
    }
}

/// the exec-outside tag in a process env if it was started by one
fn outside_tag(pid: i32) -> Option<String> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{}=", crate::actions::OUTSIDE_ENV);
    environ
        .split(|b| *b == 0)
        .find_map(|var| var.strip_prefix(prefix.as_bytes()))
        .map(|tag| String::from_utf8_lossy(tag).into_owned())
}
