//! Emersia wire protocol, shared by the host daemon and the Quest client.
//!
//! - [`crypto`]: device/host identities, session keys, AEAD seal/open (ADR 0006)
//! - [`transport`]: RTP-style framing, replay window, reassembly, the
//!   forward-secret handshake and the UDP endpoints (ADR 0003)
//! - [`codec`]: payload types and Annex-B access-unit parsing
//! - [`pairing`]: discovery beacons + SAS pairing exchange (ADR 0008)
//! - [`input`]: device → host input events (pointer, buttons, scroll, keys)
//!
//! One definition of the protocol means the headset cannot drift from the host:
//! both sides compile the same code.

pub mod codec;
pub mod crypto;
pub mod input;
pub mod pairing;
pub mod transport;
