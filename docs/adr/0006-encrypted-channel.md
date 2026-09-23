# ADR 0006: MVP encrypted-channel construction (Noise-style over UDP)

- Status: accepted (construction); measurement results **pending M1.4**
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

## Known limitations (deliberate, and not yet fixed)

These are stated plainly because they are security-relevant and **not** addressed
by the code in this milestone:

1. **No forward secrecy yet.** The current key schedule derives from a
   static-static X25519 agreement. Compromise of a device's long-term private key
   would decrypt traffic recorded from that device. M1.4 must move the handshake
   to an ephemeral-static pattern (Noise `IK`, or a plain ephemeral X25519 per
   session mixed into the HKDF input) to obtain forward secrecy. This is the
   single most important follow-up.
2. **No replay window.** The sequence number is authenticated, but nothing yet
   rejects a *replayed* record that repeats a sequence the receiver has already
   accepted. M1.4 must keep a sliding receive window and request a keyframe
   after a gap.
3. **No rekeying.** Keys live for the whole session. Long sessions are not
   expected in the MVP, but rekeying is the natural place to fold in (1).
4. **Revocation is not yet enforced on a live session.** The pairing database can
   revoke an identity, but nothing terminates an in-flight channel yet; the M1
   acceptance criterion "revoke kills an active session" is still open.
5. **No independent security audit.** The composition is small and tested, but
   it has not been reviewed by someone outside this project, and the pinned
   primitive versions have not been reviewed here either.

## Measurement plan (why this ADR has no numbers)

ADR 0005 asked for a spike "with latency measurements". Those numbers cannot be
honestly produced yet: there is no transport, so there is nothing to measure
end to end. Fabricating them would be worse than omitting them. M1.4 will record
the following once the transport exists:

- handshake round trips and wall-clock time to first frame after `start`
- per-record AEAD CPU cost at 1080p60, host-seal and device-open
- throughput ceiling and added latency versus plaintext framing on the LAN
- behaviour with and without a video keyframe after loss

Until those are recorded, "accepted" above covers the *construction choice*
only, not its performance.

## Consequences

+ Small, reviewable surface: composition of primitives rather than a new protocol.
+ The pairing code stays a consent signal; the X25519 public key is the actual
  trust anchor, so a short code does not have to carry 100+ bits of security.
+ Direction separation and authenticated sequence come for free from the AEAD's
  AAD.
− Forward secrecy, replay defence, rekeying and session-kill-on-revoke are all
  still ahead of us (items 1–4 above); none may be called "done" until they are.
− A bespoke composition is still a bespoke composition. If the limitations above
  prove awkward, moving the handshake to a real Noise implementation library
  remains an option — the trust store and identity format would not change.
