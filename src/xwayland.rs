//! x11 apps run thru xwayland-satellite so sevenwm needs no x11 code

use std::path::Path;
use std::process::{Child, Command};

use crate::state::Seven;

impl Seven {
    /// start xwayland-satellite on a free display and point every app at it
    pub fn start_xwayland(&mut self) {
        if !self.config.xwayland.enabled {
            return;
        }
        let Some(display) = free_display() else {
            tracing::warn!("xwayland: no free X display number");
            return;
        };
        let name = format!(":{display}");
        match Command::new(&self.config.xwayland.path)
            .arg(&name)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env_remove("DISPLAY")
            .spawn()
        {
            Ok(child) => {
                tracing::info!("X11 apps: DISPLAY={name}");
                self.x_display = Some(name);
                self.xwayland = Some(child);
            }
            Err(err) => tracing::warn!(
                "xwayland: couldn't start {} ({err}); X11 apps won't run",
                self.config.xwayland.path
            ),
        }
    }

    /// every sec it does a thing where xwayland-satellite gets restarted if it died but it gives up on a crash loop
    pub fn xwayland_tick(&mut self) {
        let Some(child) = &mut self.xwayland else {
            return;
        };
        let Ok(Some(status)) = child.try_wait() else {
            return;
        };
        self.xwayland = None;
        self.x_display = None;
        let now = std::time::Instant::now();
        self.xwayland_restarts
            .retain(|at| now.duration_since(*at) < std::time::Duration::from_secs(60));
        if self.xwayland_restarts.len() >= 5 {
            tracing::warn!("xwayland-satellite exited ({status}) again; not restarting it");
            return;
        }
        tracing::warn!("xwayland-satellite exited ({status}); restarting it");
        self.xwayland_restarts.push(now);
        self.start_xwayland();
    }

    pub fn stop_xwayland(&mut self) {
        if let Some(mut child) = self.xwayland.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// the lowest x display number w no lock file or socket
fn free_display() -> Option<u32> {
    (1..64).find(|n| {
        !Path::new(&format!("/tmp/.X{n}-lock")).exists()
            && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
    })
}

/// a running xwayland-satellite
pub type XwaylandChild = Child;
