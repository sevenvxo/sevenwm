//! applies the input settings like keyboard layout and mouse stuff on real hardware

use smithay::input::keyboard::XkbConfig;
use smithay::reexports::input::{self as libinput, DeviceCapability};

use crate::config::AccelProfile;
use crate::state::Seven;

impl Seven {
    /// load the keyboard layout and repeat rate
    pub fn apply_keyboard_settings(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let kb = &self.config.input.keyboard;
        let (layout, variant, model) = (kb.layout.clone(), kb.variant.clone(), kb.model.clone());
        let options = (!kb.options.is_empty()).then(|| kb.options.clone());
        let (rate, delay) = (kb.repeat_rate, kb.repeat_delay);
        let xkb = XkbConfig {
            rules: "",
            model: &model,
            layout: &layout,
            variant: &variant,
            options,
        };
        if let Err(err) = keyboard.set_xkb_config(self, xkb) {
            tracing::warn!("keyboard layout '{layout}' didn't load ({err:?}); keeping the old one");
        }
        keyboard.change_repeat_info(rate, delay);
    }

    /// a mouse or touchpad got plugged in or was there at start
    pub fn add_input_device(&mut self, mut device: libinput::Device) {
        self.configure_device(&mut device);
        self.input_devices.push(device);
    }

    pub fn remove_input_device(&mut self, device: &libinput::Device) {
        self.input_devices.retain(|d| d != device);
    }

    /// apply settings to every device again after a reload
    pub fn reconfigure_input_devices(&mut self) {
        let mut devices = std::mem::take(&mut self.input_devices);
        for device in &mut devices {
            self.configure_device(device);
        }
        self.input_devices = devices;
    }

    fn configure_device(&self, device: &mut libinput::Device) {
        if !device.has_capability(DeviceCapability::Pointer) {
            return;
        }
        // touchpads are the pointers that can tap
        let touchpad = device.config_tap_finger_count() > 0;
        let input = &self.config.input;
        let (profile, speed, natural, left_handed) = if touchpad {
            let t = &input.touchpad;
            (
                t.accel_profile,
                t.accel_speed,
                t.natural_scroll,
                t.left_handed,
            )
        } else {
            let m = &input.mouse;
            (
                m.accel_profile,
                m.accel_speed,
                m.natural_scroll,
                m.left_handed,
            )
        };
        let profile = match profile {
            AccelProfile::Flat => libinput::AccelProfile::Flat,
            AccelProfile::Adaptive => libinput::AccelProfile::Adaptive,
        };
        // devices that dont have a setting refuse it and thats prolly fine
        let _ = device.config_accel_set_profile(profile);
        let _ = device.config_accel_set_speed(speed);
        let _ = device.config_scroll_set_natural_scroll_enabled(natural);
        let _ = device.config_left_handed_set(left_handed);
        if touchpad {
            let _ = device.config_tap_set_enabled(input.touchpad.tap);
            let _ = device.config_dwt_set_enabled(input.touchpad.disable_while_typing);
        }
        tracing::info!(
            "{} {}: {profile:?}, speed {speed}",
            if touchpad { "touchpad" } else { "mouse" },
            device.name()
        );
    }
}
