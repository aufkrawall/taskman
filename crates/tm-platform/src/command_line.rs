//! Minimal command-line tokenization for shell-free launches.
//!
//! The Run dialog accepts a command line, but Unix launch paths must not pass
//! that text to a shell. This parser supports whitespace, single/double quotes,
//! and backslash escapes while deliberately treating every shell operator as
//! an ordinary argument byte.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandLineError {
    message: &'static str,
}

impl CommandLineError {
    #[cfg(test)]
    pub(crate) fn message(&self) -> &'static str {
        self.message
    }
}

impl std::fmt::Display for CommandLineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for CommandLineError {}

/// Split a command line without invoking a shell.
///
/// Variable expansion, globbing, redirection, pipelines, and command
/// substitution are intentionally unsupported. They remain literal argument
/// text and therefore cannot create another command.
pub(crate) fn split(command_line: &str) -> Result<Vec<String>, CommandLineError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote = Quote::None;
    let mut chars = command_line.chars();

    while let Some(ch) = chars.next() {
        match quote {
            Quote::None => match ch {
                '\'' => {
                    quote = Quote::Single;
                    started = true;
                }
                '"' => {
                    quote = Quote::Double;
                    started = true;
                }
                '\\' => {
                    let Some(escaped) = chars.next() else {
                        return Err(CommandLineError {
                            message: "trailing escape in command line",
                        });
                    };
                    current.push(escaped);
                    started = true;
                }
                ch if ch.is_whitespace() => {
                    if started {
                        args.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                ch => {
                    current.push(ch);
                    started = true;
                }
            },
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => {
                    let Some(escaped) = chars.next() else {
                        return Err(CommandLineError {
                            message: "trailing escape in command line",
                        });
                    };
                    // Inside double quotes a backslash only escapes the
                    // characters that still carry a meaning there (`$`,
                    // backtick, `"`, `\`); a newline is a line continuation.
                    // Before anything else the backslash is literal, so
                    // `grep "\."` keeps the pattern it was typed with instead
                    // of silently becoming `grep .`.
                    if matches!(escaped, '$' | '`' | '"' | '\\') {
                        current.push(escaped);
                    } else if escaped != '\n' {
                        current.push('\\');
                        current.push(escaped);
                    }
                }
                ch => current.push(ch),
            },
        }
    }

    if quote != Quote::None {
        return Err(CommandLineError {
            message: "unterminated quote in command line",
        });
    }
    if started {
        args.push(current);
    }
    if args.is_empty() {
        return Err(CommandLineError {
            message: "empty command line",
        });
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_escapes_produce_direct_arguments() {
        assert_eq!(
            split(r#"firefox --new-window "two words" 'three words' four\ five"#).unwrap(),
            vec![
                "firefox".to_string(),
                "--new-window".to_string(),
                "two words".to_string(),
                "three words".to_string(),
                "four five".to_string()
            ]
        );
        assert_eq!(
            split(r#""quoted program" "" tail\ slash"#).unwrap(),
            vec![
                "quoted program".to_string(),
                String::new(),
                "tail slash".to_string()
            ]
        );
    }

    /// POSIX keeps a backslash inside double quotes unless it precedes `$`,
    /// a backtick, `"`, `\` or a newline. Anything else is literal, so the
    /// pattern the user typed survives instead of being silently unescaped.
    #[test]
    fn double_quotes_only_strip_the_escapes_that_are_special_there() {
        assert_eq!(
            split(r#"grep "\." "a\nb" "q\"x" "back\\slash" "d\$e""#).unwrap(),
            vec![
                "grep".to_string(),
                r"\.".to_string(),
                r"a\nb".to_string(),
                "q\"x".to_string(),
                r"back\slash".to_string(),
                "d$e".to_string()
            ]
        );
    }

    #[test]
    fn shell_operators_remain_literal_arguments() {
        assert_eq!(
            split("echo a; rm -rf /tmp/x | cat $(id) > out").unwrap(),
            vec![
                "echo".to_string(),
                "a;".to_string(),
                "rm".to_string(),
                "-rf".to_string(),
                "/tmp/x".to_string(),
                "|".to_string(),
                "cat".to_string(),
                "$(id)".to_string(),
                ">".to_string(),
                "out".to_string()
            ]
        );
    }

    #[test]
    fn malformed_quoting_is_rejected() {
        assert_eq!(
            split("echo 'unterminated").unwrap_err().message(),
            "unterminated quote in command line"
        );
        assert_eq!(
            split("echo trailing\\").unwrap_err().message(),
            "trailing escape in command line"
        );
        assert_eq!(split("   ").unwrap_err().message(), "empty command line");
    }
}
