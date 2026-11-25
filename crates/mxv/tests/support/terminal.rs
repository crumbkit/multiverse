//! Runs a command at a pseudo-terminal that the `script` program opens, so
//! its standard input and standard error are both terminals, as they are
//! for a person who types the command.

use serde_json::Value;
use snapbox::cmd::Command;

/// Returns a `script` command that runs `words` at a pseudo-terminal,
/// prints nothing of its own and exits with the code of the command.
#[cfg(target_os = "macos")]
pub(crate) fn script(words: &[String]) -> Command {
    Command::new("script")
        .args(["-q", "-e", "/dev/null"])
        .args(words)
}

/// Returns a `script` command that runs `words` at a pseudo-terminal,
/// prints nothing of its own and exits with the code of the command.
/// util-linux's `script` takes the command as one line of shell, so each
/// word is quoted.
#[cfg(not(target_os = "macos"))]
pub(crate) fn script(words: &[String]) -> Command {
    let line = words
        .iter()
        .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ");
    Command::new("script").args(["-q", "-e", "-c", &line, "/dev/null"])
}

/// Returns the JSON document in `text`, what a command that printed one
/// document showed at the terminal, or panics naming `what`. The terminal
/// joins standard output and standard error, ends lines with a carriage
/// return, and echoes the end of input that `script` passes on, so the
/// document is the text between the first brace and the last.
pub(crate) fn document(what: &str, text: &str) -> Value {
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        panic!("{what} printed no JSON document at the terminal: {text}");
    };
    serde_json::from_str(&text[start..=end])
        .unwrap_or_else(|error| panic!("{what} at the terminal: {error}: {text}"))
}
