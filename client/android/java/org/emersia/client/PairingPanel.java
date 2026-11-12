package org.emersia.client;

import android.content.Context;
import android.graphics.Color;
import android.graphics.Typeface;
import android.util.TypedValue;
import android.view.Gravity;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;

/**
 * In-headset pairing UI (ADR 0008). Shows what the native pairing state
 * machine reports: searching, the code to compare with the PC, then success
 * or failure. The headset user must tap "Codes match" before anything is
 * stored, independently of the PC's confirmation.
 */
final class PairingPanel extends LinearLayout {
    interface Listener {
        void onConfirm();
        void onCancel();
        void onRetry();
    }

    private final TextView title, code, detail;
    private final Button confirm, cancel, retry;

    PairingPanel(Context c, Listener listener) {
        super(c);
        setOrientation(VERTICAL);
        setGravity(Gravity.CENTER);
        setBackgroundColor(0xEE101418);
        int pad = dp(32);
        setPadding(pad, pad, pad, pad);

        title = text(c, 26, Typeface.BOLD);
        code = text(c, 64, Typeface.BOLD);
        code.setTypeface(Typeface.MONOSPACE, Typeface.BOLD);
        code.setLetterSpacing(0.1f);
        detail = text(c, 17, Typeface.NORMAL);
        detail.setTextColor(0xFFB8C0CC);
        addView(title);
        addView(code);
        addView(detail);

        LinearLayout buttons = new LinearLayout(c);
        buttons.setGravity(Gravity.CENTER);
        confirm = button(c, "Codes match", v -> listener.onConfirm());
        cancel = button(c, "Cancel", v -> listener.onCancel());
        retry = button(c, "Try again", v -> listener.onRetry());
        buttons.addView(confirm);
        buttons.addView(cancel);
        buttons.addView(retry);
        LayoutParams lp = new LayoutParams(-2, -2);
        lp.topMargin = dp(24);
        addView(buttons, lp);
    }

    private int dp(int v) {
        return (int) (v * getResources().getDisplayMetrics().density);
    }

    private TextView text(Context c, int sp, int style) {
        TextView t = new TextView(c);
        t.setTextColor(Color.WHITE);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, sp);
        t.setTypeface(Typeface.DEFAULT, style);
        t.setGravity(Gravity.CENTER);
        LayoutParams lp = new LayoutParams(-2, -2);
        lp.topMargin = dp(8);
        t.setLayoutParams(lp);
        return t;
    }

    private Button button(Context c, String label, OnClickListener l) {
        Button b = new Button(c);
        b.setText(label);
        b.setAllCaps(false);
        b.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
        b.setOnClickListener(l);
        LayoutParams lp = new LayoutParams(-2, -2);
        lp.leftMargin = lp.rightMargin = dp(8);
        b.setLayoutParams(lp);
        return b;
    }

    private void show(String t, String c, String d, boolean confirmB, boolean cancelB, boolean retryB) {
        title.setText(t);
        code.setText(c);
        code.setVisibility(c.isEmpty() ? GONE : VISIBLE);
        detail.setText(d);
        confirm.setVisibility(confirmB ? VISIBLE : GONE);
        cancel.setVisibility(cancelB ? VISIBLE : GONE);
        retry.setVisibility(retryB ? VISIBLE : GONE);
    }

    /** Render one native status line. */
    void render(String line) {
        String[] p = line.split("\\|", -1);
        switch (p[0]) {
            case "searching":
                show("Pair with a PC", "",
                        "On your PC run:\n\nemersia pair headset\n\n"
                                + "Both devices must be on the same network.", false, true, false);
                break;
            case "code": {
                boolean confirmed = p.length > 3 && p[3].equals("1");
                String host = p.length > 2 ? p[2] : "your PC";
                show("Pairing with " + host, p[1],
                        confirmed ? "Waiting for the PC to confirm…"
                                : "Does the PC show exactly this code?\nIf it differs, cancel — someone else may be answering.",
                        !confirmed, true, false);
                break;
            }
            case "rejected":
                show("Not paired", "", "The PC declined (or the codes did not match).", false, false, true);
                break;
            case "error":
                show("Pairing failed", "", p.length > 1 ? p[1] : "", false, false, true);
                break;
            default:
                show("Paired", "", "", false, false, false);
        }
    }
}
