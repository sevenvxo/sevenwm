//! window decorations on the gpu like rounded corners a border and a soft shadow

use std::cell::RefCell;

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName,
    UniformType, UniformValue,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::desktop::Window;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::config::Color;

/// signed distance from a point to a rounded box centered on the origin
const ROUNDED_BOX: &str = "
float rounded_box(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
";

/// the default texture shader plus a mask outside the rounded rect that works for any transform
const CLIP_SHADER: &str = "
//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform mat3 ndc_to_output;
uniform vec2 fb_size;
uniform vec4 geo;
uniform float radius;

ROUNDED_BOX

void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

    vec2 ndc = gl_FragCoord.xy / fb_size * 2.0 - 1.0;
    vec2 p = (ndc_to_output * vec3(ndc, 1.0)).xy;
    vec2 half_size = geo.zw * 0.5;
    float d = rounded_box(p - (geo.xy + half_size), half_size, radius);
    color *= clamp(0.5 - d, 0.0, 1.0);

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color;
}
";

/// a rounded ring width wide around a box w corner radius
const RING_SHADER: &str = "
precision highp float;
varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
uniform vec4 color;
uniform float radius;
uniform float width;

ROUNDED_BOX

void main() {
    vec2 p = v_coords * size - size * 0.5;
    float outer_r = radius > 0.0 ? radius + width : 0.0;
    float outer = rounded_box(p, size * 0.5, outer_r);
    float inner = rounded_box(p, size * 0.5 - width, radius);
    float a = clamp(0.5 - outer, 0.0, 1.0) * clamp(0.5 + inner, 0.0, 1.0);
    gl_FragColor = color * a * alpha;
}
";

/// a soft shadow that fades over blur and skips the area under the window
const SHADOW_SHADER: &str = "
precision highp float;
varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
uniform vec4 color;
uniform float radius;
uniform float blur;
uniform vec4 shadow;
uniform vec4 window;

ROUNDED_BOX

void main() {
    vec2 p = v_coords * size;
    float d = rounded_box(p - (shadow.xy + shadow.zw * 0.5), shadow.zw * 0.5, radius);
    float a = 1.0 - smoothstep(-blur * 0.5, blur, d);
    float under = rounded_box(p - (window.xy + window.zw * 0.5), window.zw * 0.5, radius);
    a *= clamp(under + 0.5, 0.0, 1.0);
    gl_FragColor = color * a * alpha;
}
";

/// the compiled shaders made once per renderer
pub struct Shaders {
    clip: GlesTexProgram,
    ring: GlesPixelProgram,
    shadow: GlesPixelProgram,
}

impl Shaders {
    pub fn compile(renderer: &mut GlesRenderer) -> Result<Self, GlesError> {
        let with_box = |src: &str| src.replace("ROUNDED_BOX", ROUNDED_BOX);
        let clip = renderer.compile_custom_texture_shader(
            with_box(CLIP_SHADER),
            &[
                UniformName::new("ndc_to_output", UniformType::Matrix3x3),
                UniformName::new("fb_size", UniformType::_2f),
                UniformName::new("geo", UniformType::_4f),
                UniformName::new("radius", UniformType::_1f),
            ],
        )?;
        let ring = renderer.compile_custom_pixel_shader(
            with_box(RING_SHADER),
            &[
                UniformName::new("color", UniformType::_4f),
                UniformName::new("radius", UniformType::_1f),
                UniformName::new("width", UniformType::_1f),
            ],
        )?;
        let shadow = renderer.compile_custom_pixel_shader(
            with_box(SHADOW_SHADER),
            &[
                UniformName::new("color", UniformType::_4f),
                UniformName::new("radius", UniformType::_1f),
                UniformName::new("blur", UniformType::_1f),
                UniformName::new("shadow", UniformType::_4f),
                UniformName::new("window", UniformType::_4f),
            ],
        )?;
        Ok(Self { clip, ring, shadow })
    }
}

/// what gets clipped which is prolly a live surface or a closing windows last picture
// boxing the big variant would cost an allocation per surface per frame
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Clip {
    Surface(RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>),
    Snapshot(TextureRenderElement<GlesTexture>),
}

macro_rules! clip_inner {
    ($self:expr, $e:ident => $body:expr) => {
        match &$self.inner {
            Clip::Surface($e) => $body,
            Clip::Snapshot($e) => $body,
        }
    };
}

/// a piece of a window drawn thru the corner clipping shader
#[derive(Debug)]
pub struct ClippedSurface {
    inner: Clip,
    program: GlesTexProgram,
    /// the windows visible rect on the output in physical pixels
    geo: Rectangle<f64, Physical>,
    radius: f32,
    /// the framebuffer size in pixels
    fb_size: Size<i32, Physical>,
}

impl ClippedSurface {
    pub fn new(
        inner: Clip,
        shaders: &Shaders,
        geo: Rectangle<f64, Physical>,
        radius: f32,
        fb_size: Size<i32, Physical>,
    ) -> Self {
        Self {
            inner,
            program: shaders.clip.clone(),
            geo,
            radius,
            fb_size,
        }
    }
}

/// the inverse of a column major 3x3 matrix or none if singular
fn invert(m: &[f32; 9]) -> Option<[f32; 9]> {
    let at = |r: usize, c: usize| m[c * 3 + r] as f64;
    let (a, b, c) = (at(0, 0), at(0, 1), at(0, 2));
    let (d, e, f) = (at(1, 0), at(1, 1), at(1, 2));
    let (g, h, i) = (at(2, 0), at(2, 1), at(2, 2));
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if det.abs() < 1e-12 {
        return None;
    }
    // row major adjugate over the determinant
    let rows = [
        [e * i - f * h, c * h - b * i, b * f - c * e],
        [f * g - d * i, a * i - c * g, c * d - a * f],
        [d * h - e * g, b * g - a * h, a * e - b * d],
    ];
    let mut out = [0.0; 9];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            out[c * 3 + r] = (v / det) as f32;
        }
    }
    Some(out)
}

impl Element for ClippedSurface {
    fn id(&self) -> &Id {
        clip_inner!(self, e => e.id())
    }
    fn current_commit(&self) -> CommitCounter {
        clip_inner!(self, e => e.current_commit())
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        clip_inner!(self, e => e.src())
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        clip_inner!(self, e => e.geometry(scale))
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        clip_inner!(self, e => e.location(scale))
    }
    fn transform(&self) -> Transform {
        clip_inner!(self, e => e.transform())
    }
    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        clip_inner!(self, e => e.damage_since(scale, commit))
    }
    /// the corners are see thru now so nothing is promised opaque
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }
    fn alpha(&self) -> f32 {
        clip_inner!(self, e => e.alpha())
    }
    fn kind(&self) -> Kind {
        clip_inner!(self, e => e.kind())
    }
}

impl RenderElement<GlesRenderer> for ClippedSurface {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(ndc_to_output) = invert(frame.projection()) else {
            return clip_inner!(self, e => RenderElement::<GlesRenderer>::draw(e, frame, src, dst, damage, opaque_regions, cache));
        };
        let g = self.geo;
        frame.override_default_tex_program(
            self.program.clone(),
            vec![
                Uniform::new(
                    "ndc_to_output",
                    UniformValue::Matrix3x3 {
                        matrices: vec![ndc_to_output],
                        transpose: false,
                    },
                ),
                Uniform::new("fb_size", (self.fb_size.w as f32, self.fb_size.h as f32)),
                Uniform::new(
                    "geo",
                    (g.loc.x as f32, g.loc.y as f32, g.size.w as f32, g.size.h as f32),
                ),
                Uniform::new("radius", self.radius),
            ],
        );
        let result = clip_inner!(self, e => RenderElement::<GlesRenderer>::draw(e, frame, src, dst, damage, opaque_regions, cache));
        frame.clear_tex_program_override();
        result
    }

    /// clipped content cant go straight to a display plane
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// a windows border and shadow kept on the window so they keep their ids
#[derive(Default)]
struct Cached {
    ring: Option<(PixelShaderElement, Vec<f32>)>,
    shadow: Option<(PixelShaderElement, Vec<f32>)>,
}

/// reuse the slots element and only redraw it if key changed
fn reuse(
    slot: &mut Option<(PixelShaderElement, Vec<f32>)>,
    key: Vec<f32>,
    make: impl FnOnce() -> PixelShaderElement,
    area: Rectangle<i32, Logical>,
    uniforms: Vec<Uniform<'_>>,
) -> PixelShaderElement {
    match slot {
        Some((element, old)) if *old == key => element.clone(),
        Some((element, old)) => {
            element.resize(area, None);
            element.update_uniforms(uniforms);
            *old = key;
            element.clone()
        }
        None => {
            let element = make();
            *slot = Some((element.clone(), key));
            element
        }
    }
}

fn cached(window: &Window) -> &RefCell<Cached> {
    window.user_data().get_or_insert(|| RefCell::new(Cached::default()))
}

fn rgba(c: Color) -> (f32, f32, f32, f32) {
    (c[0], c[1], c[2], c[3])
}

/// the border ring width wide right outside rect
pub fn ring(
    shaders: &Shaders,
    window: &Window,
    rect: Rectangle<f64, Logical>,
    radius: f32,
    width: f32,
    color: Color,
) -> PixelShaderElement {
    let area = Rectangle::new(
        Point::from((rect.loc.x - width as f64, rect.loc.y - width as f64)),
        Size::from((rect.size.w + 2.0 * width as f64, rect.size.h + 2.0 * width as f64)),
    )
    .to_i32_round();
    let uniforms = vec![
        Uniform::new("color", rgba(color)),
        Uniform::new("radius", radius),
        Uniform::new("width", width),
    ];
    let key = vec![
        area.loc.x as f32,
        area.loc.y as f32,
        area.size.w as f32,
        area.size.h as f32,
        radius,
        width,
        color[0],
        color[1],
        color[2],
        color[3],
    ];
    let make_uniforms = uniforms.clone();
    reuse(
        &mut cached(window).borrow_mut().ring,
        key,
        || {
            PixelShaderElement::new(shaders.ring.clone(), area, None, 1.0, make_uniforms, Kind::Unspecified)
        },
        area,
        uniforms,
    )
}

/// the shadow under rect blur wide and shifted by offset
pub fn shadow(
    shaders: &Shaders,
    window: &Window,
    rect: Rectangle<f64, Logical>,
    radius: f32,
    blur: f32,
    offset: Point<f64, Logical>,
    color: Color,
) -> PixelShaderElement {
    let blur_f = blur as f64;
    let shadow_rect = Rectangle::new(rect.loc + offset, rect.size);
    let area = Rectangle::new(
        Point::from((
            rect.loc.x.min(shadow_rect.loc.x) - blur_f,
            rect.loc.y.min(shadow_rect.loc.y) - blur_f,
        )),
        Size::from((
            rect.size.w + offset.x.abs() + 2.0 * blur_f,
            rect.size.h + offset.y.abs() + 2.0 * blur_f,
        )),
    )
    .to_i32_round::<i32>();
    let local = |r: Rectangle<f64, Logical>| {
        (
            (r.loc.x - area.loc.x as f64) as f32,
            (r.loc.y - area.loc.y as f64) as f32,
            r.size.w as f32,
            r.size.h as f32,
        )
    };
    let uniforms = vec![
        Uniform::new("color", rgba(color)),
        Uniform::new("radius", radius),
        Uniform::new("blur", blur),
        Uniform::new("shadow", local(shadow_rect)),
        Uniform::new("window", local(rect)),
    ];
    let (s, w) = (local(shadow_rect), local(rect));
    let key = vec![
        area.loc.x as f32,
        area.loc.y as f32,
        area.size.w as f32,
        area.size.h as f32,
        radius,
        blur,
        color[0],
        color[1],
        color[2],
        color[3],
        s.0,
        s.1,
        s.2,
        s.3,
        w.0,
        w.1,
        w.2,
        w.3,
    ];
    let make_uniforms = uniforms.clone();
    reuse(
        &mut cached(window).borrow_mut().shadow,
        key,
        || {
            PixelShaderElement::new(shaders.shadow.clone(), area, None, 1.0, make_uniforms, Kind::Unspecified)
        },
        area,
        uniforms,
    )
}

#[cfg(test)]
mod tests {
    use super::invert;

    #[test]
    fn inverse_undoes_the_matrix() {
        // column major so scale x by 2 and y by -3 then move by 5 7
        let m = [2.0, 0.0, 0.0, 0.0, -3.0, 0.0, 5.0, 7.0, 1.0];
        let inv = invert(&m).unwrap();
        let apply = |m: &[f32; 9], p: [f32; 3]| {
            [
                m[0] * p[0] + m[3] * p[1] + m[6] * p[2],
                m[1] * p[0] + m[4] * p[1] + m[7] * p[2],
                m[2] * p[0] + m[5] * p[1] + m[8] * p[2],
            ]
        };
        let p = [11.0, -4.0, 1.0];
        let back = apply(&inv, apply(&m, p));
        for (a, b) in back.iter().zip(p) {
            assert!((a - b).abs() < 1e-4, "{back:?}");
        }
    }
}
