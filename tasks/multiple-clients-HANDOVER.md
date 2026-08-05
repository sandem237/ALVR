# Handover: multi-device ALVR (shelved)

**Branch:** `multiple_clients` (from `full_transparency` @ `3391a20b`)
**State:** Phase 1 complete and verified. Phase 2 ~80%, with one known defect.
**Build:** `cargo build --workspace` clean. 11 unit tests pass.

Read [`multiple-clients.md`](multiple-clients.md) for the full architecture and every measured
finding. This file is the "where do I pick up" summary.

---

## The goal

Serve **several XR headsets simultaneously** from one PC. Each connected headset can be driven by its
own OpenXR application process; the app picks which device to target via an ALVR OpenXR extension.
ALVR *enables* orchestration, it does not implement the policy.

## What works today (verified, not assumed)

**Phase 1 — per-client streaming in `server_core`.** Two headsets streaming concurrently at
**70 fps / 1.20 Mbit/s each**, over **both UDP and TCP**, with one force-dropped and reconnected while
the other streamed unaffected.

- `ClientSession` holds per-client streaming state in a hostname-keyed map. Previously *any* single
  disconnect nulled the video/haptics senders for **every** client — a real bug, now fixed.
- `ServerCoreEvent` variants carry `client_id`. The C ABI stays single-client by design.
- `ServerCoreConfig { restart_on_settings_change }` gates the SteamVR driver-restart path (default
  `true` preserves SteamVR behaviour; a multi-headset backend must set `false`).
- Per-client stream ports on both ends, negotiated via `NegotiatedStreamingConfigExt` (server→client)
  and `ClientControlPacket::StreamReady` (client→server), both falling back to the configured port.
- `server_openvr` binds to one client and ignores others, since SteamVR has one HMD per `vrserver`.

**Phase 2 — the background service.** `alvr_service` owns headset connections and serves apps over a
named pipe. Verified end to end: two headsets enumerate with correct state/availability, claiming one
marks it unavailable while the other stays free, and **killing the claimant releases its device**.

**The UWP question — settled by measurement.** A genuinely sandboxed packaged UWP app (no
`runFullTrust`, no capabilities) **connected outward** to the service-created `Global\` pipe carrying an
`S-1-15-2-1` ACL. Negative control: without the ACL the same open fails `ACCESS_DENIED`.

## The one known defect — start here

**Pushed events (`DevicesChanged`) never reach clients.**

Diagnosed precisely: the pipe handle is created **without `FILE_FLAG_OVERLAPPED`**, and Windows
serializes *all* I/O on a synchronous handle at the file-object level. The event pump's `WriteFile`
queues behind the request loop's outstanding `ReadFile` and never runs — a guaranteed circular
deadlock, since the client is blocked reading whatever the write would have delivered.

Confirmed by instrumentation: the registry notifies, the subscriber closure queues, the pump thread
starts — and no write is ever attempted.

**Fix:** overlapped I/O in [`alvr/service/src/pipe.rs`](../alvr/service/src/pipe.rs). Contained to that
one file; the protocol and registry need no changes.

- **Recommended: use the `interprocess` crate.** It forces `FILE_FLAG_OVERLAPPED` while exposing a
  blocking API, and handles message mode correctly.
- **Avoid tokio's named pipes here** — mio's readiness bridge uses a fixed 4 KiB buffer and *silently
  swallows* `ERROR_MORE_DATA`, so message boundaries are lost above 4096 bytes with no error
  (tokio #5307, #6460).
- If hand-rolling: a separate `OVERLAPPED` + its own event per concurrent operation; never wait on the
  pipe handle itself; `ERROR_IO_PENDING` is success-pending; with an overlapped handle
  `ConnectNamedPipe` needs a non-NULL `lpOverlapped` and `ERROR_PIPE_CONNECTED` means *success*.
- `DuplicateHandle` does **not** help (same file object, same lock). `PIPE_NOWAIT` is deprecated.

`PipeConnection::split()` already exists and returns independent reader/writer halves, so the shape is
right — only the handle flags and the I/O calls need changing.

## Resume checklist, in order

1. **Fix the deadlock** (above), then re-run the pushed-event test:
   ```
   alvr_service --config-dir <tmp> --run-for 30
   alvr_mock_device --hostname mock-a --control-port 9943 --run-for 22
   alvr_service_client watch 16        # should print DevicesChanged as the headset appears
   ```
   Today the watcher prints nothing. That is the acceptance criterion.
2. **Finish Phase 2**: add `DevicesChanged` to `EventType` and a real `GET /api/devices` snapshot
   endpoint for orchestrators. Note `GET /api/session` currently returns **nothing** — it takes no
   state, returns `()`, and merely *broadcasts* an event, so there is no request/response way to read
   state. Without a snapshot endpoint, "start immediately if a device is free" races the subscription.
3. **Decide the rendezvous.** Not needed for the control channel (the app connects outward), but if
   per-session object names are negotiated, follow SteamVR: exchange them over the pipe rather than
   hardcoding. See `CSharedResourceNamespaceClient` in `multiple-clients.md`.
4. **Phase 3**: `alvr/server_openxr` runtime `cdylib` + `XR_ALVR_device_selection`. The
   AppContainer requirements are enumerated in `multiple-clients.md` — in particular the installer must
   ACL the install tree for `S-1-15-2-1`, and the runtime must never try to launch the service.

## Test tooling built along the way

All of it works and is the reason each finding is measured rather than argued.

| Tool | Purpose |
|---|---|
| `alvr/mock_device` | Headless ALVR client. One process per emulated headset; per-instance hostname/control/stream ports. |
| `alvr/mock_server` | Headless server driving the real `ServerCoreContext`, synthetic video, no SteamVR. Superseded by `alvr/service` but still useful. |
| `alvr/service_client` | Control-pipe client: `list`, `watch`, `claim`, `wait-and-claim`. Also the reference for how the runtime should talk to the service. |
| `tools/appcontainer-run` | Runs any exe inside a real AppContainer. Fast iteration, supports negative controls. Cannot catch Store API-surface restrictions. |
| `tools/uwp-ipc-test` | **Genuine packaged UWP app** probing pipe / shared memory / shared texture. Catches API-surface issues the harness cannot. |
| `tools/uwp-probe-host` | Desktop counterpart creating the ACL'd objects; its output is the authoritative result. |

Typical two-headset run:
```
alvr_service --protocol udp --auto-trust --config-dir <tmp> --run-for 40
alvr_mock_device --hostname mock-a --control-port 9943 --run-for 34
alvr_mock_device --hostname mock-b --control-port 9953 --run-for 34
```

## Environment notes

- Build with `cargo`; on this machine it is **not on the Bash tool's PATH** — use PowerShell with
  `$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"`.
- A **git worktree does not populate submodules**: run `git submodule update --init --depth 1 openvr`,
  and copy `deps/` from the main checkout (gitignored prebuilt libs; `libvpl` is needed by
  `server_openvr`'s C++).
- **ALVR filters logs.** `info!`/`debug!` may not surface; the file sink is `Error` only. Use `error!`
  for diagnostics you need to see, or you will misread silence as absence. This cost real time.
- UWP builds need `MSBuild` from VS 2022; the UWP C++ targets live under
  `MSBuild\Microsoft\VC\v170\Application Type\Windows Store` (**not** `Platforms\UAP`).
- The VS UWP project needs `CompileAsWinRT=false` (C++/WinRT, not CX), a `wWinMain` entry point, and
  `mp:PhoneIdentity` or packaging fails `APPX1673`.

## Hard-won lessons worth not relearning

1. **Trust the test over the docs.** `CreateFile2`'s docs say a Store app *"can't open named pipes"*.
   Measurement says otherwise. Reversing a verified result on the strength of a doc sentence caused a
   long detour and left this task file with three mutually contradictory sections at one point.
2. **`Global\` means different things per object type.** For **pipes** it is just part of the name in a
   single machine-wide namespace — no privilege needed. For **sections and events** it is a real
   namespace requiring `SeCreateGlobalPrivilege`. Measured unelevated: `CreateFileMapping` in `Global\`
   is `ACCESS_DENIED`, while `CreateNamedPipe`/`CreateEvent`/`CreateMutex` succeed. Do not generalise
   one denial across object types — that error sent the design the wrong way for a while.
3. **`SO_REUSEADDR` does not make UDP multi-client on Windows.** Sockets bind and connect, then never
   receive. Windows does not demultiplex connected UDP sockets sharing a local port the way Linux
   `SO_REUSEPORT` does. The fix is one local port per client.
4. **Check the build actually succeeded** before interpreting a run. Two debugging cycles were spent
   reading a stale binary after a `cargo build` failure scrolled past.
5. **Two UWP harnesses are both needed.** The AppContainer simulator tests the *security boundary*; only
   a real packaged UWP app catches *API-surface* restrictions (that is how `CreateFileW`'s absence was
   found).

## Things deliberately not done

- **Multi-HMD inside SteamVR is impossible** and out of scope: one HMD per `vrserver`, one compositor,
  one runtime path. `server_openvr` stays single-client. See "Rejected alternatives".
- Idle "no signal" video is designed and shown feasible (the service *can* own the shared texture) but
  not implemented — it belongs with the Phase 3 encoder work.
- D3D12/Vulkan app support: the named-shared-resource trick is D3D11-only; others need handle
  duplication. Out of scope while D3D11 interop suffices.
