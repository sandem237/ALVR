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

### IPC: control channel

**Named pipe**, service is the listener:

- Service creates `\\.\pipe\alvr_service` in the **`Global\`** namespace with a security descriptor
  granting `ALL_APPLICATION_PACKAGES` (`S-1-15-2-1`), so AppContainer (UWP) apps can open it.
  SDDL roughly `D:(A;;GA;;;S-1-15-2-1)(A;;GA;;;AU)`.
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

## Spikes required before implementation

Each is small and isolated. They exist because a failure changes the architecture, so they must be
measured rather than argued.

### Spike 1 — `Global\` named pipe openable from an AppContainer *(highest risk)*

**Question.** Can an **unpackaged** desktop process create `\\.\pipe\alvr_service` in `Global\` with an
`S-1-15-2-1` ACL that a UWP AppContainer app can then open?

**Sub-question that could force a design change.** Creating in `Global\` normally needs
`SE_CREATE_GLOBAL_NAME`, which SYSTEM/elevated processes have but a plain user-session process may
not. If it is unavailable, the service must run elevated or as a Windows service.

**Pass:** UWP app opens the pipe, writes a message, service reads it.
**If it fails:** fall back to running the service as a Windows service (SYSTEM), or reconsider
machine-wide `checknetisolation` + TCP.

### Spike 2 — ACL'd named shared texture, service-created, app-opened

**Question.** Can the unpackaged service create a named shared texture
(`SHARED_NTHANDLE | SHARED_KEYEDMUTEX`, `SECURITY_ATTRIBUTES` granting `S-1-15-2-1`) that a UWP app
opens by name with `OpenSharedResourceByName` **while the service is already running**, and can both
sides ping-pong the keyed mutex?

**Pass:** app writes a colour into the texture, service reads it back and observes the change.
**If it fails:** invert to the reference's direction (app creates, service opens via PID), and drop
idle video to (b) semantics — the service cannot own a texture before an app exists.

### Spike 3 — `GetAppContainerNamedObjectPath` from an unpackaged process

**Question.** Only needed if Spike 2 forces the inversion: can an unpackaged process resolve an
AppContainer's namespace root and open a bare-named object in it, or does that need privilege the
reference happened to get via its package?

**Pass:** unpackaged process resolves the root for a UWP PID and opens an object the UWP app created.

### Spike 4 — NVENC concurrent session limit

**Question.** How many simultaneous encode sessions does the target GPU allow? Consumer GeForce
drivers historically cap these, which bounds how many headsets can be served regardless of software.

**Pass:** measure the actual cap on the target hardware.

### Deferred (not blocking)

- OpenXR runtime conformance surface — large but well-specified, no architectural unknown.
- D3D12/Vulkan app support — needs handle duplication; out of scope while D3D11 interop suffices.

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
