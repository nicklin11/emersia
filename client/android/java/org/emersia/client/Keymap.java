package org.emersia.client;

import android.util.SparseIntArray;
import android.view.KeyEvent;

/** Android key codes → Linux evdev codes, for keys that carry no scan code. */
final class Keymap {
    static final int KEY_LEFTSHIFT = 42;
    private static final SparseIntArray MAP = new SparseIntArray();

    private static void put(int android, int evdev) {
        MAP.put(android, evdev);
    }

    static {
        // Letters: evdev follows the QWERTY layout, not the alphabet.
        int[] letters = {30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
                         49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44};
        for (int i = 0; i < 26; i++) put(KeyEvent.KEYCODE_A + i, letters[i]);
        // Digits 1..9 are evdev 2..10, 0 is 11.
        for (int i = 1; i <= 9; i++) put(KeyEvent.KEYCODE_0 + i, i + 1);
        put(KeyEvent.KEYCODE_0, 11);
        put(KeyEvent.KEYCODE_ESCAPE, 1);
        put(KeyEvent.KEYCODE_MINUS, 12);
        put(KeyEvent.KEYCODE_EQUALS, 13);
        put(KeyEvent.KEYCODE_DEL, 14);
        put(KeyEvent.KEYCODE_TAB, 15);
        put(KeyEvent.KEYCODE_LEFT_BRACKET, 26);
        put(KeyEvent.KEYCODE_RIGHT_BRACKET, 27);
        put(KeyEvent.KEYCODE_ENTER, 28);
        put(KeyEvent.KEYCODE_CTRL_LEFT, 29);
        put(KeyEvent.KEYCODE_SEMICOLON, 39);
        put(KeyEvent.KEYCODE_APOSTROPHE, 40);
        put(KeyEvent.KEYCODE_GRAVE, 41);
        put(KeyEvent.KEYCODE_SHIFT_LEFT, KEY_LEFTSHIFT);
        put(KeyEvent.KEYCODE_BACKSLASH, 43);
        put(KeyEvent.KEYCODE_COMMA, 51);
        put(KeyEvent.KEYCODE_PERIOD, 52);
        put(KeyEvent.KEYCODE_SLASH, 53);
        put(KeyEvent.KEYCODE_SHIFT_RIGHT, 54);
        put(KeyEvent.KEYCODE_ALT_LEFT, 56);
        put(KeyEvent.KEYCODE_SPACE, 57);
        put(KeyEvent.KEYCODE_CAPS_LOCK, 58);
        for (int i = 0; i < 10; i++) put(KeyEvent.KEYCODE_F1 + i, 59 + i);
        put(KeyEvent.KEYCODE_F11, 87);
        put(KeyEvent.KEYCODE_F12, 88);
        put(KeyEvent.KEYCODE_CTRL_RIGHT, 97);
        put(KeyEvent.KEYCODE_ALT_RIGHT, 100);
        put(KeyEvent.KEYCODE_MOVE_HOME, 102);
        put(KeyEvent.KEYCODE_DPAD_UP, 103);
        put(KeyEvent.KEYCODE_PAGE_UP, 104);
        put(KeyEvent.KEYCODE_DPAD_LEFT, 105);
        put(KeyEvent.KEYCODE_DPAD_RIGHT, 106);
        put(KeyEvent.KEYCODE_MOVE_END, 107);
        put(KeyEvent.KEYCODE_DPAD_DOWN, 108);
        put(KeyEvent.KEYCODE_PAGE_DOWN, 109);
        put(KeyEvent.KEYCODE_INSERT, 110);
        put(KeyEvent.KEYCODE_FORWARD_DEL, 111);
        put(KeyEvent.KEYCODE_META_LEFT, 125);
        put(KeyEvent.KEYCODE_META_RIGHT, 126);
    }

    private Keymap() {}

    /** evdev code, or 0 when unmapped. */
    static int evdev(int androidKeyCode) {
        return MAP.get(androidKeyCode, 0);
    }

    static boolean isModifier(int androidKeyCode) {
        return KeyEvent.isModifierKey(androidKeyCode);
    }
}
