//! remembers the layout across restarts in session.json and puts windows back when their apps reopen but never when nested

use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use smithay::desktop::Window;
use smithay::utils::{IsAlive, Point, Rectangle, Size};

use crate::state::Seven;
use crate::workspaces::Workspace;

#[derive(Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub workspaces: Vec<SavedWorkspace>,
    pub monitors: Vec<SavedMonitor>,
    pub windows: Vec<SavedWindow>,
}

#[derive(Serialize, Deserialize, PartialEq)]
pub struct SavedWorkspace {
    pub number: u32,
    pub rect: [i32; 4],
    pub output: String,
}

#[derive(Serialize, Deserialize, PartialEq)]
pub struct SavedMonitor {
    pub name: String,
    pub camera: [f64; 2],
    pub zoom: f64,
    pub home: u32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedWindow {
    pub app_id: String,
    pub title: String,
    pub rect: [i32; 4],
    /// the workspace its tiled in its spot among the tiles and its uhhhh split share
    pub workspace: Option<u32>,
    pub index: usize,
    pub ratio: f64,
    /// collapsed w its marker here on the canvas
    pub marker: Option<[f64; 2]>,
}

/// remembered windows still waiting for their app to open them
pub struct Pending {
    pub windows: Vec<SavedWindow>,
    pub until: Instant,
    /// tiles already put back w their remembered spot so later ones slot in around them
    pub placed: Vec<(Window, usize)>,
}

/// how long every window has to stay closed before the session forgets them
const ALL_CLOSED_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// SEVENWM_SESSION_FILE overrides the file and makes nested runs use it too for testing
fn path() -> Option<PathBuf> {
    if let Some(file) = std::env::var_os("SEVENWM_SESSION_FILE") {
        return Some(PathBuf::from(file));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("sevenwm").join("session.json"))
}

fn rect_of(r: [i32; 4]) -> crate::layout::Rect {
    Rectangle::new(Point::from((r[0], r[1])), Size::from((r[2].max(1), r[3].max(1))))
}

impl Seven {
    fn session_enabled(&self) -> bool {
        self.config.session.restore
            && (!self.nested || std::env::var_os("SEVENWM_SESSION_FILE").is_some())
    }

    /// at startup bring back the workspaces and hold the windows till their apps open them
    pub fn load_session(&mut self) {
        if !self.session_enabled() {
            return;
        }
        let Some(session) = path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str::<Session>(&text).ok())
        else {
            return;
        };
        self.no_workspaces_saved = session.workspaces.is_empty();
        for ws in &session.workspaces {
            if self.ws_index(ws.number).is_none() {
                self.workspaces
                    .push(Workspace::new(ws.number, rect_of(ws.rect), ws.output.clone()));
            }
        }
        let secs = self.config.session.restore_within_seconds;
        self.pending = Some(Pending {
            placed: Vec::new(),
            windows: session.windows,
            until: Instant::now() + std::time::Duration::from_secs(secs),
        });
        tracing::info!(
            "session: {} workspaces back, waiting {secs}s for windows",
            self.workspaces.len()
        );
        self.saved_monitors = session.monitors;
    }

    /// a monitor showed up so point its camera where it was last time
    pub fn restore_monitor_view(&mut self, name: &str) {
        let Some(saved) = self.saved_monitors.iter().find(|m| m.name == name) else {
            return;
        };
        let (camera, zoom, home) = (saved.camera, saved.zoom, saved.home);
        let zoom = zoom.clamp(self.config.view.zoom_min, self.config.view.zoom_max);
        let camera = Point::from((camera[0], camera[1]));
        if self.output.as_ref().is_some_and(|o| o.name() == name) {
            self.view.set(camera, zoom);
            if self.ws_index(home).is_some() {
                self.home = home;
            }
        } else if let Some(monitor) = self.monitors.iter_mut().find(|m| m.output.name() == name) {
            monitor.view.set(camera, zoom);
            if self.workspaces.iter().any(|w| w.number == home) {
                monitor.home = home;
            }
        }
    }

    /// a new window so if we remember it put it back and return true
    pub fn place_remembered(&mut self, window: &Window) -> bool {
        let Some(pending) = &mut self.pending else {
            return false;
        };
        if Instant::now() > pending.until {
            self.pending = None;
            return false;
        }
        let (title, app_id) = crate::screencast::title_and_app_id(window);
        let candidates = || pending.windows.iter().enumerate().filter(|(_, s)| s.app_id == app_id);
        let Some(i) = candidates()
            .find(|(_, s)| s.title == title)
            .or_else(|| candidates().next())
            .map(|(i, _)| i)
        else {
            return false;
        };
        let saved = pending.windows.remove(i);
        let mut placed = std::mem::take(&mut pending.placed);
        tracing::info!("session: {app_id} goes back where it was");
        let rect = rect_of(saved.rect);
        match saved.workspace.and_then(|n| self.ws_index(n)) {
            Some(ws) => {
                self.space.map_element(window.clone(), rect.loc, false);
                self.drop_maximized(window);
                // it goes before the first restored tile that came after it and tiles u opened meanwhile stay put
                placed.retain(|(w, _)| w.alive());
                let tiled = &mut self.workspaces[ws].tiled;
                let at = tiled
                    .iter()
                    .position(|(w, _)| placed.iter().any(|(p, k)| p == w && *k > saved.index))
                    .unwrap_or(tiled.len());
                tiled.insert(at, (window.clone(), saved.ratio));
                placed.push((window.clone(), saved.index));
                self.retile();
                self.restack();
            }
            None => self.resize_window(window, rect),
        }
        if let Some(pending) = &mut self.pending {
            pending.placed = placed;
        }
        if let Some([x, y]) = saved.marker {
            self.collapse(window);
            // it hasnt drawn yet so its size is the remembered one
            if let Some(entry) = self.collapsed.iter_mut().find(|c| c.window == *window) {
                entry.anchor = Point::from((x, y));
                entry.rect = rect;
            }
        } else {
            self.focus(Some(window));
        }
        true
    }

    /// everything worth remembering as it is rn prolly
    fn session(&self) -> Session {
        let workspaces = self
            .workspaces
            .iter()
            .map(|ws| SavedWorkspace {
                number: ws.number,
                rect: [ws.rect.loc.x, ws.rect.loc.y, ws.rect.size.w, ws.rect.size.h],
                output: ws.output.clone(),
            })
            .collect();
        let mut monitors: Vec<SavedMonitor> = self
            .monitors
            .iter()
            .map(|m| SavedMonitor {
                name: m.output.name(),
                camera: [m.view.destination().0.x, m.view.destination().0.y],
                zoom: m.view.destination().1,
                home: m.home,
            })
            .collect();
        if let Some(output) = &self.output {
            let (camera, zoom) = self.view.destination();
            monitors.push(SavedMonitor {
                name: output.name(),
                camera: [camera.x, camera.y],
                zoom,
                home: self.home,
            });
        }
        let arr = |r: crate::layout::Rect| [r.loc.x, r.loc.y, r.size.w, r.size.h];
        let mut windows: Vec<SavedWindow> = Vec::new();
        for window in self.space.elements() {
            let (title, app_id) = crate::screencast::title_and_app_id(window);
            // fullscreen windows are remembered where they go back to
            let rect = self
                .fullscreen
                .iter()
                .find(|(w, _)| w == window)
                .map(|(_, before)| *before)
                .or_else(|| self.frame(window))
                .unwrap_or_default();
            let tile = self.ws_of(window).and_then(|i| {
                let ws = &self.workspaces[i];
                let index = ws.tiled.iter().position(|(w, _)| w == window)?;
                Some((ws.number, index, ws.tiled[index].1))
            });
            windows.push(SavedWindow {
                app_id,
                title,
                rect: arr(rect),
                workspace: tile.map(|t| t.0),
                index: tile.map_or(0, |t| t.1),
                ratio: tile.map_or(self.config.tiling.split_ratio, |t| t.2),
                marker: None,
            });
        }
        for c in &self.collapsed {
            let (title, app_id) = crate::screencast::title_and_app_id(&c.window);
            windows.push(SavedWindow {
                app_id,
                title,
                rect: arr(c.rect),
                workspace: c.workspace,
                index: 0,
                ratio: self.config.tiling.split_ratio,
                marker: Some([c.anchor.x, c.anchor.y]),
            });
        }
        // windows not back yet stay remembered for a while then get forgotten
        if let Some(pending) = &self.pending
            && Instant::now() <= pending.until
        {
            windows.extend(pending.windows.iter().cloned());
        }
        Session {
            workspaces,
            monitors,
            windows,
        }
    }

    /// once a sec and when quitting write the session if it changed
    pub fn save_session(&mut self) {
        if !self.session_enabled() || self.output.is_none() {
            return;
        }
        if self.pending.as_ref().is_some_and(|p| Instant::now() > p.until) {
            self.pending = None;
        }
        let mut session = self.session();
        if !session.windows.is_empty() {
            self.windows_seen = Instant::now();
        }
        // at shutdown apps can die right before sevenwm so windows that js went are still remembered
        if session.windows.is_empty()
            && self.windows_seen.elapsed() < ALL_CLOSED_GRACE
            && let Some(saved) = &self.session_saved
        {
            session.windows = saved.windows.clone();
        }
        if self.session_saved.as_ref() == Some(&session) {
            return;
        }
        let Some(path) = path() else {
            return;
        };
        let written = serde_json::to_string_pretty(&session)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                // write then rename so a crash never leaves half a file
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
                std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
            });
        match written {
            Ok(()) => self.session_saved = Some(session),
            Err(err) => tracing::warn!("session: couldn't save: {err}"),
        }
    }
}
