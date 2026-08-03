//! A headless ALVR server for multi-device testing.
//!
//! Drives the real `ServerCoreContext` directly, so the connection handshake, per-client streaming
//! state and event routing are all the production code paths. What it does *not* do is talk to
//! SteamVR: instead of an encoder it feeds synthetic video NALs, so the whole thing runs without a
//! GPU, without a driver registration and without touching the machine's real ALVR config.
//!
//! Together with `alvr_mock_device` this gives an end-to-end multi-headset test on one PC:
//! ```text
//! alvr_mock_server --protocol tcp --auto-trust
//! alvr_mock_device --hostname mock-a --control-port 9943
//! alvr_mock_device --hostname mock-b --control-port 9953
//! ```
//! Expect both devices to reach `ClientConnected`, and disconnecting one to leave the other
//! streaming, which is the regression this whole change is about.

use alvr_common::ViewParams;
use alvr_filesystem as afs;
use alvr_server_core::{ServerCoreConfig, ServerCoreContext, ServerCoreEvent};
use alvr_session::{CodecType, SessionConfig, SocketProtocol};
use serde_json::json;
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

struct Args {
    protocol: SocketProtocol,
    auto_trust: bool,
    config_dir: PathBuf,
    run_for: Option<Duration>,
    report_interval: Duration,
    /// Disconnect this client partway through, to prove other clients keep streaming.
    drop_client_after: Option<(String, Duration)>,
}

fn usage() -> String {
    "\
Usage: alvr_mock_server [options]

A headless ALVR server that runs the real connection and streaming code without SteamVR,
feeding synthetic video instead of using an encoder. For multi-device testing.

Options:
  --protocol <tcp|udp>       Stream protocol (default tcp; udp is single-client on Windows)
  --auto-trust               Trust discovered clients automatically (default on)
  --no-auto-trust            Require clients to be trusted manually
  --config-dir <PATH>        Isolated config dir (default: a temp dir, never the real one)
  --run-for <SECONDS>        Exit after this long (default: run until Ctrl-C)
  --report-interval <SEC>    Connected-client reporting interval (default 3)
  --drop-client <NAME>:<SEC> Force-disconnect a client after N seconds, to verify that
                             other clients keep streaming
  -h, --help                 Show this help"
        .to_owned()
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut protocol = SocketProtocol::Tcp;
        let mut auto_trust = true;
        let mut config_dir = env::temp_dir().join("alvr_mock_server");
        let mut run_for = None;
        let mut report_interval = Duration::from_secs(3);
        let mut drop_client_after = None;

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
                        "tcp" => SocketProtocol::Tcp,
                        "udp" => SocketProtocol::Udp,
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
                "--report-interval" => {
                    let secs: u64 = next("--report-interval")?
                        .parse()
                        .map_err(|e| format!("{e}"))?;
                    report_interval = Duration::from_secs(secs.max(1));
                }
                "--drop-client" => {
                    let value = next("--drop-client")?;
                    let (name, secs) = value
                        .rsplit_once(':')
                        .ok_or_else(|| "--drop-client expects <NAME>:<SECONDS>".to_owned())?;
                    let secs: u64 = secs.parse().map_err(|e| format!("{e}"))?;
                    drop_client_after = Some((name.to_owned(), Duration::from_secs(secs)));
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
            report_interval,
            drop_client_after,
        })
    }
}

/// Writes a session.json configured for this test, so the run never depends on (or disturbs) the
/// machine's real ALVR configuration.
///
/// The `SessionSettings` types are macro-generated, so this edits the serialized JSON rather than
/// naming generated fields in Rust.
fn write_session(args: &Args) -> Result<(), String> {
    fs::create_dir_all(&args.config_dir).map_err(|e| format!("{e}"))?;

    let session = SessionConfig::default();
    let mut json = serde_json::to_value(&session).map_err(|e| format!("{e}"))?;

    let settings = json
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

    // Keep the harness cheap and side-effect free.
    settings["audio"]["game_audio"]["enabled"] = json!(false);
    settings["audio"]["microphone"]["enabled"] = json!(false);

    let json = serde_json::to_string_pretty(&json).map_err(|e| format!("{e}"))?;
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

struct ClientState {
    connected_at: Instant,
    frames_sent: u64,
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

    let protocol_name = match args.protocol {
        SocketProtocol::Tcp => "tcp",
        SocketProtocol::Udp => "udp",
    };
    println!(
        "[server] starting: protocol={protocol_name} auto_trust={} config_dir={}",
        args.auto_trust,
        args.config_dir.display()
    );

    alvr_server_core::initialize_environment(layout(&args.config_dir));
    alvr_server_core::init_logging(None, None);

    // No SteamVR here, so never restart the driver on a settings change: this backend reconfigures
    // per client, and a restart would tear down every other connected client.
    let (context, events_receiver) = ServerCoreContext::with_config(ServerCoreConfig {
        restart_on_settings_change: false,
    });
    context.start_connection();

    // Synthetic video: a fake IDR-then-delta stream, so the server exercises its real send path.
    let mut clients: HashMap<String, ClientState> = HashMap::new();
    let started = Instant::now();
    let mut next_report = started + args.report_interval;
    let mut frame_index: u64 = 0;
    let mut dropped_client = false;

    loop {
        if let Some(run_for) = args.run_for
            && started.elapsed() >= run_for
        {
            println!("[server] run duration reached");
            break;
        }

        while let Ok(event) = events_receiver.try_recv() {
            match event {
                ServerCoreEvent::ClientConnected { client_id, config } => {
                    println!(
                        "[server] CLIENT CONNECTED: {client_id} ({}x{} @ {}Hz, codec {:?})",
                        config.transcoding_view_resolution.x,
                        config.transcoding_view_resolution.y,
                        config.refresh_rate,
                        config.codec,
                    );
                    // A real backend would send this client's encoder config here. Per-client, so
                    // each headset gets its own decoder config rather than sharing one.
                    context.set_video_config_nals_for_client(
                        &client_id,
                        vec![0, 0, 0, 1, 0x67],
                        CodecType::H264,
                    );

                    clients.insert(
                        client_id,
                        ClientState {
                            connected_at: Instant::now(),
                            frames_sent: 0,
                        },
                    );
                }
                ServerCoreEvent::ClientDisconnected { client_id } => {
                    println!("[server] CLIENT DISCONNECTED: {client_id}");
                    clients.remove(&client_id);
                }
                ServerCoreEvent::Battery { client_id, info } => {
                    println!(
                        "[server] battery {client_id}: {:.0}%",
                        info.gauge_value * 100.0
                    );
                }
                ServerCoreEvent::PlayspaceSync { client_id, area } => {
                    println!("[server] playspace {client_id}: {}x{}", area.x, area.y);
                }
                ServerCoreEvent::RequestIDR => {
                    // Next frame is already flagged as IDR periodically below.
                }
                ServerCoreEvent::ShutdownPending | ServerCoreEvent::RestartPending => {
                    println!("[server] shutdown requested");
                    return ExitCode::SUCCESS;
                }
                // Tracking / buttons / view params arrive constantly; not interesting here.
                _ => (),
            }
        }

        // Force a disconnect to prove the other clients are unaffected. This is the regression that
        // motivated the per-client refactor: previously any disconnect nulled every client's sinks.
        if let Some((name, after)) = &args.drop_client_after
            && !dropped_client
            && started.elapsed() >= *after
            && clients.contains_key(name)
        {
            println!("[server] forcing disconnect of {name}");
            context.disconnect_client(name);
            dropped_client = true;
        }

        // Feed synthetic video to every connected client independently. This is the multi-headset
        // path: each client gets its own frames, rather than one "active" client getting them all.
        if !clients.is_empty() {
            frame_index += 1;
            let is_idr = frame_index % 72 == 1;
            // Small but non-trivial payload, so throughput numbers are meaningful.
            let nal = vec![frame_index as u8; if is_idr { 8192 } else { 2048 }];

            for (client_id, state) in clients.iter_mut() {
                context.send_video_nal_to_client(
                    client_id,
                    Duration::from_millis(frame_index * 14),
                    [ViewParams::DUMMY; 2],
                    is_idr,
                    nal.clone(),
                );
                state.frames_sent += 1;
            }
        }

        if Instant::now() >= next_report {
            next_report += args.report_interval;

            if clients.is_empty() {
                println!("[server] no clients connected");
            } else {
                let mut names = clients.keys().cloned().collect::<Vec<_>>();
                names.sort();
                println!("[server] {} client(s): {}", clients.len(), names.join(", "));

                for name in names {
                    let state = &clients[&name];
                    println!(
                        "[server]   {name}: connected {:.0}s",
                        state.connected_at.elapsed().as_secs_f32()
                    );
                }
            }
        }

        // ~72 Hz frame cadence.
        thread::sleep(Duration::from_millis(14));
    }

    println!("[server] stopping");
    drop(context);

    ExitCode::SUCCESS
}
