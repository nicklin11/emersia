package org.emersia.client;

import android.app.Activity;
import android.graphics.Color;
import android.os.Bundle;
import android.view.KeyEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.WindowManager;
import android.widget.FrameLayout;

/**
 * A secondary Horizon OS panel showing one extra stream (another captured
 * output). It owns only a Surface: the transport session belongs to
 * {@link MainActivity}, and this panel attaches its Surface to stream N of it.
 */
public final class PanelActivity extends Activity implements SurfaceHolder.Callback {
    static final String EXTRA_STREAM = "stream";
    private static final int VIDEO_W = 1920, VIDEO_H = 1080;

    private int stream;
    private InputMapper input;
    private boolean attached;

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        stream = getIntent().getIntExtra(EXTRA_STREAM, 1);
        setTitle("Emersia — screen " + (stream + 1));
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);
        MainActivity.AspectFrame frame = new MainActivity.AspectFrame(this, VIDEO_W, VIDEO_H);
        SurfaceView video = new SurfaceView(this);
        frame.addView(video, new FrameLayout.LayoutParams(-1, -1));
        root.addView(frame, new FrameLayout.LayoutParams(-1, -1, android.view.Gravity.CENTER));
        setContentView(root);
        input = new InputMapper(video, stream);
        video.setOnTouchListener((v, e) -> input.onTouch(e));
        video.setOnGenericMotionListener((v, e) -> input.onGenericMotion(e));
        video.getHolder().setFixedSize(VIDEO_W, VIDEO_H);
        video.getHolder().addCallback(this);
        MainActivity.panelOpened(stream);
    }

    @Override public void surfaceCreated(SurfaceHolder h) {
        long session = NativeBridge.session;
        if (session != 0 && NativeBridge.nativeAttach(session, stream, h.getSurface())) {
            attached = true;
            input.setHandle(session);
        } else {
            finish(); // no session to show anything from
        }
    }

    @Override public void surfaceChanged(SurfaceHolder h, int f, int w, int hgt) {}

    @Override public void surfaceDestroyed(SurfaceHolder h) {
        detach();
    }

    private void detach() {
        if (attached) {
            input.setHandle(0);
            NativeBridge.nativeDetach(NativeBridge.session, stream);
            attached = false;
        }
    }

    @Override public boolean dispatchKeyEvent(KeyEvent e) {
        return input.onKey(e) || super.dispatchKeyEvent(e);
    }

    @Override protected void onDestroy() {
        detach();
        MainActivity.panelClosed(stream);
        super.onDestroy();
    }
}
