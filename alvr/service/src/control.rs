//! Control-channel session handling: one thread per connected client.
//!
//! Each connection is an independent pipe instance, so clients cannot block one another and a
//! misbehaving client only loses its own channel. Dropping a connection releases its device claims,
//! which is what stops a crashed application from orphaning a headset.

use crate::{
    pipe::{PipeConnection, PipeListener, PipeReader, PipeWriter, log_client_error},
    registry::DeviceRegistry,
};
use alvr_common::{anyhow::Result, error, info, warn};
use alvr_server_core::ServerCoreContext;
use alvr_service_protocol::{
    Event, PROTOCOL_VERSION, Request, Response, decode, encode, shared_texture_name,
};
use std::{
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
};

/// Starts the accept loop. Returns `None` if the pipe could not be created, in which case the
/// service still runs but serves no applications — worth logging loudly rather than aborting, since
/// headsets can still stream idle content.
pub fn spawn_listener(
    registry: Arc<DeviceRegistry>,
    context: Arc<ServerCoreContext>,
) -> Option<JoinHandle<()>> {
    let listener = match PipeListener::new() {
        Ok(listener) => listener,
        Err(e) => {
            error!("Control pipe unavailable, no applications can connect: {e}");
            return None;
        }
    };

    Some(thread::spawn(move || {
        loop {
            // Blocks until a client attaches. Pipes do not queue, so the next instance is created on
            // the following iteration, immediately after this one is handed off.
            match listener.accept() {
                Ok(connection) => {
                    let registry = Arc::clone(&registry);
                    let context = Arc::clone(&context);
                    thread::spawn(move || serve_client(connection, registry, context));
                }
                Err(e) => {
                    error!("Control pipe accept failed: {e}");
                    // Avoid a hot loop if the pipe is permanently broken.
                    thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }))
}

fn serve_client(
    connection: PipeConnection,
    registry: Arc<DeviceRegistry>,
    _context: Arc<ServerCoreContext>,
) {
    let connection_id = registry.new_connection_id();
    info!("Control client connected ({connection_id:?})");

    // Read and write halves are separate: the request loop blocks in a read for as long as the
    // client is idle, so sharing one lock with the event pump would starve every pushed event.
    let (mut reader, writer) = connection.split();

    // Events are queued rather than written inline, so a registry notification never writes to a
    // pipe while holding the registry lock.
    let (event_sender, event_receiver) = mpsc::channel::<Event>();
    registry.subscribe(
        connection_id,
        Box::new(move |event| {
            event_sender.send(event).ok();
        }),
    );

    let event_thread = thread::spawn({
        let writer = writer.clone();
        move || {
            while let Ok(event) = event_receiver.recv() {
                let Ok(payload) = encode(&Response::Event(event)) else {
                    continue;
                };
                if writer.write_message(&payload).is_err() {
                    // The client is gone; the request loop will notice too.
                    break;
                }
            }
            alvr_common::debug!("Event pump for {connection_id:?} ended");
        }
    });

    if let Err(e) = request_loop(&mut reader, &writer, &registry, connection_id) {
        log_client_error("request loop", &e);
    }

    // Always release claims, however the client went away.
    registry.disconnect(connection_id);
    info!("Control client disconnected ({connection_id:?})");

    // Dropping the subscription's sender ends the event thread.
    drop(event_thread);
}

fn request_loop(
    reader: &mut PipeReader,
    writer: &PipeWriter,
    registry: &Arc<DeviceRegistry>,
    connection_id: crate::registry::ConnectionId,
) -> Result<()> {
    let mut claimant = String::from("unknown");
    let mut process_id = 0u32;

    loop {
        let Some(message) = reader.read_message()? else {
            // Clean disconnect.
            return Ok(());
        };

        let request: Request = match decode(&message) {
            Ok(request) => request,
            Err(e) => {
                warn!("Malformed control request: {e}");
                let response = Response::Error {
                    request: "<undecodable>".into(),
                    message: format!("{e}"),
                };
                writer.write_message(&encode(&response)?)?;
                continue;
            }
        };

        let response = match request {
            Request::Hello {
                protocol_version,
                claimant: name,
                process_id: pid,
            } => {
                claimant = name;
                process_id = pid;

                if protocol_version != PROTOCOL_VERSION {
                    warn!(
                        "Control client {claimant} speaks protocol {protocol_version}, \
                         service speaks {PROTOCOL_VERSION}"
                    );
                }

                info!("Control client identified: {claimant} (pid {process_id})");

                // The snapshot ships with Hello so a client can decide to wait or start immediately
                // without a second round trip, and without racing the event subscription.
                Response::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    service_version: alvr_common::ALVR_VERSION.to_string(),
                    devices: registry.devices(),
                }
            }
            Request::ListDevices => Response::Devices(registry.devices()),
            Request::ClaimDevice { id } => {
                match registry.claim(&id, connection_id, process_id, &claimant) {
                    Ok(params) => Response::DeviceClaimed {
                        shared_texture_name: shared_texture_name(&id),
                        id,
                        view_resolution: params.view_resolution,
                        refresh_rate: params.refresh_rate,
                    },
                    Err(message) => Response::Error {
                        request: "ClaimDevice".into(),
                        message,
                    },
                }
            }
            Request::ReleaseDevice { id } => match registry.release(&id, connection_id) {
                Ok(()) => Response::DeviceReleased { id },
                Err(message) => Response::Error {
                    request: "ReleaseDevice".into(),
                    message,
                },
            },
            Request::Ping => Response::Pong,
        };

        writer.write_message(&encode(&response)?)?;
    }
}
