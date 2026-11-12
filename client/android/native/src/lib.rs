//! Emersia Quest client core (ADR 0007).
//!
//! `session` (connect, decrypt, reassemble, keyframe gating), `identity` and
//! `status` are plain Rust and tested on the host. `decoder` (NDK MediaCodec)
//! and `jni_bridge` exist only on Android.

pub mod audio_buffer;
pub mod identity;
pub mod layout;
pub mod pairing;
pub mod session;
pub mod status;

#[cfg(target_os = "android")]
mod audio_out;
#[cfg(target_os = "android")]
mod decoder;
#[cfg(target_os = "android")]
mod immersive;
#[cfg(target_os = "android")]
mod jni_bridge;
