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

## Consequences

+ Zero signalling infrastructure; peer address is configuration, not a
  protocol — pairing (ADR 0005) supplies identity instead.
+ Terminal latency approaches encoder + payload + Wi-Fi only.
− Not reachable over the public Internet; acceptable for LAN/tailnet MVP —
  remote access revisits WebRTC as its own ADR when the requirement lands.
− UDP without retransmit: a lost packet is a brief visual glitch, not an
  error; recovery paths are keyframe requests, not re-transmission (MVP),
  with NACK/FEC measured later if glitches show up in practice.
