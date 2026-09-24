# Emersia control API (design — M0, implemented in M1)

Local control plane between the `emersia` CLI (and later the Tauri GUI) and
`emersia-daemon`. See ADR 0005.

## Transport

- Unix domain socket: `$XDG_RUNTIME_DIR/emersia/control.sock`
  (mode `0600`, directory `0700`).
- Auth: `SO_PEERCRED` — peer must have the same UID as the daemon user.
  No TCP admin surface exists.
- Framing: **JSON lines** (one JSON object per line, UTF-8, `\n`-terminated).

## Request / response

```jsonc
→ {"v":1,"id":"42","cmd":"status"}
← {"v":1,"id":"42","ok":true,"status":{ … }}
← {"v":1,"id":"42","ok":false,"error":{"code":"not_paired","message":"…"}}
```

- `v` — protocol version (breaking changes bump it).
- `id` — client-chosen correlation id, echoed back; responses may arrive
  out of order.
- Errors: `code` is a stable machine token (`unknown_cmd`, `not_paired`,
  `busy`, `no_capture_backend`, `denied`, `internal`), `message` is for
  humans.

## Commands (M1 scope)

| Command | Purpose | Key payload |
|---|---|---|
| `status` | daemon + stream state | `streaming`, `backend` (`screencopy`/`image-copy-capture`/`portal`/`x11`), `encoder`, `latency_ms`, `measured_fps`, `capture_measured_fps`, `capture_window_frames`, `capture_window_elapsed_ms`, `capture_phases`, `connected_devices` |
| `pair` | start/accept pairing | `action`: `new`\|`accept`, `code` |
| `devices` | list paired devices | `id`, `name`, `paired_at`, `revoked` |
| `revoke` | remove a paired device | `device` |
| `screens` | list capturable outputs/windows | `kind`: `output`\|`window`, `name`, `size` |
| `select` | set capture targets | `targets: [...]` (empty = none: capture stops) |
| `start` / `stop` | begin/end capture streaming | — |
| `events` | subscribe to state changes | server then pushes `{"v":1,"event":…}` lines: `stream_state`, `device_connected`, `device_revoked`, `stats` |

`status` also exposes additive M1.6 measurement fields. `measured_fps` is
the legacy instantaneous capture sample used by the encoder timing logic;
`capture_measured_fps` is a session-scoped rate over successful captures in a
fixed two-second window. It is `0` until a complete window is available and
returns to `0` when no successful capture has arrived for a window, so a
compositor stall is not reported as a stale positive rate. `capture_window_frames`
and `capture_window_elapsed_ms` expose the active sample and elapsed time.
`capture_phases` contains the last successful capture's `total_ms`,
`constraints_ms`, `frame_wait_ms`, `shm_read_ms`, `decode_ms`, and
`transform_ms`. `frame_wait_ms` includes compositor scheduling and the copy,
because the protocol exposes no separate copy-start timestamp. The capture
window and phase sample reset when a new stream starts or a Wayland connection
is replaced; cumulative frame and encoder counters remain unchanged. These
fields are measurement evidence, not a hardware acceptance claim; live backend
selection still requires compositor measurements.

## CLI mapping

Every command maps 1:1: `emersia status`, `emersia pair new|accept`,
`emersia devices`, `emersia revoke <id>`, `emersia screens`,
`emersia select …`, `emersia start|stop`, `emersia events --follow`.

## Non-goals (M1)

Remote/TCP control, multi-user permissions, hot re-safety-locks — future
ADRs if needed.
