//! layer shell for panels launchers notifications and wallpapers that sit around the windows

use smithay::desktop::{LayerSurface, PopupKind, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface};
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
    WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::wayland::shell::xdg::PopupSurface;

use crate::state::Seven;

impl WlrLayerShellHandler for Seven {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let Some(output) = output
            .as_ref()
            .and_then(Output::from_resource)
            // none named so use the monitor in use
            .or_else(|| self.pointer_monitor())
            .or_else(|| self.space.outputs().next().cloned())
        else {
            tracing::warn!("layer surface '{namespace}' has no output to go on");
            return;
        };
        tracing::info!("layer surface opened: {namespace}");
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!("failed to map layer surface: {err}");
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        // menus from panels like a tray icons right click menu
        self.keep_popup_on_screen(&popup);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(popup)) {
            tracing::warn!("failed to track layer popup: {err}");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let wl_surface = surface.wl_surface().clone();
        let mut picker = false;
        let mut zone_changed = false;
        for output in self.space.outputs() {
            let mut map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = layer {
                picker |= layer.namespace() == crate::capture::PICKER_NAMESPACE;
                let zone = map.non_exclusive_zone();
                map.unmap_layer(&layer);
                zone_changed |= zone != map.non_exclusive_zone();
            }
        }
        if picker {
            self.picker_closed();
        }
        // a bar that went away gives its strip back to the tiles
        if zone_changed {
            self.retile();
        }
        self.layers_focused.retain(|id| *id != wl_surface.id());
        // a closing launcher gives the keyboard back to the last window
        let focused = self.seat.get_keyboard().and_then(|k| k.current_focus());
        if focused.as_ref() == Some(&wl_surface) || focused.is_none() {
            let next = self.most_recent_window();
            self.focus(next.as_ref());
        }
    }
}

impl Seven {
    /// a commit on a layer surface so lay things out and give it the keyboard if it wants it
    pub fn layer_commit(&mut self, surface: &WlSurface) {
        let Some(output) = self.space.outputs().find(|o| {
            layer_map_for_output(o)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        }) else {
            return;
        };
        let output = output.clone();
        let (layer, zone_changed) = {
            let mut map = layer_map_for_output(&output);
            let zone = map.non_exclusive_zone();
            // arrange first so the configure has the right size
            map.arrange();
            (
                map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .cloned(),
                zone != map.non_exclusive_zone(),
            )
        };
        // a bar that showed up or changed size after windows tiled so they move out from under it
        if zone_changed {
            self.retile();
        }
        let Some(layer) = layer else {
            return;
        };

        let configured = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .is_some_and(|data| data.lock().unwrap().initial_configure_sent)
        });
        if !configured {
            layer.layer_surface().send_configure();
            return;
        }

        if wants_keyboard(&layer) && !self.layers_focused.contains(&surface.id()) {
            self.layers_focused.push(surface.id());
            self.focus_layer(&layer);
        }
    }

    pub fn focus_layer(&mut self, layer: &LayerSurface) {
        // behind the lock only the lock screen gets keys
        if self.is_locked() {
            return;
        }
        let keyboard = self.seat.get_keyboard().expect("the seat has a keyboard");
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Some(layer.wl_surface().clone()), serial);
    }

    /// a top layer that grabbed the keyboard like an open launcher
    pub fn exclusive_layer(&self) -> Option<LayerSurface> {
        self.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            map.layers()
                .rev()
                .find(|l| {
                    matches!(l.layer(), Layer::Top | Layer::Overlay)
                        && l.cached_state().keyboard_interactivity
                            == KeyboardInteractivity::Exclusive
                })
                .cloned()
        })
    }

    /// the layer surface under a screen point topmost first
    pub fn layer_surface_under(
        &self,
        screen: Point<f64, Logical>,
        layers: &[Layer],
    ) -> Option<(LayerSurface, WlSurface, Point<f64, Logical>)> {
        let output = self.output.as_ref()?;
        let map = layer_map_for_output(output);
        // a fullscreen window covers every panel but overlays so clicks cant reach the bar under it
        let covered = self.covering_fullscreen().is_some();
        layers.iter().find_map(|&layer| {
            let hit = map.layer_under(layer, screen)?;
            // but a launcher on the top layer that holds the keyboard is drawn over it so it takes clicks too
            if covered && layer != Layer::Overlay && !(layer == Layer::Top && grabs_keyboard(hit)) {
                return None;
            }
            let layer_loc = map.layer_geometry(hit)?.loc;
            hit.surface_under(screen - layer_loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, offset)| (hit.clone(), surface, (offset + layer_loc).to_f64()))
        })
    }

    /// the fullscreen window filling the active screen right now if theres one and the topmost if theres more
    pub fn covering_fullscreen(&self) -> Option<smithay::desktop::Window> {
        let view = &self.view;
        if view.zoom != 1.0 || self.fullscreen.is_empty() {
            return None;
        }
        self.space
            .elements()
            .rev()
            .find(|window| {
                self.is_fullscreen(window)
                    && self
                        .space
                        .element_location(window)
                        .is_some_and(|loc| view.camera == loc.to_f64())
            })
            .cloned()
    }

    /// does the layout thing for every outputs layers again
    pub fn arrange_layers(&self) {
        for output in self.space.outputs() {
            layer_map_for_output(output).arrange();
        }
    }
}

/// whether a layer surface holds the keyboard for itself like an open launcher
pub fn grabs_keyboard(layer: &LayerSurface) -> bool {
    layer.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
}

/// whether a layer surface takes the keyboard when it opens
pub fn wants_keyboard(layer: &LayerSurface) -> bool {
    layer.cached_state().keyboard_interactivity != KeyboardInteractivity::None
}

/// how far an outputs panels are slid away and where that slide started
#[derive(Default)]
struct PanelSlide(std::cell::Cell<Option<(bool, std::time::Instant, f64)>>);

/// how far the panels are slid off from 0 shown to 1 gone and it starts sliding when hide flips
pub fn panel_slide(output: &Output, hide: bool, ms: u64, curve: crate::animation::Curve) -> f64 {
    let slot = output.user_data().get_or_insert(PanelSlide::default);
    let now = std::time::Instant::now();
    let at = |(hiding, start, from): (bool, std::time::Instant, f64)| {
        let t = if ms == 0 {
            1.0
        } else {
            now.duration_since(start).as_secs_f64() * 1000.0 / ms as f64
        };
        let to = if hiding { 1.0 } else { 0.0 };
        (from + (to - from) * curve.ease(t)).clamp(0.0, 1.0)
    };
    let slide = slot.0.get();
    let slid = slide.map_or(0.0, at);
    match slide {
        Some((hiding, _, _)) if hiding == hide => slid,
        None if !hide => 0.0,
        _ => {
            slot.0.set(Some((hide, now, slid)));
            slid
        }
    }
}
