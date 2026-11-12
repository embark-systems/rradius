//! What the Lua scripts of the user manual's scenarios (examples/scenarios) write and decide,
//! run on the test database with each scenario's own schema. tests/scenarios_test.rs starts
//! the scenarios end to end but cannot look inside the database.
//!
//! The test database is shared, so each test renames the scenario's tables with a prefix of
//! its own, in the schema and the script alike.

use crate::lua_engine::{DbValue, LuaAuthEngine, LuaRequest};
use crate::test_support::{test_db, test_vms};
use crate::common::{AttrValue, Outcome};

const QUICK_START_SCHEMA: &str = include_str!("../../rradius-server/examples/scenarios/quick-start/migrations/1_schema.sql");
const QUICK_START_ACCOUNTING: &str = include_str!("../../rradius-server/examples/scenarios/quick-start/accounting.lua");
const DEVICE_ADMIN_SCHEMA: &str = include_str!("../../rradius-server/examples/scenarios/device-admin/migrations/1_schema.sql");
const DEVICE_ADMIN_DATA: &str = include_str!("../../rradius-server/examples/scenarios/device-admin/migrations/2_demo_data.sql");
const DEVICE_ADMIN_SCRIPT: &str = include_str!("../../rradius-server/examples/scenarios/device-admin/tacacs.lua");

/// `text` with each of `tables` renamed to `prefix_table`, whole words only, and not where the
/// word is a quoted string of its own, such as the phase `'accounting'`.
fn renamed(text: &str, prefix: &str, tables: &[&str]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if i == start {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let word: String = chars[start..i].iter().collect();
        let quoted = start > 0 && chars[start - 1] == '\'' && chars.get(i) == Some(&'\'');
        if tables.contains(&word.as_str()) && !quoted {
            out.push_str(prefix);
            out.push('_');
        }
        out.push_str(&word);
    }
    out
}

/// Runs each statement of `sql`, comments dropped.
async fn apply(sql: &str) {
    let text: String = sql.lines().map(|line| line.split("--").next().unwrap()).collect::<Vec<_>>().join("\n");
    for statement in text.split(';').map(str::trim).filter(|statement| !statement.is_empty()) {
        test_db().execute(statement).await.unwrap_or_else(|e| panic!("{statement}: {e}"));
    }
}

fn number(value: f64) -> AttrValue {
    AttrValue::Number(value)
}

fn bytes(value: &str) -> AttrValue {
    AttrValue::Bytes(value.as_bytes().to_vec())
}

async fn run(script: &str, protocol: &'static str, phase: &'static str, client: &str, auth_user: Option<&str>, attributes: Vec<(&str, AttrValue)>) -> (Outcome, Vec<(String, AttrValue)>) {
    let engine = LuaAuthEngine::new(test_vms(), test_db(), script).unwrap();
    let mut request = LuaRequest::new(protocol, phase, attributes.into_iter().map(|(name, value)| (name.to_string(), value)).collect());
    request.client = client.to_string();
    request.auth_user = auth_user.map(str::to_string);
    let decision = engine.authenticate(request).await;
    (decision.outcome, decision.response_attributes)
}

async fn rows(sql: &str) -> Vec<Vec<DbValue>> {
    test_db().query(sql).await.unwrap().into_iter().map(|row| row.into_iter().map(|(_, value)| value).collect()).collect()
}

fn int(value: i64) -> DbValue {
    DbValue::Integer(value)
}

fn txt(value: &str) -> DbValue {
    DbValue::Text(value.to_string())
}

/// The quick start's accounting: one row per session, safe to run twice, and an
/// Accounting-On closing what the NAS had open.
#[tokio::test]
async fn test_quick_start_accounting() {
    let tables = ["users", "user_reply", "accounting", "user_reply_user", "accounting_user"];
    apply(&renamed(QUICK_START_SCHEMA, "qs", &tables)).await;
    let script = renamed(QUICK_START_ACCOUNTING, "qs", &tables);
    let packet = |status: f64, extra: Vec<(&'static str, AttrValue)>| {
        let mut attributes = vec![("User-Name", bytes("alice")), ("Acct-Session-Id", bytes("s1")), ("Acct-Status-Type", number(status))];
        attributes.extend(extra);
        attributes
    };
    let accept = |(outcome, _): (Outcome, _)| assert_eq!(outcome, Outcome::Accept);

    accept(run(&script, "radius", "accounting", "nas-a", None, packet(1.0, vec![])).await);
    let interim = vec![("Acct-Session-Time", number(60.0)), ("Acct-Input-Octets", number(10.0)), ("Acct-Input-Gigawords", number(1.0)), ("Acct-Output-Octets", number(20.0))];
    accept(run(&script, "radius", "accounting", "nas-a", None, packet(3.0, interim.clone())).await);
    accept(run(&script, "radius", "accounting", "nas-a", None, packet(3.0, interim)).await);
    let stop = vec![("Acct-Session-Time", number(120.0)), ("Acct-Input-Octets", number(30.0)), ("Acct-Terminate-Cause", number(1.0))];
    accept(run(&script, "radius", "accounting", "nas-a", None, packet(2.0, stop)).await);
    // Another Stop for the same session does not change how it ended.
    let again = vec![("Acct-Session-Time", number(120.0)), ("Acct-Terminate-Cause", number(2.0)), ("Acct-Delay-Time", number(30.0))];
    accept(run(&script, "radius", "accounting", "nas-a", None, packet(2.0, again)).await);
    // A late Interim-Update does not open the session again, take numbers back, or move
    // `updated` past `stopped`. The times are moved back first, so the Interim-Update always
    // lands in a later second than the Stop.
    apply("UPDATE qs_accounting SET updated = updated - 1000, stopped = stopped - 1000 WHERE session_id = 's1'").await;
    accept(run(&script, "radius", "accounting", "nas-a", None, packet(3.0, vec![("Acct-Session-Time", number(90.0))])).await);

    let found = rows("SELECT username, session_time, input_octets, output_octets, terminate_cause, stopped = updated FROM qs_accounting WHERE session_id = 's1'").await;
    assert_eq!(found, [vec![txt("alice"), int(120), int(4294967306), int(20), int(1), int(1)]]);

    // The same session ID from another NAS is another session; Accounting-On ends it.
    accept(run(&script, "radius", "accounting", "nas-b", None, packet(1.0, vec![])).await);
    accept(run(&script, "radius", "accounting", "nas-b", None, vec![("Acct-Status-Type", number(7.0))]).await);
    assert_eq!(rows("SELECT client, terminate_cause FROM qs_accounting WHERE session_id = 's1' ORDER BY client").await, [vec![txt("nas-a"), int(1)], vec![txt("nas-b"), int(11)]]);

    // One client for a whole network: the NAS each says it is keeps them apart.
    for address in ["10.0.0.1", "10.0.0.2"] {
        accept(run(&script, "radius", "accounting", "net", None, packet(1.0, vec![("NAS-IP-Address", bytes(address))])).await);
    }
    accept(run(&script, "radius", "accounting", "net", None, vec![("Acct-Status-Type", number(7.0)), ("NAS-IP-Address", bytes("10.0.0.1"))]).await);
    assert_eq!(
        rows("SELECT nas, stopped IS NULL FROM qs_accounting WHERE client = 'net' ORDER BY nas").await,
        [vec![txt("10.0.0.1"), int(0)], vec![txt("10.0.0.2"), int(1)]],
        "a NAS that restarts ends only its own sessions",
    );

    let (outcome, _) = run(&script, "radius", "accounting", "nas-a", None, vec![("Acct-Status-Type", number(1.0))]).await;
    assert_eq!(outcome, Outcome::Accept, "without a session ID: answered, not recorded");
    let (outcome, _) = run(&script, "radius", "authentication", "nas-a", None, vec![("User-Name", bytes("alice"))]).await;
    assert_eq!(outcome, Outcome::Reject, "never lets anyone in");
}

/// Device administration: roles, and one log row per accounting record however often it comes.
#[tokio::test]
async fn test_device_admin_script() {
    let tables = ["admins", "tacacs_log", "tacacs_log_user"];
    apply(&renamed(DEVICE_ADMIN_SCHEMA, "da", &tables)).await;
    apply(&renamed(DEVICE_ADMIN_DATA, "da", &tables)).await;
    let script = renamed(DEVICE_ADMIN_SCRIPT, "da", &tables);
    let authorize = |user: &'static str, args: Vec<(&'static str, &'static str)>| {
        let mut attributes = vec![("user", bytes(user)), ("service", bytes("shell"))];
        attributes.extend(args.into_iter().map(|(name, value)| (name, bytes(value))));
        run(&script, "tacacs", "authorization", "core-routers", None, attributes)
    };

    let (outcome, reply) = authorize("noc1", vec![("cmd", "")]).await;
    assert_eq!(outcome, Outcome::Accept);
    assert!(matches!(&reply[..], [(name, AttrValue::Number(level))] if name == "priv-lvl" && *level == 1.0), "{reply:?}");
    for config in ["startup-config", "running-config", "configuration", "archive"] {
        assert_eq!(authorize("noc1", vec![("cmd", "show"), ("cmd-arg", config)]).await.0, Outcome::Reject, "show {config}");
    }
    assert_eq!(authorize("noc1", vec![("cmd", "show")]).await.0, Outcome::Reject, "show with nothing after it");
    assert_eq!(authorize("noc1", vec![("cmd", "show"), ("cmd-arg", "version"), ("cmd-arg", "<cr>")]).await.0, Outcome::Accept);
    assert_eq!(authorize("noc1", vec![("cmd", "ping"), ("cmd-arg", "192.0.2.1")]).await.0, Outcome::Accept);
    assert_eq!(authorize("netadmin1", vec![("cmd", "reload")]).await.0, Outcome::Accept);
    test_db().execute("UPDATE da_admins SET enabled = 0 WHERE username = 'netadmin1'").await.unwrap();
    assert_eq!(authorize("netadmin1", vec![("cmd", "")]).await.0, Outcome::Reject, "disabled");

    let record = vec![("user", bytes("noc1")), ("task_id", bytes("7")), ("stop_time", bytes("1700000000")), ("rem_addr", bytes("192.0.2.50")), ("cmd", bytes("show")), ("cmd-arg", bytes("interfaces")), ("cmd-arg", bytes("<cr>"))];
    for _ in 0..2 {
        assert_eq!(run(&script, "tacacs", "accounting", "core-routers", None, record.clone()).await.0, Outcome::Accept);
    }
    assert_eq!(
        rows("SELECT device, event, username, rem_addr, cmd FROM da_tacacs_log WHERE task_id = '7'").await,
        [vec![txt("core-routers"), txt("stop"), txt("noc1"), txt("192.0.2.50"), txt("show interfaces")]],
    );
    assert_eq!(run(&script, "radius", "authentication", "x", None, vec![]).await.0, Outcome::Reject, "TACACS+ only");
}

/// The block of `lang` in `markdown` that starts with `first_line`.
fn code_block<'a>(markdown: &'a str, lang: &str, first_line: &str) -> &'a str {
    let start = markdown.find(&format!("```{lang}\n{first_line}")).unwrap_or_else(|| panic!("no {first_line} in the manual")) + lang.len() + 4;
    &markdown[start..start + markdown[start..].find("```").unwrap()]
}

/// The purge of chapter 11, as the manual prints it: once an hour, by one request, only rows
/// that are old enough.
#[tokio::test]
async fn test_manual_purge_snippet() {
    let manual = include_str!("../../docs/manual/11-operations.md");
    let tables = ["users", "user_reply", "accounting", "user_reply_user", "accounting_user", "housekeeping"];
    apply(&renamed(QUICK_START_SCHEMA, "hk", &tables)).await;
    apply(&renamed(code_block(manual, "sql", "-- 3_housekeeping.sql"), "hk", &tables)).await;
    let snippet = renamed(code_block(manual, "lua", "-- 每小時最多一次"), "hk", &tables);
    let script = format!("{snippet}\nreturn 'accept'");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let old = now - 200 * 86400;
    test_db()
        .execute(&format!(
            "INSERT INTO hk_accounting (client, nas, session_id, username, started, updated, stopped) VALUES \
             ('n', 'n', 'old', 'u', {old}, {old}, {old}), ('n', 'n', 'open', 'u', {old}, {old}, NULL), ('n', 'n', 'recent', 'u', {now}, {now}, {now})"
        ))
        .await
        .unwrap();
    assert_eq!(run(&script, "radius", "accounting", "n", None, vec![]).await.0, Outcome::Accept);
    assert_eq!(rows("SELECT session_id FROM hk_accounting ORDER BY session_id").await, [vec![txt("open")], vec![txt("recent")]]);

    test_db().execute(&format!("INSERT INTO hk_accounting (client, nas, session_id, username, started, updated, stopped) VALUES ('n', 'n', 'old2', 'u', {old}, {old}, {old})")).await.unwrap();
    assert_eq!(run(&script, "radius", "accounting", "n", None, vec![]).await.0, Outcome::Accept);
    assert_eq!(rows("SELECT count(*) FROM hk_accounting WHERE session_id = 'old2'").await, [vec![int(1)]], "not again within the hour");
}

const EXTERNAL_API_SCRIPT: &str = include_str!("../../rradius-server/examples/scenarios/external-api/member-api.lua");

/// The text a `Bytes` value of `attributes` named `name` holds.
fn reply_text(attributes: &[(String, AttrValue)], name: &str) -> Option<String> {
    attributes.iter().find_map(|(found, value)| match value {
        AttrValue::Bytes(bytes) if found == name => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    })
}

/// A member API for chapter 18 on a local port: what it was asked, and how it answers. The
/// script's URL is pointed at it.
async fn member_api(answer: impl Into<String>) -> Result<(String, std::sync::Arc<std::sync::Mutex<Vec<String>>>), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?.to_string();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = asked.clone();
    let answer: std::sync::Arc<str> = answer.into().into();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = seen.clone();
            let answer = answer.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let Ok(n) = socket.read(&mut buffer).await else { return };
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request);
                    let Some(end) = text.find("\r\n\r\n") else { continue };
                    let length = text[..end].lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|value| value.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let slow = text.contains("username=slow");
                if let Ok(mut asked) = seen.lock() {
                    asked.push(text);
                }
                if slow {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                let _ = socket.write_all(answer.as_bytes()).await;
            });
        }
    });
    Ok((address, asked))
}

async fn run_with_http(script: &str, address: &str, attributes: Vec<(&str, AttrValue)>) -> Result<(Outcome, Vec<(String, AttrValue)>), String> {
    let allowed = crate::config::AllowedHost::parse(&format!("http://{address}"))?;
    let settings = crate::config::ScriptHttpSettings { allowed_hosts: vec![allowed], timeout: std::time::Duration::from_millis(500), max_response_bytes: 4096, max_concurrent: 8, ca_file: None };
    let http = std::sync::Arc::new(crate::script_http::ScriptHttp::new(&settings, std::sync::Arc::new(crate::metrics::Metrics::default()))?);
    let script = script.replace("https://members.example.com", &format!("http://{address}"));
    let engine = LuaAuthEngine::new(test_vms(), test_db(), &script)?.with_script_http(Some(http));
    let request = LuaRequest::new("radius", "authentication", attributes.into_iter().map(|(name, value)| (name.to_string(), value)).collect());
    let decision = engine.authenticate(request).await;
    Ok((decision.outcome, decision.response_attributes))
}

/// Chapter 18: the API decides; its plan goes to the NAS only when it looks like a name, and
/// the user's name and password reach it encoded.
#[tokio::test]
async fn test_member_api_script() -> Result<(), String> {
    let login = |user: &'static str, password: &'static str| vec![("User-Name", bytes(user)), ("User-Password", bytes(password))];

    let (address, asked) = member_api("HTTP/1.1 200 OK\r\nX-Plan: gold_100\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
    let (outcome, reply) = run_with_http(EXTERNAL_API_SCRIPT, &address, login("dave&co", "p@ss word=1")).await?;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(reply_text(&reply, "Filter-Id").as_deref(), Some("gold_100"));
    assert!(reply.iter().any(|(name, value)| name == "Session-Timeout" && matches!(value, AttrValue::Number(n) if *n == 14400.0)), "{reply:?}");
    let sent = asked.lock().map_err(|e| e.to_string())?.concat();
    assert!(sent.starts_with("POST /v1/radius/login "), "{sent}");
    assert!(sent.contains("Bearer member-api-key-change-me"), "{sent}");
    assert!(sent.contains("username=dave%26co&password=p%40ss%20word%3D1") || sent.contains("username=dave%26co&password=p%40ss+word%3D1"), "{sent}");

    let (address, _) = member_api("HTTP/1.1 200 OK\r\nX-Plan: gold; rm\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
    let (outcome, reply) = run_with_http(EXTERNAL_API_SCRIPT, &address, login("dave", "pw")).await?;
    assert_eq!(outcome, Outcome::Accept);
    assert!(reply_text(&reply, "Filter-Id").is_none(), "a plan that is not a name: {reply:?}");

    for status in ["401 Unauthorized", "403 Forbidden", "500 Internal Server Error"] {
        let (address, _) = member_api(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await?;
        assert_eq!(run_with_http(EXTERNAL_API_SCRIPT, &address, login("dave", "pw")).await?.0, Outcome::Reject, "{status}");
    }

    // No answer within the timeout, and nothing asked when there is nothing to ask with.
    let (address, asked) = member_api("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
    assert_eq!(run_with_http(EXTERNAL_API_SCRIPT, &address, login("slow", "pw")).await?.0, Outcome::Reject, "timed out");
    assert_eq!(run_with_http(EXTERNAL_API_SCRIPT, &address, login("dave", "")).await?.0, Outcome::Reject);
    assert_eq!(run_with_http(EXTERNAL_API_SCRIPT, &address, vec![("User-Name", bytes("dave"))]).await?.0, Outcome::Reject, "CHAP has no password to send");
    assert_eq!(asked.lock().map_err(|e| e.to_string())?.len(), 1, "only the slow one was asked");
    Ok(())
}
