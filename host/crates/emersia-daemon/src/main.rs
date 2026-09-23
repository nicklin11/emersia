//! Emersia host daemon (stub — the streaming engine arrives in milestone M1).

/// Crate version, e.g. `0.0.1`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => println!("emersia-daemon {VERSION}"),
        _ => {
            println!("emersia-daemon {VERSION} (stub)");
            println!(
                "The streaming engine (capture, encode, transport, input) lands in milestone M1.\n\
                 See docs/adr/ for the architecture decisions and docs/design/control-api.md \
                 for the control socket design."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_is_semver() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH, got {VERSION}");
        for p in parts {
            assert!(
                p.chars().all(|c| c.is_ascii_digit()),
                "non-numeric part in {VERSION}"
            );
        }
    }
}
