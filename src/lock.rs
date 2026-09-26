//! locking for swaylock plus idle stuff like telling swayidle and turning the screen off

use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

use crate::state::Seven;

pub enum Lock {
    Unlocked,
    /// a locker asked and its told locked once a frame w only the lock gets drawn
    Locking(SessionLocker),
    Locked,
}

impl Seven {
    pub fn is_locked(&self) -> bool {
        !matches!(self.lock, Lock::Unlocked)
    }

    /// after a frame a pending lock is on screen so its in force
    pub fn confirm_lock(&mut self) {
        if matches!(self.lock, Lock::Locking(_))
            && let Lock::Locking(locker) = std::mem::replace(&mut self.lock, Lock::Locked)
        {
            locker.lock();
            tracing::info!("session locked");
        }
    }

    /// the surface that gets input while locked
    pub fn lock_surface(&self) -> Option<&WlSurface> {
        self.lock_surfaces
            .iter()
            .find(|(_, o)| Some(o) == self.output.as_ref())
            .or(self.lock_surfaces.first())
            .map(|(s, _)| s.wl_surface())
    }

    /// the lock surface drawn on output
    pub fn lock_surface_on(&self, output: &smithay::output::Output) -> Option<&LockSurface> {
        self.lock_surfaces
            .iter()
            .find(|(_, o)| o == output)
            .map(|(s, _)| s)
    }

    /// any input so tell idle watchers and wake the screen
    pub fn activity(&mut self) {
        self.last_activity = Instant::now();
        self.idle_locked = false;
        self.idle_suspended = false;
        self.screen_off = false;
        self.idle_notifier_state.notify_activity(&self.seat);
    }

    /// whether the screen should be off rn
    pub fn should_blank(&self, now: Instant) -> bool {
        let after = self.config.idle.screen_off_after;
        after > 0
            && self.idle_inhibitors.is_empty()
            // not till the lock screen is uhh drawn
            && !matches!(self.lock, Lock::Locking(_))
            && now.duration_since(self.last_activity) >= Duration::from_secs(after)
    }
}

impl Seven {
    /// every sec lock and suspend when idle and bring the lock screen back if it died
    pub fn idle_tick(&mut self) {
        let now = Instant::now();
        let idle = now.duration_since(self.last_activity);
        let held = !self.idle_inhibitors.is_empty();
        let config = &self.config.idle;
        let reached = |after: u64| after > 0 && idle >= Duration::from_secs(after);
        if !held && !self.idle_locked && !self.is_locked() && reached(config.lock_after) {
            self.idle_locked = true;
            tracing::info!("idle: locking");
            let command = config.lock_command.clone();
            self.spawn(&command);
        }
        let config = &self.config.idle;
        if !held && !self.idle_suspended && reached(config.suspend_after) {
            self.idle_suspended = true;
            // nested its the hosts machine so dont put it to sleep
            if self.nested {
                tracing::info!("idle: would suspend now (not while nested)");
            } else {
                tracing::info!("idle: suspending");
                let command = config.suspend_command.clone();
                self.spawn(&command);
            }
        }

        // locked but the lock screen died so start another one to unlock w
        self.lock_surfaces.retain(|(surface, _)| surface.alive());
        if matches!(self.lock, Lock::Locked)
            && self.lock_surfaces.is_empty()
            && self
                .locker_respawned
                .is_none_or(|at| now.duration_since(at) > Duration::from_secs(5))
        {
            tracing::warn!("the lock screen is gone while locked; starting it again");
            self.locker_respawned = Some(now);
            let command = self.config.idle.lock_command.clone();
            self.spawn(&command);
        }
    }
}

impl Seven {
    /// end menus drags and pans so nothing keeps going behind the lock
    fn cancel_interactions(&mut self) {
        self.close_menu();
        self.pending_menu = None;
        // a dragged window shouldnt tile itself when its grab ends here
        self.drop_target = None;
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().expect("the seat has a pointer");
        pointer.unset_grab(self, serial, smithay::backend::input::InputTime::now());
        self.pending_menu = None;
        let keyboard = self.seat.get_keyboard().expect("the seat has a keyboard");
        keyboard.unset_grab(self);
        // close apps open menus too
        let roots: Vec<WlSurface> = self
            .space
            .elements()
            .filter_map(|w| w.toplevel().map(|t| t.wl_surface().clone()))
            .chain(self.space.outputs().flat_map(|o| {
                smithay::desktop::layer_map_for_output(o)
                    .layers()
                    .map(|l| l.wl_surface().clone())
                    .collect::<Vec<_>>()
            }))
            .collect();
        for root in roots {
            for (popup, _) in smithay::desktop::PopupManager::popups_for_surface(&root) {
                let _ = smithay::desktop::PopupManager::dismiss_popup(&root, &popup);
            }
        }
    }
}

impl SessionLockHandler for Seven {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        tracing::info!("locking");
        self.lock = Lock::Locking(confirmation);
        self.overview = None;
        self.freeze = None;
        self.cancel_interactions();
        // only the lock gets input from here on
        let surface = self.lock_surface().cloned();
        let keyboard = self.seat.get_keyboard().expect("the seat has a keyboard");
        keyboard.set_focus(self, surface, smithay::utils::SERIAL_COUNTER.next_serial());
        // wake the screen for the lock but dont restart the idle clock
        self.screen_off = false;
    }

    fn unlock(&mut self) {
        tracing::info!("unlocked");
        self.lock = Lock::Unlocked;
        self.lock_surfaces.clear();
        let last = self.most_recent_window();
        self.focus(last.as_ref());
        self.refresh_pointer();
    }

    /// the lockers full screen uhh surface for an output
    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = smithay::output::Output::from_resource(&output) else {
            return;
        };
        let size = output.current_mode().map_or(self.screen_size(), |mode| {
            let scale = output.current_scale().fractional_scale();
            mode.size.to_f64().to_logical(scale).to_i32_round()
        });
        surface.with_pending_state(|state| {
            state.size = Some((size.w as u32, size.h as u32).into());
        });
        surface.send_configure();
        let wl_surface = surface.wl_surface().clone();
        // a dead lockers surfaces would get drawn instead
        self.lock_surfaces.retain(|(s, _)| s.alive());
        self.lock_surfaces.push((surface, output));
        let keyboard = self.seat.get_keyboard().expect("the seat has a keyboard");
        keyboard.set_focus(
            self,
            Some(wl_surface),
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
    }
}

impl IdleNotifierHandler for Seven {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

/// video players and such keep the session from going idle
impl IdleInhibitHandler for Seven {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.push(surface);
        self.idle_notifier_state.set_is_inhibited(true);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.retain(|s| *s != surface);
        let inhibited = !self.idle_inhibitors.is_empty();
        self.idle_notifier_state.set_is_inhibited(inhibited);
        // an inhibitor ending restarts the idle clock instead of blanking right away
        self.last_activity = Instant::now();
    }
}
