//! `emersia virtual add|remove|list`: headset-only monitors.
//!
//! Runs in the CLI, not the daemon, because it talks to the compositor's own
//! IPC (swaymsg / hyprctl), whose socket is known in the user's session
//! environment. The daemon notices the new output by itself (hot plug) and can
//! stream it: `emersia start DP-1 HEADLESS-1`. See docs/design/virtual-outputs.md.

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    Sway,
    Hyprland,
    Niri,
    Unknown,
}

/// Which compositor this session runs, from the IPC variables each sets.
pub fn detect(env: impl Fn(&str) -> Option<String>) -> Compositor {
    if env("SWAYSOCK").is_some() {
        Compositor::Sway
    } else if env("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        Compositor::Hyprland
    } else if env("NIRI_SOCKET").is_some()
        || env("XDG_CURRENT_DESKTOP").is_some_and(|d| d.eq_ignore_ascii_case("niri"))
    {
        Compositor::Niri
    } else {
        Compositor::Unknown
    }
}

/// Parse `WIDTHxHEIGHT`.
pub fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    ((320..=7680).contains(&w) && (240..=4320).contains(&h)).then_some((w, h))
}

/// Names present in `after` but not in `before`.
pub fn new_names(before: &[String], after: &[String]) -> Vec<String> {
    after
        .iter()
        .filter(|n| !before.contains(n))
        .cloned()
        .collect()
}

const NIRI_EXPLANATION: &str = "\
niri cannot create virtual outputs (as of 26.04). Options:
  - light up an unused GPU connector with an EDID override (kernel command line:
    drm.edid_firmware=HDMI-A-2:edid/2560x1440.bin video=HDMI-A-2:e), which niri
    then treats as a real monitor;
  - a dummy HDMI/DP plug (EDID emulator) does the same in hardware;
  - or mirror the outputs you have: emersia start DP-1 HDMI-A-1.
See docs/design/virtual-outputs.md.";

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("could not run {cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn output_names(c: Compositor) -> Result<Vec<String>, String> {
    let json = match c {
        Compositor::Sway => run("swaymsg", &["-r", "-t", "get_outputs"])?,
        Compositor::Hyprland => run("hyprctl", &["monitors", "all", "-j"])?,
        _ => return Ok(Vec::new()),
    };
    let v: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("unexpected output list: {e}"))?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| o["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// Create a virtual output; returns its name.
pub fn add(c: Compositor, size: (u32, u32)) -> Result<String, String> {
    let (w, h) = size;
    let before = output_names(c)?;
    match c {
        Compositor::Sway => {
            run("swaymsg", &["create_output"])?;
        }
        Compositor::Hyprland => {
            run("hyprctl", &["output", "create", "headless"])?;
        }
        Compositor::Niri => return Err(NIRI_EXPLANATION.into()),
        Compositor::Unknown => {
            return Err("unknown compositor: virtual outputs need sway or hyprland".into())
        }
    }
    let name = new_names(&before, &output_names(c)?)
        .into_iter()
        .next()
        .ok_or("the compositor did not report a new output")?;
    let mode = match c {
        Compositor::Sway => run("swaymsg", &["output", &name, "mode", &format!("{w}x{h}")]),
        _ => run(
            "hyprctl",
            &["keyword", "monitor", &format!("{name},{w}x{h}@60,auto,1")],
        ),
    };
    if let Err(e) = mode {
        let _ = remove(c, &name);
        return Err(format!("could not set {w}x{h} on {name}: {e}"));
    }
    Ok(name)
}

pub fn remove(c: Compositor, name: &str) -> Result<(), String> {
    match c {
        Compositor::Sway => run("swaymsg", &["output", name, "unplug"]).map(|_| ()),
        Compositor::Hyprland => run("hyprctl", &["output", "remove", name]).map(|_| ()),
        Compositor::Niri => Err(NIRI_EXPLANATION.into()),
        Compositor::Unknown => Err("unknown compositor".into()),
    }
}

/// Virtual outputs currently present (headless outputs).
pub fn list(c: Compositor) -> Result<Vec<String>, String> {
    if c == Compositor::Niri {
        return Err(NIRI_EXPLANATION.into());
    }
    Ok(output_names(c)?
        .into_iter()
        .filter(|n| n.starts_with("HEADLESS"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detects_compositor_from_its_ipc_variable() {
        assert_eq!(detect(env(&[("SWAYSOCK", "/run/x")])), Compositor::Sway);
        assert_eq!(
            detect(env(&[("HYPRLAND_INSTANCE_SIGNATURE", "abc")])),
            Compositor::Hyprland
        );
        assert_eq!(detect(env(&[("NIRI_SOCKET", "/run/n")])), Compositor::Niri);
        assert_eq!(
            detect(env(&[("XDG_CURRENT_DESKTOP", "niri")])),
            Compositor::Niri
        );
        assert_eq!(detect(env(&[])), Compositor::Unknown);
    }

    #[test]
    fn parses_sizes_within_sane_bounds() {
        assert_eq!(parse_size("2560x1440"), Some((2560, 1440)));
        assert_eq!(parse_size("1920X1080"), Some((1920, 1080)));
        assert_eq!(parse_size("10x10"), None);
        assert_eq!(parse_size("wide"), None);
    }

    #[test]
    fn finds_the_new_output() {
        let before = vec!["DP-1".to_string()];
        let after = vec!["DP-1".to_string(), "HEADLESS-1".to_string()];
        assert_eq!(new_names(&before, &after), vec!["HEADLESS-1".to_string()]);
    }

    #[test]
    fn niri_explains_instead_of_failing_silently() {
        let err = add(Compositor::Niri, (1920, 1080)).unwrap_err();
        assert!(err.contains("EDID"));
    }
}
