# ALVR client emulator — handover

**Branch:** `emulator` · **Last commit:** `cee3e40e Improved UI layout`
**State:** Working. Connects to a real ALVR server, streams, decodes and displays video, emulates
controllers and hand tracking. No freezing or corruption observed. Verified against SteamVR Home on
Windows with an RTX 5090.

Read [`README.md`](README.md) for how to build, run and use it. This file is the "why is it like
this, and what next" companion — the reasoning and the hard-won failures that the code comments alone
do not convey.

---

## What it is

A desktop application that pretends to be a headset. It talks to an ALVR server over the real
protocol, sends head tracking from a mouse-and-keyboard camera, decodes the video stream, and exposes
an HTTP API so the emulated headset can be inspected and driven programmatically.

Built to develop ALVR *and* applications on top of it without hardware, and to eventually run several
emulated headsets at once.

It is **not** an Android emulator and involves no OpenXR runtime. It links `alvr_client_core`
directly, which is the same crate the real client is built on, so the protocol path is genuine.

## Scope decisions already made

These were settled deliberately; re-opening them needs a reason.

| Decision | Why |
|---|---|
| Own crate, `client_mock` untouched | `client_mock` stays a low-dependency smoke test; upstream still commits to it |
| Direct `alvr_client_core` client, no OpenXR | `client_core` has no OpenXR dependency; that lives only in `client_openxr` |
| No timewarp / reprojection | Deletes the subtlest part of the client render path; not needed for a debug tool |
| eframe/egui + wgpu | Matches the dashboard, launcher and `client_mock`; one dependency gives window, UI and 3D |
| CPU video decode via ffmpeg | Measured 38–45× realtime, ~0.3 core per stream, and **3× faster than d3d11va** for one stream |
| Unlit glTF rendering | Assumes baked lighting, as a photogrammetry capture would have |
| Backward-compatible ALVR changes only | Far likelier to be accepted upstream; real clients keep working unchanged |

## Architecture

```
main.rs           eframe app: toolbar, input, paint callback, services API requests
camera.rs         first person camera, per-eye view and projection matrices
client.rs         ClientCoreContext lifecycle, tracking thread, decoder wiring, statistics,
                  controller motion / button / interaction-profile / hand skeleton sync
controllers.rs    emulated controller state, profiles, controllers.json settings file
hands.rs          hand articulation model, 26-joint skeleton synthesis, poses and timed
                  gestures, hands.json settings file
overlay.rs        device icons over the view and the 6DoF movement panel, shared by both kinds
controller_ui.rs  controller toolbar section, corner input panels
hand_ui.rs        hand toolbar section, corner pose/gesture panels, icons drawn from the poses
decoder/          VideoDecoder trait + DecodedFrame enum; software.rs is the ffmpeg CPU implementation
video.rs          decoded frames to GPU, YUV to RGB in video.wgsl, per-eye region sampling
render.rs         glTF scene pipeline, controller and skinned hand models, views and capture
scene.rs          glTF loading into a plain geometry container
skinned.rs        skinned glTF loading, and retargeting any hand rig onto the emulated joints
api.rs            HTTP control server
```

## Controller emulation notes

Added after the freeze work; see README.md for usage. Implementation points worth knowing:

- **Poses ride the existing tracking channel.** Controllers are extra `device_motions` entries in
  the same `TrackingData` as the head, at the same 3× refresh cadence. The head and the controllers
  are published by the UI thread as **one atomic snapshot** (`TrackedState`) — with separate slots,
  packets sometimes paired a fresh head pose with last frame's controllers, and head-relative
  controllers visibly flickered against the view while the camera moved.
- **All velocities are zero, deliberately, and the driver's own asymmetry is why.** `Hmd.cpp`
  submits a pose with no velocity and no `poseTimeOffset` (both left at the `DriverPose_t{}`
  zeroes), so **SteamVR can never extrapolate the head** — it renders with whatever pose ALVR
  submitted last. `Controller.cpp` submits velocity *and* `poseTimeOffset = steamvr_pipeline_frames
  × frame_interval`, so SteamVR *does* extrapolate the controllers, by an amount that depends on
  its own fluctuating time-to-photon estimate. Any nonzero controller velocity therefore makes them
  swim against a head that is physically unable to follow, which is exactly what the first
  experiment saw. Velocities on both made the view itself jitter, because the server's
  `motion.predict` then rides a motion-to-photon average against velocities derived by
  differencing a noisy signal. With every velocity zero, `DeviceMotion::predict` is a no-op
  everywhere and the rig is exactly the sent poses — measured perfectly rigid at the OpenVR API
  level (0.0 mm head-relative wander, with and without prediction). Reintroducing velocities means
  fixing the driver's head path first.
- **Tracking packets are interpolated, not resent.** The UI publishes poses at frame rate while the
  tracking thread sends at 3× the stream refresh rate, so forwarding the latest pose verbatim put a
  stair-step signal on the wire. The tracking thread reads a *history* of published states back at
  a lag (`TrackingWindow` in `client.rs`), making the signal continuous like a real headset's. The
  history matters: the UI frame interval measures 8.3 ms with 4.4 ms of mean deviation, so with
  only the last two samples the evaluation point falls outside them constantly and the
  reconstruction clamps — freeze, then jump, at the UI frame rate. The lag is sized from the
  measured jitter (`mean + 2 × deviation`), not fixed, because every millisecond of it is latency.
- **The controllers were never the thing that jittered.** They are attached to the head at a fixed
  head-relative pose, so they belong at a fixed *screen* position. Everything else in the frame
  moves with the head pose the frame was rendered from. So they are the one stationary reference in
  the picture, and every timing error in the head pose trajectory shows up as the scene sliding
  against them. Chasing the controllers was chasing the ruler, not the thing being measured. See
  "the judder" below for what it actually was.
- **The teal ghost controllers in the video lag, and that is SteamVR's compositing, not tracking.**
  Turn on the emulator's own controller models (`Display`) and turn the camera: two renderings
  separate. The solid ones are the emulator's, drawn from the displayed frame's own eye poses, and
  they stay locked to the pixel. The translucent teal ones are SteamVR's, and they swing away. Hide
  the emulator's models and only the teal ones remain, which is how to tell them apart.

  The displacement is **proportional to turn rate** — clearly separated at 120 °/s, a thin fringe at
  30 °/s — so it is a fixed time offset of roughly 20 ms, about 1.5 frames, not jitter. SteamVR
  draws those controllers into a *separate compositor layer*, and `FrameRender.cpp` composites extra
  layers by reprojecting them onto the scene layer with `HmdMatrix_AsDxMatOrientOnly` — orientation
  only, onto a quad at 700 m (lines 605 and 702). That cannot correct a controller-versus-head
  timing difference inside the layer, so the offset survives into the encoded frame. Nothing the
  client sends changes it.

  Getting the *application* to draw the controllers instead is the real fix; the emulator's own
  models are the accurate reference in the meantime, which is what the `Display` toggle is for.
  Related: `start_pitch_degrees` in `controllers.json` exists because resting the laser on SteamVR's
  status panel hands the system UI input focus, which is one way to end up with the ghosts.
- **Overlays follow the displayed video frame, not the live camera.** Measured with a pyopenvr
  background app while walking: SteamVR's device poses are perfectly rigid relative to each other
  (0.0 mm controller-vs-head wobble, even sampled with 42 ms prediction) — the tracking data is
  clean end to end. What looked like "SteamVR controllers jittering along the walk direction" was
  video frame *timing* wobble: the whole frame (scene and controllers together) lurches a little
  against real time, and a live-camera-anchored overlay is a stationary reference that makes it
  visible on the controllers. In video mode the icons and models are therefore drawn with the
  displayed frame's own eye poses, which `report_compositor_start` returns (the same data a real
  client uses for reprojection) — overlay and video move in lockstep and cannot jitter relative to
  each other. Note this makes the overlay only as good as `report_compositor_start`, which is how
  the microsecond timestamp truncation in "the judder" below became so visible: the call was
  returning the *previous* frame's poses nine times out of ten.
- **Buttons are change-driven.** `EmulatedClient::sync_buttons` mirrors what was last sent and
  transmits diffs, like the real client; releases are sent for inputs that disappear from the
  desired set. Derived inputs (touch from press, click from full pull) are computed in
  `ControllerState::effective_entries`, filtered by the active profile.
- **One interaction profile announcement for both hands.** The server rebuilds its button mapping
  manager from every `ActiveInteractionProfile` packet and keeps only the last, so the emulator
  sends the union of both hands' input ids as one announcement rather than one per hand.
- **Profiles are generated from `alvr_common::CONTROLLER_PROFILE_INFO`** into `controllers.json` on
  first run, which keeps the emulated input sets identical to what ALVR accepts from real hardware.
- **egui Areas and DPI.** The corner panels are positioned with explicit coordinates from
  `ctx.content_rect()`; `Area::anchor`/`pivot` place by the area's remembered size, which is wrong
  on the first frame. When verifying the UI with `PrintWindow` screenshots, the capturing process
  must be DPI-aware or the capture is silently cropped to the top-left corner — this cost a long
  false hunt for a rendering bug that did not exist.

Verified end-to-end against SteamVR Home: SteamVR renders the emulated Quest controllers at the
emulated poses, and button presses arrive server-side (watched via the `/api/events` WebSocket with
`log_button_presses` enabled) including the derived touch inputs and timed click releases.

## Hand emulation notes

Added on top of the controller work; see README.md for usage. What is worth knowing:

- **A hand replaces its controller on the wire, it does not accompany it.** An enabled hand sends
  `hand_skeletons[i]` and *no* `HAND_*_ID` device motion, which is what `client_openxr` does for a
  freely tracked hand. The absence is load-bearing at both ends: `server_core`'s `tracking_loop`
  runs `trigger_hand_gesture_actions` only for a hand whose device motion is missing, and
  `Controller.cpp` derives the device pose from `handSkeleton->jointPositions[0]` only when
  `controllerMotion` is null. Sending both is the *multimodal* case — a hand holding a controller —
  which is not what the toggles mean. Hence the per-side exclusion in the UI and in the API.
- **The pose model is seven numbers, not 26 rotations**, and the reason is not only usability: a
  joint-angle representation makes most of its state space anatomically impossible, so an API
  caller sweeping values would mostly produce non-hands. See `hands.rs`.
- **The thumb's base frame is two directions, not three angles.** This was the one piece of the
  geometry that had to be rebuilt. Opposition is a rotation about no axis the palm has: the
  metacarpal swings across in front of the palm *and* the digit rolls, so that flexing it
  afterwards carries the tip towards the fingers rather than towards the palm. Expressed as
  `fan × swing × roll` Euler angles the two interact, and the measured result was a thumb whose tip
  moved *further* from the index finger as opposition increased — 45 mm to 96 mm — so a pinch was
  unreachable at any curl. Interpolating a bone direction and a "where flexing takes the tip"
  direction, and building an orthonormal frame from the pair, gives 11 mm. The tests assert it.
- **The unit tests are calibrated against the server, not against taste.** `hand_gestures.rs`
  measures real distances between fingertips, adding fixed finger radii to the configured
  thresholds. The gesture tests use those same numbers, so a change to the anatomy that would stop
  Pinch registering as a pinch fails the build rather than being discovered in SteamVR.
- **Rig retargeting assumes nothing about the model.** The CC0 model that ships runs bones along
  +Y (Blender armature default) where OpenXR runs along -Z, and its bind pose is a relaxed hand,
  not a flat one. Both skeletons are reduced to a geometric frame derived only from joint positions
  (`hands::canonical_frames`), and the constant offset between a rig's own bone frame and that one
  is measured from the bind pose. Two consequences worth keeping: a naive `inv(rest) * bind`
  correction would have baked the bind pose's ~20° of curl into the flat pose, and joint
  *positions* are taken from the transmitted skeleton rather than from the model's bone lengths, so
  the drawn hand is the hand being sent.
- **Joint matrices go in a uniform buffer, not a storage buffer.** Read-only storage in the vertex
  stage is a downlevel capability that not every backend wgpu may pick offers; a fixed
  `array<mat4x4<f32>, 64>` uniform is guaranteed everywhere and a hand needs 26. Note `glam::Mat4`
  is not `Pod` in this workspace, so the block is packed column by column.
- **Hands are shaded, the scene is not.** The scene assumes baked lighting; a hand model is a bare
  mesh, and drawing it unlit produced a flat silhouette with no readable curl. One key light plus a
  hemispheric ambient, in world space, since the skinning matrices already carry the hand's
  transform.
- **The icons and the movement panel are one implementation for both device kinds** (`overlay.rs`),
  with four slots. The design called for hands to be posed identically to controllers, and two
  copies would have drifted.

### Poses against gestures

A **pose** is how a hand is held — seven numbers, held indefinitely. A **gesture** is what a hand
does — a keyframed path through those poses over a fixed duration. The split arrived after the
first real click: holding a pinch by mouse and releasing it at the right moment is fiddly and not
repeatable, and no amount of tuning a static pose fixes that, because the thing being emulated is a
movement. `Point` at phase 0, `Pinch` at 0.5, `Point` at 1 over half a second is a click, and the
button performs exactly that every time.

Worth keeping in mind when reading the code: the older naming had "gesture" meaning the static
thing, so `hands.json` written before the split will not parse. That case is handled by moving the
file to `hands.json.old` and writing fresh defaults, which is better than the alternative of
falling back to built-ins the user cannot then edit. The API renamed the pose selector to
`/articulation` and gave `/gesture` to the new concept.

### Found during the first real test

Four of these came out of the first session actually looking at the hands, and three were the
emulator's fault.

- **The palm convention is the specification's, and that was worth checking rather than
  asserting.** The OpenXR spec (12.30, conventions of hand joints) puts the palm joint "at the
  center of the middle finger's metacarpal bone", with "+Z parallel to the middle finger's
  metacarpal bone, pointing away from the finger tips" and "+Y ... perpendicular to palm surface
  and pointing towards the back of the hand". The first implementation was right in spirit but not
  exact — the middle metacarpal sat 3.5° off the palm's axis and the wrist 15° off its own — so the
  constants now place the middle metacarpal *on* the palm's Z axis and the wrist directly behind
  it, which is asserted by `palm_and_wrist_match_the_spec`. Everything downstream is tuned against
  the real convention, so being close is not the same as being right.
- **The SteamVR ray is 45° off the fingers, and that is correct.** The driver's device pose is
  `palm * left_hand_tracking_rotation_offset`, whose default `[0, -45, -90]` exists to present a
  hand-tracked hand as a *held controller*; the position offset likewise puts the device 13 cm
  ahead of the palm. Deriving what those offsets imply about the palm frame — the offset's own
  columns say it — is how this was settled without a headset to compare against. Only the middle
  value steers the ray: about `-10` aims along the emulator's index finger and `0` along the
  middle. Do not "fix" this in the emulator; it would then differ from real hardware.
- **Nothing can be clicked until `hand_tracking_interaction` is switched on**, and it is off by
  default. Gestures are the *only* source of buttons for a hand, and `SetButton` routes them to
  both the controller and the hand-tracker device, so once it is on the built-in Pinch is a trigger
  pull. Nothing to change in the emulator.
- **Do not pin the model's joints to the transmitted positions.** The first implementation did,
  reasoning that the drawn hand should be exactly the hand being sent. Measured against the model
  that ships, the emulator's phalanges are 0.84x to 1.16x of the model's and the ratio *alternates*
  along each finger, so every segment was squashed or stretched in turn and a perfectly straight
  finger rendered with a visible S-curve that read as bending backwards. Rotations now come from
  the emulator and positions from the model's own bind pose, anchored at the root: a few
  millimetres of divergence at the fingertip, and no distortion. The angles, which are what a pose
  is, were exact either way.
- **The ray's origin is a second, separate setting.** Aligning the *direction* still left the ray
  leaving from the middle of the palm, because that is where the palm joint is — the centre of the
  middle metacarpal, by definition. `left_hand_tracking_position_offset` moves it; `[0, 0.016,
  -0.036]` puts it on the index knuckle. Deliberately the knuckle and not the fingertip: the offset
  is a constant applied to the palm, so an origin fitted to the fingertip in one pose detaches from
  it in every other, while the knuckle hardly moves as the fingers curl.
- **A pinch cannot click while `steamvr_input_2_0` is on, and it is a driver bug.** That option
  routes hands to *separate hand-tracking devices*, which `props.rs` gives SteamVR's
  `svl_hand_interaction_augmented` input profile. That profile's inputs are `index_pinch`, `grip`,
  `system`, `index_point` and the skeleton — there is **no `/input/trigger`** — while
  `register_buttons` deliberately maps the tracker id back to the hand id and creates the
  *emulated controller's* components on it, and `index_pinch` appears nowhere in ALVR's source.
  Applications bind against the advertised profile, so they wait on inputs nothing ever sets: pose
  and skeleton work, no button ever does.

  **`steamvr_input_2_0 = false` makes clicking work and is still the wrong trade** — tried, and
  reverted. The hands then ride on the ordinary controller device and bring its whole presentation
  with them: controller icons in the SteamVR status window, controller render models drawn under
  the hands, a pointer taken from the oculus_touch tip pose instead of the palm (undoing the
  finger alignment), and the skeleton requested in its "with controller" range, which curls the
  fingers around a controller that is not there. The option is `steamvr-restart` flagged, so each
  experiment costs a restart. The fix belongs in the driver: drive the profile's own inputs on
  those devices, or stop advertising a profile nothing sets.

  Worth recording how this was nearly misdiagnosed. The server log shows `Received button not
  mapped: /user/hand/left/input/trigger/click`, because `get_click_bind_for_gesture` emits click
  ids that are not in `HAND_GESTURE_BUTTON_SET`. That looks like the answer and is not:
  `map_button_pair_automatic` derives a destination click from a source *value* through a
  threshold whenever the source has no click of its own, so the click still arrives. Reading the
  mapping code rather than trusting the log is what found the real cause one layer down.
- **Pinch is the right gesture to model.** It is the select gesture on Quest and in OpenXR's
  `XR_EXT_hand_interaction`, and it is what ALVR binds to the trigger. The air tap that flexes the
  index finger down and up is HoloLens', and nothing in this path uses it.
- **Icons are fitted to their cell, not just scaled.** A fixed scale keeps a fist visibly smaller
  than an open hand, which is worth having, but anchoring on the palm ran an extended index finger
  off the top of its cell. Each icon is now centred on the pose's own projected bounds and shrunk
  only if it would otherwise overflow.
- **A model can be a fingerless glove**, and drawing it in one colour makes its cuff ridge read as
  a defect. The rim turned out to sit exactly at the second knuckle — found by profiling the bind
  pose's cross-sectional radius along each finger — so `glove_color` tints everything up to there
  separately, blended across the skin weights so the seam follows the modelled ridge.

Verified against a running SteamVR with the emulator streaming, read back through pyopenvr:
enabling both hands invalidates the emulated controller devices and brings up two hand-tracking
devices (`svl_hand_interaction_augmented`), posed by the emulator; moving a hand 20 cm left, 25 cm
up and 10 cm forward through the API moves its device by exactly that; switching a side back to a
controller invalidates the hand device and revalidates the controller. The constant offset between
the palm we send and the device pose SteamVR reports is the server's own
`left_hand_tracking_position_offset`, which real hand tracking gets too.

The decoder is behind a trait with a `DecoderKind::preferred()` selector so a platform-specific
zero-copy implementation can be added later without touching the renderer. `DecodedFrame` is an enum
for the same reason: a GPU variant would carry a texture instead of CPU planes.

wgpu is asked to prefer **DirectX on Windows** (`preferred_wgpu_setup` in `main.rs`) purely so a
future D3D11 decoder shares the DXGI family with the renderer, needing a shared handle rather than
cross-API interop. Note `Backends` is a *filter*, not a priority order — preferring a backend
requires the `native_adapter_selector` callback.

## ALVR changes, and why each exists

All additive, all keeping existing clients working against a new server. An old *server* cannot reach
a client that moved off the well-known ports, which is acceptable because both sides of an emulator
setup are under our control.

**`client_core/src/sockets.rs` — announcer shutdown.** `AnnouncerSocket` created an mdns-sd
`ServiceDaemon` and never shut it down, leaving a thread parked in a blocking receive that
`ClientCoreContext::drop` then joined forever. Closing the window hung the process indefinitely
(confirmed by native stack dump; not slow, never completes). Now shuts down on drop.

**Client control port** (`sockets`, `client_core`, `server_core`). Only one process per machine can
bind `CONTROL_PORT`. A client that cannot get it falls back to an OS-assigned port and advertises it
in a new `control_port` mDNS TXT entry; a client that advertises nothing is reached on the well-known
port exactly as before.

**Client stream port** (`sockets`, `client_core`, `packets`). Same problem for UDP, but the client
cannot simply take the port: it dials the *server* by port number, so the server's port is fixed by
the protocol while the client's is not. The client therefore yields when the server is on the same
machine, binds an OS-assigned port, and reports it via a new
`ClientControlPacket::StreamReadyOnPort`. That is a **new enum variant** rather than a field on
`StreamReady`, because these packets are bincode-encoded by variant index — appending leaves existing
indices untouched, while changing `StreamReady` would reinterpret every old client's packet.

**`.cargo/config.toml`** sets `FFMPEG_DIR` to the ffmpeg that `cargo xtask prepare-deps` already
downloads. `ffmpeg-sys-next` reads it from the process environment, so a build script cannot set it.

**The server driver (`server_openvr`) is deliberately untouched.** See "the freeze" below.

## The freeze, and what actually fixed it

This consumed most of the session and is the single most important thing to understand before
changing the statistics or tracking code.

**Symptom.** Video played, then froze on the last frame. It resumed only when the *view* moved, ran a
few seconds, and froze again. SteamVR's own preview kept updating throughout, so the server was
rendering. Client-side instrumentation showed **zero** frames arriving at the decoder callback, so
nothing was reaching the wire.

**Root cause: `report_frame_decoded` was called in the wrong place.** It ran on the render thread,
once per *displayed* frame. The real client (`client_openxr`) reports it from the decoder, for every
decoded frame, as the frame is produced.

ALVR's statistics are a strict chain, each stage measured from the previous one
(`client_core/src/statistics.rs`):

```
report_video_packet_received  ->  video_decode
report_frame_decoded          ->  video_decoder_queue
report_compositor_start       ->  rendering
report_submit                 ->  total_pipeline_latency  -> sent to the server
```

Break the chain and `summary()` finds nothing, so **no statistics packet is sent at all** — the
`Statistics summary not ready!` flood. The server uses `total_pipeline_latency` for its prediction
offset, which shifts the `target_timestamp` it stamps on frames (`server_openvr/src/lib.rs:306`).
Wrong offset means timestamps that no longer match `HEAD_POSE_QUEUE`, and `VideoSend`
(`server_openvr/src/lib.rs:591-598`) then **silently drops the frame** — no log line. The
`Latency is too high. Clamping prediction` warnings in the server log were this, visible.

Two supporting fixes landed with it, both real:

- **Codec parameter sets are refreshed on every `DecoderConfig` event.** The server repeats them
  whenever asked for a recovery keyframe. Discarding the repeat left recovery keyframes without a
  sequence header, so they could not decode and corruption looked permanent.
- **Parameter sets are prepended to every keyframe**, not consumed once on the first.

### Dead ends — do not repeat these

Recorded because each looked convincing and cost real time.

- **Plane row stride.** Theory: ffmpeg's last row lacks padding so the upload was skipped. Measured:
  buffers are exactly `stride * height`, stride equals width. Wrong.
- **Timestamp round-trip through ffmpeg.** Theory: PTS was not preserved. The tiny values in the log
  are `push_nal`'s *input* — ALVR's own timestamps — not something ffmpeg mangled. Wrong.
- **Timer-paced statistics reporting.** Made the warning flood far worse. The real client reports
  only when a new frame is available, never with a repeated timestamp.
- **`enforce_server_frame_pacing` and `max_queued_server_video_frames`.** Both red herrings. The
  server's `frame_interval` is write-once from the negotiated framerate
  (`server_core/src/statistics.rs:87`) and never driven by client reports, so starving reports cannot
  change its cadence.
- **Server-side `PoseHistory::GetBestPoseMatch` change.** `GetBestPoseMatch` reverse-engineers *which
  tracking sample* a rendered frame belongs to, by nearest-orientation search over a 3-second, 360-
  entry buffer — SteamVR passes the pose but no frame ID. It compares **rotation only** (position
  ignored) and keeps the **oldest** entry on ties, so identical static poses resolve to a stale
  sample. That analysis is correct and the weakness is genuine, but it was a *consequence* of the
  broken statistics, not the cause. Changing shared server code to accommodate a synthetic client was
  the wrong trade, and it was reverted. Real clients work; the bug was ours.
- **Tracking jitter to make poses distinguishable.** Fixed the freeze but caused visible shaking and
  out-of-order frames — because it was added *together with* derived velocities, which divide the
  jittered delta by a ~4.6 ms interval and amplify the noise ~216×. Both were reverted. Tracking
  reports zero velocities, as it did originally.

**Method note.** Five wrong theories in a row were broken by having subagents read the real client's
implementation and the server's frame path, instead of continuing to reason from symptoms. Do that
earlier next time.

## The judder, and how it was finally measured

Several sessions were spent guessing at this from how it looked. What broke it open was building a
number for it instead. Do that first next time.

**The metric.** Every displayed frame carries the timestamp of the tracking sample the server
rendered it from, and `report_compositor_start` returns that frame's head pose. So the emulator can
measure, entirely from the client side:

| Readout | What it is | What a bad number means |
|---|---|---|
| `publish` | interval at which the UI hands poses to the tracking thread | uneven UI frame times, which the reconstruction has to absorb |
| `sent` | interval between tracking packets, and the rotation between consecutive ones | an uneven step here means the emulator built a bad signal |
| `world` | tracking time between consecutive displayed frames, and the head rotation between them | **this is the visible judder** |
| `screen` | real time each frame was on screen | uneven presentation by this window |
| `repeat` | frames that came back with the previous frame's pose | view parameter lookups are missing |

It is behind the toolbar's **Stats** toggle while streaming, and in `GET /api/state` under
`frame_timing`. The overlay reports the head motion as a *speed* rather than the per-frame step it
is measured as, since °/s and m/s can be compared against how fast you are actually moving.
`POST /api/drive`
holds a camera input — a *rate*, not a pose — so a scripted sweep runs down the same code path the
keyboard does. Posting individual poses instead makes the caller's own scheduling part of the
measurement, which is fatal here: an HTTP client cannot pace itself to anywhere near a frame.

Under a steady 60 °/s turn each frame should advance by exactly 60 °/s × 13.889 ms = 0.833°.
Deviation from that is scene movement with nothing behind it.

**What it found, in order of size.**

1. **The decoder truncated frame timestamps to microseconds, and 91% of view parameter lookups
   missed.** `duration_to_pts`/`pts_to_duration` in `decoder/software.rs` round-tripped the
   timestamp through a PTS, which carries microseconds. ALVR's frame timestamps are `Instant`
   elapsed times, which on Windows come from the performance counter at 100 ns resolution — so
   roughly nine in ten came back a few hundred nanoseconds off the value everything upstream is
   keyed by. `report_compositor_start` then failed to find the frame's view parameters and returned
   *the previous frame's*, so anything drawn from them lagged the video by however long ago the last
   exact match was. Per-frame rotation jitter: **1.67° against a 0.83° mean — twice the frame's own
   motion.** The decoder already kept the exact timestamps for a fallback path; the fix is to use
   the PTS only as a key to find the original. This also silently broke the statistics chain, which
   is keyed by the same timestamp — the `Statistics summary not ready!` flood this document
   previously called expected was this, not something inherent.
   → jitter 1.67° → **0.41°**, repeats 91% → **0%**.

2. **The pose reconstruction clamped on uneven UI frames.** Fixed by keeping a history; see the
   controller notes above. → **0.41° → 0.175°**, and the send-side step jitter roughly halved.

3. **`GetBestPoseMatch` cannot resolve a frame while the head is not rotating.** It picks the
   nearest-rotation entry from a 360-sample buffer, ignoring position, and keeps the *oldest* on
   ties (`minDiff > distance`, iterating oldest to newest). Walking in a straight line holds the
   orientation bit-constant, so nothing distinguishes the candidates and the choice falls to
   floating-point noise. Measured directly, walking at 2 m/s:

   | | frame timestamp spacing | per-frame step |
   |---|---|---|
   | walk only, orientation constant | 14.17 ± **4.24** ms | 28.35 ± 8.93 mm |
   | walk while turning | 13.89 ± **0.10** ms | 27.75 ± 6.10 mm |

   A 42× difference in how evenly the world advances, from nothing but whether the head happened to
   be rotating. This is the remaining third of the walking judder and it is **not fixed**. A real
   headset never hits it because its orientation always carries sensor noise. Two ways out, neither
   taken here: a tiny monotonic orientation dither on the client (needs roughly 0.1° of amplitude
   over a period longer than the 1.67 s buffer to beat the float noise, and it must be monotonic
   across the whole buffer or the aliases tie instead), or `minDiff >= distance` in
   `PoseHistory.cpp` so ties keep the newest sample rather than the oldest — one character, and
   arguably a fix for real headsets too, since a user holding still currently resolves to a
   1.67-second-old timestamp.

**Measured dead end: sending tracking faster.** It looks like it must help — ALVR's driver never
submits an HMD velocity, so SteamVR cannot extrapolate the head and the send interval *is* the time
quantum of the rendered world. Raising it from `refresh_rate * 3` (216 Hz) to 500 Hz made things
**worse**: jitter 0.155° → 0.205°, consistently across every sampling window, and the frame
timestamps spread from ±0.10 ms to ±0.31 ms. The server runs its entire tracking path per received
packet, and the extra load costs more than the finer quantum buys. The rate stays at the real
client's.

**Measured dead end: pacing the send loop more precisely.** Sleeping to 300 µs short of each
deadline and spinning the rest genuinely tightens the send interval — deviation 0.155 ms → 0.086 ms
— and the judder that reaches the screen does not improve, coming out marginally worse (0.155° →
0.174°). Same lesson as the rate: precision on the send side is not what the picture is limited by.
Plain `thread::sleep` it is; a busy-wait would buy nothing.

**Where it ended up.** Under a 60 °/s turn: 0.833° ± 0.175° per frame, from 0.93° ± 1.67° — the
error went from twice the frame's own motion to a fifth of it, about 3 pixels at this FOV and
resolution. Walking at 2 m/s: 28.1 ± 8.9 mm, with roughly a third of what remains being item 3.

## Known limitations

- **Single instance only.** The port collisions are solved, but `alvr_client_core` stores its hostname
  in one per-user config file, so concurrent instances present as the same device. Needs the
  `ALVR_CLIENT_CONFIG_DIR` / `ALVR_CLIENT_HOSTNAME` overrides prototyped on the `multiple_clients`
  branch (~30 additive lines in `client_core/src/storage.rs`).
- **Shared client identity** with any real client on this machine
  (`%APPDATA%\ALVR Client\session.json`). Note a UTF-8 BOM in that file makes the server reset all
  settings, and repackaging deletes it.
- **Capture endpoints render the scene, not the video.** `/api/view/color` and `/api/view/depth` are
  offscreen renders of the glTF scene. Capturing the decoded video is not implemented.
- **Depth values are not comparable between captures** — normalised across the range present in each
  image, because a small room occupies a tiny fraction of the 0.02–100 m frustum.
- **Statistics warnings may still appear.** `report_submit` fires only on new frames while the UI runs
  faster, so some are expected. A *flood* of them is not: that was the microsecond timestamp
  truncation in "the judder" above, which broke every lookup keyed by the frame timestamp. If the
  freeze ever returns under load, look here first.
- **Only H.264 verified.** HEVC and AV1 paths exist in the decoder but are untested. AV1's keyframe
  detection is stubbed to always-true, since it is not Annex-B framed.
- Linux is intended but unverified; nothing in the crate is Windows-specific by design.

## Test setup that works

Server settings in `build/alvr_streamer_windows/session.json`:

| Setting | Value | Note |
|---|---|---|
| `stream_protocol` | `Udp` | |
| `preferred_codec` | `H264` | the only verified path |
| `passthrough.enabled` | `false` | its `variant` must be a value this branch knows; a stale `AlphaStream` from `ar_mode` breaks negotiation |
| `client_connections` | emulator hostname trusted | |

Repackaging with `cargo xtask package-streamer` **regenerates `session.json` with defaults**, losing
trust and any settings above. Trust can be restored live without a restart:

```sh
curl -H "X-ALVR: true" -H "Content-Type: application/json" \
  -X POST -d '["<hostname>","Trust"]' \
  http://127.0.0.1:8082/api/session/client-connections
```

Note `cargo xtask package-streamer` currently fails at the license-generation step
(`cargo about` errors); the driver DLL is copied before that, so the package is usable anyway.

**Operational gotchas.**

- Starting SteamVR is best done via the ALVR dashboard, which owns the driver lifecycle.
- **Do not force-kill `vrserver`.** It orphans IPC port 27062 and the next launch fails with
  "Port 27062 in use". Ask the user to close SteamVR; a programmatic close request raises a
  confirmation dialog only they can accept.
- Kill stray `alvr_client_emulator` processes before rebuilding, or the linker cannot replace the exe
  — and a stale instance also holds port 9943, which silently breaks the next run's discovery.

## Suggested next steps

1. **Finish the walking judder** — item 3 under "the judder". Decide between the client-side
   orientation dither and the one-character `GetBestPoseMatch` tie-break, then measure it with
   `/api/drive` + `frame_timing` rather than by eye.
2. **Multiple instances.** Take the `storage.rs` identity patch; the port work is already done.
3. **Capture the decoded video** through the API, so the emulator is useful to an automated harness
   rather than only to a human watching the window.
4. **MCP or richer control surface** over the existing HTTP API.
5. **3D Gaussian splat scenes** for realistic AR environments. The `Scene` type is deliberately a
   geometry container rather than a renderer so an alternative source can slot in.
6. **Zero-copy decode**, only if profiling demands it. ffmpeg's Vulkan decoder currently fails with
   `VK_ERROR_DEVICE_LOST` on the RTX 5090 tested, so the portable GPU path is not viable yet; d3d11va
   works but was slower than CPU for a single stream.
7. **Upstream the three ALVR changes** — they are useful beyond this emulator, particularly the
   announcer shutdown, which is a real leak.
