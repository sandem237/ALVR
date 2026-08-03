//! `alvr_service` — the persistent ALVR background service.
//!
//! Owns every headset connection and streams continuously, independently of any OpenXR application.
//! Applications connect over a named pipe to enumerate devices, subscribe to changes, and claim a
//! device to render to; the service hands them a shared D3D11 texture to draw into.
//!
//! Deliberately not a SteamVR driver: it drives `ServerCoreContext` directly with
//! `restart_on_settings_change: false`, because restarting on a resolution change would tear down
//! every other connected headset.
//!
//! ```text
//! alvr_service --auto-trust
//! ```

mod control;
mod pipe;
mod registry;

use alvr_common::{ConnectionState, info, warn};
use alvr_filesystem as afs;
use alvr_server_core::{ServerCoreConfig, ServerCoreContext, ServerCoreEvent};
use alvr_service_protocol::Event;
use alvr_session::{CodecType, SessionConfig, SocketProtocol};
use registry::{DeviceRegistry, StreamParams};
use serde_json::json;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

struct Args {
    protocol: SocketProtocol,
    auto_trust: bool,
    config_dir: PathBuf,
    run_for: Option<Duration>,
}

fn usage() -> String {
    "\
Usage: alvr_service [options]

The ALVR background service: owns all headset connections and serves OpenXR applications over a
named pipe. Runs headless, without SteamVR.

Options:
  --protocol <udp|tcp>    Stream protocol (default udp)
  --auto-trust            Trust discovered headsets automatically (default on)
  --no-auto-trust         Require headsets to be trusted manually
  --config-dir <PATH>     Config directory (default: the standard ALVR config dir)
  --run-for <SECONDS>     Exit after this long (for tests; default: run until Ctrl-C)
  -h, --help              Show this help"
        .to_owned()
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut protocol = SocketProtocol::Udp;
        let mut auto_trust = true;
        let mut config_dir = env::temp_dir().join("alvr_service");
        let mut run_for = None;

        let args = env::args().skip(1).collect::<Vec<_>>();
        let mut idx = 0;
        while idx < args.len() {
            let arg = args[idx].as_str();
            let mut next = |name: &str| -> Result<String, String> {
                idx += 1;
                args.get(idx)
                    .cloned()
                    .ok_or_else(|| format!("{name} requires a value"))
            };

            match arg {
                "--protocol" => {
                    protocol = match next("--protocol")?.to_lowercase().as_str() {
                        "udp" => SocketProtocol::Udp,
                        "tcp" => SocketProtocol::Tcp,
                        other => return Err(format!("Unknown protocol: {other}")),
                    }
                }
                "--auto-trust" => auto_trust = true,
                "--no-auto-trust" => auto_trust = false,
                "--config-dir" => config_dir = PathBuf::from(next("--config-dir")?),
                "--run-for" => {
                    let secs: u64 = next("--run-for")?.parse().map_err(|e| format!("{e}"))?;
                    run_for = Some(Duration::from_secs(secs));
                }
                "--help" | "-h" => {
                    println!("{}", usage());
                    std::process::exit(0);
                }
                other => return Err(format!("Unknown argument: {other}\n\n{}", usage())),
            }

            idx += 1;
        }

        Ok(Self {
            protocol,
            auto_trust,
            config_dir,
            run_for,
        })
    }
}

/// Writes a session configured for headless multi-device operation.
///
/// `SessionSettings` is macro-generated, so this edits the serialized JSON rather than naming
/// generated field types.
fn write_session(args: &Args) -> Result<(), String> {
    fs::create_dir_all(&args.config_dir).map_err(|e| format!("{e}"))?;

    let mut value =
        serde_json::to_value(SessionConfig::default()).map_err(|e| format!("{e}"))?;

    let settings = value
        .get_mut("session_settings")
        .ok_or("missing session_settings")?;
    let connection = settings
        .get_mut("connection")
        .ok_or("missing connection settings")?;

    connection["stream_protocol"]["variant"] = match args.protocol {
        SocketProtocol::Tcp => json!("Tcp"),
        SocketProtocol::Udp => json!("Udp"),
    };
    connection["client_discovery"]["enabled"] = json!(true);
    connection["client_discovery"]["content"]["auto_trust_clients"] = json!(args.auto_trust);

    // No audio devices in the headless service for now.
    settings["audio"]["game_audio"]["enabled"] = json!(false);
    settings["audio"]["microphone"]["enabled"] = json!(false);

    let json = serde_json::to_string_pretty(&value).map_err(|e| format!("{e}"))?;
    fs::write(args.config_dir.join("session.json"), json).map_err(|e| format!("{e}"))?;

    Ok(())
}

fn layout(config_dir: &Path) -> afs::Layout {
    afs::Layout {
        config_dir: config_dir.to_owned(),
        log_dir: config_dir.to_owned(),
        ..afs::Layout::new(&env::current_dir().unwrap())
    }
}

fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(message) = write_session(&args) {
        eprintln!("Failed to write session: {message}");
        return ExitCode::FAILURE;
    }

    alvr_server_core::initialize_environment(layout(&args.config_dir));
    alvr_server_core::init_logging(None, None);

    info!(
        "ALVR service starting: protocol={} auto_trust={} config_dir={}",
        match args.protocol {
            SocketProtocol::Udp => "udp",
            SocketProtocol::Tcp => "tcp",
        },
        args.auto_trust,
        args.config_dir.display()
    );

    let registry = Arc::new(DeviceRegistry::new());

    // Never restart on a settings change: this backend reconfigures per client, and a restart would
    // tear down every other connected headset.
    let (context, events_receiver) = ServerCoreContext::with_config(ServerCoreConfig {
        restart_on_settings_change: false,
    });
    let context = Arc::new(context);
    context.start_connection();

    // Accept control clients.
    let control_handle = control::spawn_listener(Arc::clone(&registry), Arc::clone(&context));

    let started = Instant::now();
    let mut frame_index: u64 = 0;
    let mut last_sync = Instant::now() - Duration::from_secs(1);

    loop {
        if let Some(run_for) = args.run_for
            && started.elapsed() >= run_for
        {
            info!("Run duration reached");
            break;
        }

        while let Ok(event) = events_receiver.try_recv() {
            match event {
                ServerCoreEvent::ClientConnected { client_id, config } => {
                    info!(
                        "Headset connected: {client_id} ({}x{} @ {}Hz, {:?})",
                        config.transcoding_view_resolution.x,
                        config.transcoding_view_resolution.y,
                        config.refresh_rate,
                        config.codec,
                    );

                    registry.set_stream_params(
                        &client_id,
                        StreamParams {
                            view_resolution: [
                                config.transcoding_view_resolution.x,
                                config.transcoding_view_resolution.y,
                            ],
                            refresh_rate: config.refresh_rate,
                        },
                    );

                    // A real encoder would publish its own config here.
                    context.set_video_config_nals_for_client(
                        &client_id,
                        vec![0, 0, 0, 1, 0x67],
                        CodecType::H264,
                    );
                }
                ServerCoreEvent::ClientDisconnected { client_id } => {
                    info!("Headset disconnected: {client_id}");
                }
                ServerCoreEvent::ShutdownPending | ServerCoreEvent::RestartPending => {
                    info!("Shutdown requested");
                    break;
                }
                _ => (),
            }
        }

        // Mirror the session client list into the registry. Cheap, and the registry only emits an
        // event when something actually changed.
        if last_sync.elapsed() >= Duration::from_millis(250) {
            last_sync = Instant::now();

            let clients: Vec<(String, String, ConnectionState)> =
                alvr_server_core::client_list_snapshot();
            registry.sync_from_client_list(&clients);
        }

        // Placeholder video so connected headsets keep a live stream even with no app attached.
        // Phase 3 replaces this with the idle "no signal" texture and the real encoder.
        let connected = context.connected_clients();
        if !connected.is_empty() {
            frame_index += 1;
            let is_idr = frame_index % 72 == 1;
            let nal = vec![frame_index as u8; if is_idr { 8192 } else { 2048 }];

            for client_id in &connected {
                context.send_video_nal_to_client(
                    client_id,
                    Duration::from_millis(frame_index * 14),
                    [alvr_common::ViewParams::DUMMY; 2],
                    is_idr,
                    nal.clone(),
                );
            }
        }

        thread::sleep(Duration::from_millis(14));
    }

    info!("Stopping");
    registry.broadcast(Event::ServiceStopping);
    drop(context);

    if let Some(handle) = control_handle {
        // The listener blocks in ConnectNamedPipe; it exits with the process.
        drop(handle);
    }

    warn!("ALVR service stopped");

    ExitCode::SUCCESS
}
