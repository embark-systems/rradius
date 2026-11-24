//! `rradiusd query list|<name>`: the named queries of `[queries]`, listed, and one of them
//! run once through the cluster's API port, as `session` runs its statements, so that the command
//! starts no node. It runs the statement itself and does not read what the node has kept for
//! `/metrics`, and it is not held to `max_rows`.

use crate::accounts::{Cell, Row, STATUS_MISTAKE};
use crate::config::{self, ServerConfig};
use std::path::Path;

pub const USAGE: &str = "usage: rradiusd query list [--config <path>] [--json]\nusage: rradiusd query <name> [--config <path>] [--json]";

/// What the arguments ask for.
#[derive(Debug, PartialEq)]
struct Command {
    /// `None` is `list`.
    name: Option<String>,
    config: Option<String>,
    json: bool,
}

fn parse(args: &[String]) -> Result<Command, String> {
    let mut args = args.iter();
    let first = args.next().filter(|word| !word.starts_with("--")).ok_or_else(|| crate::cli::named(USAGE))?;
    let name = (first != "list").then(|| first.clone());
    let (mut config, mut json) = (None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" if config.is_none() => config = Some(args.next().ok_or_else(|| format!("--config needs a value\n{}", crate::cli::named(USAGE)))?.clone()),
            "--json" if !json => json = true,
            _ => return Err(format!("unexpected argument '{arg}'\n{}", crate::cli::named(USAGE))),
        }
    }
    Ok(Command { name, config, json })
}

/// Runs `query` with its arguments, the word `query` not included. Prints the result and returns
/// the exit status.
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
    let Some(name) = &command.name else {
        let listed = list(&config, command.json);
        if !listed.is_empty() {
            println!("{listed}");
        }
        return 0;
    };
    let Some(query) = config.queries.get(name) else {
        eprintln!("queries.{name} is not set in {config_path}");
        return STATUS_MISTAKE;
    };
    match crate::sessions::query(&config, &query.sql, Vec::new()).await {
        Err(e) => {
            eprintln!("{e}");
            e.status
        }
        Ok(rows) => {
            println!("{}", if command.json { crate::sessions::json(&rows) } else { table(&rows) });
            0
        }
    }
}

/// The queries in the order of their names: blocks of `key=value` lines with an empty line
/// between them, or a JSON array. Nothing, in text, when there are none.
fn list(config: &ServerConfig, json: bool) -> String {
    if json {
        let queries: Vec<serde_json::Value> = config.queries.iter().map(|(name, query)| serde_json::json!({"name": name, "help": query.help, "interval_ms": query.interval.as_millis() as u64, "sql": query.sql})).collect();
        return serde_json::Value::Array(queries).to_string();
    }
    // A line break inside a help text or a statement is written as \n, so that a block stays four lines.
    let one_line = |text: &str| text.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r");
    config.queries.iter().map(|(name, query)| format!("name={name}\nhelp={}\ninterval_ms={}\nsql={}", one_line(&query.help), query.interval.as_millis(), one_line(&query.sql))).collect::<Vec<_>>().join("\n\n")
}

/// The rows as a table: the column names first, every column as wide as its widest value.
fn table(rows: &[Row]) -> String {
    let Some(first) = rows.first() else { return "no rows".to_string() };
    let cell = |cell: &Cell| match cell {
        Cell::Null => "NULL".to_string(),
        other => other.text(),
    };
    let header: Vec<String> = first.iter().map(|(name, _)| name.clone()).collect();
    let body: Vec<Vec<String>> = rows.iter().map(|row| row.iter().map(|(_, value)| cell(value)).collect()).collect();
    let widths: Vec<usize> = (0..header.len()).map(|i| body.iter().map(|row| row.get(i).map_or(0, |text| text.chars().count())).chain(std::iter::once(header[i].chars().count())).max().unwrap_or(0)).collect();
    let line = |fields: &[String]| fields.iter().enumerate().map(|(i, text)| format!("{text:<width$}", width = widths[i])).collect::<Vec<_>>().join("  ").trim_end().to_string();
    std::iter::once(line(&header)).chain(body.iter().map(|row| line(row))).collect::<Vec<_>>().join("\n")
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

    #[test]
    fn test_query_command_arguments() {
        let command = |words: &str| parse(&args(words));
        assert_eq!(command("list"), Ok(Command { name: None, config: None, json: false }));
        assert_eq!(command("list --json --config c.toml"), Ok(Command { name: None, config: Some("c.toml".to_string()), json: true }));
        assert_eq!(command("pool_usage --json"), Ok(Command { name: Some("pool_usage".to_string()), config: None, json: true }));
        for (words, expected) in [("", "usage:"), ("--json", "usage:"), ("list nope", "unexpected argument 'nope'"), ("pool_usage x", "unexpected argument 'x'"), ("list --config", "--config needs a value"), ("list --json --json", "unexpected argument '--json'")] {
            let error = command(words).err().unwrap_or_default();
            assert!(error.contains(expected), "{words}: {error}");
        }
    }

    #[test]
    fn test_query_command_table() {
        let rows = vec![
            row(&[("pool", Cell::Text("residential".to_string())), ("state", Cell::Text("free".to_string())), ("n", Cell::Integer(8))]),
            row(&[("pool", Cell::Text("business".to_string())), ("state", Cell::Null), ("n", Cell::Real(2.5))]),
        ];
        assert_eq!(table(&rows), "pool         state  n\nresidential  free   8\nbusiness     NULL   2.5");
        assert_eq!(table(&[]), "no rows");
    }

    /// `list`: the queries in the order of their names, as text and as JSON; nothing without `[queries]`.
    #[test]
    fn test_query_command_list() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let (path, _) = write_config(dir.path(), "return 'accept'", None);
        let base = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let load = |queries: &str| {
            std::fs::write(&path, format!("{base}{queries}")).map_err(|e| e.to_string())?;
            config::load(&path).map_err(|e| e.to_string())
        };
        let config = load("[queries.slots]\nsql = \"SELECT state, count(*) AS value FROM session_slots GROUP BY state\"\nhelp = \"Slots\"\ninterval_ms = 10000\n[queries.pool]\nsql = \"SELECT 1 AS value\"\n")?;
        assert_eq!(list(&config, false), "name=pool\nhelp=\ninterval_ms=30000\nsql=SELECT 1 AS value\n\nname=slots\nhelp=Slots\ninterval_ms=10000\nsql=SELECT state, count(*) AS value FROM session_slots GROUP BY state");
        let listed: serde_json::Value = serde_json::from_str(&list(&config, true)).map_err(|e| e.to_string())?;
        assert_eq!(listed, serde_json::json!([
            {"name": "pool", "help": "", "interval_ms": 30000, "sql": "SELECT 1 AS value"},
            {"name": "slots", "help": "Slots", "interval_ms": 10000, "sql": "SELECT state, count(*) AS value FROM session_slots GROUP BY state"},
        ]));
        // A statement over several lines stays one line of the block.
        let config = load("[queries.multi]\nsql = \"\"\"\nSELECT 1\n\nAS value\"\"\"\nhelp = \"a\\\\b\"\n")?;
        assert_eq!(list(&config, false), "name=multi\nhelp=a\\\\b\ninterval_ms=30000\nsql=SELECT 1\\n\\nAS value");
        let config = load("")?;
        assert_eq!((list(&config, false), list(&config, true)), (String::new(), "[]".to_string()));
        Ok(())
    }
}
