//! A headless mock XR device: a real ALVR client driven by synthetic tracking instead of a
//! headset. Unlike `client_mock` (which is a GUI app), this is a console app designed to be
//! launched several times at once so that multi-device support can be tested end to end without
//! any hardware.
//!
//! Each instance needs its own identity, because the server keys clients by hostname. That is done
//! with the `ALVR_CLIENT_HOSTNAME` / `ALVR_CLIENT_CONFIG_DIR` overrides in `alvr_client_core`,
//! which this binary sets from `--hostname` before the client core reads its config.
//!
//! Example — two devices streaming at once:
//! ```text
//! alvr_mock_device --hostname mock-a
//! alvr_mock_device --hostname mock-b
//! ```
//! Both must be trusted by the server (or `auto_trust_clients` enabled) to connect.

use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::{
    DeviceMotion, HEAD_ID, Pose, RelaxedAtomic, ViewParams,
    glam::{Quat, UVec2, Vec3},
};
use alvr_packets::{FaceData, TrackingData};
use std::{
    env,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Args {
    hostname: String,
    /// Control port this instance listens on. Must be unique per instance on one machine, since
    /// the well-known CONTROL_PORT can only be bound once.
    control_port: u16,
    view_resolution: UVec2,
    refresh_rate: f32,
    /// Exit automatically after this many seconds. Useful for scripted tests.
    run_for: Option<Duration>,
    /// Report statistics on this interval.
    report_interval: Duration,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut hostname = None;
        let mut control_port = alvr_sockets::CONTROL_PORT;
        let mut width = 1920;
        let mut height = 1832;
        let mut refresh_rate = 72.0;
        let mut run_for = None;
        let mut report_interval = Duration::from_secs(5);

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
                "--hostname" => hostname = Some(next("--hostname")?),
                "--control-port" => {
                    control_port = next("--control-port")?.parse().map_err(|e| format!("{e}"))?
                }
                "--width" => width = next("--width")?.parse().map_err(|e| format!("{e}"))?,
                "--height" => height = next("--height")?.parse().map_err(|e| format!("{e}"))?,
                "--refresh-rate" => {
                    refresh_rate = next("--refresh-rate")?.parse().map_err(|e| format!("{e}"))?
                }
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
                "--help" | "-h" => {
                    println!("{}", usage());
                    std::process::exit(0);
                }
                other => return Err(format!("Unknown argument: {other}\n\n{}", usage())),
            }

            idx += 1;
        }

        // mdns-sd rejects hostnames that do not end in ".local.", and registration failing means
        // the device never announces itself at all. Real clients default to "NNNN.client.local.",
        // so append the same suffix rather than making the caller remember it.
        let hostname = hostname
            .ok_or_else(|| format!("--hostname is required\n\n{}", usage()))?
            .trim_end_matches('.')
            .to_owned();
        let hostname = if hostname.ends_with(".local") {
            format!("{hostname}.")
        } else if hostname.ends_with(".client") {
            format!("{hostname}.local.")
        } else {
            format!("{hostname}.client.local.")
        };

        Ok(Self {
            hostname,
            control_port,
            view_resolution: UVec2::new(width, height),
            refresh_rate,
            run_for,
            report_interval,
        })
    }
}

fn usage() -> String {
    "\
Usage: alvr_mock_device --hostname <NAME> [options]

A headless mock XR device that connects to a local ALVR server as a real client.
Run several instances with different hostnames to test multi-device support.

Options:
  --hostname <NAME>        Identity reported to the server (required, must be unique)
  --control-port <PORT>    Control port to listen on (default 9943). Must be unique per
                           instance, since only one process can bind a given port.
  --width <PX>             Per-eye width (default 1920)
  --height <PX>            Per-eye height (default 1832)
  --refresh-rate <HZ>      Preferred refresh rate (default 72)
  --run-for <SECONDS>      Exit after this long (default: run until Ctrl-C)
  --report-interval <SEC>  Statistics logging interval (default 5)
  -h, --help               Show this help"
        .to_owned()
}

/// Counts frames received, so the log can show that this device is really receiving its own stream.
struct Counters {
    frames: AtomicU64,
    bytes: AtomicU64,
    /// Timestamp of the most recent frame, in nanoseconds, for the compositor/submit reports.
    last_frame_ns: AtomicU64,
}

fn tracking_thread(
    context: Arc<ClientCoreContext>,
    streaming: Arc<RelaxedAtomic>,
    fps: f32,
    hostname: String,
) {
    let timestamp_origin = Instant::now();
    context.send_view_params([ViewParams::DUMMY; 2]);

    // Sweep the head slowly so the server sees plausibly changing poses rather than a static one.
    let mut loop_deadline = Instant::now();
    while streaming.value() {
        let elapsed = timestamp_origin.elapsed().as_secs_f32();
        let yaw = (elapsed * 0.3).sin() * 0.5;
        let pitch = (elapsed * 0.2).cos() * 0.2;

        context.send_tracking(TrackingData {
            poll_timestamp: timestamp_origin.elapsed(),
            device_motions: vec![(
                *HEAD_ID,
                DeviceMotion {
                    pose: Pose {
                        orientation: Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch),
                        position: Vec3::new(0.0, 1.6, 0.0),
                    },
                    linear_velocity: Vec3::ZERO,
                    angular_velocity: Vec3::ZERO,
                },
            )],
            hand_skeletons: [None, None],
            face: FaceData::default(),
            body: None,
        });

        // Match client_mock: submit tracking faster than the frame rate.
        loop_deadline += Duration::from_secs_f32(1.0 / fps / 3.0);
        thread::sleep(loop_deadline.saturating_duration_since(Instant::now()));
    }

    println!("[{hostname}] tracking thread stopped");
}

fn main() -> ExitCode {
    env_logger::init();

    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    // Must be set before ClientCoreContext::new reads the config, so this instance gets its own
    // identity and does not share the single default client config file with other instances.
    unsafe {
        env::set_var("ALVR_CLIENT_HOSTNAME", &args.hostname);
        env::set_var("ALVR_CLIENT_CONTROL_PORT", args.control_port.to_string());
        env::set_var(
            "ALVR_CLIENT_CONFIG_DIR",
            env::temp_dir()
                .join("alvr_mock_device")
                .join(&args.hostname),
        );
    }

    let hostname = args.hostname.clone();
    println!(
        "[{hostname}] starting mock device: {}x{} @ {}Hz, control port {}",
        args.view_resolution.x, args.view_resolution.y, args.refresh_rate, args.control_port
    );

    let capabilities = ClientCapabilities {
        platform: alvr_system_info::platform(None, None),
        default_view_resolution: args.view_resolution,
        max_view_resolution: args.view_resolution,
        refresh_rates: vec![60.0, 72.0, 80.0, 90.0, 120.0],
        foveated_encoding: false,
        encoder_high_profile: false,
        encoder_10_bits: false,
        encoder_av1: false,
        prefer_10bit: false,
        preferred_encoding_gamma: 1.0,
        prefer_hdr: false,
    };

    let context = Arc::new(ClientCoreContext::new(capabilities));

    let counters = Arc::new(Counters {
        frames: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        last_frame_ns: AtomicU64::new(0),
    });

    // Video arrives through this callback rather than being pulled. Counting here proves this
    // device is receiving its own stream, which is the point of the multi-device test.
    context.set_decoder_input_callback(Box::new({
        let context = Arc::clone(&context);
        let counters = Arc::clone(&counters);
        move |timestamp, nal| {
            counters.frames.fetch_add(1, Ordering::Relaxed);
            counters.bytes.fetch_add(nal.len() as u64, Ordering::Relaxed);
            counters
                .last_frame_ns
                .store(timestamp.as_nanos() as u64, Ordering::Relaxed);

            // Pretend the frame decoded instantly.
            context.report_frame_decoded(timestamp);

            true
        }
    }));

    context.resume();

    let streaming = Arc::new(RelaxedAtomic::new(false));
    let mut maybe_tracking_thread = None;

    let started = Instant::now();
    let mut next_report = started + args.report_interval;
    let mut connected = false;
    // Paces the frame loop; replaced with the negotiated rate once streaming starts.
    let mut frame_rate = args.refresh_rate;
    let mut frame_deadline = Instant::now();

    loop {
        if let Some(run_for) = args.run_for
            && started.elapsed() >= run_for
        {
            println!("[{hostname}] run duration reached, shutting down");
            break;
        }

        while let Some(event) = context.poll_event() {
            match event {
                ClientCoreEvent::UpdateHudMessage(message) => {
                    let message = message.replace('\n', " | ");
                    if !message.trim().is_empty() {
                        println!("[{hostname}] hud: {message}");
                    }
                }
                ClientCoreEvent::StreamingStarted(config) => {
                    let negotiated = &config.negotiated_config;
                    println!(
                        "[{hostname}] STREAMING STARTED: {}x{} @ {}Hz",
                        negotiated.view_resolution.x,
                        negotiated.view_resolution.y,
                        negotiated.refresh_rate_hint,
                    );
                    connected = true;
                    streaming.set(true);
                    frame_rate = negotiated.refresh_rate_hint.max(1.0);

                    maybe_tracking_thread = Some(thread::spawn({
                        let context = Arc::clone(&context);
                        let streaming = Arc::clone(&streaming);
                        let fps = negotiated.refresh_rate_hint;
                        let hostname = hostname.clone();
                        move || tracking_thread(context, streaming, fps, hostname)
                    }));
                }
                ClientCoreEvent::StreamingStopped => {
                    println!("[{hostname}] STREAMING STOPPED");
                    connected = false;
                    streaming.set(false);

                    if let Some(handle) = maybe_tracking_thread.take() {
                        handle.join().ok();
                    }
                }
                ClientCoreEvent::DecoderConfig { codec, .. } => {
                    println!("[{hostname}] decoder config: {codec:?}");
                }
                ClientCoreEvent::Haptics { .. } | ClientCoreEvent::RealTimeConfig(_) => (),
            }
        }

        // Emulate the compositor presenting the latest frame, so the server's pacing and latency
        // statistics see a client that is actually consuming the stream.
        if streaming.value() {
            let timestamp =
                Duration::from_nanos(counters.last_frame_ns.load(Ordering::Relaxed));

            context.report_compositor_start(timestamp);
            context.report_submit(timestamp, Duration::ZERO);
        }

        if Instant::now() >= next_report {
            next_report += args.report_interval;

            let frames = counters.frames.swap(0, Ordering::Relaxed);
            let bytes = counters.bytes.swap(0, Ordering::Relaxed);
            let secs = args.report_interval.as_secs_f64();

            println!(
                "[{hostname}] {} | {:.1} fps | {:.2} Mbit/s",
                if connected {
                    "streaming"
                } else {
                    "waiting for server"
                },
                frames as f64 / secs,
                (bytes as f64 * 8.0) / secs / 1_000_000.0,
            );
        }

        // Pace at the negotiated frame rate while streaming; poll lazily while idle.
        if streaming.value() {
            frame_deadline += Duration::from_secs_f32(1.0 / frame_rate);
            let now = Instant::now();
            if frame_deadline < now {
                frame_deadline = now;
            }
            thread::sleep(frame_deadline.saturating_duration_since(now));
        } else {
            frame_deadline = Instant::now();
            thread::sleep(Duration::from_millis(10));
        }
    }

    streaming.set(false);
    if let Some(handle) = maybe_tracking_thread.take() {
        handle.join().ok();
    }

    context.pause();
    println!("[{hostname}] stopped");

    ExitCode::SUCCESS
}
