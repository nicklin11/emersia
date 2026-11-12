package org.emersia.client;

import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.graphics.Color;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.WindowManager;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.TextView;

/**
 * Mirrors one host output into a Horizon OS panel.
 *
 * <p>All per-frame work is native: this class only owns the Surface, the
 * connection settings and the status overlay. Settings arrive as intent extras
 * (from scripts/quest-mirror.sh) and are remembered, so the app can later be
 * launched from the library with no arguments.
 */
public final class MainActivity extends Activity implements SurfaceHolder.Callback {
    static final String EXTRA_SERVER = "server";
    static final String EXTRA_HOST_KEY = "host_key";
    private static final String PREFS = "emersia";
    private static final long STATUS_PERIOD_MS = 500;
    /** Video size the surface buffer is allocated at; the compositor scales the panel. */
    private static final int VIDEO_W = 1920, VIDEO_H = 1080;

    private final Handler ui = new Handler(Looper.getMainLooper());
    private AspectFrame frame;
    private SurfaceView video;
    private TextView status;
    private SurfaceHolder holder;
    private long handle;
    private WifiManager.WifiLock wifiLock;
    private InputMapper input;
    /** Streams with an open secondary panel (bitmask). */
    private static volatile int openPanels;

    static void panelOpened(int stream) {
        openPanels |= 1 << stream;
    }

    static void panelClosed(int stream) {
        openPanels &= ~(1 << stream);
    }
    private PairingPanel pairingPanel;
    private Button pairButton;
    private long pairHandle;
    private WifiManager.MulticastLock multicastLock;
    private static final long PAIR_POLL_MS = 250;

    private final Runnable pairPoll = new Runnable() {
        @Override public void run() {
            if (pairHandle == 0) return;
            String line = NativeBridge.nativePairStatus(pairHandle);
            if (line.startsWith("accepted|")) {
                String[] p = line.split("\\|", -1);
                getSharedPreferences(PREFS, MODE_PRIVATE).edit()
                        .putString(EXTRA_SERVER, p[1]).putString(EXTRA_HOST_KEY, p[2]).apply();
                stopPairing();
                if (holder != null) {
                    stop();
                    start();
                }
                return;
            }
            pairingPanel.render(line);
            ui.postDelayed(this, PAIR_POLL_MS);
        }
    };
    /** When streaming began, for auto-hiding the overlay; 0 = not streaming. */
    private long streamingSince;
    /** The overlay stays this long after streaming starts, then gets out of the way. */
    private static final long OVERLAY_LINGER_MS = 5000;

    private final Runnable poll = new Runnable() {
        @Override public void run() {
            if (handle != 0) {
                // Remember where the host moved to, so the next launch goes there.
                String server = NativeBridge.nativeServer(handle);
                SharedPreferences prefs = getSharedPreferences(PREFS, MODE_PRIVATE);
                if (!server.isEmpty() && !server.equals(prefs.getString(EXTRA_SERVER, ""))) {
                    prefs.edit().putString(EXTRA_SERVER, server).apply();
                }
                openNewPanels();
                String text = NativeBridge.nativeStatus(handle);
                status.setText(text);
                long now = System.currentTimeMillis();
                if (text.contains(" · streaming\n")) {
                    if (streamingSince == 0) streamingSince = now;
                } else {
                    streamingSince = 0;
                }
                boolean show = streamingSince == 0 || now - streamingSince < OVERLAY_LINGER_MS;
                status.setVisibility(show ? View.VISIBLE : View.GONE);
                // Re-pairing is offered whenever we are not streaming.
                pairButton.setVisibility(streamingSince == 0 && pairHandle == 0 ? View.VISIBLE : View.GONE);
            }
            ui.postDelayed(this, STATUS_PERIOD_MS);
        }
    };

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        remember(getIntent());

        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);
        frame = new AspectFrame(this, VIDEO_W, VIDEO_H);
        video = new SurfaceView(this);
        frame.addView(video, new FrameLayout.LayoutParams(-1, -1));
        root.addView(frame, new FrameLayout.LayoutParams(-1, -1, Gravity.CENTER));

        status = new TextView(this);
        status.setTextColor(Color.WHITE);
        status.setBackgroundColor(0x99000000);
        status.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
        int pad = (int) (8 * getResources().getDisplayMetrics().density);
        status.setPadding(pad, pad, pad, pad);
        root.addView(status, new FrameLayout.LayoutParams(-2, -2, Gravity.TOP | Gravity.START));
        // Pointer input on the video goes to the host desktop.
        input = new InputMapper(video);
        video.setOnTouchListener((v, e) -> input.onTouch(e));
        video.setOnGenericMotionListener((v, e) -> input.onGenericMotion(e));
        status.setFocusable(false);

        pairButton = new Button(this);
        pairButton.setText("Pair with a PC");
        pairButton.setAllCaps(false);
        pairButton.setOnClickListener(v -> startPairing());
        root.addView(pairButton, new FrameLayout.LayoutParams(-2, -2, Gravity.TOP | Gravity.END));

        pairingPanel = new PairingPanel(this, new PairingPanel.Listener() {
            @Override public void onConfirm() {
                if (pairHandle != 0) NativeBridge.nativePairConfirm(pairHandle);
            }
            @Override public void onCancel() {
                stopPairing();
            }
            @Override public void onRetry() {
                stopPairing();
                startPairing();
            }
        });
        pairingPanel.setVisibility(View.GONE);
        root.addView(pairingPanel, new FrameLayout.LayoutParams(-1, -1));
        setContentView(root);

        video.getHolder().setFixedSize(VIDEO_W, VIDEO_H);
        video.getHolder().addCallback(this);
        WifiManager wifi = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        if (wifi != null) {
            wifiLock = wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "emersia");
            multicastLock = wifi.createMulticastLock("emersia-pairing");
        }
    }

    /** Listen for a PC's pairing beacon and run the code exchange (ADR 0008). */
    private void startPairing() {
        if (pairHandle != 0) return;
        // The stream listens on the discovery port for presence announcements;
        // pairing needs it. A new pairing replaces the host anyway.
        stop();
        String name = Build.MODEL != null ? Build.MODEL.replace('_', ' ') : "Quest";
        pairHandle = NativeBridge.nativePairStart(getFilesDir().getAbsolutePath(), name);
        if (pairHandle == 0) {
            status.setText("could not start pairing — see `adb logcat -s emersia`");
            return;
        }
        // Some Wi-Fi drivers drop broadcast frames unless an app holds this.
        if (multicastLock != null && !multicastLock.isHeld()) multicastLock.acquire();
        pairingPanel.setVisibility(View.VISIBLE);
        pairButton.setVisibility(View.GONE);
        ui.post(pairPoll);
    }

    private void stopPairing() {
        ui.removeCallbacks(pairPoll);
        if (pairHandle != 0) {
            NativeBridge.nativePairStop(pairHandle);
            pairHandle = 0;
        }
        if (multicastLock != null && multicastLock.isHeld()) multicastLock.release();
        pairingPanel.setVisibility(View.GONE);
    }

    @Override protected void onDestroy() {
        stopPairing();
        super.onDestroy();
    }

    @Override protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        if (remember(intent) && holder != null) {
            stop();
            start();
        }
    }

    @Override protected void onResume() {
        super.onResume();
        if (wifiLock != null) wifiLock.acquire();
        ui.post(poll);
    }

    @Override protected void onPause() {
        ui.removeCallbacks(poll);
        if (wifiLock != null && wifiLock.isHeld()) wifiLock.release();
        super.onPause();
    }

    /**
     * Opens a Horizon OS panel for every extra stream the host sends (e.g.
     * `emersia start DP-3 HDMI-A-1`): stream 0 is this panel.
     */
    private void openNewPanels() {
        int seen = NativeBridge.nativeStreams(handle);
        for (int stream = 1; stream < 4; stream++) {
            int bit = 1 << stream;
            if ((seen & bit) != 0 && (openPanels & bit) == 0) {
                openPanels |= bit; // until the panel reports itself
                Intent i = new Intent(this, PanelActivity.class)
                        .putExtra(PanelActivity.EXTRA_STREAM, stream)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_MULTIPLE_TASK);
                startActivity(i);
            }
        }
    }

    /** Persists connection extras; returns true when anything was supplied. */
    private boolean remember(Intent intent) {
        if (intent == null) return false;
        String server = intent.getStringExtra(EXTRA_SERVER);
        String key = intent.getStringExtra(EXTRA_HOST_KEY);
        if (server == null && key == null) return false;
        SharedPreferences.Editor e = getSharedPreferences(PREFS, MODE_PRIVATE).edit();
        if (server != null) e.putString(EXTRA_SERVER, server.trim());
        if (key != null) e.putString(EXTRA_HOST_KEY, key.trim());
        e.apply();
        return true;
    }

    private void start() {
        SharedPreferences p = getSharedPreferences(PREFS, MODE_PRIVATE);
        String server = p.getString(EXTRA_SERVER, null);
        String key = p.getString(EXTRA_HOST_KEY, null);
        String files = getFilesDir().getAbsolutePath();
        if (server == null || key == null) {
            status.setText(unconfigured(NativeBridge.nativeIdentity(files)));
            startPairing();
            return;
        }
        handle = NativeBridge.nativeStart(holder.getSurface(), files, server, key);
        NativeBridge.session = handle;
        input.setHandle(handle);
        streamingSince = 0;
        if (handle == 0) {
            status.setText("could not start (bad settings?) — see `adb logcat -s emersia`");
        }
    }

    private void stop() {
        if (handle != 0) {
            input.setHandle(0);
            NativeBridge.session = 0;
            NativeBridge.nativeStop(handle);
            handle = 0;
        }
    }

    private static String unconfigured(String identity) {
        String[] parts = identity.split(" ");
        if (parts.length != 2) return identity;
        return "Not paired with a PC yet. Device id " + parts[1] + ".\n"
                + "Run `emersia pair headset` on the PC, or scripts/quest-mirror.sh over USB.";
    }

    @Override public boolean dispatchKeyEvent(KeyEvent e) {
        return (input != null && input.onKey(e)) || super.dispatchKeyEvent(e);
    }

    @Override public void surfaceCreated(SurfaceHolder h) {
        holder = h;
        start();
    }

    @Override public void surfaceChanged(SurfaceHolder h, int format, int w, int hgt) {}

    @Override public void surfaceDestroyed(SurfaceHolder h) {
        // The native thread must be done with the surface before we return.
        stop();
        holder = null;
    }

    /** Letterboxes its child to a fixed aspect ratio so the desktop is never stretched. */
    static final class AspectFrame extends FrameLayout {
        // Shared with PanelActivity.
        private final int aw, ah;

        AspectFrame(Context c, int aw, int ah) {
            super(c);
            this.aw = aw;
            this.ah = ah;
        }

        @Override protected void onMeasure(int wSpec, int hSpec) {
            int w = MeasureSpec.getSize(wSpec), h = MeasureSpec.getSize(hSpec);
            if ((long) w * ah > (long) h * aw) {
                w = (int) ((long) h * aw / ah);
            } else {
                h = (int) ((long) w * ah / aw);
            }
            super.onMeasure(MeasureSpec.makeMeasureSpec(w, MeasureSpec.EXACTLY),
                    MeasureSpec.makeMeasureSpec(h, MeasureSpec.EXACTLY));
        }
    }
}
