# ADR 0005: Pairing, trust and control-plane model

- Status: accepted
- Date: 2026-09-23

## Context

The host daemon can (a) capture the screen and (b) inject input into the
user's session via `uinput` (ADR 0002). Anything that can connect and type is
a remote-control backdoor if the trust model is sloppy. Pairing, revocation
and a visible consent surface are therefore core features, not polish.

Threat model (MVP): trusted home/office LAN; attacker can send UDP packets to
the host; headset and host are physically co-owned. Out of scope for MVP:
WAN exposure (no port forwarding — remote access is a later milestone),
multi-user servers.

## Decision

1. **Pairing by PIN:** first contact requires a short-lived pairing code
   shown in the headset / entered on the host (`emersia pair`). Pairing
   establishes a long-term device identity; every later connection is
   authenticated against it.
2. **Encrypted session:** all stream + input traffic between paired devices
   runs inside an authenticated encrypted channel derived from the pairing
   secret. Exact construction (DTLS 1.3 vs Noise-style over UDP) is selected
   in an M1 spike with latency measurements; the protocol carries a version
   field either way.
3. **Default deny:** only paired device identities may connect; unpaired
   packets are dropped without response. One-click revoke
   (`emersia revoke …`, GUI in M4) destroys the identity server-side.
4. **Local control plane:** the daemon exposes a **unix domain socket**
   (`$XDG_RUNTIME_DIR/emersia/control.sock`), authenticated by `SO_PEERCRED`
   (same-UID only). CLI and future Tauri GUI are clients of this API; no TCP
   admin surface exists. See `docs/design/control-api.md`.
5. **Visible consent:** while a capture session is active the daemon reports
   it (`emersia status`, later a tray indicator in M4) so screen sharing is
   never silent.

## Consequences

- Security UX (pairing, device list, revoke, sharing indicator) is a
  first-class milestone (M4) rather than an afterthought; M1 ships the same
  flows via CLI.
- The pairing database lives in `~/.config/emersia/` with `0600` permissions.
- uinput access needs a udev/group setup step — documented alongside pairing
  in M1.
- A crypto-transport spike ADR (0006) will record the M1 measurement results.
