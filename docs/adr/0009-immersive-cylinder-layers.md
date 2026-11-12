# ADR 0009: Immersive view — OpenXR cylinder layers fed directly by MediaCodec

- Status: accepted
- Date: 2026-11-12

## Context

Panel mode (ADR 0007) puts each stream in a flat Horizon OS window. M3 needs
an immersive workspace: screens around the user, curved, at a comfortable
distance. ADR 0004 left the immersive layer open between Godot and native
OpenXR, to be decided against measured latency.

The decoded image is the only thing on screen. Any engine (Godot, a GL/Vulkan
renderer) would sample the decoder's output into an eye buffer, adding a GPU
pass, a copy into the projection layer and a second resampling by the
compositor (projection → lens distortion), which softens text.

## Decision

Native OpenXR (Khronos loader 1.1.65, bundled and hash-pinned), no engine:

- One `XR_KHR_android_surface_swapchain` per stream. The runtime's Surface is
  that stream's MediaCodec output Surface.
- Each stream is submitted as an `XR_KHR_composition_layer_cylinder`
  (radius 1.7 m, 1.6 m arc per screen, 0.05 rad gaps, laid out left to right
  and centred ahead). The compositor samples the decoded frame **once**, at
  display resolution, with timewarp. We render nothing; an EGL context exists
  only because a session needs a graphics binding.
- Swapchain counts must be zero (`sample/face/array/mip`): Meta's runtime
  returns `ERROR_VALIDATION_FAILURE` otherwise. The returned Surface is a
  global reference owned by the runtime (deleting it aborts under CheckJNI).
- `ImmersiveActivity` (VR + `IMMERSIVE_HMD` categories) runs a headless
  transport session and attaches surfaces through the same decoder registry as
  panels (#109).

## Consequences

- Zero copies end to end on the headset; measured on a Quest 2 with two
  streams: compositor at 72/72 FPS with `LCnt=2`, 0.36 ms CPU+GPU per frame,
  4% GPU; decode latency unchanged from panel mode (~4.4 ms).
- Godot is no longer planned for the client; the presentation layer is native.
  This supersedes the Quest-client row of ADR 0004.
- Controller input (#128): the active hand's aim ray is intersected with the
  cylinders and sent as `PointerOn{stream}`; trigger = click, thumbstick =
  scroll. The pointer the user sees is the desktop's own cursor, composited by
  the host — nothing extra is rendered.
- Passthrough (#130): with `XR_FB_passthrough` the room is the bottom layer
  and the screens float in it (keyboard and desk stay visible); black when the
  runtime lacks it. Quest 2: `LCnt=3` (passthrough + 2 screens) at 72/72 FPS.
- Recentering is the system's (hold the Meta button); LOCAL space follows it.
- Not yet: user-adjustable layout (distance, size, arrangement).
- Verification gap: compositor stats prove the layers are composited, but an
  idle headset captures black, so the image itself still needs to be checked
  by someone wearing it.
