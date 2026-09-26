mod actions;
mod animation;
mod audio;
mod capture;
mod closing;
mod collapse;
mod config;
mod cursor;
mod decorations;
mod grabs;
mod handlers;
mod input;
mod input_settings;
mod ipc;
mod layers;
mod layout;
mod lock;
mod menu;
mod monitors;
mod render;
mod screencast;
mod session;
mod state;
mod text;
mod titlebar;
mod tiling;
mod udev;
mod view;
mod wallpaper;
mod winit;
mod workspaces;
mod xwayland;

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;

use state::Seven;

/// what sevenwm launches inside itself when u give no command
const DEFAULT_CLIENT: &str = "kitty";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // sevenwm --check-config checks a config and exits
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--check-config") {
        let text = match args.next() {
            Some(path) => std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?,
            None => config::Config::path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default(),
        };
        match config::Config::from_toml(&text, false) {
            Ok(config) => {
                for warning in &config.warnings {
                    println!("warning: {warning}");
                }
                println!("ok");
                return Ok(());
            }
            Err(err) => {
                println!("error: {err}");
                std::process::exit(1);
            }
        }
    }

    init_logging();

    let mut event_loop: EventLoop<'static, Seven> = EventLoop::try_new()?;
    let display: Display<Seven> = Display::new()?;
    // inside another compositor run as a window or else take the hardware
    let args: Vec<String> = std::env::args().skip(1).collect();
    let nested = if args.iter().any(|a| a == "--hardware") {
        false
    } else {
        args.iter().any(|a| a == "--nested")
            || std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DISPLAY").is_some()
    };
    let mut state = Seven::new(&mut event_loop, display, nested);
    // sevenwm --greeter runs the login screen for greetd
    let command = args.iter().find(|a| !a.starts_with("--")).cloned();
    if args.iter().any(|a| a == "--greeter") {
        let Some(command) = command.clone() else {
            return Err("--greeter needs the greeter's command, e.g. \"sevenshell greeter\"".into());
        };
        state.config.restrict_for_greeter();
        state.greeter = Some(command);
    }
    state.apply_keyboard_settings();
    state.load_session();

    if nested {
        winit::init(&mut event_loop, &mut state)?;
    } else {
        udev::init(&mut event_loop, &mut state)?;
    }

    // kids have to connect to us not the host
    // safety still single threaded so nothing else reads the env yet
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
    tracing::info!("listening on {:?}", state.socket_name);

    state.start_xwayland();
    match ipc::listen(&mut event_loop, &mut state) {
        // everything sevenwm launches can find the socket
        // safety still single threaded startup
        Ok(path) => unsafe { std::env::set_var("SEVENWM_SOCK", path) },
        Err(err) => tracing::warn!("IPC socket: {err}"),
    }

    // pick up config changes within a sec and the settings app reads the defaults from here
    if let Some(dir) = crate::monitors::runtime_dir() {
        let _ = std::fs::write(dir.join("defaults.toml"), crate::config::DEFAULT_CONFIG);
    }
    event_loop.handle().insert_source(
        smithay::reexports::calloop::timer::Timer::from_duration(std::time::Duration::from_secs(1)),
        |_, _, state| {
            // once a sec redraw everything no matter what
            state.damage();
            state.reload_if_changed();
            state.reap_children();
            state.quit_after_greeter();
            state.xwayland_tick();
            state.keep_running();
            state.idle_tick();
            state.auto_collapse();
            state.update_surface_scales();
            state.save_session();
            smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                std::time::Duration::from_secs(1),
            )
        },
    )?;
    if !nested && state.greeter.is_none() {
        state.setup_portal();
    }

    // the first non flag arg is a command to start inside
    match command {
        Some(command) => state.spawn(&command),
        None if nested => state.spawn(DEFAULT_CLIENT),
        None => {
            let kept = state.config.keep_running.clone();
            for command in state.config.autostart.clone() {
                if !kept.contains(&command) {
                    state.spawn(&command);
                }
            }
            for command in &kept {
                state.spawn_kept(command);
            }
            state.kept_config = Some(kept);
        }
    }

    // a config that didnt load so say so once the notification daemon is prolly up
    event_loop.handle().insert_source(
        smithay::reexports::calloop::timer::Timer::from_duration(std::time::Duration::from_secs(4)),
        |_, _, state| {
            if let Some(err) = state.config_error.take() {
                state.notify_error("Config not applied; using the defaults", &err);
            }
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        },
    )?;

    event_loop.run(None, &mut state, |_| {})?;
    state.save_session();
    state.stop_xwayland();
    if let Some(path) = &state.ipc_path {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        // our own messages but only smithays warnings bc its info dumps every gl extension
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("sevenwm=info,warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
