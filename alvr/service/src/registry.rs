//! Device registry: the service's view of every headset, plus who is using it.
//!
//! Availability is tracked separately from connection state on purpose. A headset driven by an
//! OpenXR app is *also* `Streaming`, so `ConnectionState` alone cannot answer "may I start a session
//! on this device?" — which is exactly the question the runtime and an orchestrator need answered.
//!
//! Claims are keyed by connection, not by process id, so a claim is released the moment the pipe
//! breaks. That means a hard-crashed app cannot orphan a device.

use alvr_common::{ConnectionState, info, parking_lot::Mutex, warn};
use alvr_service_protocol::{Availability, Device, DeviceId, DeviceState, Event};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// Identifies one control-channel connection. Claims belong to these, not to process ids, so that a
/// dead connection always frees its devices.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConnectionId(u64);

/// Negotiated streaming parameters, known once a headset is streaming.
#[derive(Clone, Copy, Debug)]
pub struct StreamParams {
    pub view_resolution: [u32; 2],
    pub refresh_rate: f32,
}

struct Claim {
    connection: ConnectionId,
    process_id: u32,
    claimant: String,
}

#[derive(Default)]
struct State {
    /// Connection state per device, mirrored from the session's client list.
    states: HashMap<DeviceId, DeviceState>,
    display_names: HashMap<DeviceId, String>,
    stream_params: HashMap<DeviceId, StreamParams>,
    claims: HashMap<DeviceId, Claim>,
}

/// Notification sink. The service pushes these to every connected control client.
pub type Subscriber = Box<dyn Fn(Event) + Send + Sync>;

pub struct DeviceRegistry {
    state: Mutex<State>,
    subscribers: Mutex<Vec<(ConnectionId, Arc<Subscriber>)>>,
    next_connection: AtomicU64,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            subscribers: Mutex::new(Vec::new()),
            next_connection: AtomicU64::new(1),
        }
    }

    pub fn new_connection_id(&self) -> ConnectionId {
        ConnectionId(self.next_connection.fetch_add(1, Ordering::Relaxed))
    }

    /// Registers a notification sink for a connection. Dropped by [`Self::disconnect`].
    pub fn subscribe(&self, connection: ConnectionId, subscriber: Subscriber) {
        self.subscribers
            .lock()
            .push((connection, Arc::new(subscriber)));
    }

    /// Current device list. Ordered by id so output is stable and diffable.
    pub fn devices(&self) -> Vec<Device> {
        let state = self.state.lock();
        Self::snapshot(&state)
    }

    fn snapshot(state: &State) -> Vec<Device> {
        let mut devices: Vec<Device> = state
            .states
            .iter()
            .map(|(id, &device_state)| {
                let availability = match state.claims.get(id) {
                    Some(claim) => Availability::ClaimedBy {
                        process_id: claim.process_id,
                        claimant: claim.claimant.clone(),
                    },
                    None => Availability::Free,
                };

                let params = state.stream_params.get(id);

                Device {
                    id: id.clone(),
                    display_name: state
                        .display_names
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| id.clone()),
                    state: device_state,
                    availability,
                    view_resolution: params.map(|p| p.view_resolution),
                    refresh_rate: params.map(|p| p.refresh_rate),
                }
            })
            .collect();

        devices.sort_by(|a, b| a.id.cmp(&b.id));
        devices
    }

    /// Replaces the mirrored connection states from the session client list. Emits a change event
    /// only when something actually differs, so idle polling does not spam subscribers.
    pub fn sync_from_client_list(&self, clients: &[(DeviceId, String, ConnectionState)]) {
        // All mutation happens under the lock; notifications are sent strictly after releasing it,
        // so a subscriber callback can never re-enter the registry while it is locked.
        let mut changed = false;
        let mut revocations: Vec<(DeviceId, ConnectionId)> = Vec::new();

        {
            let mut state = self.state.lock();

            let seen: Vec<DeviceId> = clients.iter().map(|(id, _, _)| id.clone()).collect();

            for (id, display_name, connection_state) in clients {
                let mapped = map_state(connection_state.clone());
                if state.states.get(id) != Some(&mapped) {
                    state.states.insert(id.clone(), mapped);
                    changed = true;
                }
                if state.display_names.get(id) != Some(display_name) {
                    state.display_names.insert(id.clone(), display_name.clone());
                    changed = true;
                }
            }

            // Devices no longer in the client list have gone away entirely.
            let stale: Vec<DeviceId> = state
                .states
                .keys()
                .filter(|id| !seen.contains(id))
                .cloned()
                .collect();
            for id in stale {
                state.states.remove(&id);
                state.display_names.remove(&id);
                state.stream_params.remove(&id);
                changed = true;
            }

            // A device that is no longer live cannot stay claimed.
            revocations = state
                .claims
                .iter()
                .filter(|(id, _)| !state.states.get(*id).is_some_and(|s| s.is_live()))
                .map(|(id, claim)| (id.clone(), claim.connection))
                .collect();

            for (id, _) in &revocations {
                state.claims.remove(id);
                state.stream_params.remove(id);
                changed = true;
            }
        }

        for (id, connection) in revocations {
            warn!("Revoking claim on {id}: device is no longer connected");
            self.notify_one(
                connection,
                Event::ClaimRevoked {
                    id,
                    reason: "device disconnected".into(),
                },
            );
        }

        if changed {
            self.notify_devices_changed();
        }
    }

    /// Records the negotiated parameters for a streaming device.
    pub fn set_stream_params(&self, id: &DeviceId, params: StreamParams) {
        {
            let mut state = self.state.lock();
            state.stream_params.insert(id.clone(), params);
        }
        self.notify_devices_changed();
    }

    /// Claims a device for a connection.
    ///
    /// Returns the negotiated parameters on success. Fails if the device is unknown, not connected,
    /// or already claimed — including when claimed by the *same* connection, since a double claim
    /// signals a client bug rather than something to paper over.
    pub fn claim(
        &self,
        id: &DeviceId,
        connection: ConnectionId,
        process_id: u32,
        claimant: &str,
    ) -> Result<StreamParams, String> {
        let params = {
            let mut state = self.state.lock();

            let Some(device_state) = state.states.get(id).copied() else {
                return Err(format!("Unknown device: {id}"));
            };
            if !device_state.is_live() {
                return Err(format!("Device {id} is not connected ({device_state:?})"));
            }
            if let Some(claim) = state.claims.get(id) {
                return Err(format!(
                    "Device {id} is already claimed by {} (pid {})",
                    claim.claimant, claim.process_id
                ));
            }

            let Some(params) = state.stream_params.get(id).copied() else {
                return Err(format!("Device {id} has no negotiated stream parameters yet"));
            };

            state.claims.insert(
                id.clone(),
                Claim {
                    connection,
                    process_id,
                    claimant: claimant.to_owned(),
                },
            );

            params
        };

        info!("Device {id} claimed by {claimant} (pid {process_id})");
        self.notify_devices_changed();

        Ok(params)
    }

    /// Releases a claim held by this connection. Releasing something you do not hold is an error, so
    /// a confused client is told rather than silently succeeding.
    pub fn release(&self, id: &DeviceId, connection: ConnectionId) -> Result<(), String> {
        {
            let mut state = self.state.lock();
            match state.claims.get(id) {
                Some(claim) if claim.connection == connection => {
                    state.claims.remove(id);
                }
                Some(_) => return Err(format!("Device {id} is claimed by another connection")),
                None => return Err(format!("Device {id} is not claimed")),
            }
        }

        info!("Device {id} released");
        self.notify_devices_changed();

        Ok(())
    }

    /// Drops a connection: releases every claim it held and removes its subscription. This is the
    /// path that makes a crashed app harmless.
    pub fn disconnect(&self, connection: ConnectionId) {
        let released: Vec<DeviceId> = {
            let mut state = self.state.lock();
            let released: Vec<DeviceId> = state
                .claims
                .iter()
                .filter(|(_, claim)| claim.connection == connection)
                .map(|(id, _)| id.clone())
                .collect();

            for id in &released {
                state.claims.remove(id);
            }
            released
        };

        self.subscribers
            .lock()
            .retain(|(id, _)| *id != connection);

        if !released.is_empty() {
            info!(
                "Connection dropped, released {} device(s): {}",
                released.len(),
                released.join(", ")
            );
            self.notify_devices_changed();
        }
    }

    fn notify_devices_changed(&self) {
        let devices = self.devices();
        let subscribers = self.subscribers.lock().clone();

        for (_, subscriber) in subscribers {
            subscriber(Event::DevicesChanged(devices.clone()));
        }
    }

    fn notify_one(&self, connection: ConnectionId, event: Event) {
        let subscribers = self.subscribers.lock().clone();
        for (id, subscriber) in subscribers {
            if id == connection {
                subscriber(event.clone());
            }
        }
    }

    /// Broadcasts an event to every connection.
    pub fn broadcast(&self, event: Event) {
        let subscribers = self.subscribers.lock().clone();
        for (_, subscriber) in subscribers {
            subscriber(event.clone());
        }
    }
}

fn map_state(state: ConnectionState) -> DeviceState {
    match state {
        ConnectionState::Disconnected => DeviceState::Disconnected,
        ConnectionState::Connecting => DeviceState::Connecting,
        ConnectionState::Connected => DeviceState::Connected,
        ConnectionState::Streaming => DeviceState::Streaming,
        ConnectionState::Disconnecting => DeviceState::Disconnecting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streaming_registry() -> (DeviceRegistry, DeviceId) {
        let registry = DeviceRegistry::new();
        let id: DeviceId = "quest3".into();
        registry.sync_from_client_list(&[(
            id.clone(),
            "Quest 3".into(),
            ConnectionState::Streaming,
        )]);
        registry.set_stream_params(
            &id,
            StreamParams {
                view_resolution: [1920, 1832],
                refresh_rate: 72.0,
            },
        );
        (registry, id)
    }

    #[test]
    fn claim_then_release_round_trips() {
        let (registry, id) = streaming_registry();
        let connection = registry.new_connection_id();

        assert!(registry.devices()[0].is_usable());
        registry.claim(&id, connection, 42, "app").unwrap();

        let device = &registry.devices()[0];
        assert!(!device.is_usable(), "a claimed device is not usable");
        assert_eq!(
            device.availability,
            Availability::ClaimedBy {
                process_id: 42,
                claimant: "app".into()
            }
        );

        registry.release(&id, connection).unwrap();
        assert!(registry.devices()[0].is_usable());
    }

    #[test]
    fn second_claim_is_rejected() {
        let (registry, id) = streaming_registry();
        let first = registry.new_connection_id();
        let second = registry.new_connection_id();

        registry.claim(&id, first, 1, "first").unwrap();
        let error = registry.claim(&id, second, 2, "second").unwrap_err();
        assert!(error.contains("already claimed"), "{error}");
    }

    #[test]
    fn disconnect_releases_claims() {
        // The crashed-app case: no explicit release ever arrives.
        let (registry, id) = streaming_registry();
        let connection = registry.new_connection_id();

        registry.claim(&id, connection, 1, "doomed").unwrap();
        assert!(!registry.devices()[0].is_usable());

        registry.disconnect(connection);
        assert!(
            registry.devices()[0].is_usable(),
            "a dropped connection must free its devices"
        );
    }

    #[test]
    fn claim_requires_a_connected_device() {
        let registry = DeviceRegistry::new();
        let id: DeviceId = "gone".into();
        registry.sync_from_client_list(&[(
            id.clone(),
            "Gone".into(),
            ConnectionState::Disconnected,
        )]);

        let error = registry
            .claim(&id, registry.new_connection_id(), 1, "app")
            .unwrap_err();
        assert!(error.contains("not connected"), "{error}");
    }

    #[test]
    fn disconnecting_device_revokes_its_claim() {
        let (registry, id) = streaming_registry();
        let connection = registry.new_connection_id();
        registry.claim(&id, connection, 1, "app").unwrap();

        // The headset drops off.
        registry.sync_from_client_list(&[(
            id.clone(),
            "Quest 3".into(),
            ConnectionState::Disconnected,
        )]);

        assert!(matches!(
            registry.devices()[0].availability,
            Availability::Free
        ));
    }

    #[test]
    fn subscription_survives_an_mpsc_channel_like_serve_client_uses() {
        // serve_client wires the subscriber to an mpsc::Sender consumed by a separate thread. If the
        // Sender is dropped (or the closure captured wrongly), every send silently no-ops and the
        // client sees nothing -- which is exactly the symptom to guard against here.
        let registry = DeviceRegistry::new();
        let connection = registry.new_connection_id();

        let (sender, receiver) = std::sync::mpsc::channel::<Event>();
        registry.subscribe(
            connection,
            Box::new(move |event| {
                sender.send(event).ok();
            }),
        );

        registry.sync_from_client_list(&[(
            "late".into(),
            "Late".into(),
            ConnectionState::Streaming,
        )]);

        let event = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("event must reach the mpsc receiver");
        assert!(matches!(event, Event::DevicesChanged(_)));
    }

    #[test]
    fn subscribers_receive_change_events() {
        // Regression: a watcher that connects before any headset must still be told when one
        // appears, otherwise "wait for a free device" can never work.
        let registry = DeviceRegistry::new();
        let connection = registry.new_connection_id();

        let received = Arc::new(Mutex::new(Vec::new()));
        registry.subscribe(
            connection,
            Box::new({
                let received = Arc::clone(&received);
                move |event| received.lock().push(event)
            }),
        );

        registry.sync_from_client_list(&[(
            "late".into(),
            "Late".into(),
            ConnectionState::Streaming,
        )]);

        let events = received.lock();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::DevicesChanged(devices) if devices.len() == 1)),
            "expected a DevicesChanged event, got {events:?}"
        );
    }

    #[test]
    fn stale_devices_are_removed() {
        let (registry, _) = streaming_registry();
        assert_eq!(registry.devices().len(), 1);

        registry.sync_from_client_list(&[]);
        assert!(registry.devices().is_empty());
    }
}
