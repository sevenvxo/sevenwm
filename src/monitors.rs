//! all monitors look at one canvas and the one under the pointer is active w its camera in Sevens own fields

use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::state::Seven;
use crate::view::View;

/// a monitor that isnt the active one
pub struct Monitor {
    pub output: Output,
    /// top left in the monitor layout which is also where its workspace sits
    pub pos: Point<i32, Logical>,
    pub view: View,
    /// its home workspace number
    pub home: u32,
    pub overview: Option<(Point<f64, Logical>, f64)>,
}

pub fn size_of(output: &Output) -> Size<i32, Logical> {
    output
        .current_mode()
        .map_or(Size::from((1280, 720)), |mode| {
            let scale = output.current_scale().fractional_scale();
            mode.size.to_f64().to_logical(scale).to_i32_round()
        })
}

impl Seven {
    /// every monitor output w the active one first
    pub fn outputs(&self) -> Vec<Output> {
        self.output
            .iter()
            .cloned()
            .chain(self.monitors.iter().map(|m| m.output.clone()))
            .collect()
    }

    /// every monitor rect in the uhhhh layout
    fn monitor_rects(&self) -> Vec<(Output, Rectangle<i32, Logical>)> {
        let mut rects: Vec<_> = self
            .output
            .iter()
            .map(|o| (o.clone(), Rectangle::new(self.active_pos, size_of(o))))
            .collect();
        rects.extend(
            self.monitors
                .iter()
                .map(|m| (m.output.clone(), Rectangle::new(m.pos, size_of(&m.output)))),
        );
        rects
    }

    /// a monitor got plugged in so it goes right of the others on its own workspace
    pub fn add_monitor(&mut self, output: &Output) {
        self.apply_scale(output);
        let right = self
            .monitor_rects()
            .iter()
            .map(|(_, r)| r.loc.x + r.size.w)
            .max()
            .unwrap_or(0);
        let pos = self
            .config
            .monitor(&output.name())
            .and_then(|m| m.position)
            .map_or(Point::from((right, 0)), |[x, y]| Point::from((x, y)));
        self.space.map_output(output, pos);
        // sized to this monitor whichever is active rn
        let active = self.output.replace(output.clone());
        let home = self.home_for_monitor(&output.name(), pos);
        self.output = active;
        let mut view = View::default();
        if let Some(i) = self.ws_index(home) {
            view.set(self.workspaces[i].rect.loc.to_f64(), 1.0);
        }
        if self.output.is_none() {
            self.output = Some(output.clone());
            self.active_pos = pos;
            self.view = view;
            self.home = home;
        } else {
            self.monitors.push(Monitor {
                output: output.clone(),
                pos,
                view,
                home,
                overview: None,
            });
        }
        self.resize_workspaces();
        self.restore_monitor_view(&output.name());
        tracing::info!("monitor {} at {pos:?}", output.name());
        self.publish_monitors();
    }

    /// set a monitor scale from its entry
    fn apply_scale(&self, output: &Output) {
        let scale = self
            .config
            .monitor(&output.name())
            .and_then(|m| m.scale)
            .unwrap_or(1.0);
        if (output.current_scale().fractional_scale() - scale).abs() > f64::EPSILON {
            output.change_current_state(
                None,
                None,
                Some(smithay::output::Scale::Fractional(scale)),
                None,
            );
        }
    }

    /// after a config change put every monitor where its entry says and apply scales
    pub fn arrange_monitors(&mut self) {
        let active = self.output.clone();
        let outputs = self.outputs();
        for output in &outputs {
            self.apply_scale(output);
        }
        // unplaced monitors go past the right edge of the placed ones
        let mut right = outputs
            .iter()
            .filter_map(|o| {
                let [x, _] = self.config.monitor(&o.name()).and_then(|m| m.position)?;
                Some(x + size_of(o).w)
            })
            .max()
            .unwrap_or(0);
        for output in &outputs {
            let pos = match self.config.monitor(&output.name()).and_then(|m| m.position) {
                Some([x, y]) => Point::from((x, y)),
                None => {
                    let pos = Point::from((right, 0));
                    right += size_of(output).w;
                    pos
                }
            };
            self.activate(output);
            if pos != self.active_pos {
                self.active_pos = pos;
                self.space.map_output(output, pos);
            }
        }
        if let Some(active) = active {
            self.activate(&active);
        }
        self.resize_workspaces();
        self.publish_monitors();
        self.update_surface_scales();
    }

    /// tell the settings app which monitors there are in monitors.json
    pub fn publish_monitors(&self) {
        let Some(dir) = runtime_dir() else {
            return;
        };
        let monitors: Vec<serde_json::Value> = self
            .monitor_rects()
            .into_iter()
            .map(|(output, rect)| {
                let mode = output.current_mode();
                serde_json::json!({
                    "name": output.name(),
                    "position": [rect.loc.x, rect.loc.y],
                    "size": [rect.size.w, rect.size.h],
                    "scale": output.current_scale().fractional_scale(),
                    "mode": mode.map(|m| format!("{}x{}@{}", m.size.w, m.size.h, (m.refresh + 500) / 1000)),
                    "modes": output.user_data().get::<AvailableModes>().map(|m| m.0.clone()).unwrap_or_default(),
                })
            })
            .collect();
        let _ = std::fs::write(
            dir.join("monitors.json"),
            serde_json::to_string_pretty(&monitors).unwrap_or_default(),
        );
    }
}

/// a monitors modes as WxH@Hz for the settings app
pub struct AvailableModes(pub Vec<String>);

/// the runtime sevenwm folder made if its maybe missing
pub fn runtime_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("sevenwm");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

impl Seven {
    /// once a sec reload the config if the file changed
    pub fn reload_if_changed(&mut self) {
        let modified = crate::config::Config::path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        if modified.is_some() && modified != self.config_mtime {
            self.config_mtime = modified;
            self.reload_config();
        }
    }

    /// a monitor got unplugged but its workspaces stay and the ones to the right close the gap
    pub fn remove_monitor(&mut self, output: &Output) {
        if self.output.as_ref() != Some(output)
            && !self.monitors.iter().any(|m| m.output == *output)
        {
            return;
        }
        // make it active so its data is in the fields then hand off
        self.activate(output);
        // remember where it was looking so a quick unplug and replug lands u in the same spot
        let (camera, zoom) = self.view.destination();
        self.saved_monitors.retain(|m| m.name != output.name());
        self.saved_monitors.push(crate::session::SavedMonitor {
            name: output.name(),
            camera: [camera.x, camera.y],
            zoom,
            home: self.home,
        });
        self.space.unmap_output(output);
        let Some(next) = (!self.monitors.is_empty()).then(|| self.monitors.remove(0)) else {
            // the last monitor so keep everything for when one comes back
            self.output = None;
            return;
        };
        let removed_width = size_of(output).w;
        let removed_x = self.active_pos.x;
        self.output = Some(next.output);
        self.active_pos = next.pos;
        self.view = next.view;
        self.home = next.home;
        self.overview = next.overview;

        // close the gap by shifting monitors on the right to the left
        let shift = |pos: &mut Point<i32, Logical>| {
            if pos.x > removed_x {
                pos.x -= removed_width;
            }
        };
        shift(&mut self.active_pos);
        if let Some(output) = self.output.clone() {
            self.space.map_output(&output, self.active_pos);
        }
        for monitor in &mut self.monitors {
            shift(&mut monitor.pos);
            self.space.map_output(&monitor.output, monitor.pos);
        }
        self.set_pointer_global(self.pointer_global);
        self.retile();
        self.publish_monitors();
        tracing::info!("monitor {} removed", output.name());
    }

    /// make outputs monitor the active one
    pub fn activate(&mut self, output: &Output) {
        if self.output.as_ref() == Some(output) {
            return;
        }
        let Some(i) = self.monitors.iter().position(|m| m.output == *output) else {
            return;
        };
        let incoming = &mut self.monitors[i];
        let Some(current) = self.output.replace(incoming.output.clone()) else {
            return;
        };
        incoming.output = current;
        std::mem::swap(&mut incoming.pos, &mut self.active_pos);
        std::mem::swap(&mut incoming.view, &mut self.view);
        std::mem::swap(&mut incoming.home, &mut self.home);
        std::mem::swap(&mut incoming.overview, &mut self.overview);
    }

    /// tell every app the scale of the monitor its shown on
    pub fn update_surface_scales(&self) {
        use smithay::wayland::fractional_scale::with_fractional_scale;
        let scale_of = |o: &Output| o.current_scale().fractional_scale();
        let fallback = self.output.as_ref().map_or(1.0, scale_of);
        let mut views = Vec::new();
        if let Some(output) = &self.output {
            views.push((self.view.visible(self.screen_size()), scale_of(output)));
        }
        views.extend(
            self.monitors
                .iter()
                .map(|m| (m.view.visible(size_of(&m.output)), scale_of(&m.output))),
        );
        // w the states with_surfaces gives us bc locking them again would deadlock
        let set = |states: &smithay::wayland::compositor::SurfaceData, scale: f64| {
            with_fractional_scale(states, |f| f.set_preferred_scale(scale));
        };
        for window in self.space.elements() {
            let Some(bbox) = self.space.element_bbox(window) else {
                continue;
            };
            let centre = Point::from((
                bbox.loc.x as f64 + bbox.size.w as f64 / 2.0,
                bbox.loc.y as f64 + bbox.size.h as f64 / 2.0,
            ));
            let scale = views
                .iter()
                .find(|(visible, _)| visible.contains(centre))
                .map_or(fallback, |(_, scale)| *scale);
            window.with_surfaces(|_, states| set(states, scale));
        }
        for output in self.outputs() {
            let scale = scale_of(&output);
            let layers: Vec<_> = smithay::desktop::layer_map_for_output(&output)
                .layers()
                .cloned()
                .collect();
            for layer in layers {
                layer.with_surfaces(|_, states| set(states, scale));
            }
        }
    }

    /// the scale sevenwm draws its own pictures at which is the highest monitor scale rounded up
    pub fn ui_scale(&self) -> i32 {
        self.outputs()
            .iter()
            .map(|o| o.current_scale().fractional_scale())
            .fold(1.0, f64::max)
            .ceil()
            .clamp(1.0, 4.0) as i32
    }

    /// the monitor the pointer is on
    pub fn pointer_monitor(&self) -> Option<Output> {
        let outputs = self.outputs();
        self.pointer_output
            .as_ref()
            .and_then(|name| outputs.iter().find(|o| o.name() == *name).cloned())
            .or_else(|| self.output.clone())
    }

    /// the monitor that has the layout point global
    pub fn monitor_at(&self, global: Point<f64, Logical>) -> Option<Output> {
        self.monitor_rects()
            .into_iter()
            .find(|(_, r)| r.to_f64().contains(global))
            .map(|(o, _)| o)
    }

    /// keep a layout point on some monitor by clamping it into the nearest one
    pub fn clamp_to_monitors(&self, global: Point<f64, Logical>) -> Point<f64, Logical> {
        let clamp_into = |r: Rectangle<i32, Logical>| {
            Point::from((
                global
                    .x
                    .clamp(r.loc.x as f64, (r.loc.x + r.size.w) as f64 - 1.0),
                global
                    .y
                    .clamp(r.loc.y as f64, (r.loc.y + r.size.h) as f64 - 1.0),
            ))
        };
        self.monitor_rects()
            .into_iter()
            .map(|(_, r)| clamp_into(r))
            .min_by(|a: &Point<f64, Logical>, b: &Point<f64, Logical>| {
                let d =
                    |p: &Point<f64, Logical>| (p.x - global.x).powi(2) + (p.y - global.y).powi(2);
                d(a).total_cmp(&d(b))
            })
            .unwrap_or(global)
    }

    /// point the pointer at global and make the monitor under it active
    pub fn set_pointer_global(&mut self, global: Point<f64, Logical>) {
        let global = self.clamp_to_monitors(global);
        if let Some(output) = self.monitor_at(global) {
            self.activate(&output);
            self.pointer_output = Some(output.name());
        }
        self.pointer_global = global;
        self.pointer_screen = global - self.active_pos.to_f64();
    }
}
