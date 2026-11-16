//! `rradius-server session list|show`: the sessions online, with the statements `[sessions]`
//! gives. They read through the cluster's api port, so that the command starts no node, and
//! print what the statement selects as it is: the server keeps no table of sessions of its own.
//! `find` is what a command of the enterprise program uses to reach a user's sessions.

use crate::accounts::{self, query_rows, row_text, Cell, Row, CLUSTER_WAIT, STATUS_MISTAKE, STATUS_NO_MATCH, STATUS_UNREACHABLE};
use crate::config::{self, ServerConfig};
use std::path::Path;

pub const USAGE: &str = "usage: rradius-server session list [--config <path>] [--json]\nusage: rradius-server session show [--config <path>] [--json] <name>";

/// Why a command could not give the rows: the exit status, and what to say.
#[derive(Debug, PartialEq)]
pub struct SessionError {
    pub status: i32,
    pub message: String,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn mistake(message: String) -> SessionError {
    SessionError { status: STATUS_MISTAKE, message }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    List,
    Show,
}

/// What the arguments ask for.
#[derive(Debug, PartialEq)]
struct Command {
    action: Action,
    config: Option<String>,
    json: bool,
    name: Option<String>,
}

fn parse(args: &[String]) -> Result<Command, String> {
    let mut args = args.iter();
    let action = match args.next().map(String::as_str) {
        Some("list") => Action::List,
        Some("show") => Action::Show,
        _ => return Err(crate::cli::named(USAGE)),
    };
    let (mut config, mut json, mut name) = (None, false, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" if config.is_none() => config = Some(args.next().ok_or_else(|| format!("--config needs a value\n{}", crate::cli::named(USAGE)))?.clone()),
            "--json" if !json => json = true,
            _ if action == Action::Show && name.is_none() && !arg.starts_with("--") => name = Some(arg.clone()),
            _ => return Err(format!("unexpected argument '{arg}'\n{}", crate::cli::named(USAGE))),
        }
    }
    if action == Action::Show && name.is_none() {
        return Err(format!("a name is required\n{}", crate::cli::named(USAGE)));
    }
    Ok(Command { action, config, json, name })
}

/// The statement of `[sessions]` that `action` runs, and its parameters.
fn statement(config: &ServerConfig, config_path: &str, action: Action, name: Option<&str>) -> Result<(String, Vec<hiqlite::Param>), SessionError> {
    let (key, sql) = match action {
        Action::List => ("list", config.sessions.as_ref().and_then(|sessions| sessions.list.as_deref())),
        Action::Show => ("show", config.sessions.as_ref().and_then(|sessions| sessions.show.as_deref())),
    };
    let sql = sql.ok_or_else(|| mistake(format!("sessions.{key} is not set in {config_path}")))?.to_string();
    let params = match name {
        Some(name) => vec![accounts::account_name(config, name).map_err(mistake)?.into()],
        None => Vec::new(),
    };
    Ok((sql, params))
}

/// Runs `sql` through the cluster, giving it 30 seconds.
pub(crate) async fn query(config: &ServerConfig, sql: &str, params: Vec<hiqlite::Param>) -> Result<Vec<Row>, SessionError> {
    match tokio::time::timeout(CLUSTER_WAIT, query_rows(&config.cluster, sql, params)).await {
        Ok(Ok(rows)) => Ok(rows),
        Ok(Err(e)) if crate::lua_engine::cluster_unavailable(&e) => Err(SessionError { status: STATUS_UNREACHABLE, message: format!("cannot reach the cluster: {e}") }),
        Ok(Err(e)) => Err(mistake(format!("the statement failed: {e}"))),
        Err(_) => Err(SessionError { status: STATUS_UNREACHABLE, message: format!("the cluster did not answer within {} s", CLUSTER_WAIT.as_secs()) }),
    }
}

/// The sessions `[sessions].show` gives for `name`, which is normalized as a login is. For
/// the commands of the enterprise program: `config_path` is only for what is said when it is
/// not set.
pub async fn find(config: &ServerConfig, config_path: &str, name: &str) -> Result<Vec<Row>, SessionError> {
    let (sql, params) = statement(config, config_path, Action::Show, Some(name))?;
    query(config, &sql, params).await
}

/// Runs `session` with its arguments, the word `session` not included. Prints the result and
/// returns the exit status.
pub async fn run(args: &[String]) -> i32 {
    crate::logging::init_for_command();
    let command = match parse(args) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("{e}");
            return STATUS_MISTAKE;
        }
    };
    let config_path = command.config.as_deref().unwrap_or(config::default_config_path());
    let config = match config::load(Path::new(config_path)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            return STATUS_MISTAKE;
        }
    };
    let rows = match statement(&config, config_path, command.action, command.name.as_deref()) {
        Ok((sql, params)) => query(&config, &sql, params).await,
        Err(e) => Err(e),
    };
    match rows {
        Err(e) => {
            eprintln!("{e}");
            e.status
        }
        Ok(rows) if rows.is_empty() => {
            if command.json {
                println!("[]");
            } else {
                match &command.name {
                    Some(name) => println!("no sessions for {name}"),
                    None => println!("no sessions"),
                }
            }
            STATUS_NO_MATCH
        }
        Ok(rows) => {
            println!("{}", if command.json { json(&rows) } else { text(&rows) });
            0
        }
    }
}

/// The rows as blocks of `column=value` lines with an empty line between them.
fn text(rows: &[Row]) -> String {
    rows.iter().map(row_text).collect::<Vec<_>>().join("\n\n")
}

/// The rows as a JSON array of objects, the keys in the order selected.
pub(crate) fn json(rows: &[Row]) -> String {
    let string = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "null".to_string());
    let objects: Vec<String> = rows
        .iter()
        .map(|row| {
            let fields: Vec<String> = row
                .iter()
                .map(|(name, cell)| {
                    let value = match cell {
                        Cell::Null => "null".to_string(),
                        Cell::Integer(v) => v.to_string(),
                        // A real that is not finite has no JSON form.
                        Cell::Real(v) => serde_json::to_string(v).unwrap_or_else(|_| "null".to_string()),
                        Cell::Text(v) => string(v),
                        Cell::Blob(_) => string(&cell.text()),
                    };
                    format!("{}:{value}", string(name))
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        })
        .collect();
    format!("[{}]", objects.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::server::write_config;

    fn args(words: &str) -> Vec<String> {
        words.split_whitespace().map(str::to_string).collect()
    }

    fn row(columns: &[(&str, Cell)]) -> Row {
        columns.iter().map(|(name, cell)| (name.to_string(), cell.clone())).collect()
    }

    /// The two rows of the spec's example.
    fn two_rows() -> Vec<Row> {
        vec![
            row(&[("username", Cell::Text("alice".to_string())), ("nas", Cell::Text("10.0.0.5".to_string())), ("session_id", Cell::Text("s-1".to_string())), ("started", Cell::Integer(1000))]),
            row(&[("username", Cell::Text("bob".to_string())), ("nas", Cell::Text("10.0.0.6".to_string())), ("session_id", Cell::Text("s-2".to_string())), ("started", Cell::Null)]),
        ]
    }

    #[test]
    fn test_output_formats() {
        assert_eq!(text(&two_rows()), "username=alice\nnas=10.0.0.5\nsession_id=s-1\nstarted=1000\n\nusername=bob\nnas=10.0.0.6\nsession_id=s-2\nstarted=");
        assert_eq!(
            json(&two_rows()),
            r#"[{"username":"alice","nas":"10.0.0.5","session_id":"s-1","started":1000},{"username":"bob","nas":"10.0.0.6","session_id":"s-2","started":null}]"#
        );
        // Every type, with what needs escaping, in the order selected.
        let all = vec![row(&[("z", Cell::Text("a\"b\\c\nd".to_string())), ("a", Cell::Real(1.5)), ("m", Cell::Blob(vec![0, 171, 255])), ("n", Cell::Real(f64::NAN))])];
        assert_eq!(json(&all), r#"[{"z":"a\"b\\c\nd","a":1.5,"m":"00abff","n":null}]"#);
    }

    #[test]
    fn test_arguments() {
        let command = |words: &str| parse(&args(words));
        assert_eq!(command("list"), Ok(Command { action: Action::List, config: None, json: false, name: None }));
        assert_eq!(command("list --json --config c.toml"), Ok(Command { action: Action::List, config: Some("c.toml".to_string()), json: true, name: None }));
        assert_eq!(command("show alice --json"), Ok(Command { action: Action::Show, config: None, json: true, name: Some("alice".to_string()) }));
        for (words, expected) in [
            ("", "usage:"),
            ("delete", "usage:"),
            ("show", "a name is required"),
            ("show --json", "a name is required"),
            ("list alice", "unexpected argument 'alice'"),
            ("show alice bob", "unexpected argument 'bob'"),
            ("list --config", "--config needs a value"),
            ("list --verbose", "unexpected argument '--verbose'"),
            ("list --json --json", "unexpected argument '--json'"),
        ] {
            let error = command(words).err().unwrap_or_default();
            assert!(error.contains(expected), "{words}: {error}");
        }
    }

    /// The statement a command finds in the configuration, or says what is not set.
    #[test]
    fn test_statements_come_from_the_configuration() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let (path, _) = write_config(dir.path(), "return 'accept'", None);
        let base = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let load = |sessions: &str| {
            std::fs::write(&path, format!("{base}{sessions}")).map_err(|e| e.to_string())?;
            config::load(&path).map_err(|e| e.to_string())
        };
        let config = load("[sessions]\nlist = \"SELECT nas FROM accounting\"\nshow = \"SELECT nas FROM accounting WHERE username = ?1\"\n")?;
        let path = path.display().to_string();
        let (sql, params) = statement(&config, &path, Action::List, None).map_err(|e| e.to_string())?;
        assert_eq!((sql.as_str(), params.len()), ("SELECT nas FROM accounting", 0));
        let (sql, params) = statement(&config, &path, Action::Show, Some("alice")).map_err(|e| e.to_string())?;
        assert_eq!((sql.as_str(), params.len()), ("SELECT nas FROM accounting WHERE username = ?1", 1));
        // Not set: nothing is run, with the status for a mistake, naming the key and the file.
        for (sessions, action, name, key) in [("", Action::List, None, "sessions.list"), ("[sessions]\nlist = \"SELECT 1\"\n", Action::Show, Some("alice"), "sessions.show"), ("[sessions]\nshow = \"SELECT nas FROM t WHERE u = ?1\"\n", Action::List, None, "sessions.list")] {
            let config = load(sessions)?;
            let error = statement(&config, &path, action, name).err().ok_or("a statement that is not set was found")?;
            assert_eq!(error.status, STATUS_MISTAKE);
            assert!(error.message.contains(key) && error.message.contains(&path), "{error}");
        }
        // The name is normalized as a login does; one the chain refuses is a mistake.
        let config = load("[sessions]\nshow = \"SELECT nas FROM t WHERE u = ?1\"\n")?;
        assert!(statement(&config, &path, Action::Show, Some("")).is_err());
        Ok(())
    }
}
