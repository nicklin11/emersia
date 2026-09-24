# ADR 0006: MVP encrypted-channel construction (Noise-style over UDP)

- Status: accepted (construction and first measurements, M1.4)
- Date: 2026-09-24
- Implements the deferral in [ADR 0005](0005-pairing-and-trust-model.md) item 2

## Context

ADR 0005 requires that all stream and input traffic between a paired host and
headset runs inside an authenticated encrypted channel derived from the pairing
secret, and explicitly defers the exact construction to an M1 spike with latency
measurements.

Constraints for the MVP:

- One known peer per host, paired out of band (ADR 0005), on a trusted
  LAN/tailnet. No certificate authority, no public-Internet reachability.
- An attacker on the network can send UDP packets to the host and must learn
  nothing and forge nothing (ADR 0005 threat model).
- Latency budget is tight: the pipeline already spends capture + encode time, and
  the channel must not add a visible stall at 60 fps.
- The implementation must stay small enough to review. A bespoke crypto stack is
  a liability, so every primitive must come from an audited library.

Options evaluated:

1. **DTLS 1.3 over UDP** — the obvious "standard" answer. Browser-grade
   authenticated encryption, and it already solves key agreement, replay windows
   and transcript binding.
2. **Noise XX/XX handshake (Noise_XXsecp256r1/X25519 ChaChaPoly_SHA256) with
   static device keys** — a small, well-specified family of handshakes designed
   for exactly this shape of problem.
3. **Home-grown key agreement** — rejected outright. Never roll your own.

## Decision

**Option 2**, implemented in `host/crates/emersia-daemon/src/crypto.rs` using
audited RustCrypto primitives only:

- **Device identity**: X25519 static keypair per paired device
  (`x25519-dalek`). The public half is the trust anchor stored in the pairing
  database; the private half never leaves the device.
- **Key agreement**: X25519 ECDH. An all-zero shared secret (a small-order point
  supplied by an attacker) is rejected outright rather than used.
- **Key schedule**: HKDF-SHA256 over the shared secret, with a random per-session
  salt and an `info` string that binds both a protocol context
  (`emersia/v1`) and a role label. This yields two independent 32-byte keys,
  one per direction, so a record cannot be reflected back at its sender.
- **Record protection**: ChaCha20-Poly1305 AEAD. The 96-bit nonce travels in the
  clear ahead of the ciphertext, which is standard and necessary. Direction and
  a 64-bit sequence number are placed in the *additional authenticated data*, so
  reflecting or replaying a record under a different sequence fails
  authentication.

Why not DTLS 1.3: it would mean embedding a TLS stack, maintaining certificate
or PSK handling, and absorbing handshake state machines and a record layer we
would not control. For a single pre-paired peer on a LAN, that is a large amount
of surface for a threat model that does not include a certificate authority. The
Noise construction is a few hundred lines of *composition* around primitives that
are individually well tested, which is the reviewable option.

## Known limitations (what is fixed, and what is still open)

1. ~~No forward secrecy~~ **Fixed in M1.4.** The handshake is now
   ephemeral-static: every session mixes a fresh X25519 ephemeral pair into the
   key schedule, so compromising a long-term key does not reveal past session
   keys. A unit test asserts that two handshakes between the same two static
   identities derive different, non-interchangeable session keys.
2. ~~No replay window~~ **Fixed in M1.4.** Each datagram carries its own record
   sequence, checked by a 64-wide sliding window using wrap-safe serial
   arithmetic. A replayed datagram is dropped; the AEAD associated data binds
   the sequence, so it cannot be rewritten to slip past the window.
3. **No rekeying.** Keys live for the whole session. Long sessions are not
   expected in the MVP, and this is where ephemeral rekeying belongs.
4. ~~Revocation does not kill a live session~~ **Fixed in M1.4.** A revoke hands
   the device id to the engine, which drops the peer from the streaming
   endpoint on its next turn. Verified live: `connected_devices` fell from 1 to
   0 and the daemon logged the disconnect.
5. **No independent security audit.** The composition is small and heavily
   tested, but it has not been reviewed by someone outside this project, and the
   pinned primitive versions have not been reviewed here either.

Two further limits are structural rather than oversights:

- The **preview** shipped today is a downscaled raw RGBA image, because the
  encoder has not landed. A 320x180 preview is 225 KiB and 196 datagrams per
  frame. The encoder (M1.5) replaces this with a few kilobytes per frame; the
  fragmentation and framing stay as they are.
- The host is **not yet fully interoperable** with other Emersia builds beyond
  its own receiver, because the key schedule is still young. If it proves
  awkward, moving to a maintained Noise implementation remains an option — the
  trust store and identity format would not change.

## Measurements (first pass, M1.4)

Taken on the dev machine over loopback, with the `emersia-daemon receive`
harness, using a 320x180 raw-RGBA preview (pre-encoder):

| measurement | value |
|---|---|
| handshake, init → confirmed session | **3.9 ms** |
| datagrams per frame | 196 |
| bytes per frame | 225 KiB |
| received throughput | ~0.5 MiB/s at the observed frame rate |
| unpaired peer | no reply at all, connection times out (default-deny) |
| replayed datagram | dropped by the receiver's window |

The **2.1 fps** observed end-to-end is *not* a transport limit: it is the known
`wlr-screencopy` behaviour of copying on the next compositor repaint, on an
otherwise idle screen. The transport was keeping up with everything the host
produced (`max gap: 1 datagram`). Once the encoder lands, both the frame size
and the rate story change and this table must be re-measured.

Still unmeasured, and needed before the latency claim in ADR 0003 can be made:
true end-to-end latency against a headset, behaviour over real Wi-Fi rather
than loopback, and the cost of the AEAD at production frame sizes.

## Consequences

+ Small, reviewable surface: composition of primitives rather than a new protocol.
+ The pairing code stays a consent signal; the X25519 public key is the actual
  trust anchor, so a short code does not have to carry 100+ bits of security.
+ Direction separation and authenticated sequence come for free from the AEAD's
  AAD.
+ Forward secrecy, replay defence and session-kill-on-revoke are implemented and
  tested; the first measurements are recorded above.
− Rekeying and an independent audit are still open, and the pre-encoder preview
  is a placeholder that the encoder will replace.
− A bespoke composition is still a bespoke composition. If the limitations above
  prove awkward, moving the handshake to a real Noise implementation library
  remains an option — the trust store and identity format would not change.
