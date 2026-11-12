// Emersia companion UI. Talks only to the Rust backend (which talks to the
// daemon's local control socket); no network access of its own.
const invoke = window.__TAURI__.core.invoke;
const $ = (id) => document.getElementById(id);

function showError(e) {
  const el = $("error");
  if (e) {
    el.textContent = String(e);
    el.hidden = false;
  } else {
    el.hidden = true;
  }
}

function stat(label, value) {
  const dt = document.createElement("dt");
  dt.textContent = label;
  const dd = document.createElement("dd");
  dd.textContent = value;
  return [dt, dd];
}

let knownOutputs = "";

/** "just now", "5 min ago", … for a Unix time in seconds. */
function ago(unix) {
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} days ago`;
}

/** First-run guidance: only what is actually wrong or not yet proven. */
function renderSetup(setup, devices) {
  const items = [];
  const item = (text, command) => {
    const li = document.createElement("li");
    li.textContent = text;
    if (command) {
      const code = document.createElement("code");
      code.textContent = command;
      li.append(code);
    }
    items.push(li);
  };
  if (setup && setup.input_enabled && !setup.uinput_writable) {
    item(
      "Headset input is off: this user cannot write /dev/uinput. Add yourself to the input group, then log out and back in:",
      "sudo usermod -aG input $USER",
    );
  }
  const everConnected = devices.some((d) => d.last_seen);
  if (setup && setup.stream_port && !everConnected) {
    item(
      `No headset has connected yet. If a firewall is active, allow UDP port ${setup.stream_port} (e.g. ufw):`,
      `sudo ufw allow proto udp to any port ${setup.stream_port} comment 'Emersia stream'`,
    );
  }
  $("setup-items").replaceChildren(...items);
  $("setup").hidden = items.length === 0;
}

async function refresh() {
  try {
    const s = await invoke("status");
    const sharing = s.streaming && s.connected_devices > 0;
    $("badge").textContent = sharing ? "Sharing" : s.streaming ? "Ready" : "Idle";
    $("badge").className = "badge " + (sharing ? "on" : s.streaming ? "ready" : "");
    const streams = (s.streams || []).join(" + ");
    $("summary").textContent = sharing
      ? `Your screen (${streams}) is visible in ${s.connected_devices} headset(s).`
      : s.streaming
        ? `Streaming ${streams}; no headset connected.`
        : "Not sharing.";
    const stats = $("stats");
    stats.replaceChildren(
      ...stat("Capture", `${(s.capture_measured_fps || 0).toFixed(1)} fps`),
      ...stat("Host latency", `${(s.avg_latency_ms || 0).toFixed(1)} ms`),
      ...stat("Encoder", s.encoder || "—"),
      ...stat("Headsets", String(s.connected_devices || 0)),
    );
    showError(null);

    const scr = await invoke("screens");
    const targets = (scr.screens || scr.outputs || []).map((o) => (typeof o === "string" ? { name: o } : o));
    // Outputs by name; windows (kind "window") by app and title.
    const label = (t) =>
      t.kind === "window" ? `Window — ${t.app_id ? t.app_id + ": " : ""}${t.title || "untitled"}` : t.name;
    const list = targets.map((t) => t.name);
    const key = targets.map((t) => t.name + "\u0000" + label(t)).join("\u0001");
    if (key !== knownOutputs) {
      knownOutputs = key;
      const sel = $("outputs");
      const previous = new Set(Array.from(sel.selectedOptions, (o) => o.value));
      sel.replaceChildren(
        ...targets.map((t, i) => {
          const opt = document.createElement("option");
          opt.value = t.name;
          opt.textContent = label(t);
          opt.selected = previous.size ? previous.has(t.name) : i === 0;
          return opt;
        }),
      );
      sel.size = Math.max(2, list.length);
    }

    const d = await invoke("devices");
    renderSetup(s.setup, d.devices || []);
    const ul = $("devices");
    ul.replaceChildren(
      ...(d.devices || [])
        .filter((dev) => !dev.revoked)
        .map((dev) => {
          const li = document.createElement("li");
          const name = document.createElement("span");
          name.textContent = `${dev.name}  ·  ${dev.last_seen ? "seen " + ago(dev.last_seen) : "never connected"}`;
          name.title = dev.id;
          // View-only headsets watch but cannot control the desktop (#140).
          const ctl = document.createElement("label");
          ctl.className = "small";
          const box = document.createElement("input");
          box.type = "checkbox";
          box.checked = dev.input !== false;
          box.onchange = async () => {
            await invoke("set_input", { device: dev.id, input: box.checked }).catch(showError);
            refresh();
          };
          ctl.append(box, " Control");
          ctl.title = "Unchecked: view-only — the headset can watch, its input is ignored";
          const btn = document.createElement("button");
          btn.textContent = "Revoke";
          btn.className = "danger small";
          btn.onclick = async () => {
            if (confirm(`Revoke ${dev.name}? It will be disconnected and must pair again.`)) {
              await invoke("revoke", { device: dev.id }).catch(showError);
              refresh();
            }
          };
          li.append(name, ctl, btn);
          return li;
        }),
    );
    if (!ul.children.length) {
      const li = document.createElement("li");
      li.className = "muted";
      li.textContent = "No headset paired yet.";
      ul.append(li);
    }
  } catch (e) {
    $("badge").textContent = "Offline";
    $("badge").className = "badge";
    $("summary").textContent = "";
    showError(e);
  }
}

$("start").onclick = async () => {
  const outputs = [...$("outputs").selectedOptions].map((o) => o.value);
  await invoke("start", { outputs, fps: 60 }).catch(showError);
  refresh();
};
$("stop").onclick = async () => {
  await invoke("stop").catch(showError);
  refresh();
};

// In-headset pairing (ADR 0008): show the code, the user compares it with the
// headset and decides here; the headset asks for its own confirmation.
let pairTimer = null;
let pendingDevice = null;

function endPairing(message) {
  clearInterval(pairTimer);
  pairTimer = null;
  pendingDevice = null;
  $("pair-code").textContent = "";
  $("pair-text").textContent = message || "";
  $("pair-yes").hidden = $("pair-no").hidden = true;
  setTimeout(() => ($("pairing").hidden = true), message ? 4000 : 0);
}

$("pair").onclick = async () => {
  try {
    await invoke("pair_open");
  } catch (e) {
    showError(e);
    return;
  }
  $("pairing").hidden = false;
  $("pair-text").textContent = "Open Emersia in the headset and choose “Pair with a PC”. Waiting…";
  $("pair-code").textContent = "";
  $("pair-yes").hidden = $("pair-no").hidden = true;
  pairTimer = setInterval(async () => {
    try {
      const p = await invoke("pair_pending");
      if (!p.open) return endPairing("Pairing window expired.");
      const first = (p.pending || [])[0];
      if (first && pendingDevice !== first.device) {
        pendingDevice = first.device;
        $("pair-text").textContent = `“${first.name}” wants to pair. Does the headset show exactly this code?`;
        $("pair-code").textContent = first.code;
        $("pair-yes").hidden = $("pair-no").hidden = false;
      }
    } catch (e) {
      showError(e);
    }
  }, 400);
};

async function decide(accept) {
  if (!pendingDevice) return;
  try {
    await invoke("pair_decide", { device: pendingDevice, accept });
    endPairing(accept ? "Paired. Confirm on the headset too." : "Rejected; nothing was paired.");
    refresh();
  } catch (e) {
    showError(e);
  }
}
$("pair-yes").onclick = () => decide(true);
$("pair-no").onclick = () => decide(false);
$("pair-cancel").onclick = async () => {
  await invoke("pair_close").catch(() => {});
  endPairing("");
};

refresh();
setInterval(refresh, 2000);
