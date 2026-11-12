//! What the record commands of extensions share: reading their words, the configuration and the
//! standard input, and saying what went wrong. Like the `user` commands, their writes go through
//! the cluster's api port.

use crate::accounts::STATUS_MISTAKE;
use crate::config::{self, ServerConfig};
use std::io::IsTerminal;
use std::path::Path;

/// The words of a command line: the action, `--config` (or the default configuration), the
/// options it may take and one positional argument.
pub struct Words<'a> {
    pub action: &'a str,
    pub config: &'a str,
    options: Vec<(&'a str, &'a str)>,
    pub subject: &'a str,
}

impl Words<'_> {
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options.iter().find(|(option, _)| *option == name).map(|(_, value)| *value)
    }
}

/// Reads `args` with `options` as the options besides `--config`, each given at most once.
pub fn words<'a>(args: &'a [String], options: &[&str], usage: &str) -> Result<Words<'a>, String> {
    let mut args = args.iter();
    let action = args.next().map(String::as_str).ok_or_else(|| usage.to_string())?;
    let (mut config, mut found, mut subject) = (None, Vec::new(), None);
    while let Some(arg) = args.next() {
        let arg = arg.as_str();
        if arg == "--config" && config.is_none() {
            config = Some(args.next().ok_or_else(|| format!("--config needs a value\n{usage}"))?.as_str());
        } else if options.contains(&arg) && !found.iter().any(|(option, _)| *option == arg) {
            found.push((arg, args.next().ok_or_else(|| format!("{arg} needs a value\n{usage}"))?.as_str()));
        } else if subject.is_none() && !arg.starts_with("--") {
            subject = Some(arg);
        } else {
            return Err(format!("unexpected argument '{arg}'\n{usage}"));
        }
    }
    let Some(subject) = subject else {
        return Err(format!("{} is required\n{usage}", if options.is_empty() { "one argument" } else { "the last argument" }));
    };
    let config = config.unwrap_or(config::default_config_path());
    Ok(Words { action, config, options: found, subject })
}

pub fn load(path: &str) -> Result<ServerConfig, String> {
    config::load(Path::new(path)).map_err(|e| e.to_string())
}

/// The statement for `action`, or why there is none.
pub fn configured<'a>(sql: Option<&'a String>, setting: &str, config_path: &str) -> Result<&'a str, String> {
    sql.map(String::as_str).ok_or_else(|| format!("accounts.{setting} is not set in {config_path}"))
}

/// Prints a mistake found before anything was sent and returns its status.
pub fn mistake(e: String) -> i32 {
    eprintln!("{e}");
    STATUS_MISTAKE
}

/// The standard input, and whether it is a terminal.
pub fn input() -> (std::io::StdinLock<'static>, bool) {
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    (stdin.lock(), terminal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_words() -> Result<(), String> {
        let args = |line: &str| line.split(' ').map(str::to_string).collect::<Vec<_>>();
        let given = args("add --name ap-5 10.20.1.5 --config c.toml");
        let read = words(&given, &["--name"], "usage")?;
        assert_eq!((read.action, read.config, read.subject, read.option("--name")), ("add", "c.toml", "10.20.1.5", Some("ap-5")));
        let without = args("add --name ap-5 10.20.1.5");
        assert_eq!(words(&without, &["--name"], "usage")?.config, config::default_config_path());
        for wrong in ["add --config c.toml", "add", "add --config c.toml a b", "add --config c.toml --name x --name y a", "add --config c.toml --other x a", "add --config"] {
            assert!(words(&args(wrong), &["--name"], "usage").is_err(), "{wrong}");
        }
        Ok(())
    }
}
