//! `emersia` — local control client for the Emersia host daemon (stub).
//!
//! The control socket API is designed in `docs/design/control-api.md`
//! (M0) and implemented against the daemon in milestone M1.

/// Crate version, e.g. `0.0.1`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

const COMMANDS: &[&str] = &[
    "status", "pair", "devices", "revoke", "screens", "select", "start", "stop", "events",
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    match cmd {
        "--version" | "-V" => println!("emersia {VERSION}"),
        "help" | "--help" | "-H" => print_help(),
        c if COMMANDS.contains(&c) => {
            eprintln!(
                "`emersia {c}` is not implemented yet: the control socket ships in milestone M1."
            );
            std::process::exit(1);
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!("emersia {VERSION} — Emersia host control CLI (stub)");
    println!("\nUsage: emersia <command>\n");
    println!("Commands:");
    println!("  status    show daemon and stream state");
    println!("  pair      pair a headset (new|accept <code>)");
    println!("  devices   list paired headsets");
    println!("  revoke    revoke a paired headset");
    println!("  screens   list capturable outputs/windows");
    println!("  select    choose capture targets");
    println!("  start     start streaming");
    println!("  stop      stop streaming");
    println!("  events    follow daemon events (--follow)");
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, VERSION};

    #[test]
    fn version_is_semver() {
        assert_eq!(
            VERSION.matches('.').count(),
            2,
            "expected MAJOR.MINOR.PATCH"
        );
    }

    #[test]
    fn core_commands_are_wired() {
        for c in ["status", "pair", "start", "stop", "devices"] {
            assert!(COMMANDS.contains(&c), "{c} missing from COMMANDS");
        }
    }
}
