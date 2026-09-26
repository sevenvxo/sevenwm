//! easing curves and open close styles that the settings app copies so uhhhh keep them in sync

use serde::Deserialize;
use smithay::utils::{Logical, Point};

/// how progress from 0 to 1 turns into movement
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Curve {
    Linear,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
    EaseInOutCubic,
    /// goes a bit past then settles
    Back,
    /// bounces past the end and back a few times
    Spring,
}

impl Curve {
    pub fn ease(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::EaseOutQuad => 1.0 - (1.0 - t).powi(2),
            Self::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
            Self::EaseOutExpo => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - 2f64.powf(-10.0 * t)
                }
            }
            Self::EaseInOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Self::Back => {
                let (c1, c3) = (1.70158, 2.70158);
                1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
            }
            Self::Spring => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - (-6.0 * t).exp() * (10.5 * t).cos()
                }
            }
        }
    }
}

/// how a window shows up and backwards how it goes away
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Style {
    None,
    /// fades in
    Fade,
    /// grows a bit from the middle while fading in
    Zoom,
    /// grows from half size and looks best w a bouncy curve
    Pop,
    /// rises a bit while fading in
    Slide,
}

/// how a window is drawn at one moment of an animation
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub offset: Point<f64, Logical>,
    pub scale: f64,
    pub alpha: f32,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            offset: Point::default(),
            scale: 1.0,
            alpha: 1.0,
        }
    }
}

impl Style {
    /// the look p of the way in where 0 is not there and 1 is fully there
    pub fn look(self, p: f64) -> Look {
        let fade = p.clamp(0.0, 1.0) as f32;
        let grow = |from: f64| from + (1.0 - from) * p;
        match self {
            Self::None => Look::default(),
            Self::Fade => Look {
                alpha: fade,
                ..Look::default()
            },
            Self::Zoom => Look {
                scale: grow(0.85).max(0.01),
                alpha: fade,
                ..Look::default()
            },
            Self::Pop => Look {
                scale: grow(0.5).max(0.01),
                alpha: (p * 3.0).clamp(0.0, 1.0) as f32,
                ..Look::default()
            },
            Self::Slide => Look {
                offset: Point::from((0.0, 40.0 * (1.0 - p))),
                alpha: fade,
                ..Look::default()
            },
        }
    }
}

impl Look {
    /// where a canvas point of a window centered at centre gets drawn
    pub fn apply(&self, point: Point<f64, Logical>, centre: Point<f64, Logical>) -> Point<f64, Logical> {
        centre + (point - centre).upscale(self.scale) + self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_curve_starts_at_0_and_ends_at_1() {
        for curve in [
            Curve::Linear,
            Curve::EaseOutQuad,
            Curve::EaseOutCubic,
            Curve::EaseOutExpo,
            Curve::EaseInOutCubic,
            Curve::Back,
            Curve::Spring,
        ] {
            assert!(curve.ease(0.0).abs() < 1e-9, "{curve:?}");
            assert!((curve.ease(1.0) - 1.0).abs() < 1e-9, "{curve:?}");
        }
    }

    #[test]
    fn back_and_spring_overshoot() {
        let peak = |c: Curve| (0..100).map(|i| c.ease(i as f64 / 100.0)).fold(0.0, f64::max);
        assert!(peak(Curve::Back) > 1.0);
        assert!(peak(Curve::Spring) > 1.0);
        assert!(peak(Curve::EaseOutCubic) <= 1.0);
    }

    #[test]
    fn a_finished_look_is_the_window_as_is() {
        for style in [Style::None, Style::Fade, Style::Zoom, Style::Pop, Style::Slide] {
            assert_eq!(style.look(1.0), Look::default(), "{style:?}");
        }
    }
}
