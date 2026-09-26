//! real hardware where sevenwm is the whole session and drives one gpu and all its monitors

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{
    DrmCompositor, FrameError, FrameFlags, PrimaryPlaneElement,
};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::ImportDma;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::ping::make_ping;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, LoopHandle};
use smithay::reexports::drm::control::{self, Device as _, connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{DeviceFd, Transform};
use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::render::{FrameElement, Target};
use crate::state::Seven;

type GbmDrmCompositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

/// an opened gpu w its display device event source allocator and renderer
type Gpu = (
    DrmDevice,
    smithay::backend::drm::DrmDeviceNotifier,
    GbmDevice<DrmDeviceFd>,
    GlesRenderer,
    DrmNode,
);

/// formats the screen can scan out most preferred first
const COLOR_FORMATS: [Fourcc; 4] = [
    Fourcc::Xrgb8888,
    Fourcc::Xbgr8888,
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
];

/// one lit up monitor
struct Screen {
    compositor: GbmDrmCompositor,
    output: Output,
    global: GlobalId,
    /// a frame is queued and its vblank prolly hasnt come yet
    flip_pending: bool,
    /// switched off for idle
    blanked: bool,
    /// the modes the monitor offers
    modes: Vec<control::Mode>,
    /// damage_gen when the last frame was drawn and if anything was moving
    drawn_gen: Option<u64>,
    animating: bool,
    /// a retry timer is already coming
    timer_pending: bool,
    /// after a frame that wasnt queued the soonest the next can draw so a busy client cant spin the loop
    not_before: Option<std::time::Instant>,
}

struct Hardware {
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    node: DrmNode,
    renderer: GlesRenderer,
    scanner: DrmScanner,
    screens: HashMap<crtc::Handle, Screen>,
    libinput: Libinput,
    /// the session is active and not switched to another vt
    active: bool,
}

type Shared = Rc<RefCell<Hardware>>;

pub fn init(
    event_loop: &mut EventLoop<'static, Seven>,
    state: &mut Seven,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut session, session_notifier) = LibSeatSession::new().map_err(|e| {
        format!("no seat session ({e}) — run sevenwm from a TTY or a login manager")
    })?;
    let seat = session.seat();
    tracing::info!("session on seat {seat}");

    let udev_backend = UdevBackend::new(&seat)?;
    let (drm, drm_notifier, gbm, renderer, node) = open_gpu(&mut session, &udev_backend, &seat)?;

    // clients render on this gpu and prolly hand us buffers straight from it
    let formats = renderer.dmabuf_formats();
    if let Ok(feedback) = DmabufFeedbackBuilder::new(drm.device_id(), formats).build() {
        state.enable_dmabuf(&feedback);
    }

    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&seat)
        .map_err(|()| "libinput couldn't take the seat's input devices")?;
    event_loop.handle().insert_source(
        LibinputInputBackend::new(libinput.clone()),
        |event, _, state| {
            match &event {
                InputEvent::DeviceAdded { device } => state.add_input_device(device.clone()),
                InputEvent::DeviceRemoved { device } => state.remove_input_device(device),
                _ => {}
            }
            state.handle_input(event);
        },
    )?;
    state.session = Some(session);

    let hw: Shared = Rc::new(RefCell::new(Hardware {
        drm,
        gbm,
        node,
        renderer,
        scanner: DrmScanner::new(),
        screens: HashMap::new(),
        libinput,
        active: true,
    }));
    let handle = event_loop.handle();
    scan_monitors(&hw, state, &handle);
    if hw.borrow().screens.is_empty() {
        return Err("no monitor could be lit up".into());
    }
    // start the pointer in the middle of the first monitor
    let size = state.screen_size();
    let centre = state.active_pos.to_f64()
        + smithay::utils::Point::from((size.w as f64 / 2.0, size.h as f64 / 2.0));
    state.set_pointer_global(centre);

    // a config reload w a new monitor mode switches to it
    let for_modes = hw.clone();
    let modes_handle = event_loop.handle();
    state.mode_hook = Some(Box::new(move |state| {
        apply_modes(&for_modes, state, &modes_handle);
    }));

    warn_about_other_gpus(&hw.borrow().node, state, &mut Vec::new());

    // something changed so draw idle monitors now instead of at the next refresh check which made frames land late
    let (ping, ping_source) = make_ping()?;
    state.redraw = Some(ping);
    let for_redraw = hw.clone();
    let redraw_handle = event_loop.handle();
    event_loop.handle().insert_source(ping_source, move |_, _, state| {
        let now = std::time::Instant::now();
        let idle: Vec<_> = for_redraw
            .borrow()
            .screens
            .iter()
            .filter(|(_, s)| !s.flip_pending)
            .map(|(crtc, s)| (*crtc, s.not_before))
            .collect();
        for (crtc, not_before) in idle {
            match not_before {
                Some(at) if at > now => render_later(&for_redraw, crtc, &redraw_handle, at - now),
                _ => render(&for_redraw, crtc, state, &redraw_handle),
            }
        }
    })?;

    let for_vblank = hw.clone();
    let vblank_handle = event_loop.handle();
    event_loop
        .handle()
        .insert_source(drm_notifier, move |event, _, state| match event {
            DrmEvent::VBlank(crtc) => {
                if let Some(screen) = for_vblank.borrow_mut().screens.get_mut(&crtc) {
                    if let Err(err) = screen.compositor.frame_submitted() {
                        tracing::warn!("frame submitted: {err}");
                    }
                    screen.flip_pending = false;
                }
                render(&for_vblank, crtc, state, &vblank_handle);
            }
            DrmEvent::Error(err) => tracing::error!("DRM error: {err}"),
        })?;

    // monitors plugged in or out
    let for_hotplug = hw.clone();
    let hotplug_handle = event_loop.handle();
    let mut warned = Vec::new();
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state| {
            if let UdevEvent::Changed { device_id } = event {
                let node = for_hotplug.borrow().node;
                if device_id == node.dev_id() {
                    scan_monitors(&for_hotplug, state, &hotplug_handle);
                } else {
                    warn_about_other_gpus(&node, state, &mut warned);
                }
            }
        })?;

    let for_session = hw.clone();
    let session_handle = event_loop.handle();
    event_loop
        .handle()
        .insert_source(session_notifier, move |event, _, state| match event {
            SessionEvent::PauseSession => {
                tracing::info!("switched away: pausing");
                let mut hw = for_session.borrow_mut();
                hw.active = false;
                hw.libinput.suspend();
                hw.drm.pause();
            }
            SessionEvent::ActivateSession => {
                tracing::info!("switched back: resuming");
                let crtcs: Vec<crtc::Handle> = {
                    let mut hw = for_session.borrow_mut();
                    if hw.libinput.resume().is_err() {
                        tracing::warn!("libinput didn't resume");
                    }
                    if let Err(err) = hw.drm.activate(false) {
                        tracing::error!("DRM didn't resume: {err}");
                        return;
                    }
                    hw.active = true;
                    for screen in hw.screens.values_mut() {
                        if let Err(err) = screen.compositor.reset_state() {
                            tracing::warn!("resetting a display: {err}");
                        }
                        screen.flip_pending = false;
                        screen.drawn_gen = None;
                    }
                    hw.screens.keys().copied().collect()
                };
                // monitors may have changed while we were away
                scan_monitors(&for_session, state, &session_handle);
                for crtc in crtcs {
                    render(&for_session, crtc, state, &session_handle);
                }
            }
        })?;
    Ok(())
}

/// open the first gpu w a monitor plugged in and a renderer on it
fn open_gpu(
    session: &mut LibSeatSession,
    udev_backend: &UdevBackend,
    seat: &str,
) -> Result<Gpu, Box<dyn std::error::Error>> {
    let mut candidates: Vec<PathBuf> = udev::primary_gpu(seat).ok().flatten().into_iter().collect();
    for (_, path) in udev_backend.device_list() {
        if !candidates.iter().any(|p| p == path) {
            candidates.push(path.to_path_buf());
        }
    }

    for path in candidates {
        let Ok(node) = DrmNode::from_path(&path) else {
            continue;
        };
        if node.ty() != NodeType::Primary {
            continue;
        }
        let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let fd = match session.open(&path, flags) {
            Ok(fd) => DrmDeviceFd::new(DeviceFd::from(fd)),
            Err(err) => {
                tracing::warn!("{}: can't open ({err})", path.display());
                continue;
            }
        };
        let (drm, notifier) = match DrmDevice::new(fd.clone(), true) {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!("{}: not a usable display device ({err})", path.display());
                continue;
            }
        };
        if !has_monitor(&drm) {
            tracing::info!(
                "{}: no monitor connected, trying the next GPU",
                path.display()
            );
            continue;
        }
        let gbm = GbmDevice::new(fd)?;
        // safety the display outlives nothing it borrows bc gbm is cloned in
        let egl = unsafe { EGLDisplay::new(gbm.clone())? };
        let context = EGLContext::new(&egl)?;
        // safety the context is fresh and not current on any other thread
        let renderer = unsafe { GlesRenderer::new(context)? };
        tracing::info!("using GPU {}", path.display());
        return Ok((drm, notifier, gbm, renderer, node));
    }
    Err("no GPU with a monitor connected".into())
}

/// we only drive one card so say so once if a monitor is on another one
fn warn_about_other_gpus(node: &DrmNode, state: &Seven, warned: &mut Vec<String>) {
    let ours = node
        .dev_path()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // connectors look like card1-HDMI-A-1 so skip our own cards
        let Some((card, connector)) = name.split_once('-') else {
            continue;
        };
        if !card.starts_with("card") || card == ours {
            continue;
        }
        let connected = std::fs::read_to_string(entry.path().join("status"))
            .is_ok_and(|s| s.trim() == "connected");
        if connected && !warned.contains(&name) {
            warned.push(name.clone());
            let message = format!(
                "{connector} is on another graphics card ({card}); sevenwm only drives {ours}, \
                 so it stays dark. Plug it into the same card as your main monitor."
            );
            tracing::warn!("{message}");
            state.notify(&message);
        } else if !connected {
            warned.retain(|n| *n != name);
        }
    }
}

fn has_monitor(drm: &DrmDevice) -> bool {
    let Ok(resources) = drm.resource_handles() else {
        return false;
    };
    resources.connectors().iter().any(|&c| {
        drm.get_connector(c, false)
            .is_ok_and(|info| info.state() == connector::State::Connected)
    })
}

/// compare connected monitors to lit ones and light up new ones and drop unplugged ones
fn scan_monitors(hw: &Shared, state: &mut Seven, handle: &LoopHandle<'static, Seven>) {
    let events = {
        let mut guard = hw.borrow_mut();
        let Hardware { scanner, drm, .. } = &mut *guard;
        match scanner.scan_connectors(drm) {
            Ok(result) => result.into_iter().collect::<Vec<_>>(),
            Err(err) => {
                tracing::warn!("scanning monitors: {err}");
                return;
            }
        }
    };
    for event in events {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => {
                let lit = light_up(&mut hw.borrow_mut(), &connector, crtc, state);
                match lit {
                    Ok(output) => {
                        state.add_monitor(&output);
                        render(hw, crtc, state, handle);
                    }
                    Err(err) => tracing::warn!("a monitor didn't light up: {err}"),
                }
            }
            DrmScanEvent::Disconnected {
                crtc: Some(crtc), ..
            } => {
                let removed = hw.borrow_mut().screens.remove(&crtc);
                if let Some(screen) = removed {
                    state.remove_monitor(&screen.output);
                    state.display_handle.remove_global::<Seven>(screen.global);
                }
            }
            _ => {}
        }
    }
}

/// light up a monitor and wrap it in a compositor that renders into it
fn light_up(
    hw: &mut Hardware,
    connector: &connector::Info,
    crtc: crtc::Handle,
    state: &mut Seven,
) -> Result<Output, Box<dyn std::error::Error>> {
    let name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );
    let mode = pick_mode(connector.modes(), state.config.output_mode(&name))
        .ok_or("the monitor offers no modes")?;
    let (w, h) = mode.size();
    tracing::info!("{name}: {w}x{h} @ {} Hz", mode.vrefresh());
    let surface = hw.drm.create_surface(crtc, mode, &[connector.handle()])?;

    let (mm_w, mm_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        name,
        PhysicalProperties {
            size: (mm_w as i32, mm_h as i32).into(),
            subpixel: Subpixel::Unknown,
            make: "Unknown".into(),
            model: "Unknown".into(),
            serial_number: "Unknown".into(),
        },
    );
    let output_mode = Mode {
        size: (w as i32, h as i32).into(),
        refresh: mode.vrefresh() as i32 * 1000,
    };
    output.change_current_state(Some(output_mode), Some(Transform::Normal), None, None);
    output.set_preferred(output_mode);
    let mut modes: Vec<String> = connector
        .modes()
        .iter()
        .map(|m| format!("{}x{}@{}", m.size().0, m.size().1, m.vrefresh()))
        .collect();
    modes.dedup();
    output
        .user_data()
        .insert_if_missing(|| crate::monitors::AvailableModes(modes));
    let global = output.create_global::<Seven>(&state.display_handle);

    let allocator = GbmAllocator::new(
        hw.gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let compositor = DrmCompositor::new(
        &output,
        surface,
        None,
        allocator,
        GbmFramebufferExporter::new(hw.gbm.clone(), None.into()),
        COLOR_FORMATS,
        hw.renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied(),
        hw.drm.cursor_size(),
        Some(hw.gbm.clone()),
    )
    .map_err(|e| format!("setting up the display: {e:?}"))?;
    hw.screens.insert(
        crtc,
        Screen {
            compositor,
            output: output.clone(),
            global,
            flip_pending: false,
            blanked: false,
            modes: connector.modes().to_vec(),
            drawn_gen: None,
            animating: false,
            timer_pending: false,
            not_before: None,
        },
    );
    Ok(output)
}

/// switch every monitor whose set mode changed
fn apply_modes(hw: &Shared, state: &mut Seven, handle: &LoopHandle<'static, Seven>) {
    let mut changed = Vec::new();
    {
        let mut guard = hw.borrow_mut();
        for (crtc, screen) in guard.screens.iter_mut() {
            let name = screen.output.name();
            let Some(mode) = pick_mode(&screen.modes, state.config.output_mode(&name)) else {
                continue;
            };
            if mode == screen.compositor.pending_mode() {
                continue;
            }
            let (w, h) = mode.size();
            match screen.compositor.use_mode(mode) {
                Ok(()) => {
                    tracing::info!("{name}: now {w}x{h} @ {} Hz", mode.vrefresh());
                    let output_mode = Mode {
                        size: (w as i32, h as i32).into(),
                        refresh: mode.vrefresh() as i32 * 1000,
                    };
                    screen
                        .output
                        .change_current_state(Some(output_mode), None, None, None);
                    screen.drawn_gen = None;
                    changed.push(*crtc);
                }
                Err(err) => tracing::warn!("{name}: couldn't switch to {w}x{h}: {err:?}"),
            }
        }
    }
    if changed.is_empty() {
        return;
    }
    state.arrange_layers();
    state.resize_workspaces();
    state.publish_monitors();
    for crtc in changed {
        render(hw, crtc, state, handle);
    }
}

/// the set mode if the monitor has it or its best res at the highest refresh
fn pick_mode(modes: &[control::Mode], wanted: Option<(u16, u16, u32)>) -> Option<control::Mode> {
    if let Some((w, h, hz)) = wanted {
        let exact = modes
            .iter()
            .find(|m| m.size() == (w, h) && m.vrefresh() == hz);
        if exact.is_some() {
            return exact.copied();
        }
        tracing::warn!("the monitor has no {w}x{h}@{hz} mode; picking one");
    }
    let preferred = modes
        .iter()
        .find(|m| m.mode_type().contains(control::ModeTypeFlags::PREFERRED))
        .or_else(|| {
            modes
                .iter()
                .max_by_key(|m| m.size().0 as u32 * m.size().1 as u32)
        })?;
    modes
        .iter()
        .filter(|m| m.size() == preferred.size())
        .max_by_key(|m| m.vrefresh())
        .copied()
}

/// draw and queue one frame and the vblank calls back for the next or a timer retries
fn render(hw: &Shared, crtc: crtc::Handle, state: &mut Seven, handle: &LoopHandle<'static, Seven>) {
    let mut guard = hw.borrow_mut();
    let Hardware {
        renderer,
        screens,
        active,
        ..
    } = &mut *guard;
    let Some(screen) = screens.get_mut(&crtc) else {
        return;
    };
    if !*active || screen.flip_pending {
        return;
    }

    let now = std::time::Instant::now();
    if state.should_blank(now) {
        state.screen_off = true;
    }
    if state.screen_off {
        if !screen.blanked {
            tracing::info!("idle: {} off", screen.output.name());
            if let Err(err) = screen.compositor.clear() {
                tracing::warn!("turning the screen off: {err}");
            }
            screen.blanked = true;
        }
        // wait for input to wake it
        drop(guard);
        render_later(hw, crtc, handle, Duration::from_millis(100));
        return;
    }
    if std::mem::take(&mut screen.blanked) {
        tracing::info!("{} on", screen.output.name());
        screen.drawn_gen = None;
    }

    // nothing changed and nothing is moving so skip drawing till something does
    let generation = state.damage_gen.get();
    let quiet = screen.drawn_gen == Some(generation)
        && !screen.animating
        && state.pending_captures.is_empty()
        && state.freeze.is_none();
    if quiet {
        let refresh = frame_time(&screen.output);
        drop(guard);
        render_later(hw, crtc, handle, refresh);
        return;
    }

    // draw this monitor as the active one then give the pointers monitor its spot back
    let output = screen.output.clone();
    state.activate(&output);
    // only the pointers own monitor aims it bc pointer_screen means nothing thru another camera
    let flew = state.view.tick(now, state.config.animations.fly_curve);
    if flew && state.pointer_output.as_deref() == Some(output.name().as_str()) {
        state.refresh_pointer();
    }
    state.expire_freeze(now);

    crate::capture::take_freeze_still(state, renderer, &output);
    let elements: Vec<FrameElement> =
        crate::render::compose(state, renderer, &output, Target::Screen);
    screen.drawn_gen = Some(generation);
    screen.animating = state.animating.get() || state.view.is_flying();
    let background = state.config.canvas.background;
    // no overlay or cursor planes bc they stutter on nvidia but fullscreen apps can still scan out
    let flags = FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY;
    let queued = match screen
        .compositor
        .render_frame::<_, FrameElement>(renderer, &elements, background, flags)
    {
        Ok(result) => {
            // without a fence the display can take wait for the gpu
            if result.needs_sync()
                && let PrimaryPlaneElement::Swapchain(element) = &result.primary_element
            {
                let _ = element.sync.wait();
            }
            match screen.compositor.queue_frame(()) {
                Ok(()) => true,
                Err(FrameError::EmptyFrame) => false,
                Err(err) => {
                    tracing::warn!("queueing a frame: {err:?}");
                    false
                }
            }
        }
        Err(err) => {
            tracing::warn!("rendering a frame: {err:?}");
            false
        }
    };
    crate::capture::serve_captures(state, renderer, &output);
    state.frame_done(&output);
    screen.flip_pending = queued;
    screen.not_before = (!queued).then(|| now + frame_time(&output));
    if let Some(pointer_output) = state
        .pointer_output
        .clone()
        .and_then(|name| state.outputs().into_iter().find(|o| o.name() == name))
    {
        state.activate(&pointer_output);
    }
    // w the pointers monitor active again so the state doesnt flip every frame
    state.ipc_notify();

    if !queued {
        let refresh = frame_time(&output);
        drop(guard);
        render_later(hw, crtc, handle, refresh);
    }
}

/// how long one frame lasts on output
fn frame_time(output: &Output) -> Duration {
    // refresh is in mhz so one frame is alot over mhz microseconds
    let micros = output
        .current_mode()
        .map_or(16_667, |m| 1_000_000_000 / m.refresh.max(1) as u64);
    Duration::from_micros(micros.max(1_000))
}

/// try another frame after delay when no vblank is coming
fn render_later(
    hw: &Shared,
    crtc: crtc::Handle,
    handle: &LoopHandle<'static, Seven>,
    delay: Duration,
) {
    // one timer per monitor at a time bc extra chains never stopped
    {
        let mut guard = hw.borrow_mut();
        let Some(screen) = guard.screens.get_mut(&crtc) else {
            return;
        };
        if std::mem::replace(&mut screen.timer_pending, true) {
            return;
        }
    }
    let again = hw.clone();
    let timer_handle = handle.clone();
    let _ = handle.insert_source(Timer::from_duration(delay), move |_, _, state| {
        if let Some(screen) = again.borrow_mut().screens.get_mut(&crtc) {
            screen.timer_pending = false;
        }
        render(&again, crtc, state, &timer_handle);
        TimeoutAction::Drop
    });
}
