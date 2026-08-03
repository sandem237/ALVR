# appcontainer-run

Runs an arbitrary executable inside a real **AppContainer**, so UWP-facing behaviour can be tested
without installing the Visual Studio UWP C++ workload.

A UWP app's security boundary *is* an AppContainer. This tool creates an AppContainer profile with
**no extra capabilities** (the strictest case) and launches a child via `STARTUPINFOEX` +
`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, so the child faces exactly the access checks a UWP app
faces.

Used to verify that the ALVR service's named pipe and named shared texture are reachable from a
sandboxed OpenXR app — see `tasks/multiple-clients.md`, Spike 5.

## Usage

```sh
cargo build

# Is the current process sandboxed?
./target/debug/appcontainer_run whoami          # -> TokenIsAppContainer = 0

# Run something inside an AppContainer
./target/debug/appcontainer_run run <exe> [args...]
./target/debug/appcontainer_run run ./target/debug/appcontainer_run whoami   # -> 1
```

## Gotcha

An AppContainer has no access to your user profile by default, so the target executable (and anything
it loads) must be readable by the container SID:

```sh
icacls <dir> /grant "*S-1-15-2-1:(OI)(CI)(RX)" /T /Q
```

A packaged UWP app gets this from its package layout instead. For ALVR this only matters for test
binaries: the runtime DLL is loaded from a path the host app can already read, so only the two named
kernel objects (pipe, shared texture) need explicit ACLs.

## Interpreting results

Always pair a positive test with a **negative control** — run the same check against an object created
*without* the `S-1-15-2-1` grant. That should fail with `Access is denied` (`0x80070005`). Without the
control, a pass may only mean the object had a permissive default DACL.
