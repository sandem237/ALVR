// AppContainer test harness for Spikes 1 and 2.
//
// The VS UWP C++ workload is not installed, and installing it is a large machine-wide change. But a
// UWP app's security boundary IS an AppContainer, so we can test the thing that actually matters:
// create a real AppContainer profile and launch a child process inside it via
// STARTUPINFOEX + PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES. That child is subject to the same
// AppContainer access checks a UWP app is, so if it can open our ACL'd pipe and shared texture, a
// UWP app can too.
//
// Usage:
//   spike_ac run <exe> [args...]   -- run <exe> inside a fresh AppContainer
//   spike_ac whoami                -- report whether the current process is in an AppContainer

use std::{env, ffi::c_void, mem, ptr};
use windows::{
    core::{HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, GetLastError, HANDLE},
        Security::{
            Isolation::{CreateAppContainerProfile, DeleteAppContainerProfile},
            TOKEN_QUERY, SECURITY_CAPABILITIES, TokenIsAppContainer,
        },
        System::{
            Threading::{
                CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
                InitializeProcThreadAttributeList, OpenProcessToken, UpdateProcThreadAttribute,
                WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
                LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, STARTUPINFOEXW, STARTUPINFOW,
            },
        },
    },
};

const CONTAINER_NAME: &str = "AlvrSpikeAppContainer";
// PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES
const ATTR_SECURITY_CAPABILITIES: usize = 0x0002_0009;

fn report_is_appcontainer() {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            println!("FAIL: OpenProcessToken");
            return;
        }

        let mut is_ac = 0u32;
        let mut len = 0u32;
        let ok = windows::Win32::Security::GetTokenInformation(
            token,
            TokenIsAppContainer,
            Some(&mut is_ac as *mut _ as *mut c_void),
            mem::size_of::<u32>() as u32,
            &mut len,
        )
        .is_ok();

        if ok {
            println!(
                "TokenIsAppContainer = {} ({})",
                is_ac,
                if is_ac != 0 {
                    "INSIDE an AppContainer"
                } else {
                    "NOT in an AppContainer"
                }
            );
        } else {
            println!("FAIL: GetTokenInformation: {:?}", GetLastError());
        }

        CloseHandle(token).ok();
    }
}

fn run_in_appcontainer(command: &str) -> i32 {
    unsafe {
        let name = HSTRING::from(CONTAINER_NAME);
        let display = HSTRING::from("ALVR spike AppContainer");

        // Recreate cleanly so repeated runs are deterministic.
        DeleteAppContainerProfile(PCWSTR(name.as_ptr())).ok();

        let sid = match CreateAppContainerProfile(
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            PCWSTR(display.as_ptr()),
            None, // no extra capabilities: the strictest case
        ) {
            Ok(sid) => sid,
            Err(e) => {
                println!("FAIL: CreateAppContainerProfile: {e:?}");
                return 1;
            }
        };
        println!("[harness] AppContainer profile created: {CONTAINER_NAME}");

        let mut caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: ptr::null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };

        // Build the attribute list that puts the child in the container.
        let mut size = 0usize;
        InitializeProcThreadAttributeList(
            LPPROC_THREAD_ATTRIBUTE_LIST(ptr::null_mut()),
            1,
            0,
            &mut size,
        )
        .ok();

        let mut buffer = vec![0u8; size];
        let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr() as *mut c_void);

        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut size).is_err() {
            println!("FAIL: InitializeProcThreadAttributeList: {:?}", GetLastError());
            return 1;
        }

        if UpdateProcThreadAttribute(
            attr_list,
            0,
            ATTR_SECURITY_CAPABILITIES,
            Some(&mut caps as *mut _ as *const c_void),
            mem::size_of::<SECURITY_CAPABILITIES>(),
            None,
            None,
        )
        .is_err()
        {
            println!("FAIL: UpdateProcThreadAttribute: {:?}", GetLastError());
            return 1;
        }

        let mut si = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: mem::size_of::<STARTUPINFOEXW>() as u32,
                ..Default::default()
            },
            lpAttributeList: attr_list,
        };
        let mut pi = PROCESS_INFORMATION::default();

        let mut cmd: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        println!("[harness] launching inside AppContainer: {command}\n");

        let created = CreateProcessW(
            None,
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT,
            None,
            None,
            &mut si.StartupInfo,
            &mut pi,
        );

        DeleteProcThreadAttributeList(attr_list);

        if created.is_err() {
            println!("FAIL: CreateProcessW: {:?}", GetLastError());
            println!("(the child exe must be readable by the AppContainer SID)");
            return 1;
        }

        WaitForSingleObject(pi.hProcess, INFINITE);

        let mut code = 0u32;
        windows::Win32::System::Threading::GetExitCodeProcess(pi.hProcess, &mut code).ok();

        CloseHandle(pi.hProcess).ok();
        CloseHandle(pi.hThread).ok();

        println!("\n[harness] child exited with code {code}");
        code as i32
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("whoami");

    match mode {
        "whoami" => report_is_appcontainer(),
        "run" => {
            if args.len() < 3 {
                println!("usage: spike_ac run <exe> [args...]");
                return;
            }
            // Quote the exe path, leave args bare.
            let exe = &args[2];
            let rest = args[3..].join(" ");
            let command = if rest.is_empty() {
                format!("\"{exe}\"")
            } else {
                format!("\"{exe}\" {rest}")
            };
            let code = run_in_appcontainer(&command);
            std::process::exit(code);
        }
        other => println!("unknown mode: {other}"),
    }
}
