mod bitrate;
mod c_api;
mod connection;
mod hand_gestures;
mod haptics;
mod input_mapping;
mod logging_backend;
mod sockets;
mod statistics;
mod tracking;
mod web_server;

pub use c_api::*;
pub use logging_backend::init_logging;
pub use tracking::HandType;

use crate::connection::VideoPacket;
use alvr_common::{
    ConnectionState, DEVICE_ID_TO_PATH, DeviceMotion, LifecycleState, Pose, ViewParams,
    dbg_server_core, error,
    glam::{UVec2, Vec2},
    parking_lot::{Mutex, RwLock},
    settings_schema::Switch,
    warn,
};
use alvr_events::{EventType, HapticsEvent};
use alvr_filesystem as afs;
use alvr_packets::{
    BatteryInfo, ButtonEntry, ClientConnectionsAction, DecoderInitializationConfig, Haptics,
    VideoPacketHeader,
};
use alvr_server_io::ServerSessionManager;
use alvr_session::{CodecType, H264Profile, OpenvrProperty, Settings, SteamvrHmdInitConfig};
use alvr_sockets::StreamSender;
use bitrate::{BitrateManager, DynamicEncoderParams};
use statistics::StatisticsManager;
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsStr,
    fs::File,
    io::Write,
    sync::{
        Arc, LazyLock, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::broadcast};
use tracking::TrackingManager;

static FILESYSTEM_LAYOUT: OnceLock<afs::Layout> = OnceLock::new();

// This is lazily initialized when initializing logging or ServerCoreContext. So FILESYSTEM_LAYOUT
// needs to be initialized first using initialize_environment().
// NB: this must remain a global because only one instance should exist for the whole application
// execution time.
static SESSION_MANAGER: LazyLock<RwLock<ServerSessionManager>> = LazyLock::new(|| {
    RwLock::new(ServerSessionManager::new(
        FILESYSTEM_LAYOUT.get().map(|l| l.session()),
    ))
});

pub fn initialize_environment(layout: afs::Layout) {
    FILESYSTEM_LAYOUT.set(layout).unwrap();

    // This ensures that the session is written to disk
    SESSION_MANAGER.write().session_mut();
}

pub struct ServerNegotiatedStreamingConfig {
    pub transcoding_view_resolution: UVec2,
    pub emulated_headset_view_resolution: UVec2,
    pub refresh_rate: f32,
    pub enable_foveated_encoding: bool,
    pub codec: CodecType,
    pub h264_profile: H264Profile,
    pub use_10bit_encoder: bool,
    pub encoding_gamma: f32,
    pub enable_hdr: bool,
}

/// Events emitted by the server core. Variants that originate from a specific headset carry the
/// `client_id` (the client hostname) so that a backend serving several headsets at once can route
/// them, instead of assuming there is only ever one connected client.
pub enum ServerCoreEvent {
    SetOpenvrProperty {
        device_id: u64,
        prop: OpenvrProperty,
    },
    ClientConnected {
        client_id: String,
        config: ServerNegotiatedStreamingConfig,
    },
    ClientDisconnected {
        client_id: String,
    },
    Battery {
        client_id: String,
        info: BatteryInfo,
    },
    PlayspaceSync {
        client_id: String,
        area: Vec2,
    },
    /// In relation to head
    LocalViewParams {
        client_id: String,
        params: [ViewParams; 2],
    },
    Tracking {
        client_id: String,
        poll_timestamp: Duration,
    },
    /// Note: this is after mapping
    Buttons {
        client_id: String,
        entries: Vec<ButtonEntry>,
    },
    RequestIDR,
    CaptureFrame,
    GameRenderLatencyFeedback(Duration), // only used for SteamVR
    ShutdownPending,
    RestartPending,
    ProximityState {
        client_id: String,
        is_worn: bool,
    },
}

/// Per-client streaming state. One of these exists for each client that is connected or streaming,
/// so that multiple headsets can be served concurrently without stealing each other's streams.
///
/// NB: everything that is scoped to a single streaming session belongs here rather than in
/// `ConnectionContext`, otherwise a second client would overwrite the first client's sinks and a
/// single disconnect would tear down every client's streams.
pub struct ClientSession {
    pub hostname: String,
    statistics_manager: RwLock<Option<StatisticsManager>>,
    bitrate_manager: Mutex<BitrateManager>,
    tracking_manager: RwLock<TrackingManager>,
    decoder_config: Mutex<Option<DecoderInitializationConfig>>,
    video_recording_file: Mutex<Option<File>>,
    video_channel_sender: Mutex<Option<SyncSender<VideoPacket>>>,
    haptics_sender: Mutex<Option<StreamSender<Haptics>>>,
    /// Starts in the corrupted state: the client has not received the initial IDR yet.
    stream_corrupted: AtomicBool,
    last_idr_instant: Mutex<Instant>,
}

impl ClientSession {
    pub fn stop_recording(&self) {
        *self.video_recording_file.lock() = None;
    }

    fn new(hostname: String, settings: &Settings) -> Self {
        Self {
            hostname,
            statistics_manager: RwLock::new(None),
            bitrate_manager: Mutex::new(BitrateManager::new(256, 60.0)),
            tracking_manager: RwLock::new(TrackingManager::new(
                settings.connection.statistics_history_size,
            )),
            decoder_config: Mutex::new(None),
            video_recording_file: Mutex::new(None),
            video_channel_sender: Mutex::new(None),
            haptics_sender: Mutex::new(None),
            stream_corrupted: AtomicBool::new(true),
            last_idr_instant: Mutex::new(Instant::now()),
        }
    }
}

pub struct ConnectionContext {
    events_sender: mpsc::Sender<ServerCoreEvent>,
    /// Per-client streaming state, keyed by client hostname.
    clients: RwLock<HashMap<String, Arc<ClientSession>>>,
    /// The client that the single-HMD (OpenVR) backend is currently bound to. Backends that can
    /// serve several headsets at once address clients by hostname instead and ignore this.
    active_client: RwLock<Option<String>>,
    video_mirror_sender: Mutex<Option<broadcast::Sender<Vec<u8>>>>,
    connection_threads: Mutex<Vec<JoinHandle<()>>>,
    clients_to_be_removed: Mutex<HashSet<String>>,
    /// Whether a client negotiating different resolution/refresh rate should restart the whole
    /// driver. Required for SteamVR, which fixes those at driver startup. Backends that reconfigure
    /// per client must leave this off: a restart would tear down every other client's session.
    restart_on_settings_change: bool,
}

impl ConnectionContext {
    /// Registers a fresh streaming session for the given client, replacing any stale one.
    fn create_client_session(&self, hostname: &str, settings: &Settings) -> Arc<ClientSession> {
        let session = Arc::new(ClientSession::new(hostname.to_owned(), settings));
        self.clients
            .write()
            .insert(hostname.to_owned(), Arc::clone(&session));

        // Bind the single-HMD backend to the first client to arrive.
        let mut active_client = self.active_client.write();
        if active_client.is_none() {
            *active_client = Some(hostname.to_owned());
        }

        session
    }

    /// Tears down only the given client's session, leaving other clients streaming.
    fn remove_client_session(&self, hostname: &str) {
        self.clients.write().remove(hostname);

        let mut active_client = self.active_client.write();
        if active_client.as_deref() == Some(hostname) {
            // Hand the single-HMD backend over to any other client that is still connected.
            *active_client = self.clients.read().keys().next().cloned();
        }
    }

    pub fn client_session(&self, hostname: &str) -> Option<Arc<ClientSession>> {
        self.clients.read().get(hostname).cloned()
    }

    /// Every client that currently has a streaming session.
    pub fn client_sessions(&self) -> Vec<Arc<ClientSession>> {
        self.clients.read().values().cloned().collect()
    }

    /// The client the single-HMD backend is bound to, if any.
    pub fn active_client_id(&self) -> Option<String> {
        self.active_client.read().clone()
    }

    /// The session the single-HMD backend is bound to, if any.
    fn active_session(&self) -> Option<Arc<ClientSession>> {
        let hostname = self.active_client.read().clone()?;
        self.client_session(&hostname)
    }
}

pub fn create_recording_file(
    connection_context: &ConnectionContext,
    session: &ClientSession,
    settings: &Settings,
) {
    let codec = settings.video.preferred_codec;
    let ext = match codec {
        CodecType::H264 => "h264",
        CodecType::Hevc => "h265",
        CodecType::AV1 => "av1",
    };

    // Include the hostname so concurrent clients cannot collide on the same recording file.
    let path = FILESYSTEM_LAYOUT.get().unwrap().log_dir.join(format!(
        "recording.{}.{}.{ext}",
        chrono::Local::now().format("%F.%H-%M-%S"),
        session.hostname,
    ));

    match File::create(path) {
        Ok(mut file) => {
            if let Some(config) = &*session.decoder_config.lock() {
                file.write_all(&config.config_buffer).ok();
            }

            *session.video_recording_file.lock() = Some(file);

            connection_context
                .events_sender
                .send(ServerCoreEvent::RequestIDR)
                .ok();
        }
        Err(e) => {
            error!("Failed to record video on disk: {e}");
        }
    }
}

pub fn notify_restart_driver() {
    if sysinfo::System::new_all()
        .processes_by_name(OsStr::new(&afs::dashboard_fname()))
        .next()
        .is_some()
    {
        alvr_events::send_event(EventType::ServerRequestsSelfRestart);
    } else {
        error!("Cannot restart SteamVR. No dashboard process found on local device.");
    }
}

pub fn settings() -> Settings {
    SESSION_MANAGER.read().settings().clone()
}

/// Every known client as `(hostname, display_name, connection_state)`.
///
/// Lets a backend mirror the session's client list without depending on the session types, which is
/// what the multi-device service builds its device registry from.
pub fn client_list_snapshot() -> Vec<(String, String, ConnectionState)> {
    SESSION_MANAGER
        .read()
        .client_list()
        .iter()
        .map(|(hostname, config)| {
            let display_name = if config.display_name.is_empty() {
                hostname.clone()
            } else {
                config.display_name.clone()
            };

            (
                hostname.clone(),
                display_name,
                config.connection_state.clone(),
            )
        })
        .collect()
}

pub fn steamvr_hmd_init_config() -> SteamvrHmdInitConfig {
    SESSION_MANAGER
        .read()
        .session()
        .steamvr_hmd_init_config
        .clone()
}

pub fn registered_button_set() -> HashSet<u64> {
    let session_manager = SESSION_MANAGER.read();
    if let Switch::Enabled(input_mapping) = &session_manager.settings().headset.controllers {
        input_mapping::registered_button_set(&input_mapping.emulation_mode)
    } else {
        HashSet::new()
    }
}

pub struct ServerCoreContext {
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    connection_context: Arc<ConnectionContext>,
    connection_thread: Arc<RwLock<Option<JoinHandle<()>>>>,
    webserver_runtime: Option<Runtime>,
}

/// How a backend wants the server core to behave.
pub struct ServerCoreConfig {
    /// Restart the driver when a client negotiates a different resolution or refresh rate. SteamVR
    /// needs this because it fixes those at driver startup. Backends that can reconfigure per client
    /// must leave this off, since a restart tears down every other connected client.
    pub restart_on_settings_change: bool,
}

impl Default for ServerCoreConfig {
    fn default() -> Self {
        // Matches the historical SteamVR behaviour.
        Self {
            restart_on_settings_change: true,
        }
    }
}

impl ServerCoreContext {
    pub fn new() -> (Self, mpsc::Receiver<ServerCoreEvent>) {
        Self::with_config(ServerCoreConfig::default())
    }

    pub fn with_config(config: ServerCoreConfig) -> (Self, mpsc::Receiver<ServerCoreEvent>) {
        dbg_server_core!("Creating");

        if SESSION_MANAGER
            .read()
            .settings()
            .extra
            .logging
            .prefer_backtrace
        {
            unsafe { env::set_var("RUST_BACKTRACE", "1") };
        }

        SESSION_MANAGER.write().clean_client_list();

        let (events_sender, events_receiver) = mpsc::channel();

        let connection_context = Arc::new(ConnectionContext {
            events_sender,
            clients: RwLock::new(HashMap::new()),
            active_client: RwLock::new(None),
            video_mirror_sender: Mutex::new(None),
            connection_threads: Mutex::new(Vec::new()),
            clients_to_be_removed: Mutex::new(HashSet::new()),
            restart_on_settings_change: config.restart_on_settings_change,
        });

        let webserver_runtime = Runtime::new().unwrap();
        webserver_runtime.spawn({
            let connection_context = Arc::clone(&connection_context);
            async move { alvr_common::show_err(web_server::web_server(connection_context).await) }
        });

        (
            Self {
                lifecycle_state: Arc::new(RwLock::new(LifecycleState::StartingUp)),
                connection_context,
                connection_thread: Arc::new(RwLock::new(None)),
                webserver_runtime: Some(webserver_runtime),
            },
            events_receiver,
        )
    }

    pub fn start_connection(&self) {
        dbg_server_core!("start_connection");

        // Note: Idle state is not used on the server side
        *self.lifecycle_state.write() = LifecycleState::Resumed;

        let connection_context = Arc::clone(&self.connection_context);
        let lifecycle_state = Arc::clone(&self.lifecycle_state);
        *self.connection_thread.write() = Some(thread::spawn(move || {
            connection::handshake_loop(connection_context, lifecycle_state);
        }));
    }

    /// Hostnames of the clients that currently have a streaming session.
    pub fn connected_clients(&self) -> Vec<String> {
        self.connection_context
            .client_sessions()
            .into_iter()
            .map(|session| session.hostname.clone())
            .collect()
    }

    /// Requests a single client to disconnect, leaving other clients streaming. The streaming
    /// threads observe this state change and shut themselves down.
    pub fn disconnect_client(&self, hostname: &str) {
        SESSION_MANAGER.write().update_client_connections(
            hostname.to_owned(),
            ClientConnectionsAction::SetConnectionState(ConnectionState::Disconnecting),
        );
    }

    pub fn get_device_motion(
        &self,
        device_id: u64,
        sample_timestamp: Duration,
    ) -> Option<DeviceMotion> {
        dbg_server_core!("get_device_motion: dev={device_id} sample_ts={sample_timestamp:?}");

        self.connection_context
            .active_session()?
            .tracking_manager
            .read()
            .get_device_motion(device_id, sample_timestamp)
    }

    pub fn get_hand_skeleton(
        &self,
        hand_type: HandType,
        timestamp: Duration,
    ) -> Option<[Pose; 26]> {
        dbg_server_core!("get_hand_skeleton: hand={hand_type:?} ts={timestamp:?}");

        self.connection_context
            .active_session()?
            .tracking_manager
            .read()
            .get_hand_skeleton(hand_type, timestamp)
            .copied()
    }

    pub fn get_motion_to_photon_latency(&self) -> Duration {
        dbg_server_core!("get_motion_to_photon_latency");

        let latency = self
            .connection_context
            .active_session()
            .and_then(|session| {
                session
                    .statistics_manager
                    .read()
                    .as_ref()
                    .map(|stats| stats.motion_to_photon_latency_average())
            })
            .unwrap_or_default();

        let max_prediction =
            Duration::from_millis(SESSION_MANAGER.read().settings().headset.max_prediction_ms);

        if latency > max_prediction {
            warn!("Latency is too high. Clamping prediction");

            max_prediction
        } else {
            latency
        }
    }

    pub fn get_tracker_pose_time_offset(&self) -> Duration {
        dbg_server_core!("get_tracker_pose_time_offset");

        self.connection_context
            .active_session()
            .and_then(|session| {
                session
                    .statistics_manager
                    .read()
                    .as_ref()
                    .map(|stats| stats.tracker_pose_time_offset())
            })
            .unwrap_or_default()
    }

    pub fn send_haptics(&self, haptics: Haptics) {
        dbg_server_core!("send_haptics");

        let haptics_config = {
            let session_manager_lock = SESSION_MANAGER.read();

            if session_manager_lock.settings().extra.logging.log_haptics {
                alvr_events::send_event(EventType::Haptics(HapticsEvent {
                    path: DEVICE_ID_TO_PATH.get(&haptics.device_id).map_or_else(
                        || format!("Unknown (ID: {:#16x})", haptics.device_id),
                        |p| (*p).to_owned(),
                    ),
                    duration: haptics.duration,
                    frequency: haptics.frequency,
                    amplitude: haptics.amplitude,
                }))
            }

            session_manager_lock
                .settings()
                .headset
                .controllers
                .as_option()
                .and_then(|c| c.haptics.as_option().cloned())
        };

        let Some(session) = self.connection_context.active_session() else {
            return;
        };

        if let (Some(config), Some(sender)) = (haptics_config, &mut *session.haptics_sender.lock()) {
            sender
                .send_header(&haptics::map_haptics(&config, haptics))
                .ok();
        }
    }

    pub fn set_video_config_nals(&self, config_buffer: Vec<u8>, codec: CodecType) {
        dbg_server_core!("set_video_config_nals");

        if let Some(sender) = &*self.connection_context.video_mirror_sender.lock() {
            sender.send(config_buffer.clone()).ok();
        }

        let Some(session) = self.connection_context.active_session() else {
            return;
        };

        Self::set_session_video_config(&session, config_buffer, codec);
    }

    /// Sets the decoder config for one specific client, for backends driving several headsets.
    pub fn set_video_config_nals_for_client(
        &self,
        client_id: &str,
        config_buffer: Vec<u8>,
        codec: CodecType,
    ) {
        if let Some(sender) = &*self.connection_context.video_mirror_sender.lock() {
            sender.send(config_buffer.clone()).ok();
        }

        let Some(session) = self.connection_context.client_session(client_id) else {
            return;
        };

        Self::set_session_video_config(&session, config_buffer, codec);
    }

    fn set_session_video_config(
        session: &ClientSession,
        config_buffer: Vec<u8>,
        codec: CodecType,
    ) {
        if let Some(file) = &mut *session.video_recording_file.lock() {
            file.write_all(&config_buffer).ok();
        }

        *session.decoder_config.lock() = Some(DecoderInitializationConfig {
            codec,
            config_buffer,
            ext_str: String::new(),
        });
    }

    /// Sends video to the client the single-HMD backend is bound to.
    pub fn send_video_nal(
        &self,
        timestamp: Duration,
        global_view_params: [ViewParams; 2],
        is_idr: bool,
        nal_buffer: Vec<u8>,
    ) {
        dbg_server_core!("send_video_nal");

        let Some(session) = self.connection_context.active_session() else {
            return;
        };

        self.send_video_nal_to_session(&session, timestamp, global_view_params, is_idr, nal_buffer);
    }

    /// Sends video to one specific client. Backends that drive several headsets at once encode per
    /// client and use this instead of [`Self::send_video_nal`].
    pub fn send_video_nal_to_client(
        &self,
        client_id: &str,
        timestamp: Duration,
        global_view_params: [ViewParams; 2],
        is_idr: bool,
        nal_buffer: Vec<u8>,
    ) {
        let Some(session) = self.connection_context.client_session(client_id) else {
            return;
        };

        self.send_video_nal_to_session(&session, timestamp, global_view_params, is_idr, nal_buffer);
    }

    fn send_video_nal_to_session(
        &self,
        session: &ClientSession,
        timestamp: Duration,
        global_view_params: [ViewParams; 2],
        is_idr: bool,
        nal_buffer: Vec<u8>,
    ) {
        if let Some(sender) = &*session.video_channel_sender.lock() {
            let buffer_size = nal_buffer.len();

            if is_idr {
                session.stream_corrupted.store(false, Ordering::SeqCst);
            }

            if let Switch::Enabled(config) = &SESSION_MANAGER
                .read()
                .settings()
                .extra
                .capture
                .rolling_video_files
                && Instant::now()
                    > *session.last_idr_instant.lock() + Duration::from_secs(config.duration_s)
            {
                self.connection_context
                    .events_sender
                    .send(ServerCoreEvent::RequestIDR)
                    .ok();

                if is_idr {
                    create_recording_file(
                        &self.connection_context,
                        session,
                        SESSION_MANAGER.read().settings(),
                    );
                    *session.last_idr_instant.lock() = Instant::now();
                }
            }

            if !session.stream_corrupted.load(Ordering::SeqCst)
                || !SESSION_MANAGER
                    .read()
                    .settings()
                    .connection
                    .avoid_video_glitching
            {
                if let Some(sender) = &*self.connection_context.video_mirror_sender.lock() {
                    sender.send(nal_buffer.clone()).ok();
                }

                if let Some(file) = &mut *session.video_recording_file.lock() {
                    file.write_all(&nal_buffer).ok();
                }

                let sender_result = sender.try_send(VideoPacket {
                    header: VideoPacketHeader {
                        timestamp,
                        global_view_params,
                        is_idr,
                    },
                    payload: nal_buffer,
                });
                if matches!(sender_result, Err(TrySendError::Full(_))) {
                    session.stream_corrupted.store(true, Ordering::SeqCst);
                    self.connection_context
                        .events_sender
                        .send(ServerCoreEvent::RequestIDR)
                        .ok();
                    warn!("Dropping video packet. Reason: Can't push to network");
                }
            } else {
                warn!("Dropping video packet. Reason: Waiting for IDR frame");
            }

            if let Some(stats) = &mut *session.statistics_manager.write() {
                let encoder_latency = stats.report_frame_encoded(timestamp, buffer_size);

                session
                    .bitrate_manager
                    .lock()
                    .report_frame_encoded(timestamp, encoder_latency, buffer_size);
            }
        }
    }

    pub fn get_dynamic_encoder_params(&self) -> Option<DynamicEncoderParams> {
        dbg_server_core!("get_dynamic_encoder_params");

        let session = self.connection_context.active_session()?;

        let pair = {
            let session_manager_lock = SESSION_MANAGER.read();
            session
                .bitrate_manager
                .lock()
                .get_encoder_params(&session_manager_lock.settings().video.bitrate)
        };

        pair.map(|(params, stats)| {
            if let Some(stats_manager) = &mut *session.statistics_manager.write() {
                stats_manager.report_throughput_stats(stats);
            }
            params
        })
    }

    pub fn report_composed(&self, target_timestamp: Duration, offset: Duration) {
        dbg_server_core!("report_composed");

        if let Some(session) = self.connection_context.active_session()
            && let Some(stats) = &mut *session.statistics_manager.write()
        {
            stats.report_frame_composed(target_timestamp, offset);
        }
    }

    pub fn report_present(&self, target_timestamp: Duration, offset: Duration) {
        dbg_server_core!("report_present");

        let Some(session) = self.connection_context.active_session() else {
            return;
        };

        if let Some(stats) = &mut *session.statistics_manager.write() {
            stats.report_frame_present(target_timestamp, offset);
        }

        let session_manager_lock = SESSION_MANAGER.read();
        session.bitrate_manager.lock().report_frame_present(
            &session_manager_lock
                .settings()
                .video
                .bitrate
                .adapt_to_framerate,
        );
    }

    pub fn duration_until_next_vsync(&self) -> Option<Duration> {
        dbg_server_core!("duration_until_next_vsync");

        self.connection_context
            .active_session()?
            .statistics_manager
            .write()
            .as_mut()
            .map(|stats| stats.duration_until_next_vsync())
    }
}

impl Drop for ServerCoreContext {
    fn drop(&mut self) {
        dbg_server_core!("Drop");

        // Invoke connection runtimes shutdown
        *self.lifecycle_state.write() = LifecycleState::ShuttingDown;

        dbg_server_core!("Setting clients as Disconnecting");
        {
            let mut session_manager_lock = SESSION_MANAGER.write();

            let hostnames = session_manager_lock
                .client_list()
                .iter()
                .filter(|&(_, info)| {
                    !matches!(
                        info.connection_state,
                        ConnectionState::Disconnected | ConnectionState::Disconnecting
                    )
                })
                .map(|(hostname, _)| hostname.clone())
                .collect::<Vec<_>>();

            for hostname in hostnames {
                session_manager_lock.update_client_connections(
                    hostname,
                    ClientConnectionsAction::SetConnectionState(ConnectionState::Disconnecting),
                );
            }
        }

        dbg_server_core!("Joining connection thread");
        if let Some(thread) = self.connection_thread.write().take() {
            thread.join().ok();
        }

        // apply openvr config for the next launch
        dbg_server_core!("Setting restart settings cache");
        {
            let mut session_manager_lock = SESSION_MANAGER.write();
            let new_steamvr_hmd_init_config = session_manager_lock
                .session()
                .steamvr_hmd_init_config
                .clone();
            let settings = session_manager_lock.session().to_settings();
            let new_hash =
                connection::compute_restart_settings_hash(&new_steamvr_hmd_init_config, &settings);
            let mut session = session_manager_lock.session_mut();
            session.steamvr_hmd_init_config = new_steamvr_hmd_init_config;
            session.restart_settings_hash = new_hash;
        }

        // todo: check if this is still needed
        while SESSION_MANAGER
            .read()
            .client_list()
            .iter()
            .any(|(_, info)| info.connection_state != ConnectionState::Disconnected)
        {
            thread::sleep(Duration::from_millis(100));
        }

        // Dropping the webserver runtime is bugged on linux and will prevent StemVR shutdown
        if !cfg!(target_os = "linux") {
            self.webserver_runtime.take();
        }
    }
}
