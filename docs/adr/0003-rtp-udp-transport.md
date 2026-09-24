# ADR 0003: MVP transport — direct RTP-style framing over UDP (no WebRTC)

- Status: accepted
- Date: 2026-09-23
- Continues the transport direction of libworkspaceVR (0003-rtp-transport-mvp)

## Context

Capture (ADR 0002) delivers raw frames that must reach a Quest headset over
Wi-Fi with the lowest practical latency. Host GPU on the dev machine: AMD
Navi 22 (Radeon RX 6700/6800-class, VCN hardware encoder via Mesa VA-API);
other users bring NVIDIA (NVENC) or Intel (QSV).

Options evaluated:

1. **Direct RTP-style/UDP** on the LAN — stateless, single UDP flow, minimal
   machinery between encoder and decoder.
2. **WebRTC** (SDP/ICE/DTLS/SRTP) — browser-grade congestion control and
   NAT traversal; heavier setup, more latency surface, signalling required.
3. **SRT** — robust over lossy WAN; the LAN/tailnet MVP does not need it.

## Decision

**Direct RTP-style framing over UDP** as the MVP transport: one flow per
video stream carrying RTP-like headers (sequence, timestamp, payload type)
plus the input/feedback channel; link integrity delegated to the LAN (and
tailnet/WireGuard retransmits when used). The transport stage does not own
retransmission in MVP — optional NACK/FEC is a later, measured addition.

Encoder default: **hardware** (`h264_vaapi` / `hevc_vaapi`, with NVENC/AMF/QSV
equivalents selected at runtime); software x264 kept only as a debug fallback
(`--encoder sw`). Codec ladder: H.264 baseline+main first (Quest 2+), HEVC
where it measurably wins, AV1 where hardware allows (Quest 3).

All of it runs inside the pairing-encrypted channel per ADR 0005.

### Parameters proven by the libworkspaceVR prototype

The superseded GStreamer prototype streamed 1080p60 to a headset over RTP/UDP
and was verified end to end before the Rust rewrite. Four constants from it
survive into this design and are worth keeping:

| parameter | value | why |
|---|---|---|
| RTP video clock rate | 90000 Hz | standard for video; RTP timestamps count in 90 kHz units regardless of the actual frame rate |
| payload type | 96 | conventional dynamic type for H.264, leaving the static range free |
| in-band config interval | every packet | repeat SPS/PPS continuously so a client that joins mid-stream can decode immediately instead of waiting for the next keyframe |
| sink sync | disabled | never pace output against a wall clock — emit as fast as the encoder produces |

GOP in that prototype was `key-int-period=150` at 60 fps (≈2.5 s) at 30 Mbit/s
for 1080p60. Treat both as inherited starting points, not targets: re-measure
end-to-end latency and glitch rate once the encoder stage actually runs.

## Encoder implementation: the ffmpeg CLI as a child process

The encoder runs as a separate `ffmpeg` process fed RGBA frames on stdin and
read back as Annex-B, rather than linking `libavcodec`.

**For:** one code path reaches VAAPI, NVENC, AMF and QSV. Each vendor's encoder
is selected by name, so there is no per-vendor Rust binding to write, maintain
or cross-compile, and encoder support tracks the ffmpeg build the user already
has. **Against:** a process boundary per stream, and no in-process control over
rate control or a forced keyframe. Frame delivery is over pipes, so a stalled
encoder appears as pipe backpressure rather than a hang.

If measurement later shows the process boundary costs more than it saves, moving
to `libavcodec` is a contained change: only the `Encoder` type is affected.

### Capability is probed, not declared

`vainfo` is not sufficient. On the dev machine's Navi 22, `VAProfileH264High`
reports `VAEntrypointEncSlice` and **not** `VAEntrypointEncPicture`, so
ffmpeg's `-low_power 1` (which requests EncPicture) fails with "No usable
encoding entrypoint" while the default slice-mode path works. Reading `vainfo`
would have given the wrong answer.

Selection therefore runs each candidate encoder on a real frame and keeps the
first that produces output. `auto` probes VAAPI, NVENC, AMF, QSV, then falls back
to `libx264`. A forced choice (`--encoder vaapi`) never falls back to software:
a silent downgrade would be worse than a clear failure. The `--encoder sw`
debug path is labelled as software everywhere it is selected, and `status`
reports both the encoder name and whether it is software.

### The encoder is told the rate capture actually achieves

This was not obvious and cost a real debugging session. The encoder's clock
comes from the **declared** input rate, so telling it 60 fps while
`wlr-screencopy` delivers 2.5 fps makes every duration it computes 24x too
long. A two-second keyframe interval became 48 seconds, and a device that
joined mid-stream received 30 frames and not one keyframe — a stream that
reassembles perfectly and decodes to nothing.

The engine now feeds the encoder the *measured* capture rate and restarts it
when that rate moves by more than 25%. Keyframes then land every two seconds of
real time regardless of rate: measured 10 keyframes in 60 frames, against 1
before the fix.

## Measurements (M1.5, niri 26.04, AMD Navi 22, Mesa 26.2.3, loopback)

Encoder selected at runtime: **`h264_vaapi`** (hardware), profile High,
1920x1080, yuv420p, 20 Mbit/s target.

| measurement | value |
|---|---|
| handshake, init to confirmed session | 2.9 ms |
| encoder throughput, 60 frames of 1080p | 0.35 s wall (**2.6x realtime** headroom) |
| `h264_vaapi`, 5 s of 1080p60 | 1.89 s wall (2.64x realtime) |
| `hevc_vaapi`, 5 s of 1080p60 | 1.18 s wall (4.24x realtime) |
| stream, 60 frames received | 1341 datagrams, 1.5 MiB, 25.6 s |
| per frame, fullscreen video content | 22 datagrams, **25 KiB** |
| per frame, previous raw-RGBA preview | 196 datagrams, 225 KiB |
| keyframes in 60 frames | 10 (one per ~2 s) |
| max gap | 1 datagram |
| `ffprobe` on the reassembled stream | h264 High 1920x1080, 60 frames decoded |

The decoded frames are the live desktop: niri's bar, clock, system indicators
and the animated wallpaper, confirmed by decoding to PNG and inspecting the
image rather than trusting the byte count.

**What is still not met:** the 60 fps acceptance criterion. Capture delivers
2.5 fps on this machine regardless of content, because `wlr-screencopy` copies
on the next compositor repaint. That is the M1.1 finding, not an encoder
limitation — the encoder has over 2x realtime headroom at 1080p60. Closing it
needs dmabuf or the portal/PipeWire path, which is the next capture milestone.

Inherited parameters re-measured: the 30 Mbit/s / 2.5 s GOP from
libworkspaceVR is workable but was tuned for a 60 fps source. At 20 Mbit/s and a
measured-rate GOP of 2 s, a fullscreen video frame costs 25 KiB. Bitrate tuning
per content is not done; these are starting points that work, not targets.

## Consequences

+ Zero signalling infrastructure; peer address is configuration, not a
  protocol — pairing (ADR 0005) supplies identity instead.
+ Terminal latency approaches encoder + payload + Wi-Fi only.
− Not reachable over the public Internet; acceptable for LAN/tailnet MVP —
  remote access revisits WebRTC as its own ADR when the requirement lands.
− UDP without retransmit: a lost packet is a brief visual glitch, not an
  error; recovery paths are keyframe requests, not re-transmission (MVP),
  with NACK/FEC measured later if glitches show up in practice.
