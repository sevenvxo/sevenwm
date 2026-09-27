//! the camera onto the canvas where screen is canvas minus camera then * zoom

use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point, Rectangle, Size};

pub struct View {
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
    flight: Option<Flight>,
}

/// an animated move from one camera and zoom to another
struct Flight {
    from: (Point<f64, Logical>, f64),
    to: (Point<f64, Logical>, f64),
    start: Instant,
    duration: Duration,
}

impl Default for View {
    fn default() -> Self {
        Self {
            camera: Point::from((0.0, 0.0)),
            zoom: 1.0,
            flight: None,
        }
    }
}

impl View {
    pub fn to_canvas(&self, screen: Point<f64, Logical>) -> Point<f64, Logical> {
        Point::from((
            screen.x / self.zoom + self.camera.x,
            screen.y / self.zoom + self.camera.y,
        ))
    }

    pub fn to_screen(&self, canvas: Point<f64, Logical>) -> Point<f64, Logical> {
        Point::from((
            (canvas.x - self.camera.x) * self.zoom,
            (canvas.y - self.camera.y) * self.zoom,
        ))
    }

    /// the part of the canvas a screen of this size shows
    pub fn visible(&self, screen: Size<i32, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(
            self.camera,
            Size::from((screen.w as f64 / self.zoom, screen.h as f64 / self.zoom)),
        )
    }

    pub fn is_flying(&self) -> bool {
        self.flight.is_some()
    }

    /// jump straight to a camera and zoom and uhh cancel any flight
    pub fn set(&mut self, camera: Point<f64, Logical>, zoom: f64) {
        self.flight = None;
        self.camera = camera;
        self.zoom = zoom;
    }

    /// animate to a camera and zoom over duration and zero jumps
    pub fn fly_to(&mut self, camera: Point<f64, Logical>, zoom: f64, duration: Duration) {
        if duration.is_zero() {
            self.set(camera, zoom);
            return;
        }
        self.flight = Some(Flight {
            from: (self.camera, self.zoom),
            to: (camera, zoom),
            start: Instant::now(),
            duration,
        });
    }

    /// the camera and zoom a flight will land on
    pub fn destination(&self) -> (Point<f64, Logical>, f64) {
        self.flight
            .as_ref()
            .map_or((self.camera, self.zoom), |f| f.to)
    }

    /// move a flight forward to now along curve and say if the view moved
    pub fn tick(&mut self, now: Instant, curve: crate::animation::Curve) -> bool {
        let Some(flight) = &self.flight else {
            return false;
        };
        let t = (now.saturating_duration_since(flight.start).as_secs_f64()
            / flight.duration.as_secs_f64())
        .min(1.0);
        let eased = curve.ease(t);
        let lerp = |a: f64, b: f64| a + (b - a) * eased;
        let ((from_cam, from_zoom), (to_cam, to_zoom)) = (flight.from, flight.to);
        self.camera = Point::from((lerp(from_cam.x, to_cam.x), lerp(from_cam.y, to_cam.y)));
        // zoom blends on a log scale so a curve that overshoots cant push it to zero or below
        self.zoom = from_zoom.max(1e-6) * (to_zoom.max(1e-6) / from_zoom.max(1e-6)).powf(eased);
        if t >= 1.0 {
            self.flight = None;
        }
        true
    }

    /// zoom by factor keeping the point under screen still
    pub fn zoom_at(&mut self, screen: Point<f64, Logical>, factor: f64, min: f64, max: f64) {
        let anchor = self.to_canvas(screen);
        let zoom = (self.zoom * factor).clamp(min, max);
        self.set(
            Point::from((anchor.x - screen.x / zoom, anchor.y - screen.y / zoom)),
            zoom,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_and_canvas_round_trip() {
        let mut view = View::default();
        view.set(Point::from((100.0, -50.0)), 0.5);
        let canvas = Point::from((300.0, 250.0));
        let back = view.to_canvas(view.to_screen(canvas));
        assert!((back.x - canvas.x).abs() < 1e-9 && (back.y - canvas.y).abs() < 1e-9);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let mut view = View::default();
        let cursor = Point::from((400.0, 300.0));
        let before = view.to_canvas(cursor);
        view.zoom_at(cursor, 0.5, 0.1, 2.0);
        let after = view.to_canvas(cursor);
        assert!((before.x - after.x).abs() < 1e-9 && (before.y - after.y).abs() < 1e-9);
        assert_eq!(view.zoom, 0.5);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut view = View::default();
        view.zoom_at(Point::from((0.0, 0.0)), 100.0, 0.1, 2.0);
        assert_eq!(view.zoom, 2.0);
    }

    #[test]
    fn a_flight_lands_exactly() {
        let mut view = View::default();
        view.fly_to(Point::from((500.0, 500.0)), 0.5, Duration::from_millis(10));
        assert_eq!(view.destination(), (Point::from((500.0, 500.0)), 0.5));
        std::thread::sleep(Duration::from_millis(15));
        let curve = crate::animation::Curve::EaseOutCubic;
        assert!(view.tick(Instant::now(), curve));
        assert_eq!((view.camera, view.zoom), (Point::from((500.0, 500.0)), 0.5));
        assert!(!view.tick(Instant::now(), curve), "the flight is over");
    }

    #[test]
    fn overshooting_curves_keep_zoom_positive() {
        let mut view = View::default();
        view.set(Point::from((0.0, 0.0)), 1.0);
        let start = Instant::now();
        view.fly_to(Point::from((0.0, 0.0)), 0.1, std::time::Duration::from_millis(100));
        for ms in 0..=120 {
            let now = start + std::time::Duration::from_millis(ms);
            view.tick(now, crate::animation::Curve::Spring);
            assert!(view.zoom > 0.0, "zoom {} at {ms}ms", view.zoom);
        }
    }
}
