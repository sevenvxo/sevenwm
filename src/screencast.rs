//! screen sharing where apps go thru the portal and xdg-desktop-portal-wlr does the capture thing for the screen or one window

use std::cell::RefCell;

use smithay::desktop::Window;
use smithay::wayland::compositor::with_states;
use smithay::wayland::foreign_toplevel_list::{
    ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
};
use smithay::wayland::image_capture_source::{
    ImageCaptureSource, ToplevelCaptureSourceHandler, ToplevelCaptureSourceState,
};
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use crate::state::Seven;

/// sends screen sharing to xdg-desktop-portal-wlr and everything else to the gtk portal
const PORTALS_CONF: &str = "[preferred]
default=gtk
org.freedesktop.impl.portal.ScreenCast=wlr
org.freedesktop.impl.portal.Screenshot=wlr
";

/// a windows entry in the window list we publish
#[derive(Default)]
struct ListEntry(RefCell<Option<ForeignToplevelHandle>>);

/// marks a capture source as one window by its list id
pub struct WindowSource(pub String);

impl ForeignToplevelListHandler for Seven {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

impl ToplevelCaptureSourceHandler for Seven {
    fn toplevel_capture_source_state(&mut self) -> &mut ToplevelCaptureSourceState {
        &mut self.toplevel_capture_source_state
    }

    fn toplevel_source_created(
        &mut self,
        source: ImageCaptureSource,
        toplevel: ForeignToplevelHandle,
    ) {
        source
            .user_data()
            .insert_if_missing(|| WindowSource(toplevel.identifier()));
    }
}

pub fn title_and_app_id(window: &Window) -> (String, String) {
    let Some(toplevel) = window.toplevel() else {
        return Default::default();
    };
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map(|data| {
                let data = data.lock().unwrap();
                (
                    data.title.clone().unwrap_or_default(),
                    data.app_id.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default()
    })
}

impl Seven {
    /// keep the windows list entry up to date and its cheap when nothing changed
    pub fn update_window_list(&mut self, window: &Window) {
        let (title, app_id) = title_and_app_id(window);
        let entry = window.user_data().get_or_insert(ListEntry::default);
        let mut entry = entry.0.borrow_mut();
        match &*entry {
            Some(handle) => {
                let mut changed = false;
                if handle.title() != title {
                    handle.send_title(&title);
                    changed = true;
                }
                if handle.app_id() != app_id {
                    handle.send_app_id(&app_id);
                    changed = true;
                }
                if changed {
                    handle.send_done();
                }
            }
            None => {
                *entry = Some(
                    self.foreign_toplevel_list_state
                        .new_toplevel::<Self>(title, app_id),
                );
            }
        }
    }

    pub fn remove_from_window_list(&mut self, window: &Window) {
        let handle = window
            .user_data()
            .get::<ListEntry>()
            .and_then(|entry| entry.0.borrow_mut().take());
        if let Some(handle) = handle {
            handle.send_closed();
            self.foreign_toplevel_list_state.remove_toplevel(&handle);
        }
    }

    /// the window a capture source names if any
    pub fn window_for_source(&self, source: &ImageCaptureSource) -> Option<Window> {
        let id = &source.user_data().get::<WindowSource>()?.0;
        // collapsed windows too so a share survives collapsing
        self.space
            .elements()
            .chain(self.collapsed.iter().map(|c| &c.window))
            .find(|w| {
                w.user_data()
                    .get::<ListEntry>()
                    .and_then(|e| e.0.borrow().as_ref().map(|h| &h.identifier() == id))
                    .unwrap_or(false)
            })
            .cloned()
    }

    /// as the session tell the portal about the desktop and where sharing requests go
    pub fn setup_portal(&self) {
        // safety still single threaded startup
        unsafe {
            std::env::set_var("XDG_CURRENT_DESKTOP", "sevenwm");
            std::env::set_var("XDG_SESSION_TYPE", "wayland");
        }
        if let Some(home) = std::env::var_os("HOME") {
            let path =
                std::path::Path::new(&home).join(".config/xdg-desktop-portal/sevenwm-portals.conf");
            if !path.exists() {
                let written = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&path, PORTALS_CONF));
                if let Err(err) = written {
                    tracing::warn!("screen sharing: couldn't write {}: {err}", path.display());
                }
            }
        }
        // the portal runs under systemd so give it our env and restart it
        self.spawn(
            "dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY \
             XDG_CURRENT_DESKTOP XDG_SESSION_TYPE; \
             systemctl --user restart xdg-desktop-portal xdg-desktop-portal-wlr",
        );
    }
}
