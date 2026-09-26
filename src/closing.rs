//! the close animation which keeps the windows last picture since its surface is gone by then

use std::time::Instant;

use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::backend::renderer::utils::{SurfaceView, with_renderer_surface_state};
use smithay::backend::renderer::ContextId;
use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Transform};

use crate::layout::Rect;
use crate::state::Seven;

pub struct Closing {
    pub texture: GlesTexture,
    pub context: ContextId<GlesTexture>,
    pub view: SurfaceView,
    pub buffer_scale: i32,
    pub transform: Transform,
    /// where the surface was on the canvas and the frame around it
    pub origin: Point<f64, Logical>,
    pub frame: Rect,
    /// corner radius on the canvas and 0 if it was fullscreen
    pub radius: f64,
    pub start: Instant,
    pub id: Id,
    /// left out of screen captures like the window was
    pub hidden_from_capture: bool,
}

impl Seven {
    /// the window is closing so keep its last picture to animate
    pub fn keep_closing_picture(&mut self, window: &Window) {
        let config = &self.config.animations;
        if !config.enabled
            || config.close_style == crate::animation::Style::None
            || config.close_ms == 0
        {
            return;
        }
        let (Some(context), Some(toplevel)) = (self.gles_context.clone(), window.toplevel()) else {
            return;
        };
        let (Some(loc), Some(frame)) = (self.space.element_location(window), self.frame(window)) else {
            return;
        };
        let picture = with_renderer_surface_state(toplevel.wl_surface(), |state| {
            Some((
                state.texture::<GlesTexture>(context.clone())?.clone(),
                state.view()?,
                state.buffer_scale(),
                state.buffer_transform(),
            ))
        })
        .flatten();
        let Some((texture, view, buffer_scale, transform)) = picture else {
            return;
        };
        let radius = if self.is_fullscreen(window) {
            0.0
        } else {
            self.config.decorations.corner_radius as f64
        };
        self.closing.push(Closing {
            texture,
            context,
            view,
            buffer_scale,
            transform,
            origin: (loc - window.geometry().loc).to_f64(),
            frame,
            radius,
            start: Instant::now(),
            id: Id::new(),
            hidden_from_capture: crate::menu::hidden_from_capture(window),
        });
    }

    /// forget pictures whose animation is over
    pub fn expire_closing(&mut self) {
        let ms = self.config.animations.close_ms as f64;
        self.closing
            .retain(|c| c.start.elapsed().as_secs_f64() * 1000.0 < ms);
    }
}
