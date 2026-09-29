//! builds one frame w everything on screen top to bottom thru the camera

use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use std::cell::RefCell;
use std::time::Instant;

use smithay::backend::renderer::element::render_elements;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Id, Kind};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::desktop::{Window, layer_map_for_output};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::Layer;

use crate::animation::Look;
use crate::config::Color;
use crate::state::Seven;

/// width of the workspace and canvas outlines in screen pixels
const OUTLINE: i32 = 2;

fn outline_px(output_scale: f64) -> i32 {
    ((OUTLINE as f64) * output_scale).round().max(1.0) as i32
}

render_elements! {
    pub FrameElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Zoomed=RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Clipped=crate::decorations::ClippedSurface,
    Shader=smithay::backend::renderer::gles::element::PixelShaderElement,
    Solid=SolidColorRenderElement,
    Texture=TextureRenderElement<GlesTexture>,
    Memory=MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// what a frame is for
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// the monitor
    Screen,
    /// a screenshot or screencast where hidden windows show black and our menu is left out
    Capture { cursor: bool },
}

impl Target {
    fn is_capture(self) -> bool {
        matches!(self, Target::Capture { .. })
    }
}

/// every element of the frame for output topmost first
pub fn compose(
    state: &mut Seven,
    renderer: &mut GlesRenderer,
    output: &Output,
    target: Target,
) -> Vec<FrameElement> {
    let output_scale = output.current_scale().fractional_scale();
    let wallpaper = output.current_mode().and_then(|mode| {
        let canvas = &state.config.canvas;
        let (path, wmode, background) = (
            canvas.wallpaper.clone(),
            canvas.wallpaper_mode,
            canvas.background,
        );
        state
            .wallpaper
            .for_size(&path, wmode, background, mode.size)
            .cloned()
    });
    state.update_home();
    if state.shaders.is_none() {
        state.shaders = Some(match crate::decorations::Shaders::compile(renderer) {
            Ok(shaders) => Ok(std::rc::Rc::new(shaders)),
            Err(err) => {
                tracing::warn!("decoration shaders didn't compile, so no rounded corners: {err}");
                Err(())
            }
        });
    }
    state.gles_context = Some(smithay::backend::renderer::Renderer::context_id(renderer));
    state.expire_closing();
    state.animating.set(
        !state.closing.is_empty()
            || state.view.is_flying()
            || state.freeze.is_some()
            || state.wallpaper.is_loading(),
    );
    // title bar images need the font so draw them before the frame
    let focused = state.focused_window();
    let with_bars: Vec<Window> = state
        .space
        .elements()
        .filter(|w| state.has_titlebar(w))
        .cloned()
        .collect();
    state.titlebars = with_bars
        .into_iter()
        .filter_map(|w| {
            let image = state.titlebar_image(&w, focused.as_ref() == Some(&w))?;
            Some((w, image))
        })
        .collect();
    let fb_size = output.current_mode().map_or(Size::from((1, 1)), |m| m.size);
    // the wallpaper and freeze still are the monitors pixel size so they fill it at any scale
    let fb_logical = fb_size.to_f64().to_logical(output_scale).to_i32_round();
    // all of the image not js the part the logical size would crop to
    let fb_whole = Some(Rectangle::<f64, Logical>::from_size(Size::from((
        fb_size.w as f64,
        fb_size.h as f64,
    ))));
    let mut elements = match target {
        Target::Capture { cursor: false } => Vec::new(),
        _ => cursor_elements(state, renderer, output_scale),
    };
    // the menus w a submenu over its menu on the monitor they opened on
    for menu in [&state.submenu, &state.menu] {
        if target == Target::Screen
            && let Some(menu) = menu
            && menu.output == output.name()
            && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                menu.pos.to_f64().to_physical(output_scale),
                &menu.image,
                None,
                None,
                None,
                Kind::Unspecified,
            )
        {
            elements.push(FrameElement::Memory(element));
        }
    }
    let state = &*state;

    // locked so js the lock screen on black and it stays black if the locker dies
    if state.is_locked() {
        if let Some(lock) = state.lock_surface_on(output) {
            elements.extend(render_elements_from_surface_tree(
                renderer,
                lock.wl_surface(),
                Point::<i32, Physical>::from((0, 0)),
                Scale::from(output_scale),
                1.0,
                Kind::Unspecified,
            ));
        }
        let screen = Rectangle::from_size(state.screen_size())
            .to_f64()
            .to_physical(output_scale)
            .to_i32_round();
        elements.push(FrameElement::Solid(SolidColorRenderElement::new(
            state.decoration_ids[0].clone(),
            screen,
            color_commit([0.0, 0.0, 0.0, 1.0]),
            [0.0, 0.0, 0.0, 1.0],
            Kind::Unspecified,
        )));
        return elements;
    }

    // a fullscreen window u look at covers panels but not overlays
    let view = &state.view;
    let fullscreen = state.covering_fullscreen();
    push_layers(
        &mut elements,
        renderer,
        output,
        output_scale,
        &[Layer::Overlay],
    );
    if let Some(window) = &fullscreen {
        push_window(&mut elements, state, renderer, window, output_scale, fb_size, target);
    }
    push_layers(&mut elements, renderer, output, output_scale, &[Layer::Top]);

    // a freeze screenshot where the still covers everything under the picker
    if let Some(still) = state
        .freeze
        .as_ref()
        .and_then(|f| f.still_for(&output.name()))
    {
        elements.push(FrameElement::Texture(
            TextureRenderElement::from_texture_buffer(
                Point::<f64, Physical>::from((0.0, 0.0)),
                still,
                None,
                fb_whole,
                Some(fb_logical),
                Kind::Unspecified,
            ),
        ));
        return elements;
    }

    tracing::trace!(camera = ?view.camera, zoom = view.zoom, active_pos = ?state.active_pos, "view");
    // collapsed window markers above the windows
    for entry in state.collapsed.iter().rev() {
        // a hidden windows marker shows its title so not in captures
        if target.is_capture() && crate::menu::hidden_from_capture(&entry.window) {
            continue;
        }
        let rect = state.marker_rect(entry.anchor);
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            rect.loc.to_physical(output_scale),
            &entry.image,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            elements.push(FrameElement::Memory(element));
        }
    }
    push_closing(&mut elements, state, output_scale, fb_size, target);
    // windows front to back thru the camera
    let visible = view.visible(state.screen_size());
    let focused = state.focused_window();
    for window in state.space.elements().rev() {
        if Some(window) == fullscreen.as_ref() {
            continue;
        }
        if let Some(bbox) = state.space.element_bbox(window)
            && visible.overlaps(bbox.to_f64())
        {
            let under_bar = elements.len();
            let look = push_window(
                &mut elements,
                state,
                renderer,
                window,
                output_scale,
                fb_size,
                target,
            );
            push_border(
                &mut elements,
                state,
                window,
                focused.as_ref(),
                output_scale,
                look,
            );
            // a hidden windows title bar shows its title so not in captures
            let hidden = target.is_capture() && crate::menu::hidden_from_capture(window);
            if !hidden
                && let Some(bar) = titlebar_element(state, renderer, window, look, output_scale)
            {
                elements.insert(under_bar, bar);
            }
            push_shadow(&mut elements, state, window, look);
        }
    }

    push_workspaces(&mut elements, state, output_scale);
    let ids = &state.decoration_ids;
    if let Some(bounds) = state.bounds() {
        elements.extend(outline(
            &ids[5..9],
            state,
            bounds,
            output_scale,
            state.config.canvas.bounds_outline,
            outline_px(output_scale),
        ));
    }

    push_layers(
        &mut elements,
        renderer,
        output,
        output_scale,
        &[Layer::Bottom, Layer::Background],
    );
    if let Some(wallpaper) = wallpaper
        && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((0.0, 0.0)),
            &wallpaper,
            None,
            fb_whole,
            Some(fb_logical),
            Kind::Unspecified,
        )
    {
        elements.push(FrameElement::Memory(element));
    }
    elements
}

/// each workspace edge unless it fills the screen rn plus the drop highlight
fn push_workspaces(
    elements: &mut Vec<FrameElement>,
    state: &Seven,
    output_scale: f64,
) {
    let view = &state.view;
    let visible = view.visible(state.screen_size());
    for (i, ws) in state.workspaces.iter().enumerate() {
        let area = state.ws_area(i);
        let seen_whole = view.zoom == 1.0 && view.camera == ws.rect.loc.to_f64();
        if seen_whole || !visible.overlaps(area.to_f64()) {
            continue;
        }
        elements.extend(outline(
            &ws.ids[1..5],
            state,
            area,
            output_scale,
            state.config.canvas.region_outline,
            outline_px(output_scale),
        ));
    }
    if let Some(i) = state.drop_target.and_then(|n| state.ws_index(n)) {
        elements.push(fill(
            state.workspaces[i].ids[0].clone(),
            state,
            state.ws_area(i),
            output_scale,
            state.config.canvas.drop_highlight,
        ));
    }
}

fn push_window(
    elements: &mut Vec<FrameElement>,
    state: &Seven,
    renderer: &mut GlesRenderer,
    window: &Window,
    output_scale: f64,
    fb_size: Size<i32, Physical>,
    target: Target,
) -> Look {
    if target.is_capture() && crate::menu::hidden_from_capture(window) {
        // captures get a black box where the window is
        if let Some(rect) = state.frame(window) {
            let id = window.user_data().get_or_insert(|| HiddenBoxId(Id::new()));
            elements.push(fill(
                id.0.clone(),
                state,
                rect,
                output_scale,
                [0.0, 0.0, 0.0, 1.0],
            ));
        }
        return Look::default();
    }
    let (Some(loc), Some(frame)) = (state.space.element_location(window), state.frame(window)) else {
        return Look::default();
    };
    let look = animation(state, window, loc);
    let alpha = look.alpha;
    let centre = rect_centre(frame);
    // where the surface origin is drawn and how much its scaled
    let origin = look.apply((loc - window.geometry().loc).to_f64(), centre);
    let physical = to_physical_point(state.view.to_screen(origin), output_scale);
    // surfaces come out at the monitor scale and zoom shrinks or grows them around that origin
    let zoom = Scale::from(state.view.zoom * look.scale);
    let rescale = |s| RescaleRenderElement::from_element(s, physical, zoom);
    let radius = corner_radius(state, window) * look.scale;
    let shaders = state.shaders.as_ref().and_then(|s| s.as_ref().ok());
    let (Some(shaders), Some(toplevel), true) = (shaders, window.toplevel(), radius > 0.0) else {
        let surfaces = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            physical,
            Scale::from(output_scale),
            alpha,
        );
        elements.extend(surfaces.into_iter().map(|s| FrameElement::Zoomed(rescale(s))));
        return look;
    };
    // popups float free and the window gets uhh clipped to its rounded rect
    let surface = toplevel.wl_surface();
    let scale = Scale::from(output_scale);
    for (popup, popup_offset) in smithay::desktop::PopupManager::popups_for_surface(surface) {
        let at = (window.geometry().loc + popup_offset - popup.geometry().loc)
            .to_physical_precise_round(scale);
        let popup_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                popup.wl_surface(),
                physical + at,
                scale,
                alpha,
                Kind::Unspecified,
            );
        elements.extend(popup_elements.into_iter().map(|s| FrameElement::Zoomed(rescale(s))));
    }
    // clipped to the whole frame so under a title bar only the bottom corners round
    let Some(drawn) = drawn_rect(state, window, look) else {
        return look;
    };
    let geo_physical = drawn.to_physical(output_scale);
    let radius_physical = (radius * output_scale) as f32;
    let body: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = render_elements_from_surface_tree(
        renderer,
        surface,
        physical,
        scale,
        alpha,
        Kind::Unspecified,
    );
    elements.extend(body.into_iter().map(|s| {
        FrameElement::Clipped(crate::decorations::ClippedSurface::new(
            crate::decorations::Clip::Surface(rescale(s)),
            shaders,
            geo_physical,
            radius_physical,
            fb_size,
        ))
    }));
    look
}

fn rect_centre(rect: Rectangle<i32, Logical>) -> Point<f64, Logical> {
    Point::from((
        rect.loc.x as f64 + rect.size.w as f64 / 2.0,
        rect.loc.y as f64 + rect.size.h as f64 / 2.0,
    ))
}

/// a slide from where it was drawn to its real spot and when it started
type Slide = (Point<f64, Logical>, Point<i32, Logical>, Instant);

/// a windows animation w where it was drawn the slide its on and when it first showed
#[derive(Default)]
struct Motion {
    shown: Option<Point<f64, Logical>>,
    slide: Option<Slide>,
    first_drawn: Option<Instant>,
}

/// how the window is drawn rn sliding or opening but a dragged one follows the pointer exactly
fn animation(state: &Seven, window: &Window, target: Point<i32, Logical>) -> Look {
    let config = &state.config.animations;
    let motion = window
        .user_data()
        .get_or_insert(|| RefCell::new(Motion::default()));
    let mut motion = motion.borrow_mut();
    let now = Instant::now();
    let first = *motion.first_drawn.get_or_insert(now);
    let target_f = target.to_f64();
    let carried = state
        .dragging_workspace
        .is_some_and(|n| state.ws_of(window).is_some_and(|i| state.workspaces[i].number == n));
    if !config.enabled || state.dragging.as_ref() == Some(window) || carried {
        motion.slide = None;
        motion.shown = Some(target_f);
        return Look::default();
    }
    let ease = |t: f64| config.move_curve.ease(t);
    let progress = |start: Instant, ms: u64| {
        if ms == 0 {
            1.0
        } else {
            now.duration_since(start).as_secs_f64() / (ms as f64 / 1000.0)
        }
    };
    // where its drawn rn before this frame decides
    let current = match motion.slide {
        Some((from, to, start)) => {
            let t = ease(progress(start, config.move_ms));
            from + (to.to_f64() - from).upscale(t)
        }
        None => motion.shown.unwrap_or(target_f),
    };
    let moving_to = motion
        .slide
        .map_or(motion.shown.map(|s| s.to_i32_round()), |(_, to, _)| {
            Some(to)
        });
    if moving_to != Some(target) && motion.shown.is_some() {
        motion.slide = Some((current, target, now));
    }
    let position = match motion.slide {
        Some((from, to, start)) => {
            let t = progress(start, config.move_ms);
            if t >= 1.0 {
                motion.slide = None;
                to.to_f64()
            } else {
                from + (to.to_f64() - from).upscale(ease(t))
            }
        }
        None => target_f,
    };
    motion.shown = Some(position);
    let t = progress(first, config.open_ms);
    let mut look = if t >= 1.0 {
        Look::default()
    } else {
        config.open_style.look(config.open_curve.ease(t))
    };
    if t < 1.0 || motion.slide.is_some() {
        state.animating.set(true);
    }
    look.offset += position - target_f;
    look
}

/// closed windows last pictures shrinking or fading away
fn push_closing(
    elements: &mut Vec<FrameElement>,
    state: &Seven,
    output_scale: f64,
    fb_size: Size<i32, Physical>,
    target: Target,
) {
    let config = &state.config.animations;
    let view = &state.view;
    let shaders = state.shaders.as_ref().and_then(|s| s.as_ref().ok());
    for closing in &state.closing {
        if target.is_capture() && closing.hidden_from_capture {
            continue;
        }
        let t = closing.start.elapsed().as_secs_f64() / (config.close_ms.max(1) as f64 / 1000.0);
        let look = config.close_style.look(1.0 - config.close_curve.ease(t));
        let centre = rect_centre(closing.frame);
        let zoom = view.zoom * look.scale;
        let at = view.to_screen(look.apply(closing.origin, centre));
        let size = Size::from((
            (closing.view.dst.w as f64 * zoom).round().max(1.0) as i32,
            (closing.view.dst.h as f64 * zoom).round().max(1.0) as i32,
        ));
        let picture = TextureRenderElement::from_static_texture(
            closing.id.clone(),
            closing.context.clone(),
            at.to_physical(output_scale),
            closing.texture.clone(),
            closing.buffer_scale,
            closing.transform,
            Some(look.alpha),
            Some(closing.view.src),
            Some(size),
            None,
            Kind::Unspecified,
        );
        let radius = closing.radius * zoom;
        match shaders {
            Some(shaders) if radius > 0.0 => {
                let frame_at = view.to_screen(look.apply(closing.frame.loc.to_f64(), centre));
                let drawn = Rectangle::<f64, Logical>::new(
                    frame_at,
                    Size::from((
                        closing.frame.size.w as f64 * zoom,
                        closing.frame.size.h as f64 * zoom,
                    )),
                );
                elements.push(FrameElement::Clipped(crate::decorations::ClippedSurface::new(
                    crate::decorations::Clip::Snapshot(picture),
                    shaders,
                    drawn.to_physical(output_scale),
                    (radius * output_scale) as f32,
                    fb_size,
                )));
            }
            _ => elements.push(FrameElement::Texture(picture)),
        }
    }
}

/// the window corner radius on screen and none when fullscreen
fn corner_radius(state: &Seven, window: &Window) -> f64 {
    if state.is_fullscreen(window) {
        return 0.0;
    }
    state.config.decorations.corner_radius as f64 * state.view.zoom
}

/// where the window frame is drawn on screen w look
fn drawn_rect(state: &Seven, window: &Window, look: Look) -> Option<Rectangle<f64, Logical>> {
    let frame = state.frame(window)?;
    let zoom = state.view.zoom * look.scale;
    let loc = look.apply(frame.loc.to_f64(), rect_centre(frame));
    Some(Rectangle::new(
        state.view.to_screen(loc),
        Size::from((frame.size.w as f64 * zoom, frame.size.h as f64 * zoom)),
    ))
}

/// a windows title bar drawn w look
fn titlebar_element(
    state: &Seven,
    renderer: &mut GlesRenderer,
    window: &Window,
    look: Look,
    output_scale: f64,
) -> Option<FrameElement> {
    let (_, image) = state.titlebars.iter().find(|(w, _)| w == window)?;
    let drawn = drawn_rect(state, window, look)?;
    let height = state.config.decorations.titlebar_height.max(1) as f64;
    let h = height * state.view.zoom * look.scale;
    // the whole bar scaled to the zoom bc a size alone would crop it
    let width = state.frame(window)?.size.w.max(1) as f64;
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        drawn.loc.to_physical(output_scale),
        image,
        Some(look.alpha),
        Some(Rectangle::from_size(Size::from((width, height)))),
        Some(Size::from((drawn.size.w, h)).to_i32_round()),
        Kind::Unspecified,
    )
    .ok()
    .map(FrameElement::Memory)
}

fn faded(color: Color, alpha: f32) -> Color {
    color.map(|c| c * alpha)
}

/// a soft shadow under a floating window maybe
fn push_shadow(elements: &mut Vec<FrameElement>, state: &Seven, window: &Window, look: Look) {
    let deco = &state.config.decorations;
    if !deco.shadow
        || deco.shadow_size == 0
        || state.is_fullscreen(window)
        || (!deco.shadow_on_tiles && state.is_tiled(window))
    {
        return;
    }
    let Some(Ok(shaders)) = &state.shaders else {
        return;
    };
    let Some(rect) = drawn_rect(state, window, look) else {
        return;
    };
    let zoom = state.view.zoom * look.scale;
    let shift = Point::from((
        deco.shadow_offset[0] as f64 * zoom,
        deco.shadow_offset[1] as f64 * zoom,
    ));
    elements.push(FrameElement::Shader(crate::decorations::shadow(
        shaders,
        window,
        rect,
        (corner_radius(state, window) * look.scale) as f32,
        (deco.shadow_size as f64 * zoom) as f32,
        shift,
        faded(deco.shadow_color, look.alpha),
    )));
}

/// id for the black box that stands in for a hidden window
struct HiddenBoxId(Id);

/// ids for a windows four border bars so frames reuse them
struct BorderIds([Id; 4]);

/// the window outline in the focused or unfocused color right under the window
fn push_border(
    elements: &mut Vec<FrameElement>,
    state: &Seven,
    window: &Window,
    focused: Option<&Window>,
    output_scale: f64,
    look: Look,
) {
    let border = &state.config.border;
    if border.width == 0 || state.is_fullscreen(window) {
        return;
    }
    let Some(mut rect) = state.frame(window) else {
        return;
    };
    rect.loc += look.offset.to_i32_round();
    let color = faded(
        if Some(window) == focused {
            border.focused
        } else {
            border.unfocused
        },
        look.alpha,
    );
    // rounded w the shaders or four plain bars
    if let Some(Ok(shaders)) = &state.shaders
        && let Some(screen) = drawn_rect(state, window, look)
    {
        let width = (border.width as f64 * state.view.zoom * look.scale).max(1.0 / output_scale) as f32;
        let radius = (corner_radius(state, window) * look.scale) as f32;
        elements.push(FrameElement::Shader(crate::decorations::ring(
            shaders, window, screen, radius, width, color,
        )));
        return;
    }
    let ids = window
        .user_data()
        .get_or_insert(|| BorderIds(std::array::from_fn(|_| Id::new())));
    let width = ((border.width as f64) * state.view.zoom * output_scale)
        .round()
        .max(1.0) as i32;
    elements.extend(outline(&ids.0, state, rect, output_scale, color, width));
}

fn to_physical_point(screen: Point<f64, Logical>, output_scale: f64) -> Point<i32, Physical> {
    Point::<f64, Physical>::from((screen.x * output_scale, screen.y * output_scale)).to_i32_round()
}

/// the cursor and any drag icon at the pointer
fn cursor_elements(
    state: &mut Seven,
    renderer: &mut GlesRenderer,
    output_scale: f64,
) -> Vec<FrameElement> {
    let mut elements = Vec::new();
    let pointer = state.pointer_screen;
    let scale = Scale::from(output_scale);
    // only the monitor the pointer is on shows it
    let on_this_monitor = state.pointer_output.is_none()
        || state.pointer_output.as_deref() == state.output.as_ref().map(|o| o.name()).as_deref();
    if !on_this_monitor {
        return elements;
    }
    if state.draw_cursor {
        match state.cursor_status.clone() {
            CursorImageStatus::Hidden => {}
            CursorImageStatus::Named(icon) => {
                if let Some(image) = state.cursors.get(icon, output_scale.ceil() as i32) {
                    let at = to_physical_point(pointer - image.hotspot, output_scale);
                    if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        at.to_f64(),
                        &image.buffer,
                        None,
                        Some(Rectangle::from_size(image.src)),
                        Some(image.size),
                        Kind::Cursor,
                    ) {
                        elements.push(FrameElement::Memory(element));
                    }
                }
            }
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .map_or_else(Point::default, |data| data.lock().unwrap().hotspot)
                });
                let at = to_physical_point(pointer - hotspot.to_f64(), output_scale);
                elements.extend(render_elements_from_surface_tree(
                    renderer,
                    &surface,
                    at,
                    scale,
                    1.0,
                    Kind::Cursor,
                ));
            }
        }
    }
    if let Some(icon) = &state.dnd_icon {
        let at = to_physical_point(pointer, output_scale);
        elements.extend(render_elements_from_surface_tree(
            renderer,
            icon,
            at,
            scale,
            1.0,
            Kind::Unspecified,
        ));
    }
    elements
}

/// layer surfaces sit on the screen and ignore the camera
fn push_layers(
    elements: &mut Vec<FrameElement>,
    renderer: &mut GlesRenderer,
    output: &Output,
    output_scale: f64,
    layers: &[Layer],
) {
    let map = layer_map_for_output(output);
    for &layer in layers {
        for surface in map.layers_on(layer).rev() {
            let Some(geo) = map.layer_geometry(surface) else {
                continue;
            };
            let physical = geo.loc.to_f64().to_physical(output_scale).to_i32_round();
            elements.extend(surface.render_elements::<FrameElement>(
                renderer,
                physical,
                Scale::from(output_scale),
                1.0,
            ));
        }
    }
}

/// a canvas rects spot on screen in physical pixels
fn to_physical(
    state: &Seven,
    rect: Rectangle<i32, Logical>,
    output_scale: f64,
) -> Rectangle<i32, Physical> {
    let top_left = state.view.to_screen(rect.loc.to_f64());
    let bottom_right = state.view.to_screen((rect.loc + rect.size).to_f64());
    let p = |point: Point<f64, Logical>| {
        Point::<f64, Physical>::from((point.x * output_scale, point.y * output_scale))
            .to_i32_round::<i32>()
    };
    Rectangle::from_extremities(p(top_left), p(bottom_right))
}

/// a solid fills commit is its color so a new color counts as damage
fn color_commit(color: Color) -> CommitCounter {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    color.map(f32::to_bits).hash(&mut hasher);
    CommitCounter::from(hasher.finish() as usize)
}

fn fill(
    id: Id,
    state: &Seven,
    rect: Rectangle<i32, Logical>,
    output_scale: f64,
    color: Color,
) -> FrameElement {
    FrameElement::Solid(SolidColorRenderElement::new(
        id,
        to_physical(state, rect, output_scale),
        color_commit(color),
        color,
        Kind::Unspecified,
    ))
}

/// four bars t pixels thick right outside a canvas rect
fn outline(
    ids: &[Id],
    state: &Seven,
    rect: Rectangle<i32, Logical>,
    output_scale: f64,
    color: Color,
    t: i32,
) -> Vec<FrameElement> {
    let r = to_physical(state, rect, output_scale);
    let (x, y, w, h) = (r.loc.x, r.loc.y, r.size.w, r.size.h);
    let bars = [
        Rectangle::new(Point::from((x - t, y - t)), Size::from((w + 2 * t, t))),
        Rectangle::new(Point::from((x - t, y + h)), Size::from((w + 2 * t, t))),
        Rectangle::new(Point::from((x - t, y)), Size::from((t, h))),
        Rectangle::new(Point::from((x + w, y)), Size::from((t, h))),
    ];
    ids.iter()
        .zip(bars)
        .map(|(id, bar)| {
            FrameElement::Solid(SolidColorRenderElement::new(
                id.clone(),
                bar,
                color_commit(color),
                color,
                Kind::Unspecified,
            ))
        })
        .collect()
}
