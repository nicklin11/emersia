package org.emersia.client;

import android.view.Surface;

/** JNI entry points into libemersia_client.so (client/android/native). */
final class NativeBridge {
    static {
        System.loadLibrary("emersia_client");
    }

    private NativeBridge() {}

    /** Loads or creates the device identity: "<public key hex> <device id hex>" or "error: …". */
    static native String nativeIdentity(String filesDir);

    /** Starts streaming into {@code surface}; returns a handle, or 0 on bad arguments. */
    static native long nativeStart(Surface surface, String filesDir, String server, String hostKey);

    /** Stops streaming and releases the surface. Blocks until the stream thread exits. */
    static native void nativeStop(long handle);

    /** Human-readable status block for the overlay. */
    static native String nativeStatus(long handle);

    /** The host address in use; changes when the host moved and was followed. */
    static native String nativeServer(long handle);

    /** Shows {@code stream} on {@code surface} (a secondary panel); requests a keyframe. */
    static native boolean nativeAttach(long handle, int stream, Surface surface);

    /** The panel's surface is going away; returns once the decoder released it. */
    static native void nativeDetach(long handle, int stream);

    /** Starts immersive mode (OpenXR cylinder screens) on {@code session}; returns a handle. */
    static native long nativeImmersiveStart(long session, android.app.Activity activity);

    /** Ends immersive mode; returns once the OpenXR session is gone. */
    static native void nativeImmersiveStop(long immersive);

    /** Bitmask of stream ids the host has sent. */
    static native int nativeStreams(long handle);

    /** Pointer on a given stream's panel, 0..1 on each axis. */
    static native void nativePointerOn(long handle, int stream, float x, float y);

    /** The running session, shared by every panel (one transport session per headset). */
    static volatile long session;

    /** Pointer position on the video, 0..1 on each axis. */
    static native void nativePointer(long handle, float x, float y);

    /** Button 0 = left, 1 = right, 2 = middle. */
    static native void nativeButton(long handle, int button, boolean down);

    /** Scroll in wheel detents; positive = right / down. */
    static native void nativeScroll(long handle, float dx, float dy);

    /** Linux evdev key code. */
    static native void nativeKey(long handle, int code, boolean down);

    /** Starts in-headset pairing (listens for a PC's beacon); returns a handle. */
    static native long nativePairStart(String filesDir, String deviceName);

    /** "searching" | "code|123 456|host|confirmed" | "accepted|ip:port|key" | "rejected" | "error|msg". */
    static native String nativePairStatus(long handle);

    /** The headset user confirms the codes match. */
    static native void nativePairConfirm(long handle);

    /** Cancels or finishes pairing and frees the handle. */
    static native void nativePairStop(long handle);
}
