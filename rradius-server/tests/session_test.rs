//! `rradius-server session list|show` against a running single-node cluster whose accounting
//! script keeps a table of sessions: the Session Commands and Session Command Results
//! requirements.

use rradius_core::radius::{self, RadiusPacket};
use rradius_core::test_support::server::*;
use std::net::UdpSocket;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

const SCHEMA: &str = "CREATE TABLE accounting (client TEXT NOT NULL, nas TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT, started INTEGER NOT NULL, stopped INTEGER, PRIMARY KEY (client, nas, session_id));";

/// A Start puts a row in, a Stop marks it stopped; `started` is the Acct-Session-Time the test
/// gives, so that the rows do not depend on the clock.
const SCRIPT: &str = "
if phase == 'accounting' then
  local status = get_attr('Acct-Status-Type')
  local session = get_attr('Acct-Session-Id')
  local nas = get_attr('NAS-IP-Address') or client
  local started = get_attr('Acct-Session-Time') or 0
  if status == 1 then
    sql_execute('INSERT OR REPLACE INTO accounting (client, nas, session_id, username, started, stopped) VALUES (?, ?, ?, ?, ?, NULL)', {client, nas, session, get_attr('User-Name'), started})
  elseif status == 2 then
    sql_execute('UPDATE accounting SET stopped = ? WHERE client = ? AND nas = ? AND session_id = ?', {started, client, nas, session})
  end
end
return 'accept'";

const SESSIONS: &str = "[sessions]
list = \"SELECT username, nas, session_id, started FROM accounting WHERE stopped IS NULL ORDER BY started, session_id\"
show = \"SELECT nas, session_id, username FROM accounting WHERE username = ?1 AND stopped IS NULL ORDER BY started, session_id\"
";

/// A single-node server with the table, the script and `[sessions]`, and `[auth.username]` as
/// the `user` commands have it. Waits until it answers.
fn server() -> Result<Server, String> {
    let dir = dir_with_migrations(&[("1_accounting.sql", SCHEMA)]);
    let (config, auth_port) = write_config_with(dir.path(), SCRIPT, Some("return 'accept'"), WITH_MIGRATIONS);
    let mut text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    text.push_str("[auth.username]\nstrip_realm = \"suffix\"\nlowercase = true\nallowed = \"[a-z0-9]+\"\n");
    text.push_str(SESSIONS);
    std::fs::write(&config, text).map_err(|e| e.to_string())?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    server.access_request();
    Ok(server)
}

/// Sends an Accounting-Request (Start 1 or Stop 2) and waits for its response.
fn account(server: &Server, status: u32, user: &str, session: &str, nas: [u8; 4], time: u32) -> Result<(), String> {
    let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let attributes = vec![(40, status.to_be_bytes().to_vec()), (1, user.as_bytes().to_vec()), (44, session.as_bytes().to_vec()), (4, nas.to_vec()), (46, time.to_be_bytes().to_vec())];
    let packet = RadiusPacket::create_accounting_request(1, attributes, b"s");
    socket.send_to(&packet, ("127.0.0.1", server.port_acct())).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 4096];
    let (size, _) = socket.recv_from(&mut buf).map_err(|e| format!("no answer: {e}: {}", server.stderr()))?;
    let reply = RadiusPacket::parse(&buf[..size]).map_err(|e| e.to_string())?;
    if reply.code == radius::ACCOUNTING_RESPONSE {
        Ok(())
    } else {
        Err(format!("code {}", reply.code))
    }
}

/// Runs `rradius-server session ...` on `config`: the status, standard output and error.
fn session(config: &Path, args: &[&str]) -> Result<(Option<i32>, String, String), String> {
    let output = Command::new(PROGRAM).arg("session").args(args).args(["--config", &config.display().to_string()]).output().map_err(|e| e.to_string())?;
    Ok((output.status.code(), String::from_utf8_lossy(&output.stdout).trim_end().to_string(), String::from_utf8_lossy(&output.stderr).into_owned()))
}

/// Two sessions of alice, one of bob and one of carol that has stopped.
fn fill(server: &Server) -> Result<(), String> {
    account(server, 1, "alice", "s-1", [10, 0, 0, 5], 1000)?;
    account(server, 1, "bob", "s-2", [10, 0, 0, 6], 2000)?;
    account(server, 1, "alice", "s-3", [10, 0, 0, 5], 3000)?;
    account(server, 1, "carol", "s-4", [10, 0, 0, 7], 4000)?;
    account(server, 2, "carol", "s-4", [10, 0, 0, 7], 4500)
}

/// spec Session Command Results: a node that refuses `[cluster] secret_api` is a mistake in the
/// configuration, said at once with the node's address; the cluster's own client would retry
/// for the whole 30 seconds.
#[test]
fn test_a_wrong_api_secret_is_refused_at_once() -> Result<(), String> {
    let server = server()?;
    let text = std::fs::read_to_string(&server.config).map_err(|e| e.to_string())?;
    let wrong = text.replace("secret_api = \"test-api-secret-01234\"", "secret_api = \"another-api-secret-0123\"");
    assert_ne!(wrong, text, "the secret is in the configuration");
    let path = server.config.with_file_name("wrong-secret.toml");
    std::fs::write(&path, wrong).map_err(|e| e.to_string())?;
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = session(&path, &["list"])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""), "{stderr}");
    assert!(stderr.contains("refused the api secret"), "{stderr}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    // The right secret still works.
    assert_eq!(session(&server.config, &["list"])?.0, Some(1));
    Ok(())
}

/// spec Session Commands and Session Command Results: the text and JSON output, the rows of a
/// user, and what is printed when there is none.
#[test]
fn test_list_and_show() -> Result<(), String> {
    let server = server()?;
    // Nothing yet.
    assert_eq!(session(&server.config, &["list"])?, (Some(1), "no sessions".to_string(), String::new()));
    assert_eq!(session(&server.config, &["list", "--json"])?, (Some(1), "[]".to_string(), String::new()));
    fill(&server)?;

    let (code, stdout, stderr) = session(&server.config, &["list"])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        stdout,
        "username=alice\nnas=10.0.0.5\nsession_id=s-1\nstarted=1000\n\nusername=bob\nnas=10.0.0.6\nsession_id=s-2\nstarted=2000\n\nusername=alice\nnas=10.0.0.5\nsession_id=s-3\nstarted=3000"
    );
    let (code, stdout, _) = session(&server.config, &["list", "--json"])?;
    assert_eq!(code, Some(0));
    assert_eq!(
        stdout,
        r#"[{"username":"alice","nas":"10.0.0.5","session_id":"s-1","started":1000},{"username":"bob","nas":"10.0.0.6","session_id":"s-2","started":2000},{"username":"alice","nas":"10.0.0.5","session_id":"s-3","started":3000}]"#
    );

    let (code, stdout, _) = session(&server.config, &["show", "alice"])?;
    assert_eq!((code, stdout.as_str()), (Some(0), "nas=10.0.0.5\nsession_id=s-1\nusername=alice\n\nnas=10.0.0.5\nsession_id=s-3\nusername=alice"));
    let (code, stdout, _) = session(&server.config, &["show", "--json", "bob"])?;
    assert_eq!((code, stdout.as_str()), (Some(0), r#"[{"nas":"10.0.0.6","session_id":"s-2","username":"bob"}]"#));
    // A session that has stopped is not listed by the statement.
    assert_eq!(session(&server.config, &["show", "carol"])?, (Some(1), "no sessions for carol".to_string(), String::new()));
    assert_eq!(session(&server.config, &["show", "nobody"])?, (Some(1), "no sessions for nobody".to_string(), String::new()));
    assert_eq!(session(&server.config, &["show", "nobody", "--json"])?, (Some(1), "[]".to_string(), String::new()));
    Ok(())
}

/// spec Session Commands: the name is normalized as a login does.
#[test]
fn test_name_is_normalized_like_a_login() -> Result<(), String> {
    let server = server()?;
    fill(&server)?;
    let plain = session(&server.config, &["show", "alice"])?;
    assert_eq!(plain.0, Some(0));
    assert_eq!(session(&server.config, &["show", "Alice@Corp"])?, plain);
    // A name the chain refuses is a mistake, and nothing is run.
    let (code, stdout, stderr) = session(&server.config, &["show", "bad name"])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""));
    assert!(stderr.contains("bad name"), "{stderr}");
    Ok(())
}

/// spec Session Command Results: mistakes in the command and the configuration.
#[test]
fn test_mistakes_run_nothing() -> Result<(), String> {
    let server = server()?;
    for (args, expected) in [(&["show"][..], "a name is required"), (&["list", "alice"][..], "unexpected argument"), (&["watch"][..], "usage")] {
        let (code, stdout, stderr) = session(&server.config, args)?;
        assert_eq!((code, stdout.as_str()), (Some(2), ""), "{args:?}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    // A key that is not set.
    server.write_config_text(|text| text.replace("show = ", "# show = "));
    let (code, stdout, stderr) = session(&server.config, &["show", "alice"])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""));
    assert!(stderr.contains("sessions.show") && stderr.contains("not set"), "{stderr}");
    // A statement the database refuses is the configuration's mistake.
    server.write_config_text(|text| text.replace("# show = ", "show = ").replace("FROM accounting WHERE username", "FROM nosuchtable WHERE username"));
    let (code, stdout, stderr) = session(&server.config, &["show", "alice"])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""));
    assert!(stderr.contains("the statement failed") && stderr.contains("nosuchtable"), "{stderr}");
    Ok(())
}

/// spec Session Command Results: without `[sessions]`; spec Sessions Settings: `check`.
#[test]
fn test_without_the_section_and_check() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    for args in [&["list"][..], &["show", "alice"][..]] {
        let (code, stdout, stderr) = session(&config, args)?;
        assert_eq!((code, stdout.as_str()), (Some(2), ""), "{args:?}");
        assert!(stderr.contains("sessions.") && stderr.contains("not set"), "{args:?}: {stderr}");
    }
    let check = |config: &Path| Command::new(PROGRAM).args(["check", "--config", &config.display().to_string()]).output().map_err(|e| e.to_string());
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    std::fs::write(&config, format!("{text}{SESSIONS}")).map_err(|e| e.to_string())?;
    let output = check(&config)?;
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    std::fs::write(&config, format!("{text}[sessions]\nlist = \"DELETE FROM accounting\"\n")).map_err(|e| e.to_string())?;
    let output = check(&config)?;
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("sessions.list"), "{}", String::from_utf8_lossy(&output.stderr));
    Ok(())
}

/// spec Session Command Results and Session Commands: no cluster to reach, and no data
/// directory made.
#[test]
fn test_without_the_cluster() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    std::fs::write(&config, format!("{text}{SESSIONS}")).map_err(|e| e.to_string())?;
    let (code, stdout, stderr) = session(&config, &["list"])?;
    assert_eq!((code, stdout.as_str()), (Some(3), ""), "{stderr}");
    assert!(stderr.contains("cannot reach the cluster") || stderr.contains("did not answer"), "{stderr}");
    assert!(!dir.path().join("data").exists(), "the data directory was created");
    Ok(())
}

/// The other table the manual's examples query: `active_sessions` of `examples/simultaneous-use.lua`,
/// whose Stop deletes the row.
#[test]
fn test_active_sessions_example() -> Result<(), String> {
    let schema = "CREATE TABLE active_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, last_seen INTEGER NOT NULL, nas TEXT NOT NULL, PRIMARY KEY (client, session_id));";
    let dir = dir_with_migrations(&[("1_active_sessions.sql", schema)]);
    let (config, auth_port) = write_config_with(dir.path(), include_str!("../examples/simultaneous-use.lua"), Some("return 'accept'"), WITH_MIGRATIONS);
    let mut text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    text.push_str("[sessions]\nlist = \"SELECT username, nas, session_id, last_seen FROM active_sessions WHERE last_seen > strftime('%s', 'now') - 900 ORDER BY last_seen, session_id\"\nshow = \"SELECT username, nas, session_id, last_seen FROM active_sessions WHERE username = ?1 AND last_seen > strftime('%s', 'now') - 900 ORDER BY last_seen, session_id\"\n");
    std::fs::write(&config, text).map_err(|e| e.to_string())?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    server.access_request();
    account(&server, 1, "alice", "s-1", [10, 0, 0, 5], 0)?;
    account(&server, 1, "bob", "s-2", [10, 0, 0, 6], 0)?;
    account(&server, 2, "bob", "s-2", [10, 0, 0, 6], 0)?;
    let (code, stdout, stderr) = session(&server.config, &["list", "--json"])?;
    assert_eq!(code, Some(0), "{stderr}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&stdout).map_err(|e| e.to_string())?;
    let shown: Vec<(&str, &str, &str)> = rows.iter().map(|row| (row["username"].as_str().unwrap_or(""), row["nas"].as_str().unwrap_or(""), row["session_id"].as_str().unwrap_or(""))).collect();
    assert_eq!(shown, vec![("alice", "10.0.0.5", "s-1")], "bob's Stop deleted his row: {stdout}");
    assert!(rows.iter().all(|row| row["last_seen"].is_i64()), "{stdout}");
    let (code, stdout, _) = session(&server.config, &["show", "alice"])?;
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("username=alice\nnas=10.0.0.5\nsession_id=s-1\nlast_seen="), "{stdout}");
    assert_eq!(session(&server.config, &["show", "bob"])?.0, Some(1));
    Ok(())
}
