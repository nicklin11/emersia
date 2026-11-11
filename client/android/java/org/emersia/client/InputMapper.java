package org.emersia.client;

import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;

/**
 * Turns panel pointer events (Quest controller ray, hand pinch, touch, mouse)
 * and key events into host input via {@link NativeBridge}.
 *
 * <p>Pointer coordinates are normalised to the video view, so they are
 * relative to the mirrored output whatever the panel size. Keys travel as Linux
 * evdev codes: a hardware keyboard's scan code already is one; keys without a
 * scan code (virtual keyboards) go through a small key-code table.
 */
final class InputMapper {
    private final View video;
    private long handle;
    private int heldButton = -1;

    InputMapper(View video) {
        this.video = video;
    }

    void setHandle(long handle) {
        this.handle = handle;
        heldButton = -1;
    }

    private boolean pointer(MotionEvent e) {
        int w = video.getWidth(), h = video.getHeight();
        if (handle == 0 || w <= 0 || h <= 0) return false;
        NativeBridge.nativePointer(handle, e.getX() / w, e.getY() / h);
        return true;
    }

    /** Hover (ray pointing without pinching) and wheel/thumbstick scroll. */
    boolean onGenericMotion(MotionEvent e) {
        if (handle == 0) return false;
        switch (e.getActionMasked()) {
            case MotionEvent.ACTION_HOVER_ENTER:
            case MotionEvent.ACTION_HOVER_MOVE:
                return pointer(e);
            case MotionEvent.ACTION_SCROLL: {
                pointer(e);
                float v = e.getAxisValue(MotionEvent.AXIS_VSCROLL);
                float hz = e.getAxisValue(MotionEvent.AXIS_HSCROLL);
                // Android: +VSCROLL is up; evdev hi-res wheel: + is down here.
                if (v != 0 || hz != 0) NativeBridge.nativeScroll(handle, hz, -v);
                return true;
            }
            default:
                return false;
        }
    }

    /** Press/drag/release: pinch or trigger on Quest, finger or mouse elsewhere. */
    boolean onTouch(MotionEvent e) {
        if (handle == 0) return false;
        switch (e.getActionMasked()) {
            case MotionEvent.ACTION_DOWN: {
                pointer(e);
                int buttons = e.getButtonState();
                heldButton = (buttons & MotionEvent.BUTTON_SECONDARY) != 0 ? 1
                        : (buttons & MotionEvent.BUTTON_TERTIARY) != 0 ? 2 : 0;
                NativeBridge.nativeButton(handle, heldButton, true);
                return true;
            }
            case MotionEvent.ACTION_MOVE:
                return pointer(e);
            case MotionEvent.ACTION_UP:
            case MotionEvent.ACTION_CANCEL:
                pointer(e);
                if (heldButton >= 0) {
                    NativeBridge.nativeButton(handle, heldButton, false);
                    heldButton = -1;
                }
                return true;
            default:
                return false;
        }
    }

    /** Returns true when the key was forwarded (and so consumed). */
    boolean onKey(KeyEvent e) {
        if (handle == 0) return false;
        int code = e.getKeyCode();
        // Leave system navigation to Horizon OS.
        if (code == KeyEvent.KEYCODE_BACK || code == KeyEvent.KEYCODE_HOME
                || code == KeyEvent.KEYCODE_VOLUME_UP || code == KeyEvent.KEYCODE_VOLUME_DOWN
                || (e.getSource() & InputDevice.SOURCE_GAMEPAD) == InputDevice.SOURCE_GAMEPAD) {
            return false;
        }
        boolean down;
        if (e.getAction() == KeyEvent.ACTION_DOWN) {
            if (e.getRepeatCount() > 0) return true; // host autorepeat handles it
            down = true;
        } else if (e.getAction() == KeyEvent.ACTION_UP) {
            down = false;
        } else {
            return false;
        }
        int scan = e.getScanCode();
        if (scan > 0 && scan <= 0x2ff) {
            NativeBridge.nativeKey(handle, scan, down);
            return true;
        }
        int evdev = Keymap.evdev(code);
        if (evdev == 0) return false;
        // Virtual keyboards report shifted characters on the base key.
        boolean shift = e.isShiftPressed() && !Keymap.isModifier(code);
        if (shift && down) NativeBridge.nativeKey(handle, Keymap.KEY_LEFTSHIFT, true);
        NativeBridge.nativeKey(handle, evdev, down);
        if (shift && !down) NativeBridge.nativeKey(handle, Keymap.KEY_LEFTSHIFT, false);
        return true;
    }
}
