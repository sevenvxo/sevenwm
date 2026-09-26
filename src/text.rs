//! js enough text for sevenwms own ui drawn one line at a time w fallback fonts for emoji

use std::collections::HashMap;

use swash::scale::{Render, ScaleContext, Source, StrikeWith, image::Content};
use swash::zeno::Format;
use swash::{FontRef, GlyphId};

pub struct Font {
    /// the main font first then fallbacks as needed
    faces: Vec<Vec<u8>>,
    /// which face draws each char so far
    chosen: HashMap<char, Option<usize>>,
    /// fallback fonts already loaded by path
    paths: HashMap<String, usize>,
    context: ScaleContext,
}

impl Font {
    /// the system sans serif font or noto sans
    pub fn system() -> Option<Self> {
        let path = fc_match("sans-serif")
            .unwrap_or_else(|| "/usr/share/fonts/noto/NotoSans-Regular.ttf".into());
        let data = std::fs::read(&path).ok()?;
        FontRef::from_index(&data, 0)?;
        Some(Self {
            faces: vec![data],
            chosen: HashMap::new(),
            paths: HashMap::from([(path, 0)]),
            context: ScaleContext::new(),
        })
    }

    /// the face that draws c and its glyph
    fn glyph(&mut self, c: char) -> (usize, GlyphId) {
        let face = match self.chosen.get(&c) {
            Some(face) => *face,
            None => {
                let face = self.find_face(c);
                self.chosen.insert(c, face);
                face
            }
        };
        let face = face.unwrap_or(0);
        let glyph = FontRef::from_index(&self.faces[face], 0).map_or(0, |f| f.charmap().map(c));
        (face, glyph)
    }

    fn find_face(&mut self, c: char) -> Option<usize> {
        let has = |data: &[u8]| FontRef::from_index(data, 0).is_some_and(|f| f.charmap().map(c) != 0);
        if let Some(i) = self.faces.iter().position(|d| has(d)) {
            return Some(i);
        }
        // spaces and control chars arent worth asking fontconfig
        if c.is_whitespace() || c.is_control() {
            return None;
        }
        let path = fc_match(&format!("sans-serif:charset={:x}", c as u32))?;
        if let Some(&i) = self.paths.get(&path) {
            return has(&self.faces[i]).then_some(i);
        }
        let data = std::fs::read(&path).ok()?;
        FontRef::from_index(&data, 0)?;
        let i = self.faces.len();
        let covers = has(&data);
        self.faces.push(data);
        self.paths.insert(path, i);
        covers.then_some(i)
    }

    /// width of text at size pixels
    pub fn width(&mut self, text: &str, size: f32) -> f32 {
        text.chars()
            .map(|c| {
                let (face, glyph) = self.glyph(c);
                FontRef::from_index(&self.faces[face], 0)
                    .map_or(0.0, |f| f.glyph_metrics(&[]).scale(size).advance_width(glyph))
            })
            .sum()
    }

    /// draw text w its baseline at x and baseline into pixels blending color
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        pixels: &mut [u8],
        stride: usize,
        x: f32,
        baseline: f32,
        text: &str,
        size: f32,
        color: [u8; 4],
    ) {
        let height = pixels.len() / 4 / stride;
        let mut pen = x;
        for c in text.chars() {
            let (face, glyph) = self.glyph(c);
            let Some(font) = FontRef::from_index(&self.faces[face], 0) else {
                continue;
            };
            let advance = font.glyph_metrics(&[]).scale(size).advance_width(glyph);
            let mut scaler = self.context.builder(font).size(size).hint(true).build();
            let image = Render::new(&[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
                Source::Bitmap(StrikeWith::BestFit),
            ])
            .format(Format::Alpha)
            .render(&mut scaler, glyph);
            if let Some(image) = image {
                let p = image.placement;
                for row in 0..p.height as i32 {
                    for col in 0..p.width as i32 {
                        let px = pen.round() as i32 + p.left + col;
                        let py = baseline.round() as i32 - p.top + row;
                        if px < 0 || py < 0 || px as usize >= stride || py as usize >= height {
                            continue;
                        }
                        let at = (row * p.width as i32 + col) as usize;
                        // the glyph color and uhh coverage at this pixel
                        let (rgb, alpha) = match image.content {
                            Content::Mask => {
                                let coverage = image.data[at] as u32;
                                ([color[0], color[1], color[2]], coverage * color[3] as u32 / 255)
                            }
                            Content::Color => {
                                let d = &image.data[at * 4..at * 4 + 4];
                                ([d[0], d[1], d[2]], d[3] as u32)
                            }
                            Content::SubpixelMask => continue,
                        };
                        let i = (py as usize * stride + px as usize) * 4;
                        for ch in 0..3 {
                            let dst = pixels[i + ch] as u32;
                            pixels[i + ch] =
                                ((rgb[ch] as u32 * alpha + dst * (255 - alpha)) / 255) as u8;
                        }
                        pixels[i + 3] = (alpha + pixels[i + 3] as u32 * (255 - alpha) / 255) as u8;
                    }
                }
            }
            pen += advance;
        }
    }
}

/// the font file fontconfig picks for pattern
fn fc_match(pattern: &str) -> Option<String> {
    std::process::Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .filter(|p| !p.is_empty())
}
