#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! `chattercrab`: the Chattercrab desktop client.
//!
//! The iced UI runs on the main thread. Networking (REST, signaling, reconnect) and
//! the media engine run on a separate Tokio runtime thread; the two sides talk only
//! through bounded channels.

mod config;
mod icon;
mod net;
mod ui;

use std::sync::{Arc, Mutex};

use anyhow::Context;
use iced::Size;
use tokio::sync::mpsc;

use config::Config;
use net::{APP_EVENT_CAPACITY, UI_COMMAND_CAPACITY, UiCommand};
use ui::App;

/// Log filter when `RUST_LOG` is unset: our logs at info, chatty GPU/windowing crates at warn.
/// The webrtc driver logs an ERROR for every packet sent from a host candidate on an
/// unreachable virtual adapter (Hyper-V, WSL); ICE handles that, so it is silenced.
const DEFAULT_LOG_FILTER: &str = concat!(
    "info,iced_winit=warn,iced_wgpu=warn,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn,",
    "webrtc::peer_connection::driver=off"
);

const USAGE: &str = "\
Usage: chattercrab [OPTIONS]

Options:
  --software-render   Use the tiny-skia CPU renderer instead of the GPU (wgpu)
  --config <PATH>     Use this config file instead of the platform default
  -h, --help          Print this help
  -V, --version       Print the version";

/// Parsed command-line options.
#[derive(Debug, Default, PartialEq)]
struct Args {
    software_render: bool,
    config: Option<std::path::PathBuf>,
}

/// What `main` should do after parsing the command line.
#[derive(Debug, PartialEq)]
enum Cli {
    Run(Args),
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> anyhow::Result<Cli> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--software-render" => parsed.software_render = true,
            "--config" => {
                let path = args.next().context("--config needs a path")?;
                parsed.config = Some(path.into());
            }
            "-h" | "--help" => return Ok(Cli::Help),
            "-V" | "--version" => return Ok(Cli::Version),
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(Cli::Run(parsed))
}

fn main() -> anyhow::Result<()> {
    // Release builds are GUI programs, so a terminal that launched us does not
    // receive stdout or stderr until we attach to it.
    attach_parent_console();
    let args = match parse_args(std::env::args().skip(1))? {
        Cli::Run(args) => args,
        Cli::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Cli::Version => {
            println!("chattercrab {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    };
    if args.software_render {
        // SAFETY: no other threads exist yet, so nothing can read the environment
        // concurrently. iced reads ICED_BACKEND when it creates the compositor.
        unsafe { std::env::set_var("ICED_BACKEND", "tiny-skia") };
    } else if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
        // DX12 idles with markedly less memory than Vulkan or GL on Windows drivers.
        // If it is unavailable iced falls back to tiny-skia. Users can still override
        // the choice with WGPU_BACKEND.
        // SAFETY: as above, still single-threaded.
        unsafe { std::env::set_var("WGPU_BACKEND", "dx12") };
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .with_writer(std::io::stderr)
        .init();

    // One process-wide TLS crypto provider (ring) for reqwest and tungstenite.
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("a rustls crypto provider was already installed");
    }

    let config_path = args.config.or_else(Config::default_path);
    let config = match config_path.as_deref().map(Config::load).transpose() {
        Ok(config) => config.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "ignoring unreadable config");
            Config::default()
        }
    };

    let (cmd_tx, cmd_rx) = mpsc::channel(UI_COMMAND_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(APP_EVENT_CAPACITY);
    let net_thread =
        net::backend::spawn_runtime_thread(cmd_rx, event_tx).context("spawning network thread")?;
    let events: ui::EventSlot = Arc::new(Mutex::new(Some(event_rx)));

    let window = iced::window::Settings {
        size: Size::new(1100.0, 720.0),
        min_size: Some(Size::new(720.0, 480.0)),
        icon: iced::window::icon::from_rgba(icon::crab_rgba(64), 64, 64).ok(),
        ..Default::default()
    };

    let boot_tx = cmd_tx.clone();
    let result = iced::application(
        move || {
            App::new(
                config.clone(),
                config_path.clone(),
                boot_tx.clone(),
                events.clone(),
            )
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .theme(App::theme)
    .subscription(App::subscription)
    // No canvas or meshes are drawn, so MSAA would only cost GPU memory.
    .antialiasing(false)
    .window(window)
    .run();

    let _ = cmd_tx.try_send(UiCommand::Shutdown);
    drop(cmd_tx);
    if net_thread.join().is_err() {
        tracing::error!("network thread panicked");
    }
    result.context("running the UI")
}

/// Connects a release build to the console that started it, so logs are visible
/// in that terminal. A double-clicked window has no parent console and is left alone.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console() {
    use std::ffi::c_void;

    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn SetStdHandle(std_handle: u32, handle: *mut c_void) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
    }

    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = u32::MAX - 10;
    const STD_ERROR_HANDLE: u32 = u32::MAX - 11;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const OPEN_EXISTING: u32 = 3;

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let handle = CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if handle.is_null() || handle == usize::MAX as *mut c_void {
            return;
        }
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_parent_console() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> anyhow::Result<Cli> {
        parse_args(list.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn default_log_filter_parses() {
        let filter = tracing_subscriber::EnvFilter::try_new(DEFAULT_LOG_FILTER).unwrap();
        assert!(
            filter
                .to_string()
                .contains("webrtc::peer_connection::driver=off")
        );
    }

    #[test]
    fn parses_flags() {
        assert_eq!(args(&[]).unwrap(), Cli::Run(Args::default()));
        assert_eq!(
            args(&["--software-render", "--config", "x.toml"]).unwrap(),
            Cli::Run(Args {
                software_render: true,
                config: Some("x.toml".into())
            })
        );
        assert_eq!(args(&["--help"]).unwrap(), Cli::Help);
        assert_eq!(args(&["-V"]).unwrap(), Cli::Version);
        assert!(args(&["--bogus"]).is_err());
        assert!(args(&["--config"]).is_err());
    }
}
