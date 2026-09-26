//! a private socket for sevenshell and scripts that speaks one json per line
//! get state gives the state once
//! get bindings gives the binds and mouse modifiers
//! subscribe true sends the state now and whenever it changes
//! action toggle-tiling runs any keybind action
//! focus 12 focuses a window by id and flies to it
//! fly_to x 0.0 y 0.0 zoom 1.0 moves the view
//! replies are ok or error and the path is in SEVENWM_SOCK

use std::cell::Cell;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use smithay::desktop::Window;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::utils::Point;

use crate::config::Action;
use crate::state::Seven;

/// a windows ipc id given out on first use and never reused
struct WindowId(Cell<u64>);

pub fn window_id(window: &Window) -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    window
        .user_data()
        .get_or_insert(|| WindowId(Cell::new(NEXT.fetch_add(1, Ordering::Relaxed))))
        .0
        .get()
}

/// a client that wants state updates and whats still waiting to send
pub struct Subscriber {
    stream: UnixStream,
    pending: Vec<u8>,
}

/// a subscriber this far behind gets dropped so it reconnects
const MAX_PENDING: usize = 4 << 20;

impl Subscriber {
    /// queue text and send what the socket takes or false if the subscriber is prolly gone
    fn send(&mut self, text: &[u8]) -> bool {
        self.pending.extend_from_slice(text);
        while !self.pending.is_empty() {
            match (&self.stream).write(&self.pending) {
                Ok(0) => return false,
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        self.pending.len() <= MAX_PENDING
    }
}

/// open the socket and serve it from the event loop
pub fn listen(
    event_loop: &mut EventLoop<'static, Seven>,
    state: &mut Seven,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = crate::monitors::runtime_dir().ok_or("no XDG_RUNTIME_DIR")?;
    let path = dir.join(format!("{}.sock", state.socket_name.to_string_lossy()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let handle = event_loop.handle();
    event_loop.handle().insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        move |_, listener, _state| {
            while let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(true);
                let Ok(reader) = stream.try_clone() else {
                    continue;
                };
                let mut lines = BufReader::new(reader);
                // a request can come in pieces so keep it till the newline
                let mut line = Vec::new();
                let _ = handle.insert_source(
                    Generic::new(stream, Interest::READ, Mode::Level),
                    move |_, stream, state| {
                        loop {
                            match lines.read_until(b'\n', &mut line) {
                                Ok(0) => return Ok(PostAction::Remove),
                                Ok(_) if line.ends_with(b"\n") => {
                                    let text = String::from_utf8_lossy(&line).into_owned();
                                    line.clear();
                                    state.damage();
                                    let reply = state.ipc_request(text.trim(), stream);
                                    if let Some(reply) = reply
                                        && write_line(stream, &reply).is_err()
                                    {
                                        return Ok(PostAction::Remove);
                                    }
                                }
                                // the stream ended mid line
                                Ok(_) => return Ok(PostAction::Remove),
                                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                                    return Ok(PostAction::Continue);
                                }
                                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                                Err(_) => return Ok(PostAction::Remove),
                            }
                        }
                    },
                );
            }
            Ok(PostAction::Continue)
        },
    )?;
    tracing::info!("IPC on {}", path.display());
    state.ipc_path = Some(path.clone());
    Ok(path)
}

fn write_line(mut stream: &UnixStream, value: &Value) -> std::io::Result<()> {
    let mut text = value.to_string();
    text.push('\n');
    stream.write_all(text.as_bytes())
}

impl Seven {
    /// answer one request line or none if it needs no reply
    fn ipc_request(&mut self, line: &str, stream: &UnixStream) -> Option<Value> {
        if line.is_empty() {
            return None;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => return Some(json!({ "error": format!("not JSON: {err}") })),
        };
        if request.get("get").and_then(Value::as_str) == Some("state") {
            return Some(json!({ "ok": self.ipc_state() }));
        }
        if request.get("get").and_then(Value::as_str) == Some("bindings") {
            let bindings: Vec<Value> = self
                .config
                .binding_list
                .iter()
                .map(|(keys, action)| json!({ "keys": keys, "action": action }))
                .collect();
            // the mouse modifiers too which arent keybinds
            let mouse = json!({
                "pan": format!("mod+{}", self.config.view.pan_modifier),
                "workspace_drag": format!("mod+{}", self.config.workspaces.drag_modifier),
            });
            return Some(json!({
                "ok": { "mod": self.config.mod_key, "bindings": bindings, "mouse": mouse }
            }));
        }
        if request.get("subscribe").and_then(Value::as_bool) == Some(true) {
            let Ok(copy) = stream.try_clone() else {
                return Some(json!({ "error": "can't subscribe" }));
            };
            let state = self.ipc_state();
            let mut subscriber = Subscriber {
                stream: copy,
                pending: Vec::new(),
            };
            let text = format!("{}\n", json!({ "state": state }));
            if subscriber.send(text.as_bytes()) {
                self.subscribers.push(subscriber);
            } else {
                let _ = subscriber.stream.shutdown(std::net::Shutdown::Both);
            }
            return None;
        }
        if let Some(action) = request.get("action").and_then(Value::as_str) {
            return Some(match Action::parse(action) {
                Ok(action) => {
                    self.run_action(action);
                    json!({ "ok": null })
                }
                Err(err) => json!({ "error": err }),
            });
        }
        if let Some(id) = request.get("focus").and_then(Value::as_u64) {
            let window = self
                .space
                .elements()
                .chain(self.collapsed.iter().map(|c| &c.window))
                .find(|w| window_id(w) == id)
                .cloned();
            return Some(match window {
                Some(window) => {
                    self.focus(Some(&window));
                    self.bring_into_view(&window);
                    json!({ "ok": null })
                }
                None => json!({ "error": format!("no window {id}") }),
            });
        }
        if let Some(target) = request.get("fly_to") {
            let x = target.get("x").and_then(Value::as_f64);
            let y = target.get("y").and_then(Value::as_f64);
            let zoom = target
                .get("zoom")
                .and_then(Value::as_f64)
                .unwrap_or(self.view.zoom);
            let (Some(x), Some(y)) = (x, y) else {
                return Some(json!({ "error": "fly_to needs x and y" }));
            };
            let zoom = zoom.clamp(self.config.view.zoom_min, self.config.view.zoom_max);
            let duration = std::time::Duration::from_millis(self.config.view.fly_duration_ms);
            self.view.fly_to(Point::from((x, y)), zoom, duration);
            return Some(json!({ "ok": null }));
        }
        Some(json!({ "error": "unknown request" }))
    }

    /// everything a shell shows like monitors views and every window
    pub fn ipc_state(&self) -> Value {
        let focused = self.focused_window();
        let active = self.output.as_ref().map(|o| o.name());
        let mut monitors = Vec::new();
        let size = self.screen_size();
        let region = self.region();
        let ws_rect = |number: u32| {
            self.ws_index(number).map(|i| {
                let r = self.ws_area(i);
                [r.loc.x, r.loc.y, r.size.w, r.size.h]
            })
        };
        if let Some(output) = &self.output {
            monitors.push(json!({
                "name": output.name(),
                "active": true,
                "position": [self.active_pos.x, self.active_pos.y],
                "size": [size.w, size.h],
                "camera": [self.view.camera.x, self.view.camera.y],
                "zoom": self.view.zoom,
                "region": [region.loc.x, region.loc.y, region.size.w, region.size.h],
                "workspace": self.home,
                "overview": self.overview.is_some(),
            }));
        }
        for monitor in &self.monitors {
            let mode_size = monitor.output.current_mode().map_or((0, 0), |m| {
                let scale = monitor.output.current_scale().fractional_scale();
                let s = m.size.to_f64().to_logical(scale).to_i32_round();
                (s.w, s.h)
            });
            monitors.push(json!({
                "name": monitor.output.name(),
                "active": false,
                "position": [monitor.pos.x, monitor.pos.y],
                "size": [mode_size.0, mode_size.1],
                "camera": [monitor.view.camera.x, monitor.view.camera.y],
                "zoom": monitor.view.zoom,
                "region": ws_rect(monitor.home)
                    .unwrap_or([monitor.pos.x, monitor.pos.y, mode_size.0, mode_size.1]),
                "workspace": monitor.home,
                "overview": monitor.overview.is_some(),
            }));
        }
        let windows: Vec<Value> = self
            .space
            .elements()
            .map(|window| {
                let rect = self.frame(window).unwrap_or_default();
                let (title, app_id) = crate::screencast::title_and_app_id(window);
                json!({
                    "id": window_id(window),
                    "app_id": app_id,
                    "title": title,
                    "rect": [rect.loc.x, rect.loc.y, rect.size.w, rect.size.h],
                    "tiled": self.is_tiled(window),
                    "workspace": self.ws_of(window).map(|i| self.workspaces[i].number),
                    "fullscreen": self.is_fullscreen(window),
                    "focused": focused.as_ref() == Some(window),
                    "hidden_from_screencast": crate::menu::hidden_from_capture(window),
                    "pid": self.window_pid(window),
                    "collapsed": false,
                    "recent": self.history.iter().position(|w| w == window),
                })
            })
            .chain(self.collapsed.iter().map(|c| {
                let (title, app_id) = crate::screencast::title_and_app_id(&c.window);
                let r = c.rect;
                json!({
                    "id": window_id(&c.window),
                    "app_id": app_id,
                    "title": title,
                    "rect": [r.loc.x, r.loc.y, r.size.w, r.size.h],
                    "tiled": false,
                    "workspace": c.workspace,
                    "fullscreen": false,
                    "focused": false,
                    "hidden_from_screencast": crate::menu::hidden_from_capture(&c.window),
                    "pid": self.window_pid(&c.window),
                    "collapsed": true,
                    "recent": self.history.iter().position(|w| *w == c.window),
                })
            }))
            .collect();
        let mut workspaces: Vec<Value> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, ws)| {
                let r = self.ws_area(i);
                json!({
                    "number": ws.number,
                    "rect": [r.loc.x, r.loc.y, r.size.w, r.size.h],
                    "tiles": ws.tiled.iter().map(|(w, _)| window_id(w)).collect::<Vec<_>>(),
                })
            })
            .collect();
        workspaces.sort_by_key(|w| w["number"].as_u64());
        json!({
            "active_monitor": active,
            "monitors": monitors,
            "workspaces": workspaces,
            "windows": windows,
            "locked": self.is_locked(),
        })
    }

    /// after a frame send the state to subscribers if it changed
    pub fn ipc_notify(&mut self) {
        if self.subscribers.is_empty() {
            return;
        }
        let state = self.ipc_state();
        if self.ipc_last.as_ref() == Some(&state) {
            // still uhh finish sending what a full socket held back
            self.subscribers
                .retain_mut(|s| s.pending.is_empty() || keep(s, &[]));
            return;
        }
        let text = format!("{}\n", json!({ "state": state }));
        self.subscribers.retain_mut(|s| keep(s, text.as_bytes()));
        self.ipc_last = Some(state);
    }
}

/// send to a subscriber and hang up on one thats gone or stuck
fn keep(subscriber: &mut Subscriber, text: &[u8]) -> bool {
    let ok = subscriber.send(text);
    if !ok {
        let _ = subscriber.stream.shutdown(std::net::Shutdown::Both);
    }
    ok
}
