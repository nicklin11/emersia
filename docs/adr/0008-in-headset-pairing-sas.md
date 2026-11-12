# ADR 0008: In-headset pairing — discovery beacons and SAS numeric comparison

- Status: accepted
- Date: 2026-11-12

## Context

ADR 0005 calls for pairing by a short code shown in the headset. Until now
the headset's public key reached the host over adb (`scripts/quest-mirror.sh`
reads it with `run-as`), which no ordinary user has. Pairing must work over the
LAN, where an attacker can send and receive UDP (ADR 0005 threat model).

A code derived from the device key alone ("show 6 digits of H(pk_d)") is
broken on a LAN: an attacker grinds ~10⁶ keys until one matches the displayed
code. The code must depend on randomness neither side can choose after
seeing the other's.

## Decision

Bluetooth-SSP-style numeric comparison with commitments, in
`emersia-transport::pairing` (one implementation for both ends):

1. `emersia pair headset` opens a 120 s window. While open, the daemon
   broadcasts `BEACON(pk_h, stream_port, hostname)` every 500 ms to the
   *directed* broadcast address of every interface (computed from address and
   netmask: hotspot interfaces often have no broadcast address configured, and
   255.255.255.255 only leaves through the default route), UDP port 47801.
2. Device → host `COMMIT(pk_d, SHA256(ctx‖n_d), name)`; host → device
   `NONCE(n_h)`; device → host `REVEAL(n_d)` (must open the commitment, else
   the exchange is discarded).
3. Both display `SAS = 6 digits of SHA256(ctx‖pk_h‖pk_d‖n_d‖n_h)`. Each side's
   nonce is fixed before it sees the other's, so a man in the middle running
   two exchanges gets two independent codes: success ≈ 10⁻⁶ per attempt.
4. The user confirms on **both** devices. The host stores the device on its
   user's "yes" and sends `RESULT(accepted, tag)` with
   `tag = SHA256(ctx‖pk_h‖pk_d‖n_d‖n_h‖verdict)` (unforgeable off-path). The
   headset stores the beaconed host key and address only on an authentic
   accepted RESULT **and** its own user's tap.

Pairing packets are cleartext types 0x20–0x24, answered only while a window is
open (default deny otherwise), at most 8 concurrent exchanges, 120 s each.

## Consequences

- Pairing needs no adb and no typing: compare six digits, confirm twice.
- The host's existing trust store is the single way in: a confirmed headset
  goes through the same code-validated `accept_code` path as CLI pairing.
- The stream port must be reachable (firewall), as for streaming itself; the
  headset says so when the host does not answer.
- Pinning: later sessions use the forward-secret handshake (ADR 0006) against
  the pinned host key; re-pairing is needed if the host identity changes.
- Not covered: an attacker who can make the user confirm mismatched codes.
  The UI says what a mismatch means.

## Verified

Quest 2 ↔ niri host over the host's Wi-Fi hotspot: beacon received, exchange
completed, both sides showed `617 905`, host confirmation stored the device,
the headset verified the authenticated RESULT. Loopback tests cover accept,
host reject, headset cancel, a forged reveal, a wrong-address reveal and the
MITM code mismatch.
