//! Process-level contract test for the `emersia-daemon` binary.
//!
//! Covers the parts of the CLI surface that need no Wayland session (help,
//! version, usage errors, exit codes) so they stay enforced in CI. The
//! capture paths need a live compositor and are exercised manually; see the
//! M1.1/M1.2 PRs for that evidence.

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_emersia-daemon"))
        .args(args)
        .output()
        .expect("emersia-daemon binary runs")
}

#[test]
fn help_and_version_exit_zero() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["--version"],
        vec!["-V"],
        vec![],
    ] {
        let out = run(&args);
        assert!(out.status.success(), "{args:?} should exit 0");
    }
    let help_out = run(&["--help"]);
    let help = String::from_utf8_lossy(&help_out.stdout);
    assert!(help.contains("serve"), "help documents serve mode");
    assert!(help.contains("capture"), "help documents capture mode");
}

#[test]
fn unknown_subcommand_exits_two() {
    let out = run(&["bogus"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown argument"), "{stderr}");
}

#[test]
fn unknown_flag_exits_two_and_names_the_mode() {
    let out = run(&["serve", "--nope"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("'serve'"), "error names the mode: {stderr}");
}

#[test]
fn missing_flag_values_exit_two() {
    for args in [
        ["capture", "--out"],
        ["capture", "--backend"],
        ["serve", "--socket"],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?} should exit 2");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("missing value"), "{args:?}: {stderr}");
    }
}

#[test]
fn invalid_backend_value_exits_two() {
    let out = run(&["capture", "--backend", "vulkan"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("vulkan"), "{stderr}");
}

#[test]
fn serve_without_runtime_dir_exits_one() {
    // Runtime failure (missing prerequisite), not a usage error.
    let out = Command::new(env!("CARGO_BIN_EXE_emersia-daemon"))
        .arg("serve")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("daemon runs");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("XDG_RUNTIME_DIR") || stderr.contains("compositor"),
        "clear prerequisite error: {stderr}"
    );
}
