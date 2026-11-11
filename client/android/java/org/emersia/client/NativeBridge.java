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

    /** Pointer position on the video, 0..1 on each axis. */
    static native void nativePointer(long handle, float x, float y);

    /** Button 0 = left, 1 = right, 2 = middle. */
    static native void nativeButton(long handle, int button, boolean down);

    /** Scroll in wheel detents; positive = right / down. */
    static native void nativeScroll(long handle, float dx, float dy);

    /** Linux evdev key code. */
    static native void nativeKey(long handle, int code, boolean down);
}
