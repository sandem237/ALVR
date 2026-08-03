# Multiple clients: background service + ALVR OpenXR runtime

## Goal

Serve **several XR devices simultaneously** from one PC, where each connected headset can be driven by
its own OpenXR application process.

Target workflow: someone puts on a headset, it connects, a predefined OpenXR app is launched with the
device id, and the app uses an ALVR OpenXR extension to enumerate devices and pick which one to
target. ALVR *enables* orchestration; it does not implement the orchestration policy.

## Architecture

```
   headsets (N)                    PC
  ┌──────────┐        ┌───────────────────────────────┐
  │ Quest 3  │◀──UDP──┤  alvr_service (desktop, always │
  ├──────────┤        │  running, owns all headset     │
  │ Pico 4   │◀──UDP──┤  connections + encoders)       │
  └──────────┘        └───┬────────────────▲──────────┘
                          │ named pipe     │ shared D3D11 texture
                          │ (control)      │ (frames)
                     ┌────▼────────────────┴──────────┐
                     │ OpenXR app process (UWP or     │
                     │ desktop) + alvr_openxr runtime │
                     └────────────────────────────────┘
```

- **`alvr_service`** — persistent desktop process. Owns every headset connection, tracks device
  state, owns the encoders, and *always* streams. No SteamVR involved.
- **`alvr_openxr`** — OpenXR runtime `cdylib` loaded into each app, selected via `XR_RUNTIME_JSON`.
  Connects to the service, enumerates devices, subscribes to changes, claims a device, and provides
  frames.
- **Orchestrator** — any other client of the same control API. Starts no sessions; launches apps.

### Idle video ("no signal")

The service streams **continuously**, showing a fixed "no signal" texture when no app is providing
frames. When an app connects and provides an image, the service switches source to the app's texture.

This is the important design decision: the ALVR stream stays up across app restarts. Without it, every
app launch/exit renegotiates the whole ALVR connection (handshake, resolution, decoder config) —
seconds of black screen per transition. With it, the headset never disconnects and app switching is
just a texture-source swap. The idle texture is static, so it costs one image and a low-rate encode.

Confirmed feasible by Spike 2: the service can create and own the texture before any app exists, and a
later-starting app can still attach to it by name.

### IPC: control channel

**Named pipe**, service is the listener:

- Service creates `\\.\pipe\Global\alvr_service` with a security descriptor granting
  `ALL_APPLICATION_PACKAGES` (`S-1-15-2-1`), so AppContainer (UWP) apps can open it. SDDL
  `D:(A;;GA;;;S-1-15-2-1)(A;;GA;;;AU)(A;;GA;;;SY)`.
  Verified (Spike 1) to work from a **non-elevated, unpackaged** process: for pipes, `Global\` is part
  of the name in the single machine-wide pipe namespace, not a session-namespace prefix, so
  `SeCreateGlobalPrivilege` is **not** required.
- `PIPE_UNLIMITED_INSTANCES` + `PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE`. Message mode gives framed
  messages, so a read returns exactly one message — no hand-rolled length prefixing.
- Accept loop: keep an idle instance outstanding, `ConnectNamedPipe`, hand off, create the next
  instance. If no idle instance exists, a client's `CreateFile` fails `ERROR_PIPE_BUSY` (pipes do not
  queue), so always keep one pending.
- Each app gets its own private bidirectional channel — no multiplexing, no port allocation.
- **Either side may start first**: the app retries `CreateFile` / uses `WaitNamedPipe` until the pipe
  exists; the service replenishes instances as apps come and go.
- Free liveness signal: reads fail `ERROR_BROKEN_PIPE` when an app dies, which releases its claim.

Why not loopback TCP: the UWP loopback mechanism (`windows.loopbackAccessRules`) is keyed by
`PackageFamilyName`, and **our service has no package identity**, so it cannot be named there. The only
fallback would be machine-wide `checknetisolation loopbackexempt`, requiring admin and manual upkeep.
Pipes need one ACL'd object instead.

### IPC: frame path

**Service creates the shared texture, app opens it** — the reverse of the reference implementation
(see below), chosen deliberately:

- The idle texture must exist *before* any app does, which only works if the service owns it.
- The service knows the headset's negotiated resolution, so it should define the texture size. This
  matches OpenXR anyway, where `xrEnumerateViewConfigurationViews` returns runtime-chosen sizes.
- An already-running app *can* be granted access, because named shared resources are ACL-checked at
  open time, not granted to a specific process at creation. So a later-starting app still works.

Mechanics:
- Create: `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`, then
  `IDXGIResource1::CreateSharedHandle(&security_attributes, access, name, &handle)`. Retain the handle
  only to keep the name alive. Pass real `SECURITY_ATTRIBUTES` (the reference passes `nullptr`).
- Open: `ID3D11Device1::OpenSharedResourceByName(name, access, ...)`.
- **Check the HRESULT and match access flags.** The reference opens `Read|Write` a texture created
  `SharedRead`-only and gets away with it solely because it ignores the HRESULT. Do not copy that.
- **D3D11 only.** D3D12/Vulkan have no named-resource equivalent and would need real handle transport
  (`DuplicateHandle` + `PROCESS_DUP_HANDLE`). Keeping a D3D11 interop surface avoids all of it.

Sync: **two-key keyed-mutex ping-pong** over a single texture. One side acquires/releases `{0,1}`, the
other `{1,0}`, so each releases with the key the peer waits on and access strictly alternates.
Consumer uses a short timeout and reuses its previous local copy on failure — graceful degradation
instead of a frame hitch — and blits immediately to a local texture to minimise lock hold time.

Robustness idioms to copy: wait jointly on the peer's **process handle and** a shutdown event; treat a
keyed-mutex acquire failure as "reopen the shared resource", not fatal.

### Device registry

Per device: stable id (the mDNS `device_id` hostname), display name, connection state, negotiated
resolution/refresh, **and availability**.

`ConnectionState` (`Disconnected → Connecting → Connected → Streaming → Disconnecting`) **cannot**
express availability: a device claimed by an OpenXR app is also `Streaming`. Claim ownership is
orthogonal and needs its own field:

```
availability: Free | ClaimedBy { process_id, claimant }
```

Claimed on session initiation; released on teardown, on claimant exit (process handle + broken pipe),
or on client disconnect. So a crashed app cannot orphan a device.

Exposed both over the pipe (for runtimes) and over the existing axum web server for orchestrators.
Note `GET /api/session` currently returns **nothing** — it takes no state, returns `()`, and merely
broadcasts a `Session` event, so there is no request/response way to read state today. A real
snapshot endpoint is required, otherwise "start immediately if a device is free" races the
subscription. Subscribe-then-snapshot-then-reconcile.

## Reference implementation

`C:\cae\dev\asgard-plugins\Holomaps.Plugins.Native\Holomaps.Plugins.SimulatedReality.Adapter`
plus its `.Client` sibling and `C:\cae\dev\asgard\Holomaps.Common.Shared`.

It solves UWP↔desktop IPC + texture sharing with **no ACL code at all**, by ownership direction:

> The sandboxed side **creates** named objects with **bare names** (landing implicitly in its own
> AppContainer namespace, where it has full rights); the full-trust side **opens** them, resolving the
> path via `GetAppContainerNamedObjectPath(token)` from the peer's **PID**.

Key sources:
- `Holomaps.Common.Shared/NamedObjects.cpp:10-28` — `TryGetAppContainerNamedObjectRoot(pid)`, gated
  `#if WINAPI_FAMILY_PARTITION(WINAPI_PARTITION_DESKTOP)`.
- `Texture2D.cpp:325-340` — named `CreateSharedHandle` with NT handle + keyed mutex.
- `Texture2D.cpp:867-875` — `OpenSharedResourceByName`.
- `GraphicsMutex.cpp:34-53` — keyed-mutex RAII lock with acquire/release key pair.
- `SimulatedRealityRemoteSession.cpp:108-114` — URI activation carrying the PID.
- `main.cpp:17-89` — fail-fast startup chain, joint wait on process + event.

**Where our situation differs, and why the pattern needs adapting:**

1. **No package identity.** The reference adapter is a *packaged* full-trust app (`runFullTrust` +
   `Windows.FullTrustApplication`). Our service is a plain unpackaged desktop process. Consequences:
   - URI activation (`ridge-sr-driver://connect?pid=`) is unavailable — it needs a
     `windows.protocol` extension, hence a package. Our service is long-running anyway, so it does not
     need launching; but it also cannot be *named* in `loopbackAccessRules`.
   - We therefore need a well-known, ACL'd `Global\` rendezvous object instead of URI activation. That
     is the one place ACL code is unavoidable.
2. **Reversed texture ownership**, for the idle-video reason above. This trades "no ACL code" for one
   SDDL string — near-zero marginal cost, since the bootstrap pipe needs one regardless.
3. Because the service owns the texture, the **PID handshake is not needed for the frame path**. It
   remains useful only for liveness (holding the process handle), and the pipe's `ERROR_BROKEN_PIPE`
   already covers most of that.

## Phase 1 status — DONE

Per-client streaming in `server_core`, verified with two concurrent devices at **70 fps /
1.20 Mbit/s each** over **both UDP and TCP**, one force-dropped and reconnected while the other
streamed unaffected. See git log on this branch. Highlights:

- `ClientSession` per-client state; teardown scoped to the disconnecting client. Fixed a real bug
  where any single disconnect nulled video/haptics senders for **every** client.
- `ServerCoreEvent` variants carry `client_id`.
- `ServerCoreConfig { restart_on_settings_change }` gates the SteamVR driver-restart path (default
  `true`; a multi-headset backend sets `false`, else a restart tears down every other client).
- `send_video_nal_to_client` / `set_video_config_nals_for_client`.
- Per-client stream ports both ends (`NegotiatedStreamingConfigExt` / `StreamReady`), falling back to
  the configured port so single-client behaviour is unchanged.
- Harness: `alvr_mock_device` (headless client) and `alvr_mock_server` (real `ServerCoreContext`,
  synthetic video, no SteamVR). The service is `alvr_mock_server` promoted to production.

Two earlier assumptions corrected by measurement: the `SESSION_MANAGER` write lock never blocked
concurrent streaming (`wait_rwlock` releases it), and `SO_REUSEADDR` does **not** make UDP
multi-client on Windows (binds and connects, never receives).

---

## Spikes — RESULTS

Each existed because a failure would change the architecture. Spike code lives outside the repo
(scratchpad); the findings are what matter.

### Spike 1 — `Global\` named pipe with AppContainer ACL — **PASS**

A **non-elevated, unpackaged** process created `\\.\pipe\Global\alvr_spike1` with SDDL
`D:(A;;GA;;;S-1-15-2-1)(A;;GA;;;AU)(A;;GA;;;SY)`; a separate process connected and the message was
received.

**Key finding that removes the elevation risk entirely:** for **named pipes**, `Global\` is *not* a
session-namespace prefix as it is for events and shared memory — it is simply part of the name inside
the single, machine-wide pipe namespace. Enumerating `\\.\pipe\` shows the object literally named
`\\.\pipe\Global\alvr_spike1`. Consequently **`SeCreateGlobalPrivilege` is not required**; the token
reported it as "present but NOT enabled" and creation still succeeded. So the service does **not** need
to be elevated or a Windows service.

**AppContainer verified — see Spike 5.** A client running inside a real AppContainer opened the pipe
and delivered its message.

### Spike 2 — service-created named shared texture, app-opened — **PASS**

The design's reversed ownership direction works, end to end:

- Service created a texture with `SHARED_NTHANDLE | SHARED_KEYEDMUTEX` and called
  `CreateSharedHandle` with **both a name and real `SECURITY_ATTRIBUTES`** (`S-1-15-2-1`). Accepted.
- A **separate, later-starting** process opened it via `OpenSharedResourceByName` (HRESULT checked).
- Keyed-mutex ping-pong (`{0,1}` service / `{1,0}` app) succeeded on the first attempt.
- The service observed the pixel change `0x00000000 → 0xFFFFFFFF`, proving real shared memory rather
  than two independent textures.

This confirms the service can own the idle "no signal" texture before any app exists, and that apps
can attach later. **Idle video (a) is viable.**

### Spike 5 — both channels from inside a real AppContainer — **PASS**

This is the one that de-risks UWP, and it was run with a negative control so the result means
something.

**Method.** The VS UWP C++ workload is not installed, and installing it is a large machine-wide
change. But a UWP app's security boundary *is* an AppContainer, so the harness tests the boundary
directly: `CreateAppContainerProfile` (no extra capabilities — the strictest case) plus
`STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` to launch a child inside it. The child
confirms `TokenIsAppContainer = 1`, so it is subject to exactly the access checks a UWP app faces.
Harness: `tools/appcontainer-run/`.

**Results, client running inside the AppContainer:**

| Test | Result |
|---|---|
| Open `\\.\pipe\Global\alvr_spike1` **with** `S-1-15-2-1` ACL | **PASS** — connected, message received |
| Same pipe **without** the ACL (negative control) | **Access is denied** (`0x80070005`) |
| `OpenSharedResourceByName` on the ACL'd shared texture | **PASS** — opened |
| Keyed-mutex acquire/release + pixel write from the sandbox | **PASS** — service observed `0x00000000 → 0xFFFFFFFF` |

The negative control is the important part: without the ACL the AppContainer is **denied**, so the
`S-1-15-2-1` grant is genuinely what permits access, not some permissive default.

Incidental but useful: the sandboxed child successfully created a **D3D11 hardware device** and drove
a keyed mutex, so GPU access from an AppContainer is not itself a problem.

**Caveat.** A packaged UWP app also needs its executable and working set reachable; here the harness
had to `icacls /grant *S-1-15-2-1:(RX)` the directory holding the test binaries. A real UWP app gets
this from its package layout, but the ALVR **runtime DLL** will be loaded from a path the app can
already read, so nothing extra is required for our case. Only the two named objects need explicit ACLs.

### Spike 3 — `GetAppContainerNamedObjectPath` from unpackaged — **NOT NEEDED**

Was conditional on Spike 2 failing and forcing the reference's ownership direction. Spike 2 passed, so
the PID→namespace-root resolution is not on the critical path. (It may still be wanted later for
app-created objects such as a shutdown event; not required for control or frames.)

### Spike 4 — NVENC concurrent sessions — **PASS (not a constraint)**

Target GPU is an **NVIDIA GeForce RTX 4080**, driver 610.62. Since the 530-series Windows drivers the
consumer NVENC session cap was raised from 3 to **8** concurrent sessions, and Ada Lovelace carries
dual NVENC encoders. Encode capacity is therefore not a practical limit for a handful of headsets.
(A second adapter, "Meta Virtual Monitor", is also present — a virtual display, not an encoder.)

### Deferred (not blocking)

- **AppContainer open of the pipe + texture** — verify with the UWP test app in Phase 3.
- OpenXR runtime conformance surface — large but well-specified, no architectural unknown.
- D3D12/Vulkan app support — needs handle duplication; out of scope while D3D11 interop suffices.

### Consequences for the design

1. No elevation, no Windows service, no package identity needed for the service.
2. Service owns both the control pipe **and** the shared texture; only the one SDDL string is required
   for each. ALVR has no ACL code today, so this is new but small.
3. Idle "no signal" video is confirmed feasible, so the stream can stay up across app restarts.
4. The PID handshake is **not** needed for control or frames — it remains useful only for liveness,
   which `ERROR_BROKEN_PIPE` plus a process handle already covers.

---

## Rejected alternatives

- **Fork SteamVR** — impossible; `vrserver`/`vrcompositor` are closed source. The open
  `ValveSoftware/openvr` repo is only headers plus the `openvr_api` IPC shim, which this repo already
  links. Every blocker lives on the closed side.
- **Multiple SteamVR instances** — one OpenVR runtime and one `vrserver` per machine
  (`steamvr_root_dir()` takes `.first()`); ALVR's launcher early-returns if `vrserver` is running and
  de-registers other ALVR drivers citing socket conflict. Even N `vrserver`s each bind one HMD.
- **Monado as a base** — complete open runtime, but Linux-first (Vulkan/`VK_KHR_display`, Wayland/X11);
  Windows is a hard requirement here, and adopting it would mean rewriting the working D3D11 encode
  stack. Keep as a *reference* for session/frame/space/action semantics.
- **Named pipes without the `Global\` ACL** — an AppContainer cannot open an arbitrary desktop pipe.
- **AppService / COM broker** — both need package identity on the service side.
- **"ALVR already has an OpenXR runtime"** — it does not. Verified: no `XR_RUNTIME_JSON`, no runtime
  manifest, no `ActiveRuntime` anywhere. On PC, apps reach ALVR through *SteamVR's* OpenXR runtime,
  backed by the same single `vrserver`, so it inherits every constraint above. `alvr/client_openxr` is
  an OpenXR *application* on Android.

## Reuse notes

- **Encode stack ports largely intact.** `FrameRender`, `FFR`, colour correction, the `.cso` shaders
  and all four encoders in `alvr/server_openvr/cpp/platform/win32/` operate on D3D11 textures and are
  not coupled to OpenVR — only `OvrDirectModeComponent` is. Replace it as the texture source and keep
  `CEncoder` and below. Lift the encoder out of `Hmd` into a standalone owner.
- ALVR already does cross-process shared-texture interop (`d3drender.cpp:218-236`), but with legacy
  `D3D11_RESOURCE_MISC_SHARED`. The NT-handle + keyed-mutex flags needed here are exactly the ones
  commented out at `OvrDirectModeComponent.cpp:52-54`.
- `alvr/vulkan_layer` and `alvr/vrcompositor_wrapper` are Linux-only and irrelevant on this path.
