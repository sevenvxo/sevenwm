//! the wallpaper fixed to each monitor loaded and scaled once on its own thread and cached

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, TryRecvError};

use image::imageops::FilterType;
use image::{Rgba, RgbaImage};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Physical, Size, Transform};

use crate::config::{Color, WallpaperMode};

/// a laid out wallpaper from a loader thread
type Laid = Option<(Vec<u8>, u32, u32)>;

#[derive(Default)]
pub struct Wallpaper {
    /// the image the cache came from and how it was laid out
    source: Option<(String, WallpaperMode, Color)>,
    /// the laid out wallpaper for each monitor size
    sized: HashMap<(i32, i32), MemoryRenderBuffer>,
    /// the last wallpaper shown till the new one is ready
    previous: HashMap<(i32, i32), MemoryRenderBuffer>,
    /// sizes loading on another thread so a new wallpaper never stalls a frame and sizes that failed
    loading: HashMap<(i32, i32), Receiver<Laid>>,
    failed: HashSet<(i32, i32)>,
}

impl Wallpaper {
    /// the wallpaper for this monitor size and the old one shows while a new one loads
    pub fn for_size(
        &mut self,
        path: &str,
        mode: WallpaperMode,
        background: Color,
        size: Size<i32, Physical>,
    ) -> Option<&MemoryRenderBuffer> {
        if path.is_empty() {
            return None;
        }
        let key = (path.to_string(), mode, background);
        if self.source.as_ref() != Some(&key) {
            self.previous.extend(self.sized.drain());
            self.loading.clear();
            self.failed.clear();
            self.source = Some(key);
        }
        let at = (size.w, size.h);
        if let Some(rx) = self.loading.get(&at) {
            match rx.try_recv() {
                Ok(laid) => {
                    self.loading.remove(&at);
                    self.previous.remove(&at);
                    match laid {
                        Some((pixels, w, h)) => {
                            let buffer = MemoryRenderBuffer::from_slice(
                                &pixels,
                                Fourcc::Abgr8888,
                                (w as i32, h as i32),
                                1,
                                Transform::Normal,
                                None,
                            );
                            self.sized.insert(at, buffer);
                        }
                        None => {
                            self.failed.insert(at);
                        }
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.loading.remove(&at);
                    self.failed.insert(at);
                }
            }
        } else if !self.sized.contains_key(&at) && !self.failed.contains(&at) {
            let (tx, rx) = std::sync::mpsc::channel();
            let path = path.to_string();
            std::thread::spawn(move || {
                let laid = load(&path).map(|image| {
                    let canvas = lay_out(&image, mode, background, size);
                    let (w, h) = canvas.dimensions();
                    (canvas.into_raw(), w, h)
                });
                let _ = tx.send(laid);
            });
            self.loading.insert(at, rx);
        }
        self.sized.get(&at).or_else(|| self.previous.get(&at))
    }

    /// whether a wallpaper is still loading so frames keep coming
    pub fn is_loading(&self) -> bool {
        !self.loading.is_empty()
    }
}

fn load(path: &str) -> Option<RgbaImage> {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => path.to_string(),
    };
    match image::open(&expanded) {
        Ok(image) => Some(image.to_rgba8()),
        Err(err) => {
            tracing::warn!("wallpaper {expanded}: {err}");
            None
        }
    }
}

/// image laid out on a size canvas of background the way mode says
fn lay_out(
    image: &RgbaImage,
    mode: WallpaperMode,
    background: Color,
    size: Size<i32, Physical>,
) -> RgbaImage {
    let (w, h) = (size.w.max(1) as u32, size.h.max(1) as u32);
    // the background color is premultiplied so undo that for the image
    let a = background[3].max(1e-6);
    let bg = Rgba([
        (background[0] / a * 255.0) as u8,
        (background[1] / a * 255.0) as u8,
        (background[2] / a * 255.0) as u8,
        255,
    ]);
    let mut canvas = RgbaImage::from_pixel(w, h, bg);
    let (iw, ih) = (image.width() as f64, image.height() as f64);
    let place = |canvas: &mut RgbaImage, img: &RgbaImage| {
        let x = (w as i64 - img.width() as i64) / 2;
        let y = (h as i64 - img.height() as i64) / 2;
        image::imageops::overlay(canvas, img, x, y);
    };
    match mode {
        WallpaperMode::Stretch => {
            canvas = image::imageops::resize(image, w, h, FilterType::Triangle);
        }
        WallpaperMode::Fill | WallpaperMode::Fit => {
            let scale = if mode == WallpaperMode::Fill {
                (w as f64 / iw).max(h as f64 / ih)
            } else {
                (w as f64 / iw).min(h as f64 / ih)
            };
            let scaled = image::imageops::resize(
                image,
                ((iw * scale).round() as u32).max(1),
                ((ih * scale).round() as u32).max(1),
                FilterType::Triangle,
            );
            place(&mut canvas, &scaled);
        }
        WallpaperMode::Center => place(&mut canvas, image),
        WallpaperMode::Tile => {
            for y in (0..h).step_by(image.height().max(1) as usize) {
                for x in (0..w).step_by(image.width().max(1) as usize) {
                    image::imageops::overlay(&mut canvas, image, x as i64, y as i64);
                }
            }
        }
    }
    canvas
}
