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
    /// Leave the TUI.
    Quit,
    /// Unrecognized command word, echoed back for the hint.
    Unknown(String),
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
         /quit — leave the TUI\n\
         keys: Tab focus · Enter submit · Esc parks (never cancels) · \
         Ctrl+C cancels · Ctrl+D quits"
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
        for command in ["/help", "/model", "/usage", "/quit"] {
            assert!(help.contains(command), "{help:?}");
        }
    }
}
