//! Command-line arguments.
//!
//! Hand-rolled on purpose: a single flag does not justify an argument-parsing
//! dependency, and this way the whole surface is visible in one screen.

/// Wayland `app_id` (X11 `WM_CLASS`) reported to the compositor unless
/// overridden.
pub(crate) const DEFAULT_APP_ID: &str = "slimnotes";

pub(crate) const USAGE: &str = "\
Usage: slimnotes [options]

Options:
      --app-id <ID>   Window app_id reported to the compositor
                      (default: slimnotes, or $SLIMNOTES_APP_ID).
                      Window rules can only match a window by this string, so a
                      distinct one lets one instance be treated differently —
                      e.g. launched into a scratchpad — from a normal one.
  -h, --help          Print this help.";

pub(crate) struct Args {
    /// Never empty: an empty app_id would be indistinguishable from not setting
    /// one, and window rules could not match it.
    pub(crate) app_id: String,
}

pub(crate) enum Parsed {
    Run(Args),
    /// `--help`: print [`USAGE`] and exit successfully.
    Usage,
    /// Bad invocation; the string is the reason, for stderr.
    Error(String),
}

/// `env_app_id` is `$SLIMNOTES_APP_ID`. The flag wins over it, so a window rule
/// can be driven from a launcher even inside an environment that sets the var.
pub(crate) fn parse(
    argv: impl IntoIterator<Item = String>,
    env_app_id: Option<String>,
) -> Parsed {
    let mut app_id = env_app_id.filter(|s| !s.is_empty());
    let mut argv = argv.into_iter();

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-h" | "--help" => return Parsed::Usage,
            "--app-id" => match argv.next() {
                Some(value) if !value.is_empty() => app_id = Some(value),
                Some(_) => return Parsed::Error("--app-id cannot be empty".into()),
                None => return Parsed::Error("--app-id needs a value".into()),
            },
            _ => match arg.strip_prefix("--app-id=") {
                Some("") => return Parsed::Error("--app-id cannot be empty".into()),
                Some(value) => app_id = Some(value.to_string()),
                // Unknown arguments are an error rather than being ignored: a
                // typo'd flag would otherwise silently leave the window
                // unmatched by the very rule it was meant to trigger.
                None => return Parsed::Error(format!("unrecognized argument: {arg}")),
            },
        }
    }

    Parsed::Run(Args {
        app_id: app_id.unwrap_or_else(|| DEFAULT_APP_ID.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_id(args: &[&str], env: Option<&str>) -> String {
        match parse(
            args.iter().map(|s| s.to_string()),
            env.map(|s| s.to_string()),
        ) {
            Parsed::Run(args) => args.app_id,
            Parsed::Usage => panic!("unexpected usage"),
            Parsed::Error(e) => panic!("unexpected error: {e}"),
        }
    }

    fn error(args: &[&str]) -> String {
        match parse(args.iter().map(|s| s.to_string()), None) {
            Parsed::Error(e) => e,
            _ => panic!("expected an error"),
        }
    }

    #[test]
    fn defaults_to_the_app_name() {
        assert_eq!(app_id(&[], None), DEFAULT_APP_ID);
        // An empty variable is treated as unset, not as an empty app_id.
        assert_eq!(app_id(&[], Some("")), DEFAULT_APP_ID);
    }

    #[test]
    fn flag_beats_environment() {
        assert_eq!(app_id(&["--app-id", "scratch"], Some("env")), "scratch");
        assert_eq!(app_id(&["--app-id=scratch"], Some("env")), "scratch");
        assert_eq!(app_id(&[], Some("env")), "env");
    }

    #[test]
    fn rejects_bad_invocations() {
        assert!(error(&["--app-id"]).contains("needs a value"));
        assert!(error(&["--app-id", ""]).contains("cannot be empty"));
        assert!(error(&["--app-id="]).contains("cannot be empty"));
        assert!(error(&["--vault", "x"]).contains("unrecognized"));
    }

    #[test]
    fn help_is_reported_separately() {
        assert!(matches!(
            parse(["--help".to_string()], None),
            Parsed::Usage
        ));
        assert!(matches!(parse(["-h".to_string()], None), Parsed::Usage));
    }
}
