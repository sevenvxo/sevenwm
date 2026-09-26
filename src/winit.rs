//! the nested backend where sevenwm runs as a window in ur session

use std::time::Instant;

use smithay::backend::renderer::ImportDma;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::utils::Transform;

use crate::state::Seven;

const REFRESH_MHZ: i32 = 60_000;

pub fn init(
    event_loop: &mut EventLoop<Seven>,
    state: &mut Seven,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init::<GlesRenderer>()?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: REFRESH_MHZ,
    };
    let output = Output::new(
        "sevenwm".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "sevenwm".into(),
            model: "nested".into(),
            serial_number: "0".into(),
        },
    );
    output.create_global::<Seven>(&state.display_handle);
    // winits gl surface is upside down compared to the output
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.add_monitor(&output);

    // gpu clients can hand buffers over directly
    let formats = backend.renderer().dmabuf_formats();
    state.enable_dmabuf_formats(formats);

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: REFRESH_MHZ,
                };
                output.change_current_state(Some(mode), None, None, None);
                state.arrange_layers();
                // workspaces are the screen size so they and their tiles change too
                state.resize_workspaces();
            }
            WinitEvent::Input(event) => state.handle_input(event),
            WinitEvent::Redraw => {
                tracing::trace!("redraw");
                if state.view.tick(Instant::now(), state.config.animations.fly_curve) {
                    // the view moved under a still pointer so aim it again
                    state.refresh_pointer();
                }
                state.expire_freeze(Instant::now());
                let background = state.config.canvas.background;
                // offscreen rendering happens outside the windows bind or presenting would fail
                crate::capture::take_freeze_still(state, backend.renderer(), &output);
                let rendered = {
                    let (renderer, mut framebuffer) = match backend.bind() {
                        Ok(bound) => bound,
                        Err(err) => {
                            tracing::warn!("failed to bind the window: {err}");
                            return;
                        }
                    };
                    let elements = crate::render::compose(
                        state,
                        renderer,
                        &output,
                        crate::render::Target::Screen,
                    );
                    damage_tracker
                        .render_output(renderer, &mut framebuffer, 0, &elements, background)
                        .map(|result| result.damage.cloned())
                };
                match rendered {
                    Ok(damage) => {
                        if let Err(err) = backend.submit(damage.as_deref()) {
                            tracing::warn!("failed to present: {err}");
                        }
                        crate::capture::serve_captures(state, backend.renderer(), &output);
                    }
                    Err(err) => tracing::warn!("failed to render: {err}"),
                }

                state.frame_done(&output);
                state.ipc_notify();

                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            _ => {}
        })?;

    Ok(())
}
