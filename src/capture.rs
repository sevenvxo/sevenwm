//! screen capture for tools like grim and the freeze screenshot shown while u pick an area

use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::texture::TextureBuffer;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
use smithay::output::{Output, WeakOutput};
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::image_capture_source::{
    ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
    OutputCaptureSourceState,
};
use smithay::wayland::image_copy_capture::{
    BufferConstraints, CaptureFailureReason, Frame, ImageCopyCaptureHandler, ImageCopyCaptureState,
    Session, SessionRef,
};
use smithay::wayland::shm;

use crate::render::{FrameElement, Target};
use crate::state::Seven;

/// longest a freeze can last in case the tool maybe never captures
const FREEZE_LIMIT: Duration = Duration::from_secs(60);
/// how long a freeze stays after the picker closes in case the capture is js a bit late
const CANCEL_GRACE: Duration = Duration::from_secs(1);
/// the layer shell namespace slurps picker uses
pub const PICKER_NAMESPACE: &str = "selection";

/// a freeze screenshot w a still per monitor taken on its next frame
pub struct Freeze {
    /// monitors still waiting for a still
    waiting: Vec<String>,
    stills: Vec<(String, TextureBuffer<GlesTexture>)>,
    since: Instant,
    /// when to give up once the picker closes
    until: Option<Instant>,
}

impl Freeze {
    pub fn still_for(&self, output: &str) -> Option<&TextureBuffer<GlesTexture>> {
        self.stills
            .iter()
            .find(|(o, _)| o == output)
            .map(|(_, s)| s)
    }
}

impl ImageCaptureSourceHandler for Seven {
    fn source_destroyed(&mut self, _source: ImageCaptureSource) {}
}

impl OutputCaptureSourceHandler for Seven {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for Seven {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        Some(constraints(self.capture_size(source)?))
    }

    /// keep the session bc dropping one stops it
    fn new_session(&mut self, session: Session) {
        tracing::debug!("capture session opened");
        self.capture_sessions.push(session);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.capture_sessions.retain(|s| **s != session);
    }

    /// queue the capture and the next frame fills it
    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        tracing::debug!("capture requested");
        self.pending_captures.push((frame, session.clone()));
    }
}

fn constraints(size: Size<i32, Physical>) -> BufferConstraints {
    BufferConstraints {
        size: size.to_logical(1).to_buffer(1, Transform::Normal),
        shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
        dma: None,
    }
}

impl Seven {
    /// how big a capture of source is
    fn capture_size(&self, source: &ImageCaptureSource) -> Option<Size<i32, Physical>> {
        if let Some(output) = source.user_data().get::<WeakOutput>() {
            return output.upgrade()?.current_mode().map(|m| m.size);
        }
        let window = self.window_for_source(source)?;
        let size = window.geometry().size;
        let scale = self.window_capture_scale();
        (size.w > 0 && size.h > 0).then(|| {
            Size::from(((size.w as f64 * scale).ceil() as i32, (size.h as f64 * scale).ceil() as i32))
        })
    }

    /// a window capture is drawn at the sharpest monitor scale so it isnt blurry on a hidpi screen
    fn window_capture_scale(&self) -> f64 {
        self.outputs()
            .map(|o| o.current_scale().fractional_scale())
            .fold(1.0, f64::max)
    }

    /// mod+shift+s does the screenshot thing where it freezes the screen and runs the command
    pub fn screenshot(&mut self) {
        if self.config.screenshot.freeze {
            self.freeze = Some(Freeze {
                waiting: self.outputs().map(|o| o.name()).collect(),
                stills: Vec::new(),
                since: Instant::now(),
                until: None,
            });
        }
        let command = self.config.screenshot.command.clone();
        self.spawn(&command);
    }

    /// the picker closed so unfreeze soon unless a capture comes first
    pub fn picker_closed(&mut self) {
        if let Some(freeze) = &mut self.freeze {
            freeze.until = Some(Instant::now() + CANCEL_GRACE);
        }
    }

    /// does a thing where it drops a freeze whose time is up
    pub fn expire_freeze(&mut self, now: Instant) {
        if let Some(freeze) = &self.freeze
            && (freeze.until.is_some_and(|u| now >= u)
                || now.duration_since(freeze.since) >= FREEZE_LIMIT)
        {
            self.freeze = None;
        }
    }
}

/// render elements into a new texture the size of the output
fn render_offscreen(
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
    scale: f64,
    elements: &[FrameElement],
    background: [f32; 4],
) -> Result<GlesTexture, String> {
    let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);
    let mut texture: GlesTexture =
        Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Argb8888, buffer_size)
            .map_err(|e| format!("offscreen buffer: {e}"))?;
    {
        let mut target = renderer
            .bind(&mut texture)
            .map_err(|e| format!("bind: {e}"))?;
        let mut tracker = OutputDamageTracker::new(size, Scale::from(scale), Transform::Normal);
        tracker
            .render_output(renderer, &mut target, 0, elements, background)
            .map_err(|e| format!("render: {e:?}"))?;
    }
    Ok(texture)
}

/// before a frame take the freeze still if one was asked for
pub fn take_freeze_still(state: &mut Seven, renderer: &mut GlesRenderer, output: &Output) {
    let name = output.name();
    let Some(freeze) = &mut state.freeze else {
        return;
    };
    let Some(i) = freeze.waiting.iter().position(|o| *o == name) else {
        return;
    };
    freeze.waiting.remove(i);
    let Some(size) = output.current_mode().map(|m| m.size) else {
        return;
    };
    let scale = output.current_scale().fractional_scale();
    // the still hides what captures hide and has no cursor bc the live one draws on top
    let elements = crate::render::compose(state, renderer, output, Target::Capture { cursor: false });
    let background = state.config.canvas.background;
    match render_offscreen(renderer, size, scale, &elements, background) {
        Ok(texture) => {
            let still = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
            if let Some(freeze) = &mut state.freeze {
                freeze.stills.push((name, still));
            }
        }
        Err(err) => tracing::warn!("freeze: {err}"),
    }
}

/// after a frame fill every queued capture of the screen or one window
pub fn serve_captures(state: &mut Seven, renderer: &mut GlesRenderer, output: &Output) {
    if state.pending_captures.is_empty() {
        return;
    }
    let scale = output.current_scale().fractional_scale();
    let window_scale = state.window_capture_scale();
    let now = state.start_time.elapsed();
    let mut captured = false;
    let mut later = Vec::new();
    for (frame, session) in std::mem::take(&mut state.pending_captures) {
        let source = session.source();
        // a capture of another monitor waits for that monitors frame
        if let Some(wanted) = source.user_data().get::<WeakOutput>() {
            match wanted.upgrade() {
                Some(wanted) if wanted == *output => {}
                Some(wanted) if state.outputs().any(|o| *o == wanted) => {
                    later.push((frame, session));
                    continue;
                }
                _ => {
                    frame.fail(CaptureFailureReason::Stopped);
                    continue;
                }
            }
        }
        let Some(size) = state.capture_size(&source) else {
            frame.fail(CaptureFailureReason::Stopped);
            continue;
        };
        // a window changed size so tell the client and it asks again
        if session
            .current_constraints()
            .is_some_and(|c| c.size != size.to_logical(1).to_buffer(1, Transform::Normal))
        {
            session.update_constraints(constraints(size));
            frame.fail(CaptureFailureReason::BufferConstraints);
            continue;
        }
        // a window alone sits on black and the screen on the canvas color
        let (elements, background) = match state.window_for_source(&source) {
            Some(window) => (window_alone(state, renderer, &window, window_scale), [0.0, 0.0, 0.0, 1.0]),
            None => (
                crate::render::compose(
                    state,
                    renderer,
                    output,
                    Target::Capture {
                        cursor: session.draw_cursor(),
                    },
                ),
                state.config.canvas.background,
            ),
        };
        let scale = if state.window_for_source(&source).is_some() { window_scale } else { scale };
        match copy_to_buffer(
            renderer,
            size,
            scale,
            &elements,
            background,
            &frame.buffer(),
        ) {
            Ok(()) => {
                tracing::debug!("capture served");
                frame.success(Transform::Normal, None, now);
                captured = true;
                state.last_capture = Some(std::time::Instant::now());
            }
            Err(err) => {
                tracing::warn!("capture failed: {err}");
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }
    state.pending_captures.extend(later);
    // the screenshot is done so the freeze did its job
    if captured && state.freeze.as_ref().is_some_and(|f| f.waiting.is_empty()) {
        state.freeze = None;
    }
}

/// the window drawn alone or a black frame if its hidden collapsed or locked
fn window_alone(
    state: &Seven,
    renderer: &mut GlesRenderer,
    window: &smithay::desktop::Window,
    scale: f64,
) -> Vec<FrameElement> {
    // hidden collapsed or locked so black frame
    if crate::menu::hidden_from_capture(window) || state.is_locked() || state.is_collapsed(window) {
        return Vec::new();
    }
    let loc = window.geometry().loc;
    let origin = Point::<f64, Logical>::from((-loc.x as f64, -loc.y as f64)).to_physical(scale).to_i32_round();
    window.render_elements::<FrameElement>(renderer, origin, Scale::from(scale), 1.0)
}

fn copy_to_buffer(
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
    scale: f64,
    elements: &[FrameElement],
    background: [f32; 4],
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
) -> Result<(), String> {
    let mut texture = render_offscreen(renderer, size, scale, elements, background)?;
    let region = Rectangle::from_size(size.to_logical(1).to_buffer(1, Transform::Normal));
    let target = renderer
        .bind(&mut texture)
        .map_err(|e| format!("bind: {e}"))?;
    let mapping = renderer
        .copy_framebuffer(&target, region, Fourcc::Argb8888)
        .map_err(|e| format!("read back: {e}"))?;
    drop(target);
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|e| format!("map: {e}"))?;
    tracing::debug!(
        "capture: {} elements, {} of {} bytes non-zero",
        elements.len(),
        pixels.iter().filter(|b| **b != 0).count(),
        pixels.len()
    );
    let row = size.w as usize * 4;
    shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
        let (stride, height) = (data.stride as usize, data.height as usize);
        if data.width != size.w
            || height != size.h as usize
            || stride < row
            || len < stride * height
        {
            return Err("the buffer doesn't match the screen".to_string());
        }
        for y in 0..height {
            // safety len covers stride * height bytes and each row copy stays inside both
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(y * row),
                    ptr.add(y * stride),
                    row,
                );
            }
        }
        Ok(())
    })
    .map_err(|e| format!("not a shared-memory buffer: {e}"))?
}
