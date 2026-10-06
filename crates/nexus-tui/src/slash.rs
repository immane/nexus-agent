//! Slash commands typed into the composer.
//!
//! A draft starting with `/` never reaches the runtime: it is routed
//! locally by [`SlashCommand::parse`]. Unknown commands are reported, never
//! guessed, and command names match exactly (lowercase).

/// Local composer command. Only [`SlashCommand::Quit`] leaves the loop;
/// everything else answers inline and keeps the session alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// List available commands.
    Help,
    /// Show the active model, or switch to the named configured model.
    Model(Option<String>),
    /// Show the last observed input/output token counters.
    Usage,
    /// Inspect or switch the local conversation session.
    Session(SessionArgs),
    /// Leave the TUI.
    Quit,
    /// Unrecognized command word, echoed back for the hint.
    Unknown(String),
}

/// `/session` subcommand. Sessions are local conversations, each with its
/// own runtime, event stream, and viewport history; switching never moves
/// runs, approvals, or drafts between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionArgs {
    /// Show the active session plus the full list.
    Show,
    /// Start a new empty session and switch to it.
    New,
    /// List every session with its index, label, and status.
    List,
    /// Switch by 1-based index (`2`), label (`s2`), or session id.
    Switch(String),
    /// Show session usage.
    Usage,
}

impl SessionArgs {
    /// Parses the arguments after `/session`. A bare command shows the
    /// active session; anything unrecognized explains usage instead of
    /// guessing a session.
    fn parse(args: &str) -> SessionArgs {
        let (word, rest) = match args.split_once(char::is_whitespace) {
            Some((word, rest)) => (word, rest.trim()),
            None => (args, ""),
        };
        match (word, rest) {
            ("", _) => SessionArgs::Show,
            ("new", "") => SessionArgs::New,
            ("list", "") => SessionArgs::List,
            ("switch", target) if !target.is_empty() => SessionArgs::Switch(target.to_owned()),
            ("help", _) => SessionArgs::Usage,
            _ => SessionArgs::Usage,
        }
    }

    /// Short usage listing, rendered as one transcript notice.
    #[must_use]
    pub fn usage_text() -> &'static str {
        "/session — show the active session\n\
         /session new — start a session and switch to it\n\
         /session list — list sessions\n\
         /session switch <n|sN|id> — switch sessions"
    }
}

impl SlashCommand {
    /// Parses a composer draft. Returns `None` for ordinary input; a
    /// leading `/` always routes locally, even for unknown words.
    #[must_use]
    pub fn parse(input: &str) -> Option<SlashCommand> {
        let rest = input.trim_start().strip_prefix('/')?;
        let (word, args) = match rest.split_once(char::is_whitespace) {
            Some((word, args)) => (word, args.trim()),
            None => (rest, ""),
        };
        let command = match word {
            "help" => SlashCommand::Help,
            "model" => SlashCommand::Model(if args.is_empty() {
                None
            } else {
                Some(args.to_owned())
            }),
            "usage" => SlashCommand::Usage,
            "session" => SlashCommand::Session(SessionArgs::parse(args)),
            "quit" => SlashCommand::Quit,
            _ => SlashCommand::Unknown(word.to_owned()),
        };
        Some(command)
    }

    /// Short help listing, rendered as one transcript notice.
    #[must_use]
    pub fn help_text() -> &'static str {
        "/help — list commands\n\
         /model [name] — show or switch the active model\n\
         /usage — show observed input/output tokens\n\
         /session [new|list|switch] — manage conversation sessions\n\
         /quit — leave the TUI\n\
         keys: Tab focus · Enter submit · Up/Down recall input · mouse wheel scrolls · Left/Right fold · Esc parks (never cancels) · \
         Ctrl+C clears the composer, then cancels a live run, then quits · Ctrl+D quits"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_input_never_routes() {
        for input in ["", "hello", "  hello  ", "a/b", "x /help"] {
            assert_eq!(SlashCommand::parse(input), None, "{input:?}");
        }
    }

    #[test]
    fn every_command_parses_with_and_without_arguments() {
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("  /help  "), Some(SlashCommand::Help));
        assert_eq!(
            SlashCommand::parse("/model"),
            Some(SlashCommand::Model(None))
        );
        assert_eq!(
            SlashCommand::parse("/model demo-fast"),
            Some(SlashCommand::Model(Some("demo-fast".to_owned())))
        );
        assert_eq!(
            SlashCommand::parse("/model   spaced-name  "),
            Some(SlashCommand::Model(Some("spaced-name".to_owned())))
        );
        assert_eq!(SlashCommand::parse("/usage"), Some(SlashCommand::Usage));
        assert_eq!(SlashCommand::parse("/quit"), Some(SlashCommand::Quit));
    }

    #[test]
    fn session_subcommands_parse_without_guessing() {
        use SessionArgs::*;
        assert_eq!(
            SlashCommand::parse("/session"),
            Some(SlashCommand::Session(Show))
        );
        assert_eq!(
            SlashCommand::parse("/session new"),
            Some(SlashCommand::Session(New))
        );
        assert_eq!(
            SlashCommand::parse("/session list"),
            Some(SlashCommand::Session(List))
        );
        assert_eq!(
            SlashCommand::parse("/session switch 2"),
            Some(SlashCommand::Session(Switch("2".to_owned())))
        );
        assert_eq!(
            SlashCommand::parse("/session switch s2"),
            Some(SlashCommand::Session(Switch("s2".to_owned())))
        );
        for malformed in [
            "/session bogus",
            "/session new extra",
            "/session switch",
            "/session help",
        ] {
            assert_eq!(
                SlashCommand::parse(malformed),
                Some(SlashCommand::Session(Usage)),
                "{malformed:?} explains usage instead of guessing"
            );
        }
    }

    #[test]
    fn unknown_words_are_reported_never_guessed() {
        assert_eq!(
            SlashCommand::parse("/frobnicate"),
            Some(SlashCommand::Unknown("frobnicate".to_owned()))
        );
        assert_eq!(
            SlashCommand::parse("//double"),
            Some(SlashCommand::Unknown("/double".to_owned()))
        );
        assert_eq!(
            SlashCommand::parse("/"),
            Some(SlashCommand::Unknown(String::new()))
        );
        assert_eq!(
            SlashCommand::parse("/MODEL"),
            Some(SlashCommand::Unknown("MODEL".to_owned()))
        );
    }

    #[test]
    fn help_text_names_every_command() {
        let help = SlashCommand::help_text();
        for command in ["/help", "/model", "/usage", "/session", "/quit"] {
            assert!(help.contains(command), "{help:?}");
        }
    }
}
