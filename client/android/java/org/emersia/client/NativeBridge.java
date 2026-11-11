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
}
