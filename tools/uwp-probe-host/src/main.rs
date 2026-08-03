//! Desktop host for the UWP IPC probe.
//!
//! Creates every candidate IPC object with an ACL granting `S-1-15-2-1`
//! (ALL_APPLICATION_PACKAGES), then reports which ones a sandboxed UWP app actually managed to use.
//!
//! Run this first, then launch `alvr_uwp_ipc_test`. This host's output is the authoritative result:
//! the UWP app has no console, and its own log file can be cut short when the app exits.
//!
//! Everything here is unpackaged and unelevated, matching how `alvr_service` will run.

use std::{
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ,
                D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
                D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
                ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIKeyedMutex,
                IDXGIResource1,
            },
        },
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{PIPE_ACCESS_DUPLEX, ReadFile},
        System::{
            Memory::{
                CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE,
                UnmapViewOfFile,
            },
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE,
                PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
            },
            Threading::{CreateEventW, SetEvent, WaitForSingleObject},
        },
    },
    core::{HSTRING, Interface, PCWSTR},
};

/// Grants ALL_APPLICATION_PACKAGES so an AppContainer can open these objects. Without this the
/// sandboxed side gets ACCESS_DENIED -- previously verified with a negative control.
const SDDL: &str = "D:(A;;GA;;;S-1-15-2-1)(A;;GA;;;AU)(A;;GA;;;SY)";

const PIPE_NAME: &str = r"\\.\pipe\Global\alvr_probe_pipe";
const SHMEM_NAME: &str = r"Global\alvr_probe_shmem";
const REQUEST_EVENT: &str = r"Global\alvr_probe_request";
const REPLY_EVENT: &str = r"Global\alvr_probe_reply";
const TEXTURE_NAME: &str = r"Global\alvr_probe_texture";

const W: u32 = 64;
const H: u32 = 64;

/// Builds SECURITY_ATTRIBUTES from the SDDL. The descriptor must outlive every object created with
/// it, so the caller keeps it alive.
struct Security {
    descriptor: PSECURITY_DESCRIPTOR,
}

impl Security {
    fn new() -> Self {
        let sddl = HSTRING::from(SDDL);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .expect("SDDL conversion");
        }
        Self { descriptor }
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        if !self.descriptor.is_invalid() {
            unsafe { LocalFree(HLOCAL(self.descriptor.0)) };
        }
    }
}

fn create_device() -> (ID3D11Device, ID3D11DeviceContext) {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .expect("D3D11CreateDevice");
    }
    (device.unwrap(), context.unwrap())
}

fn main() {
    println!("=== UWP probe host (unpackaged, unelevated) ===");
    println!("Creating ACL'd objects: {SDDL}\n");

    let security = Security::new();

    // --- 1. named pipe (expected to be unreachable from a Store app) ---
    let pipe = unsafe {
        let name = HSTRING::from(PIPE_NAME);
        let attributes = security.attributes();
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            Some(&attributes),
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        println!("[host] pipe   : FAILED to create: {:?}", unsafe { GetLastError() });
    } else {
        println!("[host] pipe   : created {PIPE_NAME}");
    }

    // --- 2. shared memory + events ---
    let (mapping, view) = unsafe {
        let name = HSTRING::from(SHMEM_NAME);
        let attributes = security.attributes();
        // Diagnose which part CreateFileMapping objects to: try with the SDDL, then without.
        let mapping = match CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            Some(&attributes),
            PAGE_READWRITE,
            0,
            4096,
            PCWSTR(name.as_ptr()),
        ) {
            Ok(mapping) => {
                println!("[host] shmem  : created WITH the S-1-15-2-1 ACL");
                mapping
            }
            Err(e) => {
                println!("[host] shmem  : ACL rejected ({e}); retrying with a default DACL");
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    0,
                    4096,
                    PCWSTR(name.as_ptr()),
                )
                .expect("CreateFileMapping (default DACL)")
            }
        };

        let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 4096);
        (mapping, view)
    };
    println!("[host] shmem  : created {SHMEM_NAME}");

    let (request_event, reply_event) = unsafe {
        let attributes = security.attributes();
        let request = HSTRING::from(REQUEST_EVENT);
        let reply = HSTRING::from(REPLY_EVENT);
        (
            CreateEventW(Some(&attributes), false, false, PCWSTR(request.as_ptr()))
                .expect("request event"),
            CreateEventW(Some(&attributes), false, false, PCWSTR(reply.as_ptr()))
                .expect("reply event"),
        )
    };
    println!("[host] events : created request + reply");

    // --- 3. named shared D3D11 texture ---
    let (device, context) = create_device();
    let desc = D3D11_TEXTURE2D_DESC {
        Width: W,
        Height: H,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
            | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
    };

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .expect("CreateTexture2D");
    }
    let texture = texture.unwrap();

    unsafe {
        let attributes = security.attributes();
        let name = HSTRING::from(TEXTURE_NAME);
        let resource: IDXGIResource1 = texture.cast().expect("IDXGIResource1");
        resource
            .CreateSharedHandle(
                Some(&attributes),
                (DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0) as u32,
                PCWSTR(name.as_ptr()),
            )
            .expect("CreateSharedHandle");
    }
    println!("[host] texture: created {TEXTURE_NAME}");

    let keyed_mutex: IDXGIKeyedMutex = texture.cast().expect("IDXGIKeyedMutex");

    // Staging texture to read back what the app wrote.
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            .expect("staging");
    }
    let staging = staging.unwrap();

    println!("\n[host] ready. Launch the UWP app now. Watching for 40s...\n");

    // Serve the pipe on a thread: if a Store app ever does connect, we want to know.
    // HANDLE is not Send, so move it as a raw pointer value and rebuild it inside the thread.
    let pipe_raw = pipe.0 as usize;
    thread::spawn(move || {
        if pipe_raw == INVALID_HANDLE_VALUE.0 as usize {
            return;
        }
        let pipe = HANDLE(pipe_raw as *mut core::ffi::c_void);
        unsafe {
            let connected = ConnectNamedPipe(pipe, None).is_ok() || GetLastError().0 == 535;
            if connected {
                let mut buf = [0u8; 512];
                let mut read = 0u32;
                if ReadFile(pipe, Some(&mut buf), Some(&mut read), None).is_ok() && read > 0 {
                    println!(
                        "[host] pipe   : *** UWP CONNECTED *** {:?}",
                        String::from_utf8_lossy(&buf[..read as usize])
                    );
                }
            }
            CloseHandle(pipe).ok();
        }
    });

    let deadline = Instant::now() + Duration::from_secs(40);
    let mut saw_shmem = false;
    let mut saw_texture = false;
    let mut last_pixel = 0u32;

    while Instant::now() < deadline {
        // Shared-memory round trip: the app writes a request and signals; we answer.
        if !saw_shmem
            && unsafe { WaitForSingleObject(request_event, 50) } == windows::Win32::Foundation::WAIT_OBJECT_0
        {
            let text = unsafe {
                let bytes = std::slice::from_raw_parts(view.Value as *const u8, 256);
                let end = bytes.iter().position(|&b| b == 0).unwrap_or(0);
                String::from_utf8_lossy(&bytes[..end]).to_string()
            };
            println!("[host] shmem  : *** UWP WROTE *** {text:?}");

            let answer = b"ack from desktop host\0";
            unsafe {
                std::ptr::copy_nonoverlapping(
                    answer.as_ptr(),
                    view.Value as *mut u8,
                    answer.len(),
                );
                SetEvent(reply_event).ok();
            }
            println!("[host] shmem  : replied, round trip COMPLETE");
            saw_shmem = true;
        }

        // Texture: look for pixels written by the app.
        if !saw_texture {
            unsafe {
                if keyed_mutex.AcquireSync(0, 10).is_ok() {
                    context.CopyResource(&staging, &texture);
                    keyed_mutex.ReleaseSync(1).ok();

                    let mut mapped = Default::default();
                    if context
                        .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        .is_ok()
                    {
                        let pixel = *(mapped.pData as *const u32);
                        context.Unmap(&staging, 0);

                        if pixel != 0 && pixel != last_pixel {
                            println!("[host] texture: *** UWP WROTE *** pixel 0x{pixel:08X}");
                            last_pixel = pixel;
                            saw_texture = true;
                        }
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    println!("\n=== HOST SUMMARY (authoritative) ===");
    println!(
        "  shared memory  : {}",
        if saw_shmem { "WORKS from UWP" } else { "no traffic" }
    );
    println!(
        "  shared texture : {}",
        if saw_texture { "WORKS from UWP" } else { "no traffic" }
    );
    println!("  named pipe     : see any '*** UWP CONNECTED ***' line above");

    unsafe {
        UnmapViewOfFile(view).ok();
        CloseHandle(mapping).ok();
        CloseHandle(request_event).ok();
        CloseHandle(reply_event).ok();
    }
}
