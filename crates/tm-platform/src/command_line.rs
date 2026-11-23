//! Minimal command-line tokenization shared by the Linux launch paths.
//!
//! Elevated launches must never hand a user-controlled string to a shell.
//! This parser supports the quoting users expect from the Run dialog while
//! deliberately leaving every shell operator as an ordinary argument byte.

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
/// Supported syntax is intentionally small: whitespace separates arguments,
/// single and double quotes group text, and backslash escapes the next byte.
/// There is no variable expansion, globbing, redirection, or command
/// substitution. A semicolon or pipe therefore stays inside an argument and
/// can never create a second privileged command.
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
                    current.push(escaped);
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
            [
                "firefox",
                "--new-window",
                "two words",
                "three words",
                "four five"
            ]
        );
        assert_eq!(
            split(r#""quoted program" "" tail\ slash"#).unwrap(),
            ["quoted program", "", "tail slash"]
        );
    }

    #[test]
    fn shell_operators_are_never_treated_as_commands() {
        assert_eq!(
            split("echo a; rm -rf /tmp/x | cat $(id)").unwrap(),
            ["echo", "a;", "rm", "-rf", "/tmp/x", "|", "cat", "$(id)"]
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
