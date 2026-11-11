# ADR 0007: Quest client — shared Rust core, native decode, panel first

- Status: accepted
- Date: 2026-11-12

## Context

ADR 0004 picked Godot 4 + OpenXR for the Quest client, with "a small plugin
for MediaCodec HW decode". Bringing up M2 made two things concrete:

1. The client must speak exactly the protocol the daemon speaks — X25519/HKDF/
   ChaCha20-Poly1305 sessions, the forward-secret handshake, the replay window
   and fragment reassembly (ADR 0003, 0006). A second implementation in
   Java/Kotlin/GDScript would fork the security surface and drift.
2. The per-frame path (≈1,300 datagrams/s at 1080p60) must not cross the JVM
   or a scripting VM. Decrypt → reassemble → codec input is the only copy chain
   that is acceptable; decoded pixels should never touch the CPU.

Separately, the first user-visible goal is plain **screen mirroring**, and
Horizon OS already shows an ordinary Android `Activity` as a floating,
resizable, passthrough-aware panel.

## Decision

- The wire protocol lives in a library crate, **`emersia-transport`**, used by
  both the daemon and the client. One definition; CI checks it builds for
  `aarch64-linux-android`.
- The client core is **Rust** (`client/android/native`, cdylib): handshake,
  receive, reassembly, keyframe gating, and **NDK `AMediaCodec` decoding
  straight into the `ANativeWindow`** of the panel's Surface. Java is a thin
  shell (Surface lifecycle, settings, status overlay) over four JNI calls.
- **Panel first.** M2.2 ships a 2D Activity panel: mirroring works today with
  zero XR code. The immersive layer (curved/multiple screens, OpenXR) comes
  next and reuses the same core — whether through a Godot GDExtension (ADR 0004)
  or native OpenXR is decided when that work starts, against measured latency.
- **No Gradle.** The APK is built with `aapt2` + `javac` + `d8` + `apksigner`
  (`client/android/build.sh`): seconds per build, no daemon, reproducible from
  the CLI.
- Device identity is generated on the headset and kept in app-private storage;
  only `device.pub` (public key + derived id) is readable by pairing tooling.

## Consequences

- A protocol change is one change, compiled into both ends; the session logic
  is unit- and loopback-tested on the host against the real `HostEndpoint`.
- ADR 0004's "Godot 4 client" now means "Godot (or native) *presentation
  layer*"; transport and decode are not Godot's job.
- Loss handling is "freeze until the next keyframe". Since #83 the client
  sends an authenticated keyframe request (rate-limited on both ends), so
  recovery takes one encoder restart instead of up to the 2 s forced interval.
- The debug APK is `android:debuggable` so `run-as` can read `device.pub`;
  release builds (M8) must drop that and pair through the in-headset UI.

## Measured (Quest 2, Horizon OS / Android 14, Wi-Fi via the host's hotspot)

- Handshake over Wi-Fi: 3–16 ms.
- Decoder: `OMX.qcom.video.decoder.avc`, hardware, 1920×1080 output.
- Delivered frame rate equals the host capture rate (≈4 fps on a mostly idle
  niri desktop — the damage-driven `wlr-screencopy` limit tracked in #35, not a
  client limit). With an active desktop (#83 test): **60 fps, 8.9 Mbit/s,
  1,428 frames, 0 lost, 0 skipped**.
- Latency (#85, after the #88 encoder fixes; clock offset from ping/pong,
  lowest-RTT sample of 8): **capture → frame received 18.6 ms, hardware
  decode 5.3 ms, ≈ 24 ms capture → decoded**, Wi-Fi RTT 1.5 ms, at 59 fps.
  "Capture" is when the host finished reading the frame back; the compositor
  wait before it and the headset's own scan-out are not included. Most of the
  host share is CPU readback/conversion and the RGBA pipe into ffmpeg — the
  dmabuf path (#35) is the next big win.
