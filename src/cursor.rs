//! the mouse cursor sevenwm draws on real hardware from a theme name or a surface

use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::pointer::CursorIcon;
use smithay::utils::{Logical, Point, Size, Transform};

/// a theme cursor image ready to draw
pub struct CursorImage {
    pub buffer: MemoryRenderBuffer,
    /// the size to draw it at
    pub size: Size<i32, Logical>,
    /// the whole image to scale from bc a size alone crops it
    pub src: Size<f64, Logical>,
    pub hotspot: Point<f64, Logical>,
}

/// loads cursors from the theme at the set size and caches them by name
pub struct Cursors {
    pub config: crate::config::Cursor,
    theme: xcursor::CursorTheme,
    /// by name and the uhhhh scale it was loaded for
    loaded: HashMap<(CursorIcon, i32), Option<CursorImage>>,
}

impl Cursors {
    pub fn new(config: &crate::config::Cursor) -> Self {
        Self {
            config: config.clone(),
            theme: xcursor::CursorTheme::load(&config.theme),
            loaded: HashMap::new(),
        }
    }

    /// the image for icon at scale or the default arrow if its missing
    pub fn get(&mut self, icon: CursorIcon, scale: i32) -> Option<&CursorImage> {
        let key = (icon, scale);
        if !self.loaded.contains_key(&key) {
            let image = self.load(icon, scale);
            self.loaded.insert(key, image);
        }
        if self.loaded[&key].is_none() && icon != CursorIcon::Default {
            return self.get(CursorIcon::Default, scale);
        }
        self.loaded[&key].as_ref()
    }

    fn load(&self, icon: CursorIcon, scale: i32) -> Option<CursorImage> {
        let names = std::iter::once(icon.name()).chain(icon.alt_names().iter().copied());
        let path = names
            .into_iter()
            .find_map(|name| self.theme.load_icon(name))?;
        let data = std::fs::read(path).ok()?;
        let images = xcursor::parser::parse_xcursor(&data)?;
        // smallest image at least the wanted size or the biggest and animated ones show frame one
        let size = self.config.size * scale.max(1) as u32;
        let image = images
            .iter()
            .filter(|image| image.size >= size)
            .min_by_key(|image| image.size)
            .or_else(|| images.iter().max_by_key(|image| image.size))?;
        // drawn at the set logical size
        let factor = self.config.size as f64 / image.size.max(1) as f64;
        // xcursors rgba bytes are really bgra in memory which is Fourcc::Argb8888
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Argb8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );
        Some(CursorImage {
            buffer,
            size: Size::from((
                (image.width as f64 * factor).round().max(1.0) as i32,
                (image.height as f64 * factor).round().max(1.0) as i32,
            )),
            src: Size::from((image.width as f64, image.height as f64)),
            hotspot: Point::from((image.xhot as f64 * factor, image.yhot as f64 * factor)),
        })
    }
}
