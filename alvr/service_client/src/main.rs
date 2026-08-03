//! Control-channel client for `alvr_service`.
//!
//! Doubles as a test tool and as the reference for how the ALVR OpenXR runtime will talk to the
//! service. The runtime does exactly this: connect, `Hello` (which returns a device snapshot), then
//! either claim a free device immediately or wait on `DevicesChanged` until one appears.
//!
//! Note it opens the pipe with `CreateFile2`, not `CreateFileW`: the latter is not in the UWP API
//! surface and will not compile in an AppContainer project, so the runtime must use `CreateFile2`.
//!
//! Usage:
//!   alvr_service_client list                 -- snapshot and exit
//!   alvr_service_client watch [secs]         -- stream change events
//!   alvr_service_client claim <id> [secs]    -- claim a device, hold it, then release
//!   alvr_service_client wait-and-claim [secs]-- wait for any free device, claim the first

use alvr_service_protocol::{
    Device, Event, PIPE_NAME, PROTOCOL_VERSION, Request, Response, decode, encode,
};
use std::{env, process::ExitCode, thread, time::{Duration, Instant}};
use windows::{
    Win32::{
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE},
        Storage::FileSystem::{
            CREATEFILE2_EXTENDED_PARAMETERS, CreateFile2, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
            ReadFile, WriteFile,
        },
        System::Pipes::{PIPE_READMODE_MESSAGE, SetNamedPipeHandleState},
    },
    core::{HSTRING, PCWSTR},
};

const BUFFER_SIZE: usize = 64 * 1024;

struct Client {
    handle: HANDLE,
}

impl Client {
    /// Connects, retrying briefly so either side may start first.
    fn connect(timeout: Duration) -> Result<Self, String> {
        let name = HSTRING::from(PIPE_NAME);
        let deadline = Instant::now() + timeout;

        loop {
            let mut params = CREATEFILE2_EXTENDED_PARAMETERS {
                dwSize: std::mem::size_of::<CREATEFILE2_EXTENDED_PARAMETERS>() as u32,
                dwFileAttributes: FILE_ATTRIBUTE_NORMAL.0,
                ..Default::default()
            };

            let handle = unsafe {
                CreateFile2(
                    PCWSTR(name.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    Default::default(),
                    OPEN_EXISTING,
                    Some(&mut params),
                )
            };

            match handle {
                Ok(handle) => {
                    // The server side is message mode; match it so one read is one message.
                    let mode = PIPE_READMODE_MESSAGE;
                    unsafe {
                        SetNamedPipeHandleState(handle, Some(&mode), None, None).ok();
                    }
                    return Ok(Self { handle });
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(format!("Could not connect to {PIPE_NAME}: {e}"));
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    fn send(&self, request: &Request) -> Result<(), String> {
        let payload = encode(request).map_err(|e| format!("{e}"))?;
        let mut written = 0u32;
        unsafe { WriteFile(self.handle, Some(&payload), Some(&mut written), None) }
            .map_err(|e| format!("write failed: {e}"))
    }

    fn receive(&self) -> Result<Response, String> {
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut read = 0u32;
        unsafe { ReadFile(self.handle, Some(&mut buffer), Some(&mut read), None) }
            .map_err(|e| format!("read failed: {e}"))?;

        buffer.truncate(read as usize);
        decode(&buffer).map_err(|e| format!("decode failed: {e}"))
    }

    /// Sends `Hello` and returns the device snapshot it carries.
    ///
    /// Responses and unsolicited events share one stream, so an event can arrive before the reply;
    /// skip those rather than treating them as a protocol error.
    fn hello(&self, claimant: &str) -> Result<Vec<Device>, String> {
        self.send(&Request::Hello {
            protocol_version: PROTOCOL_VERSION,
            claimant: claimant.to_owned(),
            process_id: std::process::id(),
        })?;

        loop {
            match self.receive()? {
                Response::Hello {
                    protocol_version,
                    service_version,
                    devices,
                } => {
                    println!(
                        "connected: service {service_version}, protocol {protocol_version} \
                         (client speaks {PROTOCOL_VERSION})"
                    );
                    return Ok(devices);
                }
                Response::Event(_) => continue,
                other => return Err(format!("unexpected reply to Hello: {other:?}")),
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle).ok() };
    }
}

fn print_devices(devices: &[Device]) {
    if devices.is_empty() {
        println!("  (no devices)");
        return;
    }
    for device in devices {
        let resolution = device
            .view_resolution
            .map(|[w, h]| format!("{w}x{h}"))
            .unwrap_or_else(|| "-".into());
        println!(
            "  {:<28} {:<13} {:<28} {resolution} @ {}Hz  usable={}",
            device.id,
            format!("{:?}", device.state),
            format!("{:?}", device.availability),
            device.refresh_rate.unwrap_or(0.0),
            device.is_usable(),
        );
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("list");

    let client = match Client::connect(Duration::from_secs(5)) {
        Ok(client) => client,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let devices = match client.hello("alvr_service_client") {
        Ok(devices) => devices,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    match mode {
        "list" => {
            println!("devices:");
            print_devices(&devices);
        }
        "watch" => {
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
            println!("devices at connect:");
            print_devices(&devices);
            println!("\nwatching for {secs}s...");

            let deadline = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < deadline {
                match client.receive() {
                    Ok(Response::Event(Event::DevicesChanged(devices))) => {
                        println!("[event] DevicesChanged:");
                        print_devices(&devices);
                    }
                    Ok(Response::Event(event)) => println!("[event] {event:?}"),
                    Ok(other) => println!("[unexpected] {other:?}"),
                    Err(e) => {
                        println!("disconnected: {e}");
                        break;
                    }
                }
            }
        }
        "claim" => {
            let Some(id) = args.get(2) else {
                eprintln!("usage: alvr_service_client claim <device-id> [hold-secs]");
                return ExitCode::FAILURE;
            };
            let hold: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);

            if let Err(e) = client.send(&Request::ClaimDevice { id: id.clone() }) {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
            match client.receive() {
                Ok(Response::DeviceClaimed {
                    id,
                    shared_texture_name,
                    view_resolution,
                    refresh_rate,
                }) => {
                    println!(
                        "CLAIMED {id}: texture={shared_texture_name} \
                         {}x{} @ {refresh_rate}Hz",
                        view_resolution[0], view_resolution[1]
                    );
                    println!("holding for {hold}s...");
                    thread::sleep(Duration::from_secs(hold));

                    client.send(&Request::ReleaseDevice { id }).ok();
                    match client.receive() {
                        Ok(Response::DeviceReleased { id }) => println!("RELEASED {id}"),
                        Ok(other) => println!("unexpected: {other:?}"),
                        Err(e) => println!("error: {e}"),
                    }
                }
                Ok(Response::Error { message, .. }) => {
                    eprintln!("CLAIM FAILED: {message}");
                    return ExitCode::FAILURE;
                }
                Ok(other) => println!("unexpected: {other:?}"),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        "wait-and-claim" => {
            // This is the runtime's actual startup path: use a free device now if there is one,
            // otherwise block on change events until one appears.
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
            let deadline = Instant::now() + Duration::from_secs(secs);

            let mut target = devices.iter().find(|d| d.is_usable()).map(|d| d.id.clone());
            if target.is_some() {
                println!("a device is already free, no waiting needed");
            } else {
                println!("no free device; waiting up to {secs}s...");
            }

            while target.is_none() && Instant::now() < deadline {
                match client.receive() {
                    Ok(Response::Event(Event::DevicesChanged(devices))) => {
                        target = devices.iter().find(|d| d.is_usable()).map(|d| d.id.clone());
                    }
                    Ok(_) => (),
                    Err(e) => {
                        eprintln!("disconnected: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }

            let Some(id) = target else {
                eprintln!("timed out waiting for a free device");
                return ExitCode::FAILURE;
            };

            println!("claiming {id}");
            client.send(&Request::ClaimDevice { id }).ok();
            match client.receive() {
                Ok(Response::DeviceClaimed {
                    id,
                    shared_texture_name,
                    ..
                }) => println!("CLAIMED {id}: texture={shared_texture_name}"),
                Ok(Response::Error { message, .. }) => {
                    eprintln!("CLAIM FAILED: {message}");
                    return ExitCode::FAILURE;
                }
                Ok(other) => println!("unexpected: {other:?}"),
                Err(e) => eprintln!("{e}"),
            }
        }
        other => {
            eprintln!("unknown mode: {other}");
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}
