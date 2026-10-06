use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Instant;

use smithay::backend::renderer::element::Id;
use smithay::desktop::{PopupManager, Space, Window, WindowSurfaceType};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken,
};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::wlr_layer::{Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

use crate::config::Config;
use crate::view::View;

/// called after a config reload to switch monitor modes
pub type ModeHook = Box<dyn FnMut(&mut Seven)>;

/// the whole compositor w every protocols state the windows and the seat
pub struct Seven {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,

    /// every window on the canvas back to front
    pub space: Space<Window>,
    pub popups: PopupManager,
    /// the one output
    pub output: Option<Output>,
    /// the camera onto the canvas
    pub view: View,
    /// collapsed windows and their markers
    pub collapsed: Vec<crate::collapse::Collapsed>,
    /// remembered windows and cameras and the last written session
    pub pending: Option<crate::session::Pending>,
    pub saved_monitors: Vec<crate::session::SavedMonitor>,
    /// the saved session had no workspaces so monitors dont get a new one at startup
    pub no_workspaces_saved: bool,
    pub session_saved: Option<crate::session::Session>,
    /// when a window was last open
    pub windows_seen: Instant,
    /// the decoration shaders compiled on the first frame and Err if they failed
    pub shaders: Option<Result<std::rc::Rc<crate::decorations::Shaders>, ()>>,
    /// closed windows last pictures and the renderer they belong to
    pub closing: Vec<crate::closing::Closing>,
    pub gles_context: Option<smithay::backend::renderer::ContextId<smithay::backend::renderer::gles::GlesTexture>>,
    /// this frames title bar images
    pub titlebars: std::collections::HashMap<Window, smithay::backend::renderer::element::memory::MemoryRenderBuffer>,
    /// every workspace on the canvas
    pub workspaces: Vec<crate::workspaces::Workspace>,
    /// the active monitors home workspace number
    pub home: u32,
    /// the workspace being dragged by number
    pub dragging_workspace: Option<u32>,
    /// the monitors that arent active
    pub monitors: Vec<crate::monitors::Monitor>,
    /// the active monitors top left in the layout
    pub active_pos: Point<i32, Logical>,
    /// where the pointer is in the monitor layout
    pub pointer_global: Point<f64, Logical>,
    /// the monitor the pointer is on which is the only one that draws the cursor
    pub pointer_output: Option<String>,
    /// where the pointer is on the active screen
    pub pointer_screen: Point<f64, Logical>,
    /// the window being dragged which follows the pointer without animating
    pub dragging: Option<Window>,
    /// the workspace a dragged window would uhh tile into if dropped
    pub drop_target: Option<u32>,
    /// open capture sessions
    pub capture_sessions: Vec<smithay::wayland::image_copy_capture::Session>,
    /// screen captures waiting for the next frame
    pub pending_captures: Vec<(
        smithay::wayland::image_copy_capture::Frame,
        smithay::wayland::image_copy_capture::SessionRef,
    )>,
    pub foreign_toplevel_list_state:
        smithay::wayland::foreign_toplevel_list::ForeignToplevelListState,
    pub toplevel_capture_source_state:
        smithay::wayland::image_capture_source::ToplevelCaptureSourceState,
    /// a freeze screenshot in progress
    pub freeze: Option<crate::capture::Freeze>,
    pub output_capture_source_state:
        smithay::wayland::image_capture_source::OutputCaptureSourceState,
    pub image_copy_capture_state: smithay::wayland::image_copy_capture::ImageCopyCaptureState,
    /// the open window menu
    pub menu: Option<crate::menu::Menu>,
    /// a tiles workspace menu shown beside its window menu while workspace is hovered
    pub submenu: Option<crate::menu::Menu>,
    /// a menu to open once this pointer event is done
    pub pending_menu: Option<Window>,
    pub wallpaper: crate::wallpaper::Wallpaper,
    /// when the config file was last loaded so we reload when it changes
    pub config_mtime: Option<std::time::SystemTime>,
    /// programs we started that get reaped once they exit
    pub children: std::cell::RefCell<Vec<Spawned>>,
    /// keep_running programs w their command process and last restart
    pub kept: Vec<(String, Spawned)>,
    pub kept_restarts: Vec<(String, Instant)>,
    /// the keep_running list as last applied or none when this run doesnt keep things running
    pub kept_config: Option<Vec<String>>,
    /// the ipc socket and clients following state changes
    pub ipc_path: Option<std::path::PathBuf>,
    pub subscribers: Vec<crate::ipc::Subscriber>,
    /// the state last sent to subscribers
    pub ipc_last: Option<serde_json::Value>,
    /// what that state was built from and when so unchanged or too quick frames skip it
    pub ipc_stamp: Option<crate::ipc::IpcStamp>,
    pub ipc_built: Option<Instant>,
    /// a timer is coming to send what a too quick frame held back
    pub ipc_timer: bool,
    /// the ui font loaded the first time its needed
    pub font: Option<crate::text::Font>,
    /// in the overview the view and window to go back to
    pub overview: Option<crate::tiling::Overview>,
    /// colors a shell pushed over ipc that stay on top of the config thru reloads
    pub color_override: Option<serde_json::Map<String, serde_json::Value>>,
    /// the window mod+q asked to close and where its middle was so focus goes to the nearest one after
    pub closed_by_key: Option<(Window, Point<f64, Logical>)>,
    /// exec-outside launches still waiting for their window
    pub open_outside: Vec<(Instant, String)>,
    /// stable ids for the drawn outlines so redraws reuse them
    pub decoration_ids: [Id; 9],
    /// new windows waiting for their first commit to get placed
    pub unplaced: Vec<Window>,
    /// windows by how recently they were focused
    pub history: Vec<Window>,
    /// spot in history while alt-tab is held
    pub cycle: Option<usize>,
    /// a window that just got focus and the mouse still has to move to w tries left
    pub warp_pending: Option<(Window, u8)>,
    /// fullscreen windows and the rect each goes back to
    pub fullscreen: Vec<(Window, Rectangle<i32, Logical>)>,

    pub config: Config,
    /// why the config didnt load at startup so we can say it once the desktop is up
    pub config_error: Option<String>,
    /// bumped by anything that can change the screen so an unchanged monitor can skip drawing
    pub damage_gen: std::cell::Cell<u64>,
    /// wakes the udev backend to draw idle monitors
    pub redraw: Option<smithay::reexports::calloop::ping::Ping>,
    /// something is moving in this frame so maybe draw the next one too
    pub animating: std::cell::Cell<bool>,
    /// running inside another compositor that keeps super for itself
    pub nested: bool,
    /// running the login screen and we quit once the greeter exits
    pub greeter: Option<String>,
    /// keys whose press ran a bind so their release never reaches a client
    pub suppressed_keys: HashSet<u32>,
    /// the held key whose bind is repeating and its timer bc libinput doesnt repeat
    pub key_repeat: Option<(u32, RegistrationToken)>,
    pub loop_handle: LoopHandle<'static, Seven>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    /// layer surfaces that already got the keyboard once
    pub layers_focused: Vec<ObjectId>,
    pub shm_state: ShmState,
    /// held js to keep the xdg-output protocol alive
    _output_manager_state: OutputManagerState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state:
        smithay::wayland::selection::primary_selection::PrimarySelectionState,
    pub wlr_data_control_state: smithay::wayland::selection::wlr_data_control::DataControlState,
    pub ext_data_control_state: smithay::wayland::selection::ext_data_control::DataControlState,
    /// what the cursor should look like as the focused app asked
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
    pub cursors: crate::cursor::Cursors,
    pub lock: crate::lock::Lock,
    /// the lock screen surface on each monitor
    pub lock_surfaces: Vec<(smithay::wayland::session_lock::LockSurface, Output)>,
    pub session_lock_state: smithay::wayland::session_lock::SessionLockManagerState,
    pub idle_notifier_state: smithay::wayland::idle_notify::IdleNotifierState<Seven>,
    _idle_inhibit_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    /// surfaces like video players keeping the screen on
    pub idle_inhibitors: Vec<WlSurface>,
    pub last_activity: Instant,
    /// this idle stretch already locked or suspended
    pub idle_locked: bool,
    pub idle_suspended: bool,
    /// when the lock command last ran bc the locker died
    pub locker_respawned: Option<Instant>,
    /// the screen is off for idle
    pub screen_off: bool,
    /// mice and touchpads kept to apply settings again on reload
    pub input_devices: Vec<smithay::reexports::input::Device>,
    /// xwayland-satellite and its x display
    pub xwayland: Option<crate::xwayland::XwaylandChild>,
    pub x_display: Option<String>,
    /// when xwayland-satellite last got restarted
    pub xwayland_restarts: Vec<Instant>,
    /// applies monitor modes after a reload on real hardware
    pub mode_hook: Option<ModeHook>,
    /// udev sets the monitors gamma thru this for night light
    pub gamma_hook: Option<crate::display::GammaHook>,
    /// the night light temperature last put on the monitors or none before the first try
    pub night_applied: Option<Option<u32>>,
    /// the shell asked to keep the screen awake like caffeine
    pub caffeine: bool,
    /// when something last copied the screen so the bar can show a screen share dot
    pub last_capture: Option<Instant>,
    /// the seat session on real hardware for vt switching
    pub session: Option<smithay::backend::session::libseat::LibSeatSession>,
    pub dmabuf_state: smithay::wayland::dmabuf::DmabufState,
    dmabuf_global: Option<smithay::wayland::dmabuf::DmabufGlobal>,
    pub activation_state: smithay::wayland::xdg_activation::XdgActivationState,
    /// held js to keep these protocols alive
    _fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState,
    _relative_pointer_state: smithay::wayland::relative_pointer::RelativePointerManagerState,
    _pointer_constraints_state: smithay::wayland::pointer_constraints::PointerConstraintsState,
    _viewporter_state: smithay::wayland::viewporter::ViewporterState,
    _xdg_decoration_state: smithay::wayland::shell::xdg::decoration::XdgDecorationState,
    /// apps name a cursor and we draw it so every cursor is the same size
    _cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState,
    /// draw the cursor ourselves on real hardware
    pub draw_cursor: bool,
    /// the icon of a drag and drop in progress
    pub dnd_icon: Option<WlSurface>,
    pub seat_state: SeatState<Seven>,
    pub seat: Seat<Seven>,
}

impl Seven {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        nested: bool,
    ) -> Self {
        let dh = display.handle();
        let mut config_error = None;
        let config = Config::load(nested).unwrap_or_else(|err| {
            tracing::warn!("config: {err}; using the defaults");
            config_error = Some(err);
            Config::from_toml("", nested).expect("the built-in config parses")
        });
        let cursors = crate::cursor::Cursors::new(&config.cursor);
        for warning in &config.warnings {
            tracing::warn!("config: {warning}");
        }

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        // the layout loads once the state exists and repeat settings go in now
        let kb = &config.input.keyboard;
        seat.add_keyboard(Default::default(), kb.repeat_delay, kb.repeat_rate)
            .expect("the default keymap compiles");
        seat.add_pointer();

        let socket_name = listen(display, event_loop);

        // clipboard managers do the clipboard thing thru data control
        let primary_selection_state =
            smithay::wayland::selection::primary_selection::PrimarySelectionState::new::<Self>(&dh);
        let wlr_data_control_state =
            smithay::wayland::selection::wlr_data_control::DataControlState::new::<Self, _>(
                &dh,
                Some(&primary_selection_state),
                |_| true,
            );
        let ext_data_control_state =
            smithay::wayland::selection::ext_data_control::DataControlState::new::<Self, _>(
                &dh,
                Some(&primary_selection_state),
                |_| true,
            );

        Self {
            start_time: Instant::now(),
            socket_name,
            loop_signal: event_loop.get_signal(),
            loop_handle: event_loop.handle(),
            key_repeat: None,
            space: Space::default(),
            popups: PopupManager::default(),
            output: None,
            view: View::default(),
            workspaces: Vec::new(),
            collapsed: Vec::new(),
            shaders: None,
            titlebars: Default::default(),
            closing: Vec::new(),
            gles_context: None,
            pending: None,
            saved_monitors: Vec::new(),
            no_workspaces_saved: false,
            session_saved: None,
            windows_seen: Instant::now(),
            home: 1,
            dragging_workspace: None,
            pointer_screen: Point::from((0.0, 0.0)),
            monitors: Vec::new(),
            active_pos: Point::from((0, 0)),
            pointer_global: Point::from((0.0, 0.0)),
            pointer_output: None,
            drop_target: None,
            dragging: None,
            overview: None,
            color_override: None,
            closed_by_key: None,
            open_outside: Vec::new(),
            menu: None,
            submenu: None,
            pending_menu: None,
            font: None,
            ipc_path: None,
            subscribers: Vec::new(),
            ipc_last: None,
            ipc_stamp: None,
            ipc_built: None,
            ipc_timer: false,
            wallpaper: crate::wallpaper::Wallpaper::default(),
            children: Default::default(),
            kept: Vec::new(),
            kept_restarts: Vec::new(),
            kept_config: None,
            config_mtime: crate::config::Config::path()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok()),
            capture_sessions: Vec::new(),
            pending_captures: Vec::new(),
            foreign_toplevel_list_state:
                smithay::wayland::foreign_toplevel_list::ForeignToplevelListState::new::<Self>(&dh),
            toplevel_capture_source_state:
                smithay::wayland::image_capture_source::ToplevelCaptureSourceState::new::<Self>(&dh),
            freeze: None,
            output_capture_source_state:
                smithay::wayland::image_capture_source::OutputCaptureSourceState::new::<Self>(&dh),
            image_copy_capture_state:
                smithay::wayland::image_copy_capture::ImageCopyCaptureState::new::<Self>(&dh),
            decoration_ids: std::array::from_fn(|_| Id::new()),
            unplaced: Vec::new(),
            history: Vec::new(),
            cycle: None,
            warp_pending: None,
            fullscreen: Vec::new(),
            config,
            config_error,
            damage_gen: Default::default(),
            redraw: None,
            animating: Default::default(),
            nested,
            greeter: None,
            suppressed_keys: HashSet::new(),
            compositor_state: CompositorState::new::<Self>(&dh),
            xdg_shell_state: XdgShellState::new::<Self>(&dh),
            layer_shell_state: WlrLayerShellState::new::<Self>(&dh),
            layers_focused: Vec::new(),
            shm_state: ShmState::new::<Self>(&dh, vec![]),
            _output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(&dh),
            data_device_state: DataDeviceState::new::<Self>(&dh),
            primary_selection_state,
            wlr_data_control_state,
            ext_data_control_state,
            dnd_icon: None,
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            cursors,
            draw_cursor: !nested,
            session: None,
            mode_hook: None,
            gamma_hook: None,
            night_applied: None,
            caffeine: false,
            last_capture: None,
            activation_state: smithay::wayland::xdg_activation::XdgActivationState::new::<Self>(
                &dh,
            ),
            _fractional_scale_state:
                smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<Self>(&dh),
            xwayland: None,
            x_display: None,
            xwayland_restarts: Vec::new(),
            lock: crate::lock::Lock::Unlocked,
            lock_surfaces: Vec::new(),
            session_lock_state: smithay::wayland::session_lock::SessionLockManagerState::new::<
                Self,
                _,
            >(&dh, |_| true),
            idle_notifier_state: smithay::wayland::idle_notify::IdleNotifierState::new(
                &dh,
                event_loop.handle(),
            ),
            _idle_inhibit_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Self>(
                &dh,
            ),
            idle_inhibitors: Vec::new(),
            last_activity: Instant::now(),
            idle_locked: false,
            idle_suspended: false,
            locker_respawned: None,
            screen_off: false,
            input_devices: Vec::new(),
            dmabuf_state: smithay::wayland::dmabuf::DmabufState::new(),
            dmabuf_global: None,
            _relative_pointer_state:
                smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Self>(&dh),
            _pointer_constraints_state:
                smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Self>(&dh),
            _viewporter_state: smithay::wayland::viewporter::ViewporterState::new::<Self>(&dh),
            _xdg_decoration_state:
                smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<Self>(&dh),
            _cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(
                &dh,
            ),
            seat_state,
            seat,
            display_handle: dh,
        }
    }

    /// give the window keyboard focus and raise it and make it most recent
    pub fn focus(&mut self, window: Option<&Window>) {
        // focusing a collapsed window brings it back
        if let Some(window) = window
            && self.is_collapsed(window)
        {
            self.restore(window);
            return;
        }
        if let Some(window) = window {
            crate::collapse::touch(window);
            self.cycle = None;
            self.history.retain(|w| w != window);
            self.history.insert(0, window.clone());
        }
        self.focus_without_history(window);
    }

    /// focus and raise without touching the recent order for alt-tab steps
    pub fn focus_without_history(&mut self, window: Option<&Window>) {
        if let Some(window) = window {
            self.uncover_tile(window);
            self.space.raise_element(window, true);
            self.restack();
            self.queue_warp(window);
        }
        self.set_keyboard_focus(window);
    }

    /// the mouse moved onto a window so focus it but leave the stacking alone
    pub fn focus_hovered(&mut self, window: &Window) {
        crate::collapse::touch(window);
        self.history.retain(|w| w != window);
        self.history.insert(0, window.clone());
        self.set_keyboard_focus(Some(window));
    }

    fn set_keyboard_focus(&mut self, window: Option<&Window>) {
        let serial = SERIAL_COUNTER.next_serial();
        for w in self.space.elements() {
            w.set_activated(Some(w) == window);
            if let Some(toplevel) = w.toplevel() {
                toplevel.send_pending_configure();
            }
        }
        if self.is_locked() {
            // only the lock screen gets the keyboard
            let surface = self.lock_surface().cloned();
            let keyboard = self.seat.get_keyboard().expect("the seat has a keyboard");
            keyboard.set_focus(self, surface, serial);
            return;
        }
        // an open launcher keeps the keyboard till it closes
        let surface = match self.exclusive_layer() {
            Some(layer) => Some(layer.wl_surface().clone()),
            None => window
                .and_then(|w| w.toplevel())
                .map(|t| t.wl_surface().clone()),
        };
        let keyboard = self.seat.get_keyboard().expect("the seat has a keyboard");
        keyboard.set_focus(self, surface, serial);
    }

    /// forget a window thats going away
    pub fn forget_window(&mut self, window: &Window) {
        if let Some(i) = self.history.iter().position(|w| w == window) {
            self.history.remove(i);
            // keep a cycle in progress pointing at the same window
            match self.cycle {
                Some(c) if c == i => self.cycle = None,
                Some(c) if c > i => self.cycle = Some(c - 1),
                _ => {}
            }
        }
        // closing the fullscreen window ur looking at puts the view back like leaving fullscreen does
        if self.is_fullscreen(window) {
            self.view_back_from_fullscreen(window);
        }
        self.fullscreen.retain(|(w, _)| w != window);
        self.collapsed.retain(|c| c.window != *window);
        self.space.unmap_elem(window);
        // whoever it pushed aside when it maximized or went fullscreen comes back
        self.pull_back_pushed(window);
        self.untile(window);
    }

    /// the window whose toplevel surface is surface
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .chain(&self.unplaced)
            .chain(self.collapsed.iter().map(|c| &c.window))
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    /// the surface under a screen point w its origin in canvas coords for smithays pointer
    pub fn surface_under(
        &self,
        screen: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let canvas = self.view.to_canvas(screen);
        if self.is_locked() {
            // the lock covers the screen so its local coords are the screens
            let surface = self.lock_surface()?;
            return Some((surface.clone(), canvas - screen));
        }
        let layer_hit = |layers: &[Layer]| {
            self.layer_surface_under(screen, layers)
                .map(|(_, surface, origin)| (surface, canvas - (screen - origin)))
        };
        let window_hit = || {
            let (window, loc) = self.space.element_under(canvas)?;
            window
                .surface_under(canvas - loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, offset)| (surface, (offset + loc).to_f64()))
        };
        layer_hit(&[Layer::Overlay, Layer::Top])
            .or_else(window_hit)
            .or_else(|| layer_hit(&[Layer::Bottom, Layer::Background]))
    }

    /// the window under a screen point if no uhhh panel covers it
    pub fn window_under(&self, screen: Point<f64, Logical>) -> Option<Window> {
        if self
            .layer_surface_under(screen, &[Layer::Overlay, Layer::Top])
            .is_some()
        {
            return None;
        }
        self.space
            .element_under(self.view.to_canvas(screen))
            .map(|(window, _)| window.clone())
    }

    /// let clients hand over gpu buffers in these formats
    pub fn enable_dmabuf(&mut self, feedback: &smithay::wayland::dmabuf::DmabufFeedback) {
        let global = self
            .dmabuf_state
            .create_global_with_default_feedback::<Self>(&self.display_handle, feedback);
        self.dmabuf_global = Some(global);
    }

    /// enable_dmabuf without per surface feedback for nested
    pub fn enable_dmabuf_formats(
        &mut self,
        formats: impl IntoIterator<Item = smithay::backend::allocator::Format>,
    ) {
        let global = self
            .dmabuf_state
            .create_global::<Self>(&self.display_handle, formats);
        self.dmabuf_global = Some(global);
    }

    /// after a frame tell clients to draw the next and send everything queued
    pub fn frame_done(&mut self, output: &smithay::output::Output) {
        let now = self.start_time.elapsed();
        let throttle = Some(std::time::Duration::ZERO);
        for window in self.space.elements() {
            window.send_frame(output, now, throttle, |_, _| Some(output.clone()));
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.send_frame(output, now, throttle, |_, _| Some(output.clone()));
        }
        if let Some(icon) = &self.dnd_icon {
            smithay::desktop::utils::send_frames_surface_tree(
                icon,
                output,
                now,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
        if let smithay::input::pointer::CursorImageStatus::Surface(surface) = &self.cursor_status {
            smithay::desktop::utils::send_frames_surface_tree(
                surface,
                output,
                now,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
        for (lock, _) in &self.lock_surfaces {
            smithay::desktop::utils::send_frames_surface_tree(
                lock.wl_surface(),
                output,
                now,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
        // a frame w only the lock got drawn so the lock holds
        self.confirm_lock();
        self.space.refresh();
        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    /// something might have changed so draw again
    pub fn damage(&self) {
        self.damage_gen.set(self.damage_gen.get().wrapping_add(1));
        if let Some(ping) = &self.redraw {
            ping.ping();
        }
    }

    /// run command thru the shell hooked up to this compositor
    pub fn spawn(&self, command: &str) {
        self.spawn_with(command, &[]);
    }

    /// spawn w extra env vars like the exec-outside tag
    pub fn spawn_with(&self, command: &str, env: &[(&str, &str)]) {
        tracing::info!("spawning {command}");
        let mut child = self.command(command);
        child.envs(env.iter().copied());
        match child.spawn() {
            Ok(child) => self.children.borrow_mut().push(Spawned::new(command, child)),
            Err(err) => tracing::warn!("failed to spawn {command}: {err}"),
        }
    }

    /// watch a program started some other way so it gets reaped
    pub fn watch_child(&self, command: &str, child: std::process::Child) {
        self.children.borrow_mut().push(Spawned::new(command, child));
    }

    /// run a keep_running command and restart it if it crashes
    pub fn spawn_kept(&mut self, command: &str) {
        tracing::info!("starting {command} (kept running)");
        match self.command(command).spawn() {
            Ok(child) => self.kept.push((command.to_string(), Spawned::new(command, child))),
            Err(err) => tracing::warn!("failed to spawn {command}: {err}"),
        }
    }

    /// after a reload start newly added keep_running programs and stop watching removed ones
    pub fn apply_keep_running(&mut self) {
        let Some(before) = self.kept_config.clone() else {
            return;
        };
        let wanted = self.config.keep_running.clone();
        let (kept, dropped): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.kept).into_iter().partition(|(c, _)| wanted.contains(c));
        self.kept = kept;
        self.children
            .borrow_mut()
            .extend(dropped.into_iter().map(|(_, spawned)| spawned));
        for command in &wanted {
            if !before.contains(command) {
                self.spawn_kept(command);
            }
        }
        self.kept_config = Some(wanted);
    }

    /// once a sec restart kept programs that crashed and give up after five in a minute
    pub fn keep_running(&mut self) {
        let mut crashed = Vec::new();
        let mut handed_over = Vec::new();
        self.kept.retain_mut(|(command, spawned)| match spawned.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                if !status.success() {
                    crashed.push((command.clone(), status));
                } else if spawned.started.elapsed() < HANDOVER {
                    handed_over.push(command.clone());
                }
                false
            }
            Err(_) => false,
        });
        for command in handed_over {
            let program = program_of(&command);
            let mut children = self.children.borrow_mut();
            let found = children.iter_mut().position(|c| {
                program_of(&c.command) == program && matches!(c.child.try_wait(), Ok(None))
            });
            if let Some(i) = found {
                let adopted = children.remove(i);
                tracing::info!("{command}: now watching '{}' instead", adopted.command);
                self.kept.push((command, adopted));
            }
        }
        let now = Instant::now();
        for (command, status) in crashed {
            self.kept_restarts
                .retain(|(_, at)| now.duration_since(*at) < std::time::Duration::from_secs(60));
            // each program gets its own five so one crashy program cant use up the shells
            if self.kept_restarts.iter().filter(|(c, _)| *c == command).count() >= 5 {
                tracing::warn!("{command} exited ({status}) again; not restarting it");
                self.notify(&format!("{command} keeps crashing; not restarting it"));
                continue;
            }
            tracing::warn!("{command} exited ({status}); restarting it");
            self.kept_restarts.push((command.clone(), now));
            self.spawn_kept(&command);
        }
    }

    /// command run thru the shell hooked up to this compositor
    fn command(&self, command: &str) -> std::process::Command {
        let mut child = std::process::Command::new("sh");
        child.args(["-c", command]);
        self.session_env(&mut child);
        child
    }

    /// point a program at this compositor
    pub fn session_env(&self, child: &mut std::process::Command) {
        child
            .env("WAYLAND_DISPLAY", &self.socket_name)
            // for apps that load the cursor themselves
            .env("XCURSOR_THEME", &self.config.cursor.theme)
            .env("XCURSOR_SIZE", self.config.cursor.size.to_string());
        // x11 apps go to our xwayland not the hosts
        match &self.x_display {
            Some(display) => child.env("DISPLAY", display),
            None => child.env_remove("DISPLAY"),
        };
    }

    /// once a sec on the login screen quit when the greeter is gone
    pub fn quit_after_greeter(&self) {
        let Some(command) = &self.greeter else {
            return;
        };
        if !self.children.borrow().iter().any(|c| c.command == *command) {
            tracing::info!("the greeter exited; quitting");
            self.loop_signal.stop();
        }
    }

    /// collect programs we started that exited
    pub fn reap_children(&self) {
        self.children
            .borrow_mut()
            .retain_mut(|c| matches!(c.child.try_wait(), Ok(None)));
    }
}

/// a clean exit this soon after start means it handed off to a running copy
const HANDOVER: std::time::Duration = std::time::Duration::from_secs(10);

/// a program sevenwm started w its command when and process
pub struct Spawned {
    pub command: String,
    pub started: Instant,
    pub child: std::process::Child,
}

impl Spawned {
    pub fn new(command: &str, child: std::process::Child) -> Self {
        Self {
            command: command.to_string(),
            started: Instant::now(),
            child,
        }
    }
}

/// the program a shell command runs without its uhh path
fn program_of(command: &str) -> &str {
    let first = command.split_whitespace().next().unwrap_or("");
    first.rsplit('/').next().unwrap_or(first)
}

/// open a wayland socket and hook it into the event loop and return its name
fn listen(display: Display<Seven>, event_loop: &mut EventLoop<'static, Seven>) -> OsString {
    let socket = ListeningSocketSource::new_auto().expect("a free wayland socket name");
    let socket_name = socket.socket_name().to_os_string();
    let handle = event_loop.handle();

    handle
        .insert_source(socket, |stream, _, state| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                tracing::warn!("failed to add client: {err}");
            }
        })
        .expect("the socket joins the event loop");

    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // safety the display never gets dropped while the loop runs
                unsafe { display.get_mut().dispatch_clients(state)? };
                // any client request can prolly change whats drawn
                state.damage();
                // answer right away instead of on the next redraw which a host can throttle
                let _ = state.display_handle.flush_clients();
                Ok(PostAction::Continue)
            },
        )
        .expect("the display joins the event loop");

    socket_name
}

/// per client data smithay needs
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
