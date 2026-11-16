//! The named queries against a running single-node cluster: the gauges in `/metrics` and what
//! changes them, the failures, the `query` command, and the `queries` section of `status`.

use rradius_core::test_support::server::*;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

const SCHEMA: &str = "CREATE TABLE ip_pool (pool TEXT NOT NULL, ip TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'free', PRIMARY KEY (pool, ip));
CREATE INDEX ip_pool_state ON ip_pool (pool, state);";

/// Ten addresses of `residential` and three of `business`, all free.
fn data() -> String {
    let mut rows: Vec<String> = (1..=10).map(|n| format!("('residential', '10.1.0.{n}')")).collect();
    rows.extend((1..=3).map(|n| format!("('business', '10.2.0.{n}')")));
    format!("INSERT INTO ip_pool (pool, ip) VALUES {};", rows.join(", "))
}

/// Each login takes one free address of `residential`.
const HANDLER: &str = "sql_execute(\"UPDATE ip_pool SET state = 'reserved' WHERE rowid = (SELECT rowid FROM ip_pool WHERE pool = 'residential' AND state = 'free' ORDER BY ip LIMIT 1)\")
return 'accept'";

const QUERIES: &str = "
[queries.pool_usage]
sql = \"SELECT pool, state, count(*) AS n FROM ip_pool GROUP BY pool, state ORDER BY pool, state\"
help = \"Addresses by pool and state\"
labels = [\"pool\", \"state\"]
value = \"n\"
interval_ms = 5000

[queries.broken]
sql = \"SELECT n FROM no_such_table\"
interval_ms = 5000
";

/// A single-node server with the pool table, the queries and `/metrics`, once the first run of
/// the good query has given its samples.
fn server() -> Result<(Server, u16), String> {
    let dir = dir_with_migrations(&[("1_pool.sql", SCHEMA), ("2_data.sql", &data())]);
    let (config, auth_port) = write_config_with(dir.path(), "return 'accept'", Some(HANDLER), WITH_MIGRATIONS);
    let metrics = free_port();
    let mut text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    text.push_str(&format!("[metrics]\nport = {metrics}\n{QUERIES}"));
    std::fs::write(&config, text).map_err(|e| e.to_string())?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    server.access_request();
    // The first run may come before the login above took its address; a later run shows it.
    wait_for(metrics, "rradius_query_pool_usage{pool=\"residential\",state=\"reserved\"} 1\n", &server)?;
    // And the query that cannot run has failed once, so that the tests may count on it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while scrape(metrics, "rradius_query_failures_total{query=\"broken\",reason=\"error\"}") < 1 {
        if Instant::now() >= deadline {
            return Err(format!("the query that cannot run did not fail within 30 s: {}", server.stderr()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok((server, metrics))
}

/// Waits until `/metrics` has `text`, giving it 30 seconds.
fn wait_for(metrics: u16, text: &str, server: &Server) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(rendered) = metrics_text(metrics) {
            if rendered.contains(text) {
                return Ok(rendered);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("{text:?} did not appear in /metrics: {}", server.stderr()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Runs `rradius-server` with `args` and `--config`: the status, standard output and error.
fn run(config: &Path, args: &[&str]) -> Result<(Option<i32>, String, String), String> {
    let output = Command::new(PROGRAM).args(args).args(["--config", &config.display().to_string()]).output().map_err(|e| e.to_string())?;
    Ok((output.status.code(), String::from_utf8_lossy(&output.stdout).trim_end().to_string(), String::from_utf8_lossy(&output.stderr).into_owned()))
}

/// spec Query Gauges, Background Execution and Query Failure Reporting: the samples of the first
/// run, the change a later run picks up, a query that fails beside one that works, and the
/// families that are there from the start.
#[test]
fn test_query_gauges_follow_the_data_and_failures_are_counted() -> Result<(), String> {
    let (server, metrics) = server()?;
    let rendered = metrics_text(metrics)?;
    for line in [
        "# HELP rradius_query_pool_usage Addresses by pool and state\n# TYPE rradius_query_pool_usage gauge\n",
        "rradius_query_pool_usage{pool=\"business\",state=\"free\"} 3\n",
        "rradius_query_pool_usage{pool=\"residential\",state=\"free\"} 9\n",
        "rradius_query_pool_usage{pool=\"residential\",state=\"reserved\"} 1\n",
        "rradius_query_failures_total{query=\"pool_usage\",reason=\"error\"} 0\n",
        "rradius_query_failures_total{query=\"broken\",reason=\"timeout\"} 0\n",
        "rradius_query_failures_total{query=\"broken\",reason=\"shape\"} 0\n",
        "rradius_query_last_success_timestamp_seconds{query=\"broken\"} 0\n",
    ] {
        assert!(rendered.contains(line), "{line:?} is not in\n{rendered}");
    }
    assert!(!rendered.contains("rradius_query_last_success_timestamp_seconds{query=\"pool_usage\"} 0\n"), "a success has a time");
    // The query that cannot run is counted every run, and the server answers all the same.
    let failed = scrape(metrics, "rradius_query_failures_total{query=\"broken\",reason=\"error\"}");
    assert!(failed >= 1, "a failed run for each interval that has passed: {failed}");
    assert_eq!(scrape(metrics, "rradius_query_failures_total{query=\"pool_usage\",reason=\"error\"}"), 0);
    for _ in 0..2 {
        assert_eq!(server.access_request(), 2, "an Access-Accept, whatever the queries do: {}", server.stderr());
    }
    // Two more addresses are taken; the next run shows it.
    wait_for(metrics, "rradius_query_pool_usage{pool=\"residential\",state=\"free\"} 7\n", &server)?;
    let rendered = metrics_text(metrics)?;
    assert!(rendered.contains("rradius_query_pool_usage{pool=\"residential\",state=\"reserved\"} 3\n"), "{rendered}");
    assert!(rendered.contains("rradius_query_pool_usage{pool=\"business\",state=\"free\"} 3\n"), "{rendered}");
    Ok(())
}

/// spec Query Command: `list`, the rows of one query as a table and as JSON, an unknown name and
/// a statement the database refuses.
#[test]
fn test_query_command() -> Result<(), String> {
    let (server, _) = server()?;
    let (code, stdout, stderr) = run(&server.config, &["query", "list"])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.starts_with("name=broken\nhelp=\ninterval_ms=5000\nsql=SELECT n FROM no_such_table\n\nname=pool_usage\nhelp=Addresses by pool and state\ninterval_ms=5000\nsql=SELECT pool, state, count(*) AS n"), "{stdout}");
    let (code, stdout, _) = run(&server.config, &["query", "list", "--json"])?;
    assert_eq!(code, Some(0));
    let listed: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| e.to_string())?;
    assert_eq!(listed[0]["name"], "broken");
    assert_eq!(listed[1]["interval_ms"], 5000);

    let line = |pool: &str, state: &str, n: &str| format!("{pool:<11}  {state:<8}  {n}");
    let table = [line("pool", "state", "n"), line("business", "free", "3"), line("residential", "free", "9"), line("residential", "reserved", "1")].join("\n");
    assert_eq!(run(&server.config, &["query", "pool_usage"])?, (Some(0), table, String::new()));
    let (code, stdout, _) = run(&server.config, &["query", "pool_usage", "--json"])?;
    assert_eq!(code, Some(0));
    assert_eq!(stdout, r#"[{"pool":"business","state":"free","n":3},{"pool":"residential","state":"free","n":9},{"pool":"residential","state":"reserved","n":1}]"#);

    let (code, stdout, stderr) = run(&server.config, &["query", "nosuch"])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""));
    assert!(stderr.contains("queries.nosuch is not set"), "{stderr}");
    // The database refuses the statement: a mistake with the reason, and nothing printed.
    let (code, stdout, stderr) = run(&server.config, &["query", "broken"])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""));
    assert!(stderr.contains("the statement failed"), "{stderr}");
    let (code, _, stderr) = run(&server.config, &["query", "--json"])?;
    assert_eq!(code, Some(2));
    assert!(stderr.contains("usage:"), "{stderr}");
    Ok(())
}

/// spec Query Command: a configuration that is not there is a mistake, and so is a cluster that
/// is not running a different one, each with the status the `session` commands give it and a line.
#[test]
fn test_query_command_errors_without_a_cluster() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (code, stdout, stderr) = run(&dir.path().join("nope.toml"), &["query", "pool_usage"])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""));
    assert!(!stderr.trim().is_empty(), "a line that says why");
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let mut text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    text.push_str("[queries.pool_usage]\nsql = \"SELECT 1 AS value\"\n");
    std::fs::write(&config, text).map_err(|e| e.to_string())?;
    // `list` needs no cluster.
    let (code, stdout, _) = run(&config, &["query", "list"])?;
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("name=pool_usage\n"), "{stdout}");
    // No node is running for it.
    let (code, stdout, stderr) = run(&config, &["query", "pool_usage"])?;
    assert_eq!((code, stdout.as_str()), (Some(3), ""), "{stderr}");
    assert!(stderr.contains("cannot reach the cluster") || stderr.contains("did not answer"), "{stderr}");
    Ok(())
}

/// spec Status Section of Queries: `status --json` has the section, and each query's items.
#[test]
fn test_status_shows_the_queries() -> Result<(), String> {
    let (server, _) = server()?;
    let (code, stdout, stderr) = run(&server.config, &["status", "--json"])?;
    assert_eq!(code, Some(0), "{stderr}");
    let status: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| e.to_string())?;
    let section = status["sections"].as_array().and_then(|sections| sections.iter().find(|section| section["key"] == "queries")).ok_or_else(|| format!("no queries section in {stdout}"))?;
    let item = |key: &str| section["items"].as_array().and_then(|items| items.iter().find(|item| item["key"] == key)).cloned();
    assert_eq!(item("pool_usage.rows").map(|item| item["value"].clone()), Some(serde_json::json!(3)));
    assert_eq!(item("broken.last_success_age_seconds").map(|item| item["value"].clone()), Some(serde_json::json!("never")));
    assert!(item("pool_usage.last_success_age_seconds").is_some_and(|item| item["value"].is_number()), "{section}");
    assert!(item("broken.failures").is_some_and(|item| item["total"].as_f64().is_some_and(|total| total >= 1.0)), "{section}");
    Ok(())
}
