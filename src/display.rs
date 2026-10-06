//! night light thru the monitor gamma and the anti flashbang cap for the window shader

use crate::state::Seven;

/// how much of red green and blue is left at a color temperature in kelvin
pub fn temperature_rgb(kelvin: u32) -> [f64; 3] {
    // tanner hellands fit of blackbody colors which is close enough for a screen tint
    let t = kelvin.clamp(1000, 40000) as f64 / 100.0;
    let red = if t <= 66.0 {
        255.0
    } else {
        329.698727446 * (t - 60.0).powf(-0.1332047592)
    };
    let green = if t <= 66.0 {
        99.4708025861 * t.ln() - 161.1195681661
    } else {
        288.1221695283 * (t - 60.0).powf(-0.0755148492)
    };
    let blue = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.5177312231 * (t - 10.0).ln() - 305.0447927307
    };
    // 6500k counts as untouched white so the rest is scaled against it
    let white = [255.0, 254.0, 250.0];
    [red, green, blue]
        .iter()
        .zip(white)
        .map(|(c, w)| (c.clamp(0.0, 255.0) / w).min(1.0))
        .collect::<Vec<_>>()
        .try_into()
        .unwrap_or([1.0; 3])
}

/// a gamma ramp of size entries scaled by factor
pub fn ramp(size: usize, factor: f64) -> Vec<u16> {
    (0..size)
        .map(|i| {
            let v = i as f64 / (size.max(2) - 1) as f64;
            (v * factor * 65535.0).round().clamp(0.0, 65535.0) as u16
        })
        .collect()
}

/// minutes since local midnight
fn local_minutes() -> u32 {
    // safety localtime_r only writes into the tm we hand it
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        (tm.tm_hour * 60 + tm.tm_min) as u32
    }
}

/// sets every monitors gamma to these channel factors or back to normal on none and says if it worked
pub type GammaHook = Box<dyn FnMut(Option<[f64; 3]>) -> bool>;

impl Seven {
    /// the temperature night light wants now or none for normal colors
    pub fn night_light_wanted(&self) -> Option<u32> {
        let n = &self.config.night_light;
        n.active_at(local_minutes()).then_some(n.temperature)
    }

    /// every sec it puts the gamma right if the wanted night light changed or a monitor came back
    pub fn night_light_tick(&mut self) {
        let wanted = self.night_light_wanted();
        if self.night_applied == Some(wanted) {
            return;
        }
        let Some(hook) = self.gamma_hook.as_mut() else {
            return;
        };
        if hook(wanted.map(temperature_rgb)) {
            tracing::info!("night light {}", wanted.map_or("off".into(), |k| format!("{k}K")));
            self.night_applied = Some(wanted);
        }
    }

    /// set the window brightness cap for the next frames from the config
    pub fn apply_flash_cap(&self) {
        let a = &self.config.anti_flashbang;
        crate::decorations::set_flash_cap(if a.enabled { a.max_brightness } else { 1.0 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daylight_is_untouched_and_night_is_warm() {
        let day = temperature_rgb(6500);
        assert!(day.iter().all(|c| *c > 0.98));
        let night = temperature_rgb(3000);
        assert!(night[0] > 0.99 && night[1] < 0.8 && night[2] < 0.6);
    }

    #[test]
    fn a_ramp_goes_from_black_to_the_factor() {
        let r = ramp(256, 0.5);
        assert_eq!(r[0], 0);
        assert_eq!(r[255], 32768);
    }

    #[test]
    fn schedules_wrap_past_midnight() {
        let n = crate::config::NightLight {
            enabled: true,
            temperature: 4000,
            from: "20:00".into(),
            until: "07:00".into(),
        };
        assert!(n.active_at(22 * 60));
        assert!(n.active_at(3 * 60));
        assert!(!n.active_at(12 * 60));
        let always = crate::config::NightLight { from: String::new(), until: String::new(), ..n };
        assert!(always.active_at(12 * 60));
    }
}
