//! the wayland protocols sevenwm speaks where smithay asks what to do when a client does something

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{PopupKeyboardGrab, PopupPointerGrab, PopupUngrabStrategy};
use smithay::desktop::{PopupKind, Window, WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output};
use smithay::input::dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::pointer::{Focus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{IsAlive, Serial};
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, get_parent, is_sync_subsurface,
    with_states,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, with_fractional_scale};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::wayland::selection::{ext_data_control, wlr_data_control};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

use crate::grabs::DragKind;
use crate::state::{ClientState, Seven};

impl CompositorHandler for Seven {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState")
            .compositor_state
    }

    /// a client finished updating a surface so grab its buffer and answer a first commit w a configure
    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
                self.update_window_list(&window);
                self.settle_new_window(&window);
                tracing::trace!(geometry = ?window.geometry(), "window commit");
            }
        }

        if let Some(window) = self.window_for_surface(surface) {
            let configured = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .is_some_and(|data| data.lock().unwrap().initial_configure_sent)
            });
            if !configured && let Some(toplevel) = window.toplevel() {
                if let Some(i) = self.unplaced.iter().position(|w| *w == window) {
                    self.unplaced.remove(i);
                    self.place_new_window(window.clone());
                    if window.user_data().get::<crate::actions::WantsFullscreen>().is_some() {
                        self.fullscreen(&window);
                    }
                }
                toplevel.send_configure();
            }
        }

        self.layer_commit(surface);

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            // the first configure is always allowed
            let _ = popup.send_configure();
        }
    }
}

impl XdgShellHandler for Seven {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    /// placement waits for the first commit when the app id and title are set
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.unplaced.push(Window::new_wayland_window(surface));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        tracing::info!("window closed");
        // only a window that had the keyboard hands it on so closing one in the back doesnt steal focus
        let had_focus = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|f| !f.alive() || f == *surface.wl_surface());
        self.unplaced
            .retain(|w| w.toplevel().is_some_and(|t| t != &surface));
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            if self.menu.as_ref().is_some_and(|m| m.window() == Some(&window)) {
                self.close_menu();
            }
            self.remove_from_window_list(&window);
            self.keep_closing_picture(&window);
            self.forget_window(&window);
        }
        // mod+q goes to the closest window left and anything else gives focus to the most recent one
        let closed_by_key = self
            .closed_by_key
            .take_if(|(w, _)| w.toplevel().is_some_and(|t| t == &surface));
        if let Some((_, from)) = closed_by_key {
            match self.nearest_window(from) {
                Some(next) => {
                    self.focus(Some(&next));
                    self.bring_into_view(&next);
                }
                None => self.focus(None),
            }
        } else if had_focus {
            let next = self.most_recent_window();
            self.focus(next.as_ref());
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.keep_popup_on_screen(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("failed to track popup: {err}");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.keep_popup_on_screen(&surface);
        surface.send_repositioned(token);
    }

    /// the client wants to be dragged bc its title bar was pressed
    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        let Some(button) = self.client_drag_button(&surface, serial) else {
            return;
        };
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.start_drag(window, DragKind::Move, button, serial);
        }
    }

    /// the client wants to be resized from an edge
    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some(button) = self.client_drag_button(&surface, serial) else {
            return;
        };
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.start_drag(window, DragKind::Resize(edges.into()), button, serial);
        }
    }

    /// the apps own minimize button collapses it into a marker
    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.collapse(&window);
        }
    }

    /// the apps uhh maximize button or a double click on its header bar
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.maximize(&window);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.unmaximize(&window);
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        if let Some(window) = self.window_for_surface(surface.wl_surface())
            && !self.is_fullscreen(&window)
        {
            self.fullscreen(&window);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.unfullscreen(&window);
        }
    }

    /// a menu wants the pointer and keyboard so a click anywhere else closes it
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        // behind the lock no app menu can take the keyboard
        if self.is_locked() {
            let _ = smithay::desktop::PopupManager::dismiss_popup(&root, &kind);
            return;
        }
        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else {
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
}

impl Seven {
    /// the button behind a move or resize request if its real and still held
    fn client_drag_button(&self, surface: &ToplevelSurface, serial: Serial) -> Option<u32> {
        let pointer = self.seat.get_pointer()?;
        if !pointer.has_grab(serial) {
            return None;
        }
        let start = pointer.grab_start_data()?;
        let (focus, _) = start.focus.as_ref()?;
        focus
            .id()
            .same_client_as(&surface.wl_surface().id())
            .then_some(start.button)
    }

    /// move a popup back on screen if its positioner allows it
    pub fn keep_popup_on_screen(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self.window_for_surface(&root) else {
            self.keep_layer_popup_on_screen(popup, &root);
            return;
        };
        let Some(window_geo) = self.space.element_geometry(&window) else {
            return;
        };
        // keep it inside the part of the canvas on screen
        let mut target = self.view.visible(self.screen_size()).to_i32_round();
        // the positioner works relative to the popups parent
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    /// a layer surface popup like the bar menus stays on that layers monitor
    fn keep_layer_popup_on_screen(&self, popup: &PopupSurface, root: &WlSurface) {
        for output in self.space.outputs() {
            let map = layer_map_for_output(output);
            let Some(layer) = map.layer_for_surface(root, WindowSurfaceType::TOPLEVEL) else {
                continue;
            };
            let (Some(layer_geo), Some(output_geo)) =
                (map.layer_geometry(layer), self.space.output_geometry(output))
            else {
                return;
            };
            let mut target = Rectangle::from_size(output_geo.size);
            target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
            target.loc -= layer_geo.loc;
            popup.with_pending_state(|state| {
                state.geometry = state.positioner.get_unconstrained_geometry(target);
            });
            return;
        }
    }
}

impl SeatHandler for Seven {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    /// the clipboard and primary selection follow keyboard focus
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }
}

// cursor-shape asks for this but theres no real tablet support
impl smithay::input::tablet::TabletSeatHandler for Seven {
    type ToolFocus = WlSurface;
}

impl BufferHandler for Seven {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Seven {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SelectionHandler for Seven {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Seven {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for Seven {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
    }

    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        self.dnd_icon = None;
    }
}

impl WaylandDndGrabHandler for Seven {
    /// dragging between clients where the icon follows the cursor till the drop
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        let GrabType::Pointer = type_ else {
            // touch isnt supported
            source.cancel();
            return;
        };
        let Some(pointer) = seat.get_pointer() else {
            source.cancel();
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            source.cancel();
            return;
        };
        self.dnd_icon = icon;
        let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
        pointer.set_grab(self, grab, serial, Focus::Keep);
    }
}

impl PrimarySelectionHandler for Seven {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl wlr_data_control::DataControlHandler for Seven {
    fn data_control_state(&mut self) -> &mut wlr_data_control::DataControlState {
        &mut self.wlr_data_control_state
    }
}

impl ext_data_control::DataControlHandler for Seven {
    fn data_control_state(&mut self) -> &mut ext_data_control::DataControlState {
        &mut self.ext_data_control_state
    }
}

impl OutputHandler for Seven {}

/// apps asking to be brought forward like a link opening in the uhhh browser
impl XdgActivationHandler for Seven {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // a stale token or one not from a real click or key press cant steal focus
        let from_input = token_data.serial.as_ref().is_some_and(|(serial, seat)| {
            Seat::<Self>::from_resource(seat)
                .and_then(|seat| seat.get_keyboard())
                .and_then(|k| k.last_enter())
                .is_none_or(|last| serial.is_no_older_than(&last))
        });
        if token_data.timestamp.elapsed() < ACTIVATION_TIMEOUT
            && from_input
            && let Some(window) = self.window_for_surface(&surface)
        {
            self.focus(Some(&window));
            self.bring_into_view(&window);
        }
        self.activation_state.remove_token(&token);
    }
}

/// tell each app the output scale as soon as it asks
impl FractionalScaleHandler for Seven {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self
            .output
            .as_ref()
            .map_or(1.0, |o| o.current_scale().fractional_scale());
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale));
        });
    }
}

/// how long an activation token stays good
const ACTIVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl DmabufHandler for Seven {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    /// accept the buffer and let the renderer fail on a bad one later
    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let _ = notifier.successful::<Self>();
    }
}

/// games lock or confine the pointer and it kicks in right away if the pointer is already there
impl PointerConstraintsHandler for Seven {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if pointer.current_focus().as_ref() == Some(surface) {
            smithay::wayland::pointer_constraints::with_pointer_constraint(
                surface,
                pointer,
                |constraint| {
                    if let Some(constraint) = constraint {
                        constraint.activate();
                    }
                },
            );
        }
    }
}

smithay::delegate_dispatch2!(Seven);
