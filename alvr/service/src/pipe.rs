//! Named-pipe transport for the service control channel.
//!
//! The pipe is created with a security descriptor granting `ALL_APPLICATION_PACKAGES`, which is what
//! lets a UWP AppContainer app open it. Without that grant the open fails with `ACCESS_DENIED`
//! (verified with a negative control), and with it a genuine packaged UWP app connects successfully.
//!
//! Message mode means one read returns exactly one message, so there is no length framing to get
//! wrong. Pipes do not queue connections: if no idle instance exists a client's open fails with
//! `ERROR_PIPE_BUSY`, so the accept loop always creates the next instance before handing off the
//! current one.

use alvr_common::{anyhow::Result, error, info};
use alvr_service_protocol::{OBJECT_SDDL, PIPE_NAME};
use std::{io, sync::Arc};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, HLOCAL,
            LocalFree,
        },
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{PIPE_ACCESS_DUPLEX, ReadFile, WriteFile},
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
            PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        },
    },
    core::{HSTRING, PCWSTR},
};

const BUFFER_SIZE: u32 = 64 * 1024;

/// One connected client. Closing it disconnects that client only.
///
/// Split into a reader and a writer with [`Self::split`], because the request loop blocks in a read
/// while the event pump needs to write concurrently. Sharing one lock across both would deadlock:
/// the reader would hold it for as long as it waits for a request.
pub struct PipeConnection {
    handle: HANDLE,
}

// The handle is owned exclusively by this struct and only used from the thread that owns it.
unsafe impl Send for PipeConnection {}

impl PipeConnection {
    /// Splits into a reader and a writer over the same pipe instance.
    ///
    /// Windows allows concurrent read and write on one pipe handle, so both halves share it; the
    /// handle is closed once when the last half drops.
    pub fn split(self) -> (PipeReader, PipeWriter) {
        let handle = Arc::new(OwnedPipeHandle(self.handle));
        // Prevent Drop from closing the handle: ownership moved into OwnedPipeHandle.
        std::mem::forget(self);

        (
            PipeReader {
                handle: Arc::clone(&handle),
            },
            PipeWriter { handle },
        )
    }
}

impl Drop for PipeConnection {
    fn drop(&mut self) {
        unsafe {
            DisconnectNamedPipe(self.handle).ok();
            CloseHandle(self.handle).ok();
        }
    }
}

/// Shared owner of a pipe instance; disconnects and closes when the last reference goes.
struct OwnedPipeHandle(HANDLE);

unsafe impl Send for OwnedPipeHandle {}
unsafe impl Sync for OwnedPipeHandle {}

impl Drop for OwnedPipeHandle {
    fn drop(&mut self) {
        unsafe {
            DisconnectNamedPipe(self.0).ok();
            CloseHandle(self.0).ok();
        }
    }
}

/// Read half. Blocking reads here do not block writes on the other half.
pub struct PipeReader {
    handle: Arc<OwnedPipeHandle>,
}

impl PipeReader {
    /// Reads one message, or `Ok(None)` once the peer disconnects.
    pub fn read_message(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buffer = vec![0u8; BUFFER_SIZE as usize];
        let mut read = 0u32;

        let result = unsafe { ReadFile(self.handle.0, Some(&mut buffer), Some(&mut read), None) };

        match result {
            Ok(()) => {
                buffer.truncate(read as usize);
                Ok(Some(buffer))
            }
            Err(_) => {
                let code = unsafe { GetLastError() };
                if code == ERROR_MORE_DATA {
                    alvr_common::anyhow::bail!("Control message exceeds {BUFFER_SIZE} bytes");
                }
                Ok(None)
            }
        }
    }
}

/// Write half. Cloneable so responses and pushed events can share it.
#[derive(Clone)]
pub struct PipeWriter {
    handle: Arc<OwnedPipeHandle>,
}

impl PipeWriter {
    pub fn write_message(&self, payload: &[u8]) -> Result<()> {
        let mut written = 0u32;
        unsafe { WriteFile(self.handle.0, Some(payload), Some(&mut written), None) }
            .map_err(|e| alvr_common::anyhow::anyhow!("Pipe write failed: {e}"))?;

        Ok(())
    }
}

/// Listens on the well-known control pipe.
pub struct PipeListener {
    security_descriptor: PSECURITY_DESCRIPTOR,
}

// The descriptor is allocated once, then only read (to build SECURITY_ATTRIBUTES) and freed on drop.
// It is never mutated, so sharing it across the accept thread is sound.
unsafe impl Send for PipeListener {}
unsafe impl Sync for PipeListener {}

impl PipeListener {
    pub fn new() -> Result<Self> {
        let sddl = HSTRING::from(OBJECT_SDDL);
        let mut security_descriptor = PSECURITY_DESCRIPTOR::default();

        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut security_descriptor,
                None,
            )
        }
        .map_err(|e| alvr_common::anyhow::anyhow!("Failed to build security descriptor: {e}"))?;

        info!("Control pipe: {PIPE_NAME}");

        Ok(Self {
            security_descriptor,
        })
    }

    /// Creates an instance and blocks until a client connects.
    ///
    /// Each call yields an independent channel, so several apps can be connected at once; the caller
    /// hands each connection to its own thread and calls this again to keep an instance available.
    pub fn accept(&self) -> Result<PipeConnection> {
        let name = HSTRING::from(PIPE_NAME);

        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.security_descriptor.0,
            bInheritHandle: false.into(),
        };

        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER_SIZE,
                BUFFER_SIZE,
                0,
                Some(&attributes),
            )
        };

        if handle.is_invalid() {
            let code = unsafe { GetLastError() };
            alvr_common::anyhow::bail!(
                "CreateNamedPipe failed ({code:?}): {}",
                io::Error::from_raw_os_error(code.0 as i32)
            );
        }

        // ERROR_PIPE_CONNECTED means a client attached between create and connect; that is a
        // successful connection, not a failure.
        let connected = unsafe { ConnectNamedPipe(handle, None) }.is_ok()
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;

        if !connected {
            let code = unsafe { GetLastError() };
            unsafe { CloseHandle(handle).ok() };
            alvr_common::anyhow::bail!("ConnectNamedPipe failed: {code:?}");
        }

        Ok(PipeConnection { handle })
    }
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        if !self.security_descriptor.is_invalid() {
            unsafe {
                LocalFree(HLOCAL(self.security_descriptor.0));
            }
        }
    }
}

/// Logs and swallows an error, so one misbehaving client cannot take the accept loop down.
pub fn log_client_error(context: &str, error: &alvr_common::anyhow::Error) {
    error!("Control client error ({context}): {error}");
}
