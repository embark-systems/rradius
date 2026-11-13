//! `rradius-server user ...`: adds, changes, enables, disables, deletes and shows accounts in
//! the cluster's tables, with the statements `[accounts]` gives. Writes go through the cluster's
//! api port, so they reach every node, and the next login sees them.

use crate::config::{self, AccountSettings, PasswordScheme, ServerConfig};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;
use zeroize::Zeroizing;

pub const USAGE: &str = "usage: rradius-server user add|passwd|enable|disable|delete|show [--config <path>] <name>\n  add and passwd read the password from standard input";

pub(crate) const STATUS_NO_MATCH: i32 = 1;
pub(crate) const STATUS_MISTAKE: i32 = 2;
pub(crate) const STATUS_UNREACHABLE: i32 = 3;

/// How long the cluster has to answer, connecting included. Right after the leader stops,
/// the hiqlite client first tries the old leader and gives up on each connection after 5 s
/// before it asks for the new one, so 10 s ran out in a drill where the leader's container
/// was killed; the client only reconnects, it never sends a request twice.
pub(crate) const CLUSTER_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Add,
    Passwd,
    Enable,
    Disable,
    Delete,
    Show,
}

impl Action {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "add" => Self::Add,
            "passwd" => Self::Passwd,
            "enable" => Self::Enable,
            "disable" => Self::Disable,
            "delete" => Self::Delete,
            "show" => Self::Show,
            _ => return None,
        })
    }

    fn setting(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Passwd => "passwd",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Delete => "delete",
            Self::Show => "show",
        }
    }

    fn statement(self, accounts: &AccountSettings) -> Option<&str> {
        match self {
            Self::Add => accounts.add.as_deref(),
            Self::Passwd => accounts.passwd.as_deref(),
            Self::Enable => accounts.enable.as_deref(),
            Self::Disable => accounts.disable.as_deref(),
            Self::Delete => accounts.delete.as_deref(),
            Self::Show => accounts.show.as_deref(),
        }
    }

    fn takes_password(self) -> bool {
        matches!(self, Self::Add | Self::Passwd)
    }

    fn done(self, name: &str) -> String {
        match self {
            Self::Add => format!("added {name}"),
            Self::Passwd => format!("password changed for {name}"),
            Self::Enable => format!("enabled {name}"),
            Self::Disable => format!("disabled {name}"),
            Self::Delete => format!("deleted {name}"),
            Self::Show => String::new(),
        }
    }
}

/// What a command found to do, before anything is sent.
struct Prepared {
    action: Action,
    name: String,
    sql: String,
    password: Option<Zeroizing<String>>,
    cluster: config::ClusterSettings,
}

/// Runs `user` with its arguments, the word `user` not included. Prints the result and returns
/// the exit status.
pub async fn run(args: &[String]) -> i32 {
    crate::logging::init_for_command();
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let prepared = prepare(args, &mut stdin.lock(), terminal);
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            eprintln!("{e}");
            return STATUS_MISTAKE;
        }
    };
    let mut params: Vec<hiqlite::Param> = vec![prepared.name.clone().into()];
    if let Some(password) = &prepared.password {
        params.push(password.as_str().to_string().into());
    }
    let statement = Statement { sql: prepared.sql.clone(), params, show: prepared.action == Action::Show, done: prepared.action.done(&prepared.name), none: format!("no account {}", prepared.name) };
    finish(&prepared.cluster, statement).await
}

/// One statement a command sends, and what to print for its result.
pub struct Statement {
    pub sql: String,
    pub params: Vec<hiqlite::Param>,
    /// A query, whose rows are printed; otherwise a write, done when it changed a row.
    pub show: bool,
    /// Printed when a write changed a row.
    pub done: String,
    /// Printed when nothing matched.
    pub none: String,
}

/// Sends `statement` through the cluster, prints the result and returns the exit status.
pub async fn finish(cluster: &config::ClusterSettings, statement: Statement) -> i32 {
    let sent = tokio::time::timeout(CLUSTER_WAIT, send(cluster, &statement.sql, statement.params, statement.show, &statement.done)).await.ok();
    let (status, stdout, stderr) = report(sent, &statement.none);
    if !stdout.is_empty() {
        println!("{stdout}");
    }
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }
    status
}

/// The exit status and what to print on standard output and standard error, for what the
/// cluster answered, or `None` when it did not answer in time. `none` is printed when nothing
/// matched.
fn report(sent: Option<Result<Option<String>, hiqlite::Error>>, none: &str) -> (i32, String, String) {
    match sent {
        Some(Ok(Some(lines))) => (0, lines, String::new()),
        Some(Ok(None)) => (STATUS_NO_MATCH, none.to_string(), String::new()),
        Some(Err(e)) if crate::lua_engine::cluster_unavailable(&e) => (STATUS_UNREACHABLE, String::new(), format!("cannot reach the cluster: {e}")),
        Some(Err(e)) => (STATUS_NO_MATCH, String::new(), e.to_string()),
        None => (STATUS_UNREACHABLE, String::new(), format!("the cluster did not answer within {} s", CLUSTER_WAIT.as_secs())),
    }
}

/// Reads the arguments, the configuration and the password. Nothing is sent yet.
fn prepare(args: &[String], input: &mut impl BufRead, terminal: bool) -> Result<Prepared, String> {
    let mut args = args.iter();
    let action = args.next().and_then(|word| Action::parse(word)).ok_or_else(|| crate::cli::named(USAGE))?;
    let (mut config_path, mut name) = (None, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" if config_path.is_none() => config_path = Some(args.next().ok_or_else(|| format!("--config needs a value\n{}", crate::cli::named(USAGE)))?),
            _ if name.is_none() && !arg.starts_with("--") => name = Some(arg.as_str()),
            _ => return Err(format!("unexpected argument '{arg}'\n{}", crate::cli::named(USAGE))),
        }
    }
    let Some(name) = name else {
        return Err(format!("a name is required\n{}", crate::cli::named(USAGE)));
    };
    let config_path = config_path.map_or(config::default_config_path(), String::as_str);
    let config = config::load(Path::new(config_path)).map_err(|e| e.to_string())?;
    let sql = config.accounts.as_ref().and_then(|accounts| action.statement(accounts)).ok_or_else(|| format!("accounts.{} is not set in {config_path}", action.setting()))?.to_string();
    let name = account_name(&config, name)?;
    let password = match action.takes_password() {
        true => {
            let scheme = config.accounts.as_ref().map(|accounts| accounts.scheme).unwrap_or_default();
            Some(stored_password(scheme, &read_password(input, terminal)?)?)
        }
        false => None,
    };
    Ok(Prepared { action, name, sql, password, cluster: config.cluster })
}

/// The name a login under the default chain would look up.
fn account_name(config: &ServerConfig, given: &str) -> Result<String, String> {
    let username = config.auth.as_ref().and_then(|auth| auth.default.as_ref()).and_then(|chain| chain.username.as_ref());
    match username {
        Some(settings) => crate::auth_chain::account_name(settings, given).map_err(|e| format!("'{given}': {e}")),
        None if given.is_empty() => Err("the name is empty".to_string()),
        None => Ok(given.to_string()),
    }
}

/// Runs the statement through the cluster. `None` when nothing matched; otherwise what to print:
/// the rows of a query, or `done` for a write.
async fn send(cluster: &config::ClusterSettings, sql: &str, params: Vec<hiqlite::Param>, show: bool, done: &str) -> Result<Option<String>, hiqlite::Error> {
    let nodes = cluster.nodes.iter().map(|node| node.api.clone()).collect();
    let client = hiqlite::Client::remote(nodes, false, false, cluster.secret_api.clone(), false, None).await?;
    let sql = sql.to_string();
    let result = if show {
        let rows = client.query_consistent(sql, params).await;
        rows.and_then(|rows| rows.iter().map(row_lines).collect::<Result<Vec<_>, _>>()).map(|shown| (!shown.is_empty()).then(|| shown.join("\n\n")))
    } else {
        client.execute(sql, params).await.map(|affected| (affected > 0).then(|| done.to_string()))
    };
    let _ = client.shutdown().await;
    result
}

/// A row as `column=value` lines. A remote row keeps its column names to itself, except in
/// its serialized form.
fn row_lines(row: &hiqlite::Row<'_>) -> Result<String, hiqlite::Error> {
    #[derive(serde::Deserialize)]
    struct Columns {
        columns: Vec<Column>,
    }
    #[derive(serde::Deserialize)]
    struct Column {
        name: String,
        value: Value,
    }
    #[derive(serde::Deserialize)]
    enum Value {
        Null,
        Integer(i64),
        Real(f64),
        Text(String),
        Blob(Vec<u8>),
    }
    let hiqlite::Row::Owned(row) = row else {
        return Err(hiqlite::Error::Sqlite("expected a row from the cluster".into()));
    };
    let unreadable = |e: serde_json::Error| hiqlite::Error::Sqlite(format!("cannot read a row: {e}").into());
    let columns: Columns = serde_json::to_value(row).and_then(serde_json::from_value).map_err(unreadable)?;
    let lines = columns.columns.into_iter().map(|column| {
        let value = match column.value {
            Value::Null => String::new(),
            Value::Integer(v) => v.to_string(),
            Value::Real(v) => v.to_string(),
            Value::Text(v) => v,
            Value::Blob(v) => v.iter().map(|byte| format!("{byte:02x}")).collect(),
        };
        format!("{}={value}", column.name)
    });
    Ok(lines.collect::<Vec<_>>().join("\n"))
}

/// The password, from a terminal asked twice without echo, or else the first line of `input`.
pub fn read_password(input: &mut impl BufRead, terminal: bool) -> Result<Zeroizing<String>, String> {
    let password = if terminal {
        let first = prompt("password: ", input)?;
        let second = prompt("again: ", input)?;
        if *first != *second {
            return Err("the two passwords differ; nothing was changed".to_string());
        }
        first
    } else {
        read_line(input)?
    };
    if password.is_empty() {
        return Err("the password is empty; nothing was changed".to_string());
    }
    Ok(password)
}

pub(crate) fn read_line(input: &mut impl BufRead) -> Result<Zeroizing<String>, String> {
    let mut line = Zeroizing::new(String::new());
    input.read_line(&mut line).map_err(|_| "cannot read the password from standard input".to_string())?;
    let kept = line.trim_end_matches(['\n', '\r']).len();
    line.truncate(kept);
    Ok(line)
}

/// Asks on standard error with the terminal's echo off, and puts the echo back however it ends.
pub fn prompt(question: &str, input: &mut impl BufRead) -> Result<Zeroizing<String>, String> {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let echo = EchoOff::new();
    let line = read_line(input);
    drop(echo);
    eprintln!();
    line
}

/// The terminal's settings from before the echo was turned off, for `restore_and_end`.
static SAVED_TERMINAL: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

/// The signals that end the command at a prompt, where the echo is off.
const ENDING_SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGHUP];

/// Puts the terminal back and ends the process as the signal would have. Only async-signal-safe
/// calls: an atomic read, `tcsetattr`, `signal` and `raise`.
extern "C" fn restore_and_end(signal: libc::c_int) {
    // SAFETY: the settings were stored before this handler was installed, and these calls are
    // safe in a signal handler.
    unsafe {
        if let Some(settings) = SAVED_TERMINAL.get() {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, settings);
        }
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Turns off the echo of standard input, which must be a terminal, until dropped. A signal that
/// ends the process meanwhile puts it back first, or the shell would go on hiding what is typed.
struct EchoOff {
    settings: Option<libc::termios>,
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl EchoOff {
    fn new() -> Self {
        // SAFETY: tcgetattr and sigaction write into storage we own; tcsetattr and sigaction only
        // read what they are given. None of them keeps a pointer.
        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut settings) != 0 {
                return Self { settings: None, previous: Vec::new() };
            }
            // Every prompt reads the same settings, so the first one stored serves them all.
            let _ = SAVED_TERMINAL.set(settings);
            let mut previous = Vec::with_capacity(ENDING_SIGNALS.len());
            for signal in ENDING_SIGNALS {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = restore_and_end as *const () as libc::sighandler_t;
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, &action, &mut old) == 0 {
                    previous.push((signal, old));
                }
            }
            let mut quiet = settings;
            quiet.c_lflag &= !libc::ECHO;
            quiet.c_lflag |= libc::ECHONL;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet);
            Self { settings: Some(settings), previous }
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: as in `new`.
        unsafe {
            if let Some(settings) = &self.settings {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, settings);
            }
            for (signal, old) in &self.previous {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}

/// `password` in the form `scheme` stores it, which PAP accepts for the same password.
fn stored_password(scheme: PasswordScheme, password: &str) -> Result<Zeroizing<String>, String> {
    let stored = match scheme {
        PasswordScheme::Bcrypt => format!("{{crypt}}{}", pwhash::bcrypt::hash(password).map_err(|e| format!("cannot hash the password: {e}"))?),
        PasswordScheme::Sha512Crypt => format!("{{crypt}}{}", pwhash::sha512_crypt::hash(password).map_err(|e| format!("cannot hash the password: {e}"))?),
        PasswordScheme::Nthash => {
            let hash = crate::mschap::nt_password_hash(password);
            format!("{{nthash}}{}", hash.iter().map(|byte| format!("{byte:02X}")).collect::<String>())
        }
        PasswordScheme::Clear => format!("{{clear}}{password}"),
    };
    Ok(Zeroizing::new(stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passwords::verify_pap;

    #[test]
    fn test_reports() {
        let lines = |text: &str| Some(Ok(Some(text.to_string())));
        assert_eq!(report(lines("added alice"), "no account alice"), (0, "added alice".to_string(), String::new()));
        assert_eq!(report(Some(Ok(None)), "no account alice"), (1, "no account alice".to_string(), String::new()));
        let refused = report(Some(Err(hiqlite::Error::ConstraintViolation("UNIQUE constraint failed: users.username".into()))), "no account alice");
        assert_eq!((refused.0, refused.1.as_str()), (1, ""));
        assert!(refused.2.contains("UNIQUE"), "{}", refused.2);
        // A channel to the node that closed is the cluster's trouble, not a missing account.
        let closed = report(Some(Err(hiqlite::Error::Error("response channel closed".into()))), "no account alice");
        assert_eq!((closed.0, closed.1.as_str()), (3, ""), "{}", closed.2);
        let lost = report(Some(Err(hiqlite::Error::LeaderChange("no leader".into()))), "no account alice");
        assert_eq!((lost.0, lost.1.as_str()), (3, ""));
        assert!(lost.2.contains("cannot reach the cluster"), "{}", lost.2);
        let silent = report(None, "no account alice");
        assert_eq!((silent.0, silent.1.as_str()), (3, ""));
        assert!(silent.2.contains("did not answer"), "{}", silent.2);
    }

    #[test]
    fn test_stored_by_scheme() {
        for (scheme, prefix) in [
            (PasswordScheme::Bcrypt, "{crypt}$2"),
            (PasswordScheme::Sha512Crypt, "{crypt}$6$"),
            (PasswordScheme::Nthash, "{nthash}"),
            (PasswordScheme::Clear, "{clear}s3cret"),
        ] {
            let stored = stored_password(scheme, "s3cret").unwrap();
            assert!(stored.starts_with(prefix), "{scheme:?}: {}", *stored);
            assert!(verify_pap(&stored, b"s3cret"), "{scheme:?}");
            assert!(!verify_pap(&stored, b"S3cret"), "{scheme:?}");
        }
    }

    #[test]
    fn test_salted_values_differ() {
        for scheme in [PasswordScheme::Bcrypt, PasswordScheme::Sha512Crypt] {
            assert_ne!(*stored_password(scheme, "s3cret").unwrap(), *stored_password(scheme, "s3cret").unwrap(), "{scheme:?}");
        }
    }

    #[test]
    fn test_password_from_a_pipe() {
        let read = |text: &str| read_password(&mut text.as_bytes(), false).map(|password| password.to_string());
        assert_eq!(read("s3cret\n").unwrap(), "s3cret");
        assert_eq!(read("s3cret\r\nignored\n").unwrap(), "s3cret");
        assert_eq!(read("s3cret").unwrap(), "s3cret");
        assert!(read("\nsecond\n").unwrap_err().contains("empty"));
        assert!(read("").unwrap_err().contains("empty"));
    }

    #[test]
    fn test_password_asked_twice() {
        let read = |text: &str| read_password(&mut text.as_bytes(), true).map(|password| password.to_string());
        assert_eq!(read("s3cret\ns3cret\n").unwrap(), "s3cret");
        let error = read("s3cret\nother\n").unwrap_err();
        assert!(error.contains("differ") && !error.contains("s3cret") && !error.contains("other"), "{error}");
    }
}
