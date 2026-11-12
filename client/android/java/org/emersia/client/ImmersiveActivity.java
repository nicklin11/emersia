package org.emersia.client;

import android.app.Activity;
import android.content.Intent;
import android.content.SharedPreferences;
import android.os.Bundle;
import android.view.WindowManager;

/**
 * Immersive mode (ADR 0009): every stream becomes a curved screen around the
 * user. Nothing is drawn here — the native side runs an OpenXR session whose
 * cylinder layers are fed straight by the hardware decoders.
 */
public final class ImmersiveActivity extends Activity {
    private long session;
    private long immersive;

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        SharedPreferences p = getSharedPreferences("emersia", MODE_PRIVATE);
        String server = p.getString(MainActivity.EXTRA_SERVER, null);
        String key = p.getString(MainActivity.EXTRA_HOST_KEY, null);
        if (server == null || key == null) {
            // Not paired yet: pairing happens in the panel app.
            startActivity(new Intent(this, MainActivity.class));
            finish();
            return;
        }
        // A headless session (no panel surface): the OpenXR thread attaches
        // one surface per stream.
        session = NativeBridge.nativeStart(null, getFilesDir().getAbsolutePath(), server, key);
        NativeBridge.session = session;
        if (session != 0) immersive = NativeBridge.nativeImmersiveStart(session, this, getFilesDir().getAbsolutePath());
        if (immersive == 0) finish();
    }

    @Override protected void onDestroy() {
        if (immersive != 0) {
            NativeBridge.nativeImmersiveStop(immersive);
            immersive = 0;
        }
        if (session != 0) {
            NativeBridge.session = 0;
            NativeBridge.nativeStop(session);
            session = 0;
        }
        super.onDestroy();
    }
}
