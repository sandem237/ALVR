//! Control protocol between `alvr_service` and its clients (the ALVR OpenXR runtime, and any
//! orchestrator).
//!
//! Transport is a named pipe in **message mode**, so each read returns exactly one message and no
//! length framing is needed. Messages are newline-free JSON, which keeps the wire format debuggable
//! and lets either side evolve independently: unknown fields are ignored and absent optional fields
//! fall back to defaults, so a version mismatch degrades rather than failing the handshake.
//!
//! The app side of the pipe must open it with `CreateFile2`, not `CreateFileW` — the latter is not in
//! the UWP API surface and will not compile in an AppContainer project.

use serde::{Deserialize, Serialize};

/// The well-known pipe the service listens on.
///
/// `Global\` here is part of the name inside the single machine-wide pipe namespace; unlike events or
/// shared memory it is *not* a session-namespace prefix, so creating it needs no
/// `SeCreateGlobalPrivilege` and the service can run unelevated and unpackaged.
pub const PIPE_NAME: &str = r"\\.\pipe\Global\alvr_service";

/// Security descriptor for the pipe and for service-owned shared textures.
///
/// The `S-1-15-2-1` (ALL_APPLICATION_PACKAGES) grant is what lets a UWP AppContainer app open these
/// objects; without it the open fails with `ACCESS_DENIED`. Verified with a negative control.
pub const OBJECT_SDDL: &str = "D:(A;;GA;;;S-1-15-2-1)(A;;GA;;;AU)(A;;GA;;;SY)";

/// Bumped on incompatible protocol changes. The service reports its version in
/// [`Response::Hello`] so a client can refuse to continue rather than misbehave.
pub const PROTOCOL_VERSION: u32 = 1;

/// Name of the service-owned shared D3D11 texture for a device.
///
/// `Global\` so an AppContainer app can reach it, and per-device so several headsets each get their
/// own surface. Open it with `OpenSharedResourceByName`; the service creates it with
/// `SHARED_NTHANDLE | SHARED_KEYEDMUTEX` and [`OBJECT_SDDL`].
pub fn shared_texture_name(device_id: &str) -> String {
    // Device ids are mDNS hostnames and may contain dots, which are legal in object names but noisy;
    // keep them as-is so the mapping stays trivially reversible for debugging.
    format!(r"Global\alvr_device_{device_id}")
}

/// A device's stable identity: the client hostname, which comes from the mDNS `device_id` property.
pub type DeviceId = String;

/// Whether a device can accept a new session.
///
/// This is deliberately separate from [`DeviceState`]: a device driven by an OpenXR app is *also*
/// streaming, so connection state alone cannot express availability.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// Connected and not claimed: an app may start a session on it.
    Free,
    /// Claimed by an OpenXR app. `claimant` is a free-form label for diagnostics.
    ClaimedBy { process_id: u32, claimant: String },
}

impl Availability {
    pub fn is_free(&self) -> bool {
        matches!(self, Availability::Free)
    }
}

/// Connection state of a headset, mirroring `alvr_common::ConnectionState` but kept separate so the
/// wire protocol does not depend on ALVR internals.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceState {
    Disconnected,
    Connecting,
    Connected,
    Streaming,
    Disconnecting,
}

impl DeviceState {
    /// Whether the headset is connected enough to render to.
    pub fn is_live(&self) -> bool {
        matches!(self, DeviceState::Connected | DeviceState::Streaming)
    }
}

/// A headset as seen by the service.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Device {
    pub id: DeviceId,
    pub display_name: String,
    pub state: DeviceState,
    pub availability: Availability,
    /// Per-eye resolution negotiated with this headset, once streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_resolution: Option<[u32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_rate: Option<f32>,
}

impl Device {
    /// A device an app can start a session on right now.
    pub fn is_usable(&self) -> bool {
        self.state.is_live() && self.availability.is_free()
    }
}

/// Client -> service.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Request {
    /// First message on a connection. Identifies the peer and returns the current device list, so a
    /// client can decide immediately whether to wait or start.
    Hello {
        protocol_version: u32,
        /// Free-form, for diagnostics (e.g. the app's executable name).
        claimant: String,
        process_id: u32,
    },
    /// Snapshot of all known devices. Also delivered by `Hello`, so the usual flow needs no separate
    /// round trip and cannot race the event subscription.
    ListDevices,
    /// Claim a device for this connection. Fails if it is already claimed or not connected.
    ClaimDevice { id: DeviceId },
    /// Release a device claimed by this connection. Claims are also released automatically when the
    /// connection drops, so this is an optimisation rather than a requirement.
    ReleaseDevice { id: DeviceId },
    /// Keeps the connection observably alive.
    Ping,
}

/// Service -> client. Responses and events share one stream; `Event` arrives unsolicited.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Response {
    Hello {
        protocol_version: u32,
        service_version: String,
        /// Snapshot at connection time.
        devices: Vec<Device>,
    },
    Devices(Vec<Device>),
    /// A claim succeeded. `shared_texture_name` is the service-owned named shared D3D11 texture to
    /// render into; open it with `OpenSharedResourceByName`.
    DeviceClaimed {
        id: DeviceId,
        shared_texture_name: String,
        view_resolution: [u32; 2],
        refresh_rate: f32,
    },
    DeviceReleased {
        id: DeviceId,
    },
    Pong,
    Error {
        request: String,
        message: String,
    },
    /// Unsolicited notification.
    Event(Event),
}

/// Unsolicited service -> client notifications.
///
/// A client that wants to wait for a free headset subscribes implicitly by connecting: every change
/// is pushed, so there is no polling and no window in which an event can be missed after the
/// `Hello` snapshot.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Event {
    /// A device appeared, disappeared, or changed state or availability. Carries the full list so a
    /// client never has to reconcile deltas.
    DevicesChanged(Vec<Device>),
    /// A claim held by this connection was revoked, e.g. the headset disconnected.
    ClaimRevoked { id: DeviceId, reason: String },
    /// The service is shutting down.
    ServiceStopping,
}

/// Encodes one message for message-mode pipe transport.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(value)
}

/// Decodes one message received from a message-mode pipe.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_requests() {
        let request = Request::ClaimDevice {
            id: "quest3.client.local.".into(),
        };
        let bytes = encode(&request).unwrap();
        assert!(matches!(
            decode::<Request>(&bytes).unwrap(),
            Request::ClaimDevice { .. }
        ));
    }

    #[test]
    fn availability_distinguishes_claimed_from_streaming() {
        // The whole point of a separate availability field: a streaming device may still be claimed.
        let device = Device {
            id: "a".into(),
            display_name: "A".into(),
            state: DeviceState::Streaming,
            availability: Availability::ClaimedBy {
                process_id: 1,
                claimant: "app".into(),
            },
            view_resolution: None,
            refresh_rate: None,
        };
        assert!(device.state.is_live());
        assert!(!device.is_usable());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Forward compatibility: a newer service may add fields a older client does not know.
        let json = r#"{"id":"a","display_name":"A","state":"Connected",
                       "availability":"Free","future_field":123}"#;
        let device: Device = serde_json::from_str(json).unwrap();
        assert!(device.is_usable());
    }
}
