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
const IP_POOL_SCHEMA: &str = include_str!("../../rradius-server/examples/scenarios/ip-pool/migrations/1_schema.sql");
const IP_POOL_DATA: &str = include_str!("../../rradius-server/examples/scenarios/ip-pool/migrations/2_demo_data.sql");
const IP_POOL_ALLOCATE: &str = include_str!("../../rradius-server/examples/scenarios/ip-pool/allocate.lua");
const LINE_SCHEMA: &str = include_str!("../../rradius-server/examples/scenarios/line-identity/migrations/1_schema.sql");
const LINE_DATA: &str = include_str!("../../rradius-server/examples/scenarios/line-identity/migrations/2_demo_data.sql");
const LINE_SCRIPT: &str = include_str!("../../rradius-server/examples/scenarios/line-identity/auth.lua");
const STATIC_SCHEMA: &str = include_str!("../../rradius-server/examples/scenarios/static-ip/migrations/1_schema.sql");
const STATIC_DATA: &str = include_str!("../../rradius-server/examples/scenarios/static-ip/migrations/2_demo_data.sql");
const STATIC_SCRIPT: &str = include_str!("../../rradius-server/examples/scenarios/static-ip/static.lua");
const STATIC_ACCOUNTING: &str = include_str!("../../rradius-server/examples/scenarios/static-ip/accounting.lua");
const RATE_SCHEMA: &str = include_str!("../../rradius-server/examples/scenarios/rate-limit/migrations/1_schema.sql");
const RATE_DATA: &str = include_str!("../../rradius-server/examples/scenarios/rate-limit/migrations/2_demo_data.sql");
const RATE_SCRIPT: &str = include_str!("../../rradius-server/examples/scenarios/rate-limit/rate.lua");
const PREFIX_SCHEMA: &str = include_str!("../../rradius-server/examples/scenarios/prefix-pool/migrations/1_schema.sql");
const PREFIX_DATA: &str = include_str!("../../rradius-server/examples/scenarios/prefix-pool/migrations/2_demo_data.sql");
const PREFIX_ALLOCATE: &str = include_str!("../../rradius-server/examples/scenarios/prefix-pool/allocate.lua");
const PREFIX_ACCOUNTING: &str = include_str!("../../rradius-server/examples/scenarios/prefix-pool/accounting.lua");
const IP_POOL_ACCOUNTING: &str = include_str!("../../rradius-server/examples/scenarios/ip-pool/accounting.lua");
const USAGE_SCHEMA: &str = include_str!("../../rradius-server/examples/scenarios/usage-control/migrations/1_schema.sql");
const USAGE_DATA: &str = include_str!("../../rradius-server/examples/scenarios/usage-control/migrations/2_demo_data.sql");
const USAGE_LIMITS: &str = include_str!("../../rradius-server/examples/scenarios/usage-control/limits.lua");
const USAGE_ACCOUNTING: &str = include_str!("../../rradius-server/examples/scenarios/usage-control/accounting.lua");

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

// ---- The address pool of chapter 14 ----

const POOL_TABLES: [&str; 7] = ["subscribers", "ip_pool", "housekeeping", "ip_pool_user", "ip_pool_state", "ip_pool_expiry", "ip_pool_last_user"];

/// The pool's schema and demo data under `prefix`, with the two scripts.
struct Pool {
    prefix: &'static str,
    allocate: String,
    accounting: String,
}

impl Pool {
    async fn new(prefix: &'static str) -> Pool {
        apply(&renamed(IP_POOL_SCHEMA, prefix, &POOL_TABLES)).await;
        apply(&renamed(IP_POOL_DATA, prefix, &POOL_TABLES)).await;
        // The first request would reap the table; the tests that look at a reap ask for it.
        apply(&format!("UPDATE {prefix}_housekeeping SET last_run = {}", now())).await;
        Pool { prefix, allocate: renamed(IP_POOL_ALLOCATE, prefix, &POOL_TABLES), accounting: renamed(IP_POOL_ACCOUNTING, prefix, &POOL_TABLES) }
    }

    /// The chain's lua handler for `user`: the outcome and the Framed-IP-Address it gave.
    async fn allocate(&self, user: &str) -> (Outcome, Option<String>, Vec<(String, AttrValue)>) {
        let (outcome, attributes) = run(&self.allocate, "radius", "authentication", "nas-a", Some(user), vec![("User-Name", bytes(user))]).await;
        let ip = attributes.iter().find_map(|(name, value)| match value {
            AttrValue::Bytes(found) if name == "Framed-IP-Address" => Some(String::from_utf8_lossy(found).into_owned()),
            _ => None,
        });
        (outcome, ip, attributes)
    }

    /// An accounting packet from the NAS `nas` for `user`'s session `session`.
    async fn account(&self, nas: &str, status: f64, user: &str, session: &str, ip: &str) -> Outcome {
        let attributes = vec![
            ("User-Name", bytes(user)),
            ("Acct-Session-Id", bytes(session)),
            ("Acct-Status-Type", number(status)),
            ("Framed-IP-Address", bytes(ip)),
            ("NAS-IP-Address", bytes(nas)),
        ];
        run(&self.accounting, "radius", "accounting", "nas-a", None, attributes).await.0
    }

    /// (state, user, session, nas) of the address.
    async fn row(&self, ip: &str) -> Vec<DbValue> {
        let found = rows(&format!("SELECT state, username, session_id, nas FROM {}_ip_pool WHERE ip = '{ip}'", self.prefix)).await;
        found.into_iter().next().unwrap_or_default()
    }

    async fn count(&self, state: &str) -> i64 {
        match rows(&format!("SELECT count(*) FROM {}_ip_pool WHERE state = '{state}'", self.prefix)).await.first().and_then(|row| row.first()) {
            Some(DbValue::Integer(count)) => *count,
            _ => -1,
        }
    }

    async fn execute(&self, sql: &str) {
        apply(&sql.replace("POOL", &format!("{}_ip_pool", self.prefix)).replace("SUBS", &format!("{}_subscribers", self.prefix)).replace("HOUSE", &format!("{}_housekeeping", self.prefix))).await;
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn in_range(ip: &str, prefix: &str) -> bool {
    ip.starts_with(prefix)
}

/// Who gets what: one address for each user from the user's own pool, the same one when asking
/// again, and none for an account that is disabled or unknown.
#[tokio::test]
async fn test_ip_pool_gives_each_user_an_address() -> Result<(), String> {
    let pool = Pool::new("ipa").await;
    let (outcome, alice, attributes) = pool.allocate("alice").await;
    assert_eq!(outcome, Outcome::Accept);
    let alice = alice.ok_or("no address for alice")?;
    assert!(in_range(&alice, "10.77.0."), "{alice}");
    assert!(attributes.iter().any(|(name, value)| name == "Acct-Interim-Interval" && *value == number(300.0)), "{attributes:?}");
    assert_eq!(pool.row(&alice).await[..2], [txt("reserved"), txt("alice")]);

    // Again: the same address, and no second one taken.
    assert_eq!(pool.allocate("alice").await.1.as_deref(), Some(alice.as_str()));
    assert_eq!(pool.count("reserved").await, 1);

    let bob = pool.allocate("bob").await.1.ok_or("no address for bob")?;
    let carol = pool.allocate("carol").await.1.ok_or("no address for carol")?;
    assert!(alice != bob && bob != carol && alice != carol, "{alice} {bob} {carol}");
    let dave = pool.allocate("dave").await.1.ok_or("no address for dave")?;
    assert!(in_range(&dave, "10.88.0."), "the business pool: {dave}");

    assert_eq!(pool.allocate("erin").await.0, Outcome::Reject, "disabled");
    assert_eq!(pool.allocate("nobody").await.0, Outcome::Reject, "unknown");
    assert_eq!(pool.count("reserved").await, 4, "nothing was taken for erin or nobody");
    Ok(())
}

/// Requests at the same time never get the same address, and when there are more of them than
/// addresses, exactly as many as there are addresses succeed.
#[tokio::test]
async fn test_ip_pool_at_the_same_time() -> Result<(), String> {
    let pool = Pool::new("ipc").await;
    let users: Vec<String> = (1..=12).map(|n| format!("u{n}")).collect();
    for user in &users {
        pool.execute(&format!("INSERT INTO SUBS (username, password, pool, enabled) VALUES ('{user}', 'x', 'residential', 1)")).await;
    }
    // Five at once: all get one, none the same. Then seven at once for the three that are left.
    let mut asked = futures::future::join_all(users[..5].iter().map(|user| pool.allocate(user))).await;
    assert!(asked.iter().all(|(outcome, _, _)| *outcome == Outcome::Accept), "five of eight");
    asked.extend(futures::future::join_all(users[5..].iter().map(|user| pool.allocate(user))).await);
    let mut addresses: Vec<String> = asked.iter().filter(|(outcome, _, _)| *outcome == Outcome::Accept).filter_map(|(_, ip, _)| ip.clone()).collect();
    assert_eq!(addresses.len(), 8, "eight addresses in the pool");
    addresses.sort();
    addresses.dedup();
    assert_eq!(addresses.len(), 8, "no address twice");
    let refused: Vec<_> = asked.iter().filter(|(outcome, _, _)| *outcome != Outcome::Accept).collect();
    assert_eq!(refused.len(), 4);
    assert!(refused.iter().all(|(_, ip, _)| ip.is_none()), "no address for the refused");
    assert_eq!(pool.count("reserved").await, 8);
    assert_eq!(pool.count("free").await, 3, "the business pool is untouched");
    Ok(())
}

/// Only the lowest and the highest address are free: whatever start the pick draws, both are found,
/// and the third user finds the pool used up.
#[tokio::test]
async fn test_ip_pool_finds_free_addresses_at_both_ends() -> Result<(), String> {
    let pool = Pool::new("ipe").await;
    pool.execute("UPDATE POOL SET state = 'reserved', username = 'other', expires = 4102444800 WHERE pool = 'residential' AND n NOT IN (1, 8)").await;
    let mut given = Vec::new();
    for user in ["alice", "bob"] {
        let (outcome, ip, _) = pool.allocate(user).await;
        assert_eq!(outcome, Outcome::Accept, "{user} gets one of the two free addresses");
        given.push(ip.ok_or("no address given")?);
    }
    given.sort();
    assert_eq!(given, ["10.77.0.2", "10.77.0.9"], "the lowest and the highest");
    let (outcome, _, _) = pool.allocate("carol").await;
    assert_eq!(outcome, Outcome::Reject, "nothing left for the third");
    Ok(())
}

/// The life of an address: reserved at the login, active at the Start, kept by Interim-Updates,
/// free at the Stop, and given back to the same user on the next login.
#[tokio::test]
async fn test_ip_pool_follows_the_session() -> Result<(), String> {
    let pool = Pool::new("ipl").await;
    let alice = pool.allocate("alice").await.1.ok_or("no address for alice")?;

    assert_eq!(pool.account("10.0.0.1", 1.0, "alice", "s1", &alice).await, Outcome::Accept);
    assert_eq!(pool.row(&alice).await, [txt("active"), txt("alice"), txt("s1"), txt("10.0.0.1")]);
    // The same Start twice is one Start; an Interim-Update moves the expiry.
    assert_eq!(pool.account("10.0.0.1", 1.0, "alice", "s1", &alice).await, Outcome::Accept);
    pool.execute(&format!("UPDATE POOL SET expires = 5 WHERE ip = '{alice}'")).await;
    assert_eq!(pool.account("10.0.0.1", 3.0, "alice", "s1", &alice).await, Outcome::Accept);
    let expires = rows(&format!("SELECT expires > {} FROM {}_ip_pool WHERE ip = '{alice}'", now() + 800, pool.prefix)).await;
    assert_eq!(expires, [vec![int(1)]], "about 15 minutes from now");
    // Logging in again while the session is up gives the address it has.
    assert_eq!(pool.allocate("alice").await.1.as_deref(), Some(alice.as_str()));

    // A Stop of another session of the user does not take the address from this one.
    assert_eq!(pool.account("10.0.0.1", 2.0, "alice", "old-session", &alice).await, Outcome::Accept);
    assert_eq!(pool.row(&alice).await[0], txt("active"));
    assert_eq!(pool.account("10.0.0.1", 2.0, "alice", "s1", &alice).await, Outcome::Accept);
    assert_eq!(pool.row(&alice).await, [txt("free"), DbValue::Null, DbValue::Null, DbValue::Null]);
    // The Stop again changes nothing.
    assert_eq!(pool.account("10.0.0.1", 2.0, "alice", "s1", &alice).await, Outcome::Accept);
    assert_eq!(pool.count("free").await, 11, "all eleven are free");

    // Sticky: round after round, the same address.
    for round in 0..4 {
        let again = pool.allocate("alice").await.1.ok_or("no address for alice")?;
        assert_eq!(again, alice, "round {round}");
        assert_eq!(pool.account("10.0.0.1", 1.0, "alice", "s2", &again).await, Outcome::Accept);
        assert_eq!(pool.account("10.0.0.1", 2.0, "alice", "s2", &again).await, Outcome::Accept);
    }
    // Someone else has it: alice gets another one, and is not refused.
    pool.execute(&format!("UPDATE POOL SET state = 'reserved', username = 'bob', expires = {} WHERE ip = '{alice}'", now() + 100)).await;
    let other = pool.allocate("alice").await.1.ok_or("no address for alice")?;
    assert!(other != alice && in_range(&other, "10.77.0."), "{other}");
    // After the hold time the address is nobody's in particular.
    pool.execute(&format!("UPDATE POOL SET state = 'free', username = NULL, last_user = 'alice', released = {} WHERE ip = '{alice}'", now() - 90000)).await;
    pool.execute(&format!("UPDATE POOL SET state = 'free', username = NULL WHERE ip = '{other}'")).await;
    assert_eq!(pool.count("reserved").await, 0);
    Ok(())
}

/// What is not confirmed or kept goes back: a reservation that never became a session, a session
/// whose Interim-Updates stopped, and, at once, every session of a NAS that restarted.
#[tokio::test]
async fn test_ip_pool_takes_back_what_is_not_used() -> Result<(), String> {
    let pool = Pool::new("ipr").await;
    let alice = pool.allocate("alice").await.1.ok_or("no address for alice")?;
    let bob = pool.allocate("bob").await.1.ok_or("no address for bob")?;
    let carol = pool.allocate("carol").await.1.ok_or("no address for carol")?;
    assert_eq!(pool.account("10.0.0.1", 1.0, "bob", "s-bob", &bob).await, Outcome::Accept);
    assert_eq!(pool.account("10.0.0.2", 1.0, "carol", "s-carol", &carol).await, Outcome::Accept);

    // Not yet: the reaper has run a moment ago.
    assert_eq!(pool.allocate("dave").await.0, Outcome::Accept);
    assert_eq!(pool.count("reserved").await + pool.count("active").await, 4);

    // alice never started, bob's Interim-Updates stopped, carol is fine.
    pool.execute(&format!("UPDATE POOL SET expires = 1 WHERE ip IN ('{alice}', '{bob}')")).await;
    pool.execute("UPDATE HOUSE SET last_run = 0").await;
    assert_eq!(pool.allocate("dave").await.0, Outcome::Accept);
    assert_eq!(pool.row(&alice).await[0], txt("free"));
    assert_eq!(pool.row(&bob).await[0], txt("free"));
    assert_eq!(pool.row(&carol).await[0], txt("active"));
    assert_eq!(rows(&format!("SELECT last_user FROM {}_ip_pool WHERE ip = '{bob}'", pool.prefix)).await, [vec![txt("bob")]], "the last user is kept");

    // Late packet of bob's old session: the address is free, so it is taken back.
    assert_eq!(pool.account("10.0.0.1", 3.0, "bob", "s-bob", &bob).await, Outcome::Accept);
    assert_eq!(pool.row(&bob).await[..3], [txt("active"), txt("bob"), txt("s-bob")]);
    // Late packet for an address someone else has: nothing changes, and the NAS is answered.
    pool.execute(&format!("UPDATE POOL SET state = 'reserved', username = 'carol', expires = {} WHERE ip = '{alice}'", now() + 100)).await;
    assert_eq!(pool.account("10.0.0.1", 3.0, "alice", "s-alice", &alice).await, Outcome::Accept);
    assert_eq!(pool.row(&alice).await[..2], [txt("reserved"), txt("carol")]);

    // The NAS 10.0.0.2 restarts: carol's address is free; bob's, on another NAS, is not touched.
    assert_eq!(run(&pool.accounting, "radius", "accounting", "nas-a", None, vec![("Acct-Status-Type", number(7.0)), ("NAS-IP-Address", bytes("10.0.0.2"))]).await.0, Outcome::Accept);
    assert_eq!(pool.row(&carol).await[0], txt("free"));
    assert_eq!(pool.row(&bob).await[0], txt("active"));
    Ok(())
}

// ---- The usage control of chapter 15 ----

const USAGE_TABLES: [&str; 7] = ["plans", "subscribers", "session_slots", "accounting", "housekeeping", "session_slots_expiry", "accounting_user"];

/// The scenario's schema and demo data under `prefix`, with its two scripts.
struct Usage {
    prefix: &'static str,
    limits: String,
    accounting: String,
}

impl Usage {
    async fn new(prefix: &'static str) -> Usage {
        apply(&renamed(USAGE_SCHEMA, prefix, &USAGE_TABLES)).await;
        apply(&renamed(USAGE_DATA, prefix, &USAGE_TABLES)).await;
        // The first request would reap the slots; the tests that look at a reap ask for it.
        apply(&format!("UPDATE {prefix}_housekeeping SET last_run = {}", now())).await;
        Usage { prefix, limits: renamed(USAGE_LIMITS, prefix, &USAGE_TABLES), accounting: renamed(USAGE_ACCOUNTING, prefix, &USAGE_TABLES) }
    }

    /// The chain's lua handler for `user`: the outcome and the Filter-Id it gave.
    async fn login(&self, user: &str) -> (Outcome, Option<String>) {
        let (outcome, attributes) = run(&self.limits, "radius", "authentication", "nas-a", Some(user), vec![("User-Name", bytes(user))]).await;
        let filter = attributes.iter().find_map(|(name, value)| match value {
            AttrValue::Bytes(found) if name == "Filter-Id" => Some(String::from_utf8_lossy(found).into_owned()),
            _ => None,
        });
        (outcome, filter)
    }

    /// An accounting packet from the NAS `nas`, with `extra` attributes.
    async fn account(&self, nas: &str, status: f64, user: &str, session: &str, extra: Vec<(&str, AttrValue)>) -> Outcome {
        let mut attributes = vec![("User-Name", bytes(user)), ("Acct-Session-Id", bytes(session)), ("Acct-Status-Type", number(status)), ("NAS-IP-Address", bytes(nas))];
        attributes.extend(extra);
        run(&self.accounting, "radius", "accounting", "nas-a", None, attributes).await.0
    }

    /// The states of the user's slots, in the order of the slots.
    async fn slots(&self, user: &str) -> Vec<String> {
        rows(&format!("SELECT state FROM {}_session_slots WHERE username = '{user}' ORDER BY slot", self.prefix))
            .await
            .into_iter()
            .filter_map(|row| match row.into_iter().next() {
                Some(DbValue::Text(state)) => Some(state),
                _ => None,
            })
            .collect()
    }

    async fn execute(&self, sql: &str) {
        let sql = sql
            .replace("PLANS", &format!("{}_plans", self.prefix))
            .replace("SUBS", &format!("{}_subscribers", self.prefix))
            .replace("SLOTS", &format!("{}_session_slots", self.prefix))
            .replace("ACCT", &format!("{}_accounting", self.prefix))
            .replace("HOUSE", &format!("{}_housekeeping", self.prefix));
        apply(&sql).await;
    }
}

/// The octets of an Accounting-Interim-Update, as the NAS sends them: gigawords and octets.
fn usage_of(input_gigawords: f64, input_octets: f64) -> Vec<(&'static str, AttrValue)> {
    vec![("Acct-Input-Gigawords", number(input_gigawords)), ("Acct-Input-Octets", number(input_octets)), ("Acct-Session-Time", number(60.0))]
}

/// One session slot for a plan of one session: the second login is refused until the first
/// session ends; a disabled or unknown account has none.
#[tokio::test]
async fn test_usage_control_limits_sessions() -> Result<(), String> {
    let usage = Usage::new("ucl").await;
    let (outcome, filter) = usage.login("alice").await;
    assert_eq!((outcome, filter.as_deref()), (Outcome::Accept, Some("basic")));
    assert_eq!(usage.slots("alice").await, ["reserved"]);
    // Waiting for the NAS to start the session, a second login finds no slot.
    assert_eq!(usage.login("alice").await.0, Outcome::Reject);
    assert_eq!(usage.slots("alice").await, ["reserved"], "the refused login took nothing");

    assert_eq!(usage.account("10.0.0.1", 1.0, "alice", "s1", vec![]).await, Outcome::Accept);
    assert_eq!(usage.slots("alice").await, ["active"]);
    assert_eq!(usage.login("alice").await.0, Outcome::Reject, "the session is up");
    // The same Start twice is one Start.
    assert_eq!(usage.account("10.0.0.1", 1.0, "alice", "s1", vec![]).await, Outcome::Accept);
    assert_eq!(usage.slots("alice").await, ["active"]);

    assert_eq!(usage.account("10.0.0.1", 2.0, "alice", "s1", vec![]).await, Outcome::Accept);
    assert_eq!(usage.slots("alice").await, ["free"]);
    assert_eq!(usage.account("10.0.0.1", 2.0, "alice", "s1", vec![]).await, Outcome::Accept, "the Stop again");
    assert_eq!(usage.login("alice").await.0, Outcome::Accept, "room again");

    assert_eq!(usage.login("erin").await.0, Outcome::Reject, "disabled");
    assert_eq!(usage.login("nobody").await.0, Outcome::Reject, "unknown");
    assert!(usage.slots("erin").await.is_empty() && usage.slots("nobody").await.is_empty(), "no slots for them");
    Ok(())
}

/// Logins at the same time never go past the plan's limit.
#[tokio::test]
async fn test_usage_control_at_the_same_time() -> Result<(), String> {
    let usage = Usage::new("ucc").await;
    let asked = futures::future::join_all((0..6).map(|_| usage.login("bob"))).await;
    let accepted = asked.iter().filter(|(outcome, _)| *outcome == Outcome::Accept).count();
    assert_eq!(accepted, 2, "bob's plan has two sessions");
    assert_eq!(usage.slots("bob").await, ["reserved", "reserved"], "two slots, once each");
    Ok(())
}

/// The traffic of the last period decides: the plan's `Filter-Id` while there is some left, the
/// slow one or a refusal once it is used up, at the byte; sessions still open count, old ones do not.
#[tokio::test]
async fn test_usage_control_quota() -> Result<(), String> {
    let usage = Usage::new("ucq").await;
    // The slots are freed by hand after each step, so that only the traffic decides.
    let free = |user: &'static str| {
        let sql = format!("UPDATE SLOTS SET state = 'free', session_id = NULL WHERE username = '{user}'");
        let usage = &usage;
        async move { usage.execute(&sql).await }
    };
    // dave: family plan, 100 GiB (25 gigawords) over 30 days, then slow.
    assert_eq!(usage.login("dave").await.1.as_deref(), Some("family"), "nothing used yet");
    free("dave").await;
    // A session still open, one byte short of the quota.
    assert_eq!(usage.account("10.0.0.1", 3.0, "dave", "d1", usage_of(24.0, 4294967295.0)).await, Outcome::Accept);
    free("dave").await;
    assert_eq!(usage.login("dave").await.1.as_deref(), Some("family"), "one byte short");
    free("dave").await;
    // At the quota: logged in, but slow.
    assert_eq!(usage.account("10.0.0.1", 3.0, "dave", "d1", usage_of(25.0, 0.0)).await, Outcome::Accept);
    free("dave").await;
    let (outcome, filter) = usage.login("dave").await;
    assert_eq!((outcome, filter.as_deref()), (Outcome::Accept, Some("family-slow")));
    // The traffic of a session older than the period is not counted.
    free("dave").await;
    usage.execute(&format!("UPDATE ACCT SET started = {}, updated = {}, stopped = {} WHERE username = 'dave'", now() - 31 * 86400, now() - 31 * 86400 + 60, now() - 31 * 86400 + 60)).await;
    assert_eq!(usage.login("dave").await.1.as_deref(), Some("family"), "old traffic is forgotten");

    // carol: strict plan, 5 GiB (a gigaword and a GiB), then refused.
    assert_eq!(usage.login("carol").await.0, Outcome::Accept);
    free("carol").await;
    assert_eq!(usage.account("10.0.0.2", 3.0, "carol", "c1", usage_of(1.0, 1073741823.0)).await, Outcome::Accept);
    free("carol").await;
    assert_eq!(usage.login("carol").await.0, Outcome::Accept, "one byte short");
    free("carol").await;
    assert_eq!(usage.account("10.0.0.2", 3.0, "carol", "c1", usage_of(1.0, 1073741824.0)).await, Outcome::Accept);
    free("carol").await;
    assert_eq!(usage.login("carol").await.0, Outcome::Reject, "used up");
    assert_eq!(usage.slots("carol").await, ["free"], "a refusal for the quota takes no slot");
    Ok(())
}

/// What the accounting does with slots: the traffic of the session, slots kept by Interim-Updates,
/// taken back when nothing keeps them, and freed with the NAS that restarts.
#[tokio::test]
async fn test_usage_control_follows_the_session() -> Result<(), String> {
    let usage = Usage::new("ucs").await;
    assert_eq!(usage.login("bob").await.0, Outcome::Accept);
    assert_eq!(usage.login("bob").await.0, Outcome::Accept);
    assert_eq!(usage.account("10.0.0.1", 1.0, "bob", "b1", vec![]).await, Outcome::Accept);
    assert_eq!(usage.slots("bob").await, ["active", "reserved"], "one session started, one waiting");

    // An Interim-Update keeps the slot of its session.
    usage.execute("UPDATE SLOTS SET expires = 5 WHERE username = 'bob' AND state = 'active'").await;
    assert_eq!(usage.account("10.0.0.1", 3.0, "bob", "b1", usage_of(1.0, 10.0)).await, Outcome::Accept);
    let kept = rows(&format!("SELECT expires > {} FROM {}_session_slots WHERE state = 'active'", now() + 800, usage.prefix)).await;
    assert_eq!(kept, [vec![int(1)]], "about 15 minutes from now");
    // The traffic of the session, a gigaword carried over.
    let seen = rows(&format!("SELECT input_octets, stopped IS NULL FROM {}_accounting WHERE session_id = 'b1'", usage.prefix)).await;
    assert_eq!(seen, [vec![int(4294967306), int(1)]]);

    // Nothing keeps b1 and the other login never became a session: both are taken back.
    usage.execute("UPDATE SLOTS SET expires = 1 WHERE username = 'bob'").await;
    usage.execute("UPDATE HOUSE SET last_run = 0").await;
    assert_eq!(usage.login("dave").await.0, Outcome::Accept, "any request does the reaping");
    assert_eq!(usage.slots("bob").await, ["free", "free"]);

    // An Interim-Update of b1 after that takes a slot back, if there is one free.
    assert_eq!(usage.account("10.0.0.1", 3.0, "bob", "b1", vec![]).await, Outcome::Accept);
    assert_eq!(usage.slots("bob").await, ["active", "free"]);
    // An Interim-Update of a user who never logged in has no slot to take, and is answered anyway.
    assert_eq!(usage.account("10.0.0.1", 3.0, "carol", "c9", vec![]).await, Outcome::Accept);
    assert!(usage.slots("carol").await.is_empty());

    // The NAS 10.0.0.1 restarts: its sessions end, the slots are free.
    assert_eq!(run(&usage.accounting, "radius", "accounting", "nas-a", None, vec![("Acct-Status-Type", number(7.0)), ("NAS-IP-Address", bytes("10.0.0.1"))]).await.0, Outcome::Accept);
    assert_eq!(usage.slots("bob").await, ["free", "free"]);
    let ended = rows(&format!("SELECT count(*) FROM {}_accounting WHERE stopped IS NULL AND nas = '10.0.0.1'", usage.prefix)).await;
    assert_eq!(ended, [vec![int(0)]]);
    Ok(())
}

// ---- The line identity of chapter 16 ----

const LINE_TABLES: [&str; 2] = ["plans", "lines"];

/// The line scenario's schema and demo data under `prefix`, with its script.
struct Lines {
    prefix: &'static str,
    script: String,
}

impl Lines {
    async fn new(prefix: &'static str) -> Lines {
        apply(&renamed(LINE_SCHEMA, prefix, &LINE_TABLES)).await;
        apply(&renamed(LINE_DATA, prefix, &LINE_TABLES)).await;
        Lines { prefix, script: renamed(LINE_SCRIPT, prefix, &LINE_TABLES) }
    }

    /// The decision for a request with these attributes.
    async fn ask(&self, attributes: Vec<(&str, AttrValue)>) -> (Outcome, Vec<(String, AttrValue)>) {
        run(&self.script, "radius", "authentication", "bng", None, attributes).await
    }

    /// A request for the line with circuit-id `circuit` behind `remote`, from the station `mac`.
    async fn line(&self, remote: &str, circuit: &[u8], mac: &str) -> (Outcome, Vec<(String, AttrValue)>) {
        let mut attributes = vec![("ADSL-Agent-Circuit-Id", AttrValue::Bytes(circuit.to_vec())), ("Calling-Station-Id", bytes(mac))];
        if !remote.is_empty() {
            attributes.push(("ADSL-Agent-Remote-Id", bytes(remote)));
        }
        self.ask(attributes).await
    }

    async fn column(&self, column: &str, remote: &str, circuit: &str) -> Vec<Vec<DbValue>> {
        rows(&format!("SELECT {column} FROM {}_lines WHERE remote_id = '{remote}' AND circuit_id = '{circuit}'", self.prefix)).await
    }
}

fn reply_number(attributes: &[(String, AttrValue)], name: &str) -> Option<f64> {
    attributes.iter().find_map(|(found, value)| match value {
        AttrValue::Number(number) if found == name => Some(*number),
        _ => None,
    })
}

/// A known line is accepted with its plan's pool, profile and interim interval, and the
/// subscriber in Class; the same line written with other case and spaces is the same line.
#[tokio::test]
async fn test_line_identity_known_line() -> Result<(), String> {
    let lines = Lines::new("lia").await;
    let (outcome, reply) = lines.line("olt-a", b"eth 1/1/3:100", "00:11:22:aa:bb:cc").await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(reply_text(&reply, "Framed-Pool").as_deref(), Some("residential"));
    assert_eq!(reply_text(&reply, "Filter-Id").as_deref(), Some("fiber-100m"));
    assert_eq!(reply_number(&reply, "Acct-Interim-Interval"), Some(300.0));
    assert_eq!(reply_text(&reply, "Class").as_deref(), Some("alice"));
    let (outcome, reply) = lines.line("OLT-A", b"  ETH 1/1/3:100 ", "00:11:22:aa:bb:cc").await;
    assert_eq!(outcome, Outcome::Accept, "case and spaces");
    assert_eq!(reply_text(&reply, "Class").as_deref(), Some("alice"));
    let (outcome, reply) = lines.line("olt-b", b"eth 2/0/7:200", "00:11:22:aa:bb:dd").await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(reply_text(&reply, "Framed-Pool").as_deref(), Some("business"));
    assert_eq!(reply_number(&reply, "Acct-Interim-Interval"), Some(600.0));
    Ok(())
}

/// A circuit-id of raw octets is matched as hex, and NAS-Port-Id stands in for a missing circuit-id.
#[tokio::test]
async fn test_line_identity_binary_id_and_nas_port() -> Result<(), String> {
    let lines = Lines::new("lib").await;
    let (outcome, reply) = lines.line("", &[0x00, 0x04, 0x00, 0x0a, 0x00, 0x01], "00:11:22:aa:bb:ee").await;
    assert_eq!(outcome, Outcome::Accept, "binary circuit-id, no remote-id");
    assert_eq!(reply_text(&reply, "Class").as_deref(), Some("erin"));
    let (outcome, reply) = lines.ask(vec![("NAS-Port-Id", bytes("eth 1/1/3:100")), ("ADSL-Agent-Remote-Id", bytes("olt-a"))]).await;
    assert_eq!(outcome, Outcome::Accept, "NAS-Port-Id as the circuit");
    assert_eq!(reply_text(&reply, "Class").as_deref(), Some("alice"));
    Ok(())
}

/// What is refused: an unknown line, a remote-id that is not the line's, a disabled line, a
/// request with no line at all, and a plan that does not exist.
#[tokio::test]
async fn test_line_identity_refusals() -> Result<(), String> {
    let lines = Lines::new("lic").await;
    assert_eq!(lines.line("olt-a", b"eth 9/9/9:9", "00:11:22:aa:bb:cc").await.0, Outcome::Reject, "unknown circuit");
    assert_eq!(lines.line("olt-b", b"eth 1/1/3:100", "00:11:22:aa:bb:cc").await.0, Outcome::Reject, "another access node");
    assert_eq!(lines.line("", b"eth 1/1/3:100", "00:11:22:aa:bb:cc").await.0, Outcome::Reject, "no remote-id for a line that has one");
    assert_eq!(lines.line("olt-b", b"eth 2/0/8:200", "00:11:22:aa:bb:cc").await.0, Outcome::Reject, "disabled");
    assert_eq!(lines.ask(vec![("Calling-Station-Id", bytes("00:11:22:aa:bb:cc"))]).await.0, Outcome::Reject, "no circuit-id");
    assert_eq!(lines.ask(vec![("ADSL-Agent-Circuit-Id", bytes("   "))]).await.0, Outcome::Reject, "blank circuit-id");
    test_db().execute("UPDATE lic_lines SET plan = 'gone' WHERE circuit_id = 'eth 1/1/3:100'").await.map_err(|e| e.to_string())?;
    assert_eq!(lines.line("olt-a", b"eth 1/1/3:100", "00:11:22:aa:bb:cc").await.0, Outcome::Reject, "plan missing");
    // Not RADIUS: refused; accounting: answered.
    let (outcome, _) = run(&lines.script, "tacacs", "authentication", "bng", None, vec![]).await;
    assert_eq!(outcome, Outcome::Reject, "TACACS+");
    let (outcome, _) = run(&lines.script, "radius", "accounting", "bng", None, vec![]).await;
    assert_eq!(outcome, Outcome::Accept, "accounting");
    Ok(())
}

/// The MAC address: without a lock it is only recorded; with one, the first station is bound and
/// another is refused until the binding is cleared.
#[tokio::test]
async fn test_line_identity_mac() -> Result<(), String> {
    let lines = Lines::new("lid").await;
    // alice has no lock: any station, the last one is recorded.
    assert_eq!(lines.line("olt-a", b"eth 1/1/3:100", "00-11-22-AA-BB-01").await.0, Outcome::Accept);
    assert_eq!(lines.line("olt-a", b"eth 1/1/3:100", "00:11:22:aa:bb:02").await.0, Outcome::Accept);
    assert_eq!(lines.column("last_mac", "olt-a", "eth 1/1/3:100").await, [vec![txt("001122aabb02")]]);
    assert_eq!(lines.column("bound_mac", "olt-a", "eth 1/1/3:100").await, [vec![DbValue::Null]], "not bound");
    // bob is locked: the first station is bound, another is refused, the first is accepted again.
    assert_eq!(lines.line("olt-a", b"eth 1/1/4:100", "00:11:22:aa:bb:03").await.0, Outcome::Accept);
    assert_eq!(lines.column("bound_mac", "olt-a", "eth 1/1/4:100").await, [vec![txt("001122aabb03")]]);
    assert_eq!(lines.line("olt-a", b"eth 1/1/4:100", "00:11:22:aa:bb:04").await.0, Outcome::Reject, "another station");
    assert_eq!(lines.line("olt-a", b"eth 1/1/4:100", "0011.22aa.bb03").await.0, Outcome::Accept, "the same, written differently");
    assert_eq!(lines.ask(vec![("ADSL-Agent-Circuit-Id", bytes("eth 1/1/4:100")), ("ADSL-Agent-Remote-Id", bytes("olt-a"))]).await.0, Outcome::Reject, "no MAC in a locked line");
    // Replacing the equipment: the operator clears the binding, and the next station is bound.
    test_db().execute("UPDATE lid_lines SET bound_mac = NULL WHERE circuit_id = 'eth 1/1/4:100'").await.map_err(|e| e.to_string())?;
    assert_eq!(lines.line("olt-a", b"eth 1/1/4:100", "00:11:22:aa:bb:04").await.0, Outcome::Accept);
    assert_eq!(lines.column("bound_mac", "olt-a", "eth 1/1/4:100").await, [vec![txt("001122aabb04")]]);
    Ok(())
}

/// Two first logins at once on a locked line: one station is bound and the other refused.
#[tokio::test]
async fn test_line_identity_mac_race() -> Result<(), String> {
    let lines = Lines::new("lie").await;
    let (first, second) = futures::future::join(lines.line("olt-a", b"eth 1/1/4:100", "00:11:22:aa:bb:05"), lines.line("olt-a", b"eth 1/1/4:100", "00:11:22:aa:bb:06")).await;
    let outcomes = [first.0, second.0];
    assert_eq!(outcomes.iter().filter(|outcome| **outcome == Outcome::Accept).count(), 1, "{outcomes:?}");
    let bound = lines.column("bound_mac", "olt-a", "eth 1/1/4:100").await;
    assert!(bound == [vec![txt("001122aabb05")]] || bound == [vec![txt("001122aabb06")]], "{bound:?}");
    Ok(())
}

// ---- The static addresses and prefixes of chapter 17 ----

const STATIC_TABLES: [&str; 5] = ["subscribers", "static_assignments", "static_assignments_ipv4", "static_assignments_wan_v6", "static_assignments_pd_v6"];

/// The static-ip scenario's schema and demo data under `prefix`, with its script.
struct Statics {
    prefix: &'static str,
    script: String,
}

impl Statics {
    async fn new(prefix: &'static str) -> Statics {
        apply(&renamed(STATIC_SCHEMA, prefix, &STATIC_TABLES)).await;
        apply(&renamed(STATIC_DATA, prefix, &STATIC_TABLES)).await;
        Statics { prefix, script: renamed(STATIC_SCRIPT, prefix, &STATIC_TABLES) }
    }

    /// The chain's lua handler for `user`. The checker stands in for the dictionary: it refuses a
    /// value that starts with "bad", as `set_attr` refuses what the attribute's type cannot hold.
    async fn ask(&self, user: &str) -> (Outcome, Vec<(String, AttrValue)>) {
        let engine = LuaAuthEngine::new(test_vms(), test_db(), &self.script).unwrap();
        let mut request = LuaRequest::new("radius", "authentication", vec![("User-Name".to_string(), bytes(user))]);
        request.client = "bng".to_string();
        request.auth_user = Some(user.to_string());
        request.validator = Some(std::sync::Arc::new(|name: &str, value: &AttrValue| match value {
            AttrValue::Bytes(text) if text.starts_with(b"bad") || text.windows(4).any(|part| part == b"bad ") => Err(format!("{name}: cannot hold {}", String::from_utf8_lossy(text))),
            _ => Ok(()),
        }));
        let decision = engine.authenticate(request).await;
        (decision.outcome, decision.response_attributes)
    }

    async fn execute(&self, sql: &str) -> Result<(), String> {
        test_db().execute(&sql.replace("ASSIGNMENTS", &format!("{}_static_assignments", self.prefix))).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

fn names(attributes: &[(String, AttrValue)]) -> Vec<&str> {
    let mut found: Vec<&str> = attributes.iter().map(|(name, _)| name.as_str()).collect();
    found.sort_unstable();
    found
}

/// Who gets what: everything a user has, nothing the user has not, the pool's name for a user
/// without a row, and a refusal for a disabled or unknown user.
#[tokio::test]
async fn test_static_ip_replies() -> Result<(), String> {
    let statics = Statics::new("sta").await;
    let (outcome, reply) = statics.ask("carol").await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(reply_text(&reply, "Framed-IP-Address").as_deref(), Some("203.0.113.20"));
    assert_eq!(reply_text(&reply, "Framed-IP-Netmask").as_deref(), Some("255.255.255.252"));
    assert_eq!(reply_text(&reply, "Framed-Route").as_deref(), Some("198.51.100.8/29 0.0.0.0 1"));
    assert_eq!(reply_text(&reply, "Framed-IPv6-Prefix").as_deref(), Some("2001:db8:1:1::/64"));
    assert_eq!(reply_text(&reply, "Delegated-IPv6-Prefix").as_deref(), Some("2001:db8:100::/56"));
    assert_eq!(reply_number(&reply, "Acct-Interim-Interval"), Some(300.0));
    assert_eq!(reply_text(&reply, "Framed-Pool"), None, "a static user is not sent to a pool");

    // Only an address: the netmask is a host's.
    let (outcome, reply) = statics.ask("bob").await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(names(&reply), ["Acct-Interim-Interval", "Framed-IP-Address", "Framed-IP-Netmask"]);
    assert_eq!(reply_text(&reply, "Framed-IP-Netmask").as_deref(), Some("255.255.255.255"));
    // Only IPv6 prefixes.
    let (outcome, reply) = statics.ask("dave").await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(names(&reply), ["Acct-Interim-Interval", "Delegated-IPv6-Prefix", "Framed-IPv6-Prefix"]);
    // No row: the pool's name.
    let (outcome, reply) = statics.ask("alice").await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(names(&reply), ["Framed-Pool"]);
    assert_eq!(reply_text(&reply, "Framed-Pool").as_deref(), Some("residential"));
    assert_eq!(statics.ask("erin").await.0, Outcome::Reject, "disabled");
    assert_eq!(statics.ask("nobody").await.0, Outcome::Reject, "unknown");
    // Accounting is answered, TACACS+ is not.
    let (outcome, _) = run(&renamed(STATIC_ACCOUNTING, "sta", &STATIC_TABLES), "radius", "accounting", "bng", None, vec![]).await;
    assert_eq!(outcome, Outcome::Accept);
    let (outcome, _) = run(&renamed(STATIC_ACCOUNTING, "sta", &STATIC_TABLES), "tacacs", "authentication", "bng", None, vec![]).await;
    assert_eq!(outcome, Outcome::Reject);
    Ok(())
}

/// A value the attribute cannot hold is refused, and the user and the column are in the log's
/// message; the user is not sent to a pool instead.
#[tokio::test]
async fn test_static_ip_bad_data_is_refused() -> Result<(), String> {
    let statics = Statics::new("stb").await;
    for (column, value) in [("ipv4", "bad address"), ("routed_v4", "bad subnet"), ("wan_v6", "bad prefix"), ("pd_v6", "bad prefix")] {
        statics.execute("UPDATE ASSIGNMENTS SET ipv4 = NULL, routed_v4 = NULL, wan_v6 = NULL, pd_v6 = NULL WHERE username = 'bob'").await?;
        statics.execute(&format!("UPDATE ASSIGNMENTS SET {column} = '{value}' WHERE username = 'bob'")).await?;
        let (outcome, reply) = statics.ask("bob").await;
        assert_eq!(outcome, Outcome::Reject, "{column}");
        assert_eq!(reply_text(&reply, "Framed-Pool"), None, "{column}: no pool instead");
    }
    Ok(())
}

/// What the dictionary cannot check, the script does: a subnet must be a.b.c.d/n with no host bits,
/// a netmask must be one run of ones. A row with nothing to send is a user without a row.
#[tokio::test]
async fn test_static_ip_checks_subnet_and_netmask() -> Result<(), String> {
    let statics = Statics::new("std").await;
    for (column, value) in [
        ("routed_v4", "garbage"),
        ("routed_v4", "203.0.113.999/29"),
        ("routed_v4", "198.51.100.9/29"),
        ("routed_v4", "198.51.100.8/33"),
        ("routed_v4", "198.51.100.8"),
        ("routed_v4", "2001:db8::/48"),
        ("netmask", "255.0.255.0"),
        ("netmask", "255.255.255.253"),
        ("netmask", "garbage"),
    ] {
        statics.execute("UPDATE ASSIGNMENTS SET ipv4 = '203.0.113.10', netmask = NULL, routed_v4 = NULL WHERE username = 'bob'").await?;
        statics.execute(&format!("UPDATE ASSIGNMENTS SET {column} = '{value}' WHERE username = 'bob'")).await?;
        assert_eq!(statics.ask("bob").await.0, Outcome::Reject, "{column} {value}");
    }
    for (column, value) in [("routed_v4", "198.51.100.8/29"), ("routed_v4", "0.0.0.0/0"), ("routed_v4", "198.51.100.77/32"), ("netmask", "255.255.255.0"), ("netmask", "0.0.0.0"), ("netmask", "255.255.255.255")] {
        statics.execute("UPDATE ASSIGNMENTS SET ipv4 = '203.0.113.10', netmask = NULL, routed_v4 = NULL WHERE username = 'bob'").await?;
        statics.execute(&format!("UPDATE ASSIGNMENTS SET {column} = '{value}' WHERE username = 'bob'")).await?;
        assert_eq!(statics.ask("bob").await.0, Outcome::Accept, "{column} {value}");
    }
    // Nothing to send, or only a netmask: the pool's name, as for a user without a row.
    statics.execute("UPDATE ASSIGNMENTS SET ipv4 = NULL, netmask = '255.255.255.0', routed_v4 = NULL WHERE username = 'bob'").await?;
    let (outcome, reply) = statics.ask("bob").await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(names(&reply), ["Framed-Pool"]);
    Ok(())
}

/// Two users cannot hold the same address or prefix; a column that is empty for many is fine.
#[tokio::test]
async fn test_static_ip_one_owner_for_a_value() -> Result<(), String> {
    let statics = Statics::new("stc").await;
    for (column, value) in [("ipv4", "203.0.113.10"), ("wan_v6", "2001:db8:1:2::/64"), ("pd_v6", "2001:db8:200::/56")] {
        let again = statics.execute(&format!("INSERT INTO ASSIGNMENTS (username, {column}) VALUES ('alice', '{value}')")).await;
        assert!(again.is_err(), "{column}: a second owner of {value}");
    }
    statics.execute("INSERT INTO ASSIGNMENTS (username, ipv4) VALUES ('alice', '203.0.113.11')").await?;
    statics.execute("INSERT INTO ASSIGNMENTS (username, ipv4) VALUES ('dave2', '203.0.113.12')").await?;
    assert_eq!(statics.ask("alice").await.0, Outcome::Accept);
    Ok(())
}

// ---- The rate attributes of chapter 18 ----

const RATE_TABLES: [&str; 3] = ["plans", "subscribers", "nas_styles"];

/// The rate-limit scenario's schema and demo data under `prefix`, with its script.
struct Rates {
    prefix: &'static str,
    script: String,
}

impl Rates {
    async fn new(prefix: &'static str) -> Rates {
        apply(&renamed(RATE_SCHEMA, prefix, &RATE_TABLES)).await;
        apply(&renamed(RATE_DATA, prefix, &RATE_TABLES)).await;
        Rates { prefix, script: renamed(RATE_SCRIPT, prefix, &RATE_TABLES) }
    }

    /// The chain's lua handler for `user`, asked by the RADIUS client `client`.
    async fn ask(&self, client: &str, user: &str) -> (Outcome, Vec<(String, AttrValue)>) {
        run(&self.script, "radius", "authentication", client, Some(user), vec![("User-Name", bytes(user))]).await
    }

    async fn execute(&self, sql: &str) -> Result<(), String> {
        test_db().execute(&sql.replace("STYLES", &format!("{}_nas_styles", self.prefix))).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Every style sends its own attributes for the same plan, in the unit and the direction the
/// script's comments give; a client that is not listed gets Filter-Id.
#[tokio::test]
async fn test_rate_limit_styles() -> Result<(), String> {
    let rates = Rates::new("rla").await;
    // fiber-500: 500000 kbit/s down, 250000 up.
    let (outcome, reply) = rates.ask("bng-unlisted", "bob").await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(names(&reply), ["Acct-Interim-Interval", "Filter-Id"], "an unlisted client");
    assert_eq!(reply_text(&reply, "Filter-Id").as_deref(), Some("fiber-500"));
    assert_eq!(reply_number(&reply, "Acct-Interim-Interval"), Some(300.0));

    let (_, reply) = rates.ask("bng-wispr", "bob").await;
    assert_eq!(names(&reply), ["Acct-Interim-Interval", "WISPr-Bandwidth-Max-Down", "WISPr-Bandwidth-Max-Up"]);
    assert_eq!(reply_number(&reply, "WISPr-Bandwidth-Max-Down"), Some(500_000_000.0), "bit/s");
    assert_eq!(reply_number(&reply, "WISPr-Bandwidth-Max-Up"), Some(250_000_000.0));

    let (_, reply) = rates.ask("bng-mikrotik", "bob").await;
    assert_eq!(names(&reply), ["Acct-Interim-Interval", "Mikrotik-Rate-Limit"]);
    assert_eq!(reply_text(&reply, "Mikrotik-Rate-Limit").as_deref(), Some("250000k/500000k"), "upload/download");

    let (_, reply) = rates.ask("bng-nokia", "bob").await;
    assert_eq!(names(&reply), ["Acct-Interim-Interval", "Alc-SLA-Prof-Str"]);
    assert_eq!(reply_text(&reply, "Alc-SLA-Prof-Str").as_deref(), Some("fiber-500"));

    let (_, reply) = rates.ask("bng-erx", "bob").await;
    assert_eq!(names(&reply), ["Acct-Interim-Interval", "ERX-Egress-Policy-Name", "ERX-Ingress-Policy-Name"]);
    assert_eq!(reply_text(&reply, "ERX-Ingress-Policy-Name").as_deref(), Some("fiber-500-up"));
    assert_eq!(reply_text(&reply, "ERX-Egress-Policy-Name").as_deref(), Some("fiber-500-down"));

    // Another plan, other numbers.
    let (_, reply) = rates.ask("bng-wispr", "alice").await;
    assert_eq!(reply_number(&reply, "WISPr-Bandwidth-Max-Down"), Some(100_000_000.0));
    assert_eq!(reply_number(&reply, "WISPr-Bandwidth-Max-Up"), Some(50_000_000.0));
    Ok(())
}

/// What is refused: a disabled or unknown user, a user whose plan does not exist, a client whose
/// style the script does not know, and TACACS+; accounting is answered.
#[tokio::test]
async fn test_rate_limit_refusals() -> Result<(), String> {
    let rates = Rates::new("rlb").await;
    assert_eq!(rates.ask("bng-wispr", "erin").await.0, Outcome::Reject, "disabled");
    assert_eq!(rates.ask("bng-wispr", "nobody").await.0, Outcome::Reject, "unknown");
    assert_eq!(rates.ask("bng-wispr", "dave").await.0, Outcome::Reject, "plan missing");
    rates.execute("INSERT INTO STYLES (client, style) VALUES ('bng-odd', 'no-such-style')").await?;
    assert_eq!(rates.ask("bng-odd", "alice").await.0, Outcome::Reject, "style unknown");
    let (outcome, _) = run(&rates.script, "tacacs", "authentication", "bng", None, vec![]).await;
    assert_eq!(outcome, Outcome::Reject, "TACACS+");
    let (outcome, _) = run(&rates.script, "radius", "accounting", "bng", None, vec![]).await;
    assert_eq!(outcome, Outcome::Accept, "accounting");
    Ok(())
}

/// WISPr's attributes are 32-bit integers in bit/s: a plan faster than 4294967295 bit/s is refused
/// for a WISPr client (and the log says which plan) while the styles that send names still work;
/// the largest value that fits is sent as it is.
#[tokio::test]
async fn test_rate_limit_wispr_upper_bound() -> Result<(), String> {
    let rates = Rates::new("rlc").await;
    for (plan, kbps, outcome) in [("fits", 4_294_967, Outcome::Accept), ("too-fast", 4_294_968, Outcome::Reject)] {
        test_db().execute(&format!("INSERT INTO rlc_plans (plan, down_kbps, up_kbps, profile) VALUES ('{plan}', {kbps}, 1000, '{plan}')")).await.map_err(|e| e.to_string())?;
        test_db().execute(&format!("INSERT INTO rlc_subscribers (username, password, plan, enabled) VALUES ('u-{plan}', 'x', '{plan}', 1)")).await.map_err(|e| e.to_string())?;
        assert_eq!(rates.ask("bng-wispr", &format!("u-{plan}")).await.0, outcome, "{plan} on a WISPr client");
        assert_eq!(rates.ask("bng-nokia", &format!("u-{plan}")).await.0, Outcome::Accept, "{plan} on a client that takes names");
    }
    let (_, reply) = rates.ask("bng-wispr", "u-fits").await;
    assert_eq!(reply_number(&reply, "WISPr-Bandwidth-Max-Down"), Some(4_294_967_000.0));
    Ok(())
}

// ---- The IPv6 prefix pool of chapter 19 ----

const PREFIX_TABLES: [&str; 10] = ["subscribers", "prefix_pools", "prefix_leases", "housekeeping", "prefix_leases_user", "prefix_leases_prefix", "prefix_leases_expiry", "prefix_leases_nas", "prefix_leases_pool_state", "prefix_leases_released"];

/// FNV-1a, 32 bits: what allocate.lua starts from, written here independently of the script.
fn fnv1a32(text: &str) -> u32 {
    text.bytes().fold(2166136261u32, |h, byte| (h ^ u32::from(byte)).wrapping_mul(16777619))
}

/// The prefix the pool (`base`, `dlen`) has as its `n`th, as the server writes an attribute's
/// value as text (the address, compressed and in lower case, and the length): worked out here
/// with 128-bit integers, not with the script's SQL.
fn prefix_text(base: i64, dlen: u32, n: u64) -> String {
    let high = (base as u64) | (n << (64 - dlen));
    format!("{}/{dlen}", std::net::Ipv6Addr::from(u128::from(high) << 64))
}

/// The prefix pool's schema and demo data under `prefix`, with its two scripts.
struct Prefixes {
    prefix: &'static str,
    allocate: String,
    accounting: String,
}

impl Prefixes {
    async fn new(prefix: &'static str) -> Prefixes {
        apply(&renamed(PREFIX_SCHEMA, prefix, &PREFIX_TABLES)).await;
        apply(&renamed(PREFIX_DATA, prefix, &PREFIX_TABLES)).await;
        // The first request would reap the table; the tests that look at a reap ask for it.
        apply(&format!("UPDATE {prefix}_housekeeping SET last_run = {}", now())).await;
        Prefixes { prefix, allocate: renamed(PREFIX_ALLOCATE, prefix, &PREFIX_TABLES), accounting: renamed(PREFIX_ACCOUNTING, prefix, &PREFIX_TABLES) }
    }

    /// A pool of `plen` to `dlen`, and `users` (named `<pool>-<i>`) that belong to it.
    async fn add_pool(&self, pool: &str, base: i64, plen: u32, dlen: u32, users: usize) -> Result<(), String> {
        self.sql(&format!("INSERT INTO PREFIX_POOLS (pool, base, plen, dlen) VALUES ('{pool}', {base}, {plen}, {dlen})")).await?;
        for i in 0..users {
            self.sql(&format!("INSERT INTO SUBS (username, password, pool, enabled) VALUES ('{pool}-{i}', 'x', '{pool}', 1)")).await?;
        }
        Ok(())
    }

    /// The chain's lua handler for `user`: the outcome and the Delegated-IPv6-Prefix it gave.
    async fn allocate(&self, user: &str) -> (Outcome, Option<String>, Vec<(String, AttrValue)>) {
        let (outcome, attributes) = run(&self.allocate, "radius", "authentication", "nas-a", Some(user), vec![("User-Name", bytes(user))]).await;
        let prefix = reply_text(&attributes, "Delegated-IPv6-Prefix");
        (outcome, prefix, attributes)
    }

    /// An accounting packet from the NAS `nas` for `user`'s session `session`; the prefix is in it or not.
    async fn account(&self, nas: &str, status: f64, user: &str, session: &str, prefix: Option<&str>) -> Outcome {
        let mut attributes = vec![("User-Name", bytes(user)), ("Acct-Session-Id", bytes(session)), ("Acct-Status-Type", number(status)), ("NAS-IP-Address", bytes(nas))];
        if let Some(prefix) = prefix {
            attributes.push(("Delegated-IPv6-Prefix", bytes(prefix)));
        }
        run(&self.accounting, "radius", "accounting", "nas-a", None, attributes).await.0
    }

    /// (state, session, nas, prefix) of the user's lease; nothing when there is none.
    async fn lease(&self, user: &str) -> Vec<DbValue> {
        rows(&format!("SELECT state, session_id, nas, prefix FROM {}_prefix_leases WHERE username = '{user}'", self.prefix)).await.into_iter().next().unwrap_or_default()
    }

    async fn count(&self) -> i64 {
        match rows(&format!("SELECT count(*) FROM {}_prefix_leases", self.prefix)).await.first().and_then(|row| row.first()) {
            Some(DbValue::Integer(count)) => *count,
            _ => -1,
        }
    }

    async fn sql(&self, sql: &str) -> Result<(), String> {
        let sql = sql.replace("PREFIX_POOLS", &format!("{}_prefix_pools", self.prefix)).replace("SUBS", &format!("{}_subscribers", self.prefix)).replace("LEASES", &format!("{}_prefix_leases", self.prefix)).replace("HOUSE", &format!("{}_housekeeping", self.prefix));
        test_db().execute(&sql).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Usernames of the pool that all start at `start` (the hash of the name modulo `capacity`).
fn starting_at(pool: &str, capacity: u32, start: u32, count: usize) -> Vec<String> {
    (0..).map(|i| format!("{pool}-{i}")).filter(|name| fnv1a32(name) % capacity == start).take(count).collect()
}

/// The prefix of every lease is the text the server decodes an attribute to: for pools of every
/// shape, each `n` of the small ones and some forty of a big one, the base with the top bit set
/// (a negative number to SQLite) and all zeros among them.
#[tokio::test]
async fn test_prefix_pool_text_matches_what_the_server_decodes() -> Result<(), String> {
    let prefixes = Prefixes::new("pxa").await;
    // (pool, base, plen, dlen): /56 with an inner zero group, /52 and /60 and /64, a ULA, the
    // lowest base, a base that is negative, and a big pool of which only some of the n are used.
    let shapes: [(&str, i64, u32, u32); 9] = [
        ("inner", 0x20010db800000000, 48, 52),
        ("plain", 0x20010db801000000, 48, 52),
        ("ulatwo", 0xfd123456789a0000u64 as i64, 48, 52),
        ("sixty", 0x20010db8000a0100, 56, 60),
        ("sixtyfour", 0x20010db8000a0100, 60, 64),
        ("zero", 0, 60, 64),
        ("negative", 0x8000000000000000u64 as i64, 60, 64),
        ("high", 0xffff000000000000u64 as i64, 48, 52),
        ("big", 0x20010db800000000, 32, 56),
    ];
    for (pool, base, plen, dlen) in shapes {
        let capacity = 1u64 << (dlen - plen);
        let users = if pool == "big" { 40 } else { capacity as usize };
        prefixes.add_pool(pool, base, plen, dlen, users).await?;
        for i in 0..users {
            let (outcome, given, _) = prefixes.allocate(&format!("{pool}-{i}")).await;
            assert_eq!(outcome, Outcome::Accept, "{pool}-{i}");
            assert!(given.is_some(), "{pool}-{i}");
        }
        let stored = rows(&format!("SELECT n, prefix FROM pxa_prefix_leases WHERE pool = '{pool}' ORDER BY n")).await;
        assert_eq!(stored.len(), users, "{pool}: one lease for each user");
        for row in &stored {
            let (DbValue::Integer(n), DbValue::Text(text)) = (&row[0], &row[1]) else { return Err(format!("{pool}: {row:?}")) };
            assert!((*n as u64) < capacity, "{pool}: n {n} is outside the pool");
            assert_eq!(text, &prefix_text(base, dlen, *n as u64), "{pool}: n {n}");
        }
        if pool != "big" {
            assert_eq!(stored.len() as u64, capacity, "{pool}: every n was given out once");
        }
    }
    // The spec's three examples, by name.
    async fn named(pool: &str, n: u64) -> Vec<Vec<DbValue>> {
        rows(&format!("SELECT prefix FROM pxa_prefix_leases WHERE pool = '{pool}' AND n = {n}")).await
    }
    assert_eq!(named("inner", 0).await, [vec![txt("2001:db8::/52")]]);
    assert_eq!(named("sixty", 3).await, [vec![txt("2001:db8:a:130::/60")]]);
    assert_eq!(named("zero", 0).await, [vec![txt("::/64")]]);
    assert_eq!(named("negative", 5).await, [vec![txt("8000:0:0:5::/64")]]);
    assert_eq!(prefix_text(0x20010db800000000, 56, 1), "2001:db8:0:100::/56");
    assert_eq!(prefix_text(0x20010db801000000, 56, 1), "2001:db8:100:100::/56");
    Ok(())
}

/// A pool has to be aligned and its lengths have to make sense: the table refuses the rest.
#[tokio::test]
async fn test_prefix_pool_misaligned_pool_is_refused() -> Result<(), String> {
    let prefixes = Prefixes::new("pxb").await;
    async fn pool(prefixes: &Prefixes, pool: &str, base: i64, plen: u32, dlen: u32) -> Result<(), String> {
        prefixes.sql(&format!("INSERT INTO PREFIX_POOLS (pool, base, plen, dlen) VALUES ('{pool}', {base}, {plen}, {dlen})")).await
    }
    let add = |name: &'static str, base: i64, plen: u32, dlen: u32| {
        let prefixes = &prefixes;
        async move { pool(prefixes, name, base, plen, dlen).await }
    };
    add("ok", 0x20010db801000000, 48, 56).await?;
    assert!(add("low-bit", 0x20010db801000100, 48, 56).await.is_err(), "a base that is not aligned to the pool");
    assert!(add("low-bit-too", 0x20010db801000001, 63, 64).await.is_err());
    assert!(add("too-long", 0x20010db801000000, 48, 65).await.is_err(), "dlen above 64");
    assert!(add("backwards", 0x20010db801000000, 56, 48).await.is_err(), "dlen below plen");
    assert!(add("huge", 0x20010db800000000, 8, 48).await.is_err(), "more than 2^30 prefixes");
    assert!(add("zero-length", 0, 0, 0).await.is_err(), "dlen of 0");
    add("whole", 0, 0, 30).await?;
    add("last-aligned", 0x20010db801000100, 56, 64).await?;
    Ok(())
}

/// Each user gets one prefix, the same again, from where the name's hash starts; an account that
/// is disabled or unknown gets none.
#[tokio::test]
async fn test_prefix_pool_gives_each_user_a_prefix() -> Result<(), String> {
    let prefixes = Prefixes::new("pxc").await;
    let (outcome, alice, attributes) = prefixes.allocate("alice").await;
    assert_eq!(outcome, Outcome::Accept);
    // An empty pool: the first free n is the start itself, the hash of the name modulo the capacity.
    let capacity = 256;
    let n = fnv1a32("alice") % capacity;
    assert_eq!(alice, Some(prefix_text(0x20010db801000000, 56, u64::from(n))), "alice starts at {n}");
    assert_eq!(reply_text(&attributes, "Framed-IPv6-Pool").as_deref(), Some("residential"));
    assert_eq!(reply_number(&attributes, "Acct-Interim-Interval"), Some(300.0));
    assert_eq!(prefixes.lease("alice").await[0], txt("reserved"));
    // Again: the one she has, and no second row.
    assert_eq!(prefixes.allocate("alice").await.1, alice);
    assert_eq!(prefixes.count().await, 1);
    let bob = prefixes.allocate("bob").await.1;
    assert!(bob.is_some() && bob != alice);
    // Another pool: its own range, the ULA's with the top bit set.
    let carol = prefixes.allocate("carol").await.1.ok_or("no prefix for carol")?;
    assert!(carol.starts_with("2001:db8:200:"), "{carol}");
    let dave = prefixes.allocate("dave").await.1.ok_or("no prefix for dave")?;
    assert!(dave.starts_with("fd12:3456:789a:"), "{dave}");
    for user in ["erin", "nobody"] {
        let (outcome, given, _) = prefixes.allocate(user).await;
        assert_eq!((outcome, given), (Outcome::Reject, None), "{user}");
    }
    assert_eq!(prefixes.count().await, 4, "a refusal takes no prefix");
    Ok(())
}

/// The first free n from the start, going forward, wrapping to 0, and a refusal only when no n is free.
#[tokio::test]
async fn test_prefix_pool_goes_forward_wraps_and_runs_out() -> Result<(), String> {
    let prefixes = Prefixes::new("pxd").await;
    prefixes.add_pool("four", 0x20010db803000000, 62, 64, 0).await?;
    // Six users whose names all start at 3.
    let names = starting_at("four", 4, 3, 6);
    for name in &names {
        prefixes.sql(&format!("INSERT INTO SUBS (username, password, pool, enabled) VALUES ('{name}', 'x', 'four', 1)")).await?;
    }
    let mut taken = Vec::new();
    for name in &names[..4] {
        let (outcome, given, _) = prefixes.allocate(name).await;
        assert_eq!(outcome, Outcome::Accept, "{name}");
        taken.push(given.ok_or("no prefix")?);
    }
    // 3 is the start; then the pool wraps to 0, 1, 2.
    let expected: Vec<String> = [3u64, 0, 1, 2].iter().map(|n| prefix_text(0x20010db803000000, 64, *n)).collect();
    assert_eq!(taken, expected);
    // The pool is full: the fifth and the sixth are refused, and the log says why.
    for name in &names[4..] {
        let (outcome, given, _) = prefixes.allocate(name).await;
        assert_eq!((outcome, given), (Outcome::Reject, None), "{name}");
    }
    assert_eq!(prefixes.count().await, 4);
    Ok(())
}

/// A prefix that is given back is kept for the user who had it: she gets it again, even when
/// another user asks first; the pool gives it to someone else only when nothing else is free.
#[tokio::test]
async fn test_prefix_pool_released_prefix_is_kept_for_its_user() -> Result<(), String> {
    let prefixes = Prefixes::new("pxe").await;
    prefixes.add_pool("two", 0x20010db804000000, 63, 64, 0).await?;
    let names = starting_at("two", 2, 0, 3);
    for name in &names {
        prefixes.sql(&format!("INSERT INTO SUBS (username, password, pool, enabled) VALUES ('{name}', 'x', 'two', 1)")).await?;
    }
    let first = prefixes.allocate(&names[0]).await.1.ok_or("no prefix")?;
    let second = prefixes.allocate(&names[1]).await.1.ok_or("no prefix")?;
    assert_ne!(first, second);
    assert_eq!(prefixes.allocate(&names[2]).await.0, Outcome::Reject, "full: both are reserved");
    // The first user's Stop gives hers back: released, kept for her, with no session.
    assert_eq!(prefixes.account("10.0.0.5", 1.0, &names[0], "s-1", Some(&first)).await, Outcome::Accept);
    assert_eq!(prefixes.account("10.0.0.5", 2.0, &names[0], "s-1", Some(&first)).await, Outcome::Accept);
    assert_eq!(prefixes.lease(&names[0]).await, [txt("released"), DbValue::Null, DbValue::Null, txt(&first)]);
    // She comes back: the same prefix, reserved again.
    assert_eq!(prefixes.allocate(&names[0]).await.1, Some(first.clone()));
    assert_eq!(prefixes.lease(&names[0]).await[0], txt("reserved"));
    // Nothing is free for the third: the pool is full of reserved prefixes.
    assert_eq!(prefixes.allocate(&names[2]).await.0, Outcome::Reject);
    Ok(())
}

/// Nothing is free in the window: the oldest released prefix is taken, and its user, who comes
/// back, is given another one; with a free n in the window, a released prefix is not touched.
#[tokio::test]
async fn test_prefix_pool_takes_the_oldest_released_only_when_nothing_is_free() -> Result<(), String> {
    let prefixes = Prefixes::new("pxk").await;
    let base = 0x20010db807000000;
    prefixes.add_pool("tiny", base, 62, 64, 0).await?;
    for name in ["old", "newer", "busy", "late", "later"] {
        prefixes.sql(&format!("INSERT INTO SUBS (username, password, pool, enabled) VALUES ('{name}', 'x', 'tiny', 1)")).await?;
    }
    // n 0 and 1 are released (0 the older), n 2 is in use, n 3 is free.
    let held = |n: u64, state: &str, user: &str, updated: i64| format!("INSERT INTO LEASES (pool, n, prefix, state, username, expires, updated) VALUES ('tiny', {n}, '{}', '{state}', '{user}', 4102444800, {updated})", prefix_text(base, 64, n));
    prefixes.sql(&held(0, "released", "old", 100)).await?;
    prefixes.sql(&held(1, "released", "newer", 200)).await?;
    prefixes.sql(&held(2, "active", "busy", 300)).await?;
    // A free n (3) in the window: it is taken, and neither released one is touched.
    let late = prefixes.allocate("late").await.1.ok_or("no prefix")?;
    assert_eq!(late, prefix_text(base, 64, 3));
    assert_eq!(prefixes.lease("old").await[0], txt("released"));
    assert_eq!(prefixes.lease("newer").await[0], txt("released"));
    // Now nothing is free: the oldest released one goes, at random among the oldest eight, so
    // either of the two; the user who had it gets nothing back of it.
    let later = prefixes.allocate("later").await.1.ok_or("no prefix")?;
    assert!([prefix_text(base, 64, 0), prefix_text(base, 64, 1)].contains(&later), "{later}");
    assert_eq!(prefixes.lease("later").await[0], txt("reserved"));
    let taken_from = if later == prefix_text(base, 64, 0) { "old" } else { "newer" };
    assert!(prefixes.lease(taken_from).await.is_empty(), "{taken_from} lost it");
    // The user who lost it comes back: the other released one is taken, and its user, who comes
    // back too, finds the pool full of prefixes in use.
    assert_eq!(prefixes.allocate(taken_from).await.0, Outcome::Accept);
    assert_eq!(prefixes.count().await, 4);
    let other = if taken_from == "old" { "newer" } else { "old" };
    assert_eq!(prefixes.allocate(other).await.0, Outcome::Reject);
    Ok(())
}

/// Users who ask at the same time, some from the same start: each gets one prefix, no prefix twice.
#[tokio::test]
async fn test_prefix_pool_at_the_same_time() -> Result<(), String> {
    let prefixes = Prefixes::new("pxf").await;
    prefixes.add_pool("many", 0x20010db805000000, 56, 64, 0).await?;
    // Twelve that all start at 7 and twelve that start at 9, in a pool of 256.
    let mut names = starting_at("many", 256, 7, 12);
    names.extend(starting_at("many", 256, 9, 12));
    for name in &names {
        prefixes.sql(&format!("INSERT INTO SUBS (username, password, pool, enabled) VALUES ('{name}', 'x', 'many', 1)")).await?;
    }
    let asked = futures::future::join_all(names.iter().map(|name| prefixes.allocate(name))).await;
    assert!(asked.iter().all(|(outcome, _, _)| *outcome == Outcome::Accept), "all of them: {:?}", asked.iter().map(|(outcome, _, _)| *outcome).collect::<Vec<_>>());
    let mut given: Vec<String> = asked.iter().filter_map(|(_, prefix, _)| prefix.clone()).collect();
    assert_eq!(given.len(), names.len());
    given.sort();
    given.dedup();
    assert_eq!(given.len(), names.len(), "no prefix twice");
    assert_eq!(prefixes.count().await as usize, names.len());
    // The same user twice at once: one lease.
    let twice = futures::future::join_all((0..4).map(|_| prefixes.allocate(&names[0]))).await;
    assert!(twice.iter().all(|(outcome, prefix, _)| *outcome == Outcome::Accept && *prefix == twice[0].1));
    assert_eq!(prefixes.count().await as usize, names.len());
    Ok(())
}

/// A pool that is nearly full, whose free n are single and scattered, with more users asking at
/// once than the script has tries: they all get one. (When each lost race sent the user to "the
/// next free n", they all chased the same one and only one of them won each round: found by the
/// test with the server at 90% use, where 2.5% of the logins were refused with free n left.)
#[tokio::test]
async fn test_prefix_pool_single_free_n_and_many_at_once() -> Result<(), String> {
    let prefixes = Prefixes::new("pxj").await;
    let base = 0x20010db806000000;
    prefixes.add_pool("dense", base, 58, 64, 0).await?;
    // The even n are taken, by users who are not in the test.
    for n in (0..64u64).step_by(2) {
        prefixes.sql(&format!("INSERT INTO LEASES (pool, n, prefix, state, username, expires, updated) VALUES ('dense', {n}, '{}', 'active', 'held-{n}', 4102444800, 0)", prefix_text(base, 64, n))).await?;
    }
    // Twenty-four users who all start at 5, with 32 free n in the pool.
    let names = starting_at("dense", 64, 5, 24);
    for name in &names {
        prefixes.sql(&format!("INSERT INTO SUBS (username, password, pool, enabled) VALUES ('{name}', 'x', 'dense', 1)")).await?;
    }
    let asked = futures::future::join_all(names.iter().map(|name| prefixes.allocate(name))).await;
    let outcomes: Vec<Outcome> = asked.iter().map(|(outcome, _, _)| *outcome).collect();
    assert!(outcomes.iter().all(|outcome| *outcome == Outcome::Accept), "all of them: {outcomes:?}");
    let mut given: Vec<String> = asked.iter().filter_map(|(_, prefix, _)| prefix.clone()).collect();
    given.sort();
    given.dedup();
    assert_eq!(given.len(), names.len(), "no prefix twice");
    assert_eq!(prefixes.count().await as usize, 32 + names.len());
    Ok(())
}

/// A pool that is full of released prefixes, and more users asking at once than the script has
/// tries: each takes one, none the same, and each of the old users is left with nothing.
#[tokio::test]
async fn test_prefix_pool_many_take_released_at_once() -> Result<(), String> {
    let prefixes = Prefixes::new("pxm").await;
    let base = 0x20010db808000000;
    prefixes.add_pool("full", base, 58, 64, 0).await?;
    for n in 0..64u64 {
        prefixes.sql(&format!("INSERT INTO LEASES (pool, n, prefix, state, username, expires, updated) VALUES ('full', {n}, '{}', 'released', 'gone-{n}', 4102444800, {})", prefix_text(base, 64, n), 100 + n)).await?;
    }
    let names = starting_at("full", 64, 5, 24);
    for name in &names {
        prefixes.sql(&format!("INSERT INTO SUBS (username, password, pool, enabled) VALUES ('{name}', 'x', 'full', 1)")).await?;
    }
    let asked = futures::future::join_all(names.iter().map(|name| prefixes.allocate(name))).await;
    let outcomes: Vec<Outcome> = asked.iter().map(|(outcome, _, _)| *outcome).collect();
    assert!(outcomes.iter().all(|outcome| *outcome == Outcome::Accept), "all of them: {outcomes:?}");
    let mut given: Vec<String> = asked.iter().filter_map(|(_, prefix, _)| prefix.clone()).collect();
    given.sort();
    given.dedup();
    assert_eq!(given.len(), names.len(), "no prefix twice");
    assert_eq!(prefixes.count().await, 64, "the rows are the same ones, with other users");
    Ok(())
}

/// Start and Interim confirm the lease the prefix names, with the user or without the prefix in
/// the packet; a prefix that is not the user's changes nothing.
#[tokio::test]
async fn test_prefix_pool_accounting_confirms_and_extends() -> Result<(), String> {
    let prefixes = Prefixes::new("pxg").await;
    let alice = prefixes.allocate("alice").await.1.ok_or("no prefix")?;
    let bob = prefixes.allocate("bob").await.1.ok_or("no prefix")?;
    // A packet with a prefix that is not hers: the lease stays reserved.
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "alice", "s-1", Some(&bob)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("alice").await[0], txt("reserved"));
    // Her own: active, with the session and the NAS, and the time three Interim intervals on.
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "alice", "s-1", Some(&alice)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("alice").await, [txt("active"), txt("s-1"), txt("10.0.0.5"), txt(&alice)]);
    let expires = rows(&format!("SELECT expires >= {} AND expires <= {} FROM pxg_prefix_leases WHERE username = 'alice'", now() + 890, now() + 910)).await;
    assert_eq!(expires, [vec![int(1)]], "900 seconds from now");
    // No prefix in the packet: found by the user.
    assert_eq!(prefixes.account("10.0.0.6", 1.0, "bob", "s-2", None).await, Outcome::Accept);
    assert_eq!(prefixes.lease("bob").await, [txt("active"), txt("s-2"), txt("10.0.0.6"), txt(&bob)]);
    // An Interim extends it; the same packet twice is the same as once.
    prefixes.sql("UPDATE LEASES SET expires = 5 WHERE username = 'alice'").await?;
    for _ in 0..2 {
        assert_eq!(prefixes.account("10.0.0.5", 3.0, "alice", "s-1", Some(&alice)).await, Outcome::Accept);
    }
    let expires = rows(&format!("SELECT expires > {} FROM pxg_prefix_leases WHERE username = 'alice'", now() + 800)).await;
    assert_eq!(expires, [vec![int(1)]]);
    assert_eq!(prefixes.count().await, 2);
    // Not a user of the pool, or a packet with no session: answered, nothing changes.
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "nobody", "s-9", Some(&alice)).await, Outcome::Accept);
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "alice", "", Some(&alice)).await, Outcome::Accept);
    // An authentication is refused (the chain's other handlers do that); the status of a packet
    // this script has no use for is answered.
    let (outcome, _) = run(&prefixes.accounting, "radius", "authentication", "nas-a", None, vec![]).await;
    assert_eq!(outcome, Outcome::Reject);
    assert_eq!(prefixes.account("10.0.0.5", 9.0, "alice", "s-1", None).await, Outcome::Accept);
    Ok(())
}

/// A Stop releases the lease of its own session (kept for the user); the Stop of an older
/// session does not touch the new one; an Accounting-On or -Off releases what a NAS had; a late
/// Interim does not bring a released prefix back, a Start does.
#[tokio::test]
async fn test_prefix_pool_accounting_releases() -> Result<(), String> {
    let prefixes = Prefixes::new("pxh").await;
    let alice = prefixes.allocate("alice").await.1.ok_or("no prefix")?;
    let bob = prefixes.allocate("bob").await.1.ok_or("no prefix")?;
    let carol = prefixes.allocate("carol").await.1.ok_or("no prefix")?;
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "alice", "s-new", Some(&alice)).await, Outcome::Accept);
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "bob", "s-b", Some(&bob)).await, Outcome::Accept);
    assert_eq!(prefixes.account("10.0.0.6", 1.0, "carol", "s-c", Some(&carol)).await, Outcome::Accept);
    // The Stop of an older session of alice's: nothing.
    assert_eq!(prefixes.account("10.0.0.5", 2.0, "alice", "s-old", Some(&alice)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("alice").await[0], txt("active"));
    // Her own Stop: released, for her, with no session and no NAS, for HOLD_SECONDS from now.
    assert_eq!(prefixes.account("10.0.0.5", 2.0, "alice", "s-new", Some(&alice)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("alice").await, [txt("released"), DbValue::Null, DbValue::Null, txt(&alice)]);
    let expires = rows(&format!("SELECT expires >= {} AND expires <= {} FROM pxh_prefix_leases WHERE username = 'alice'", now() + 86390, now() + 86410)).await;
    assert_eq!(expires, [vec![int(1)]], "a day from now");
    assert_eq!(prefixes.count().await, 3, "the row stays");
    // A late Interim of that session does not make it active again.
    assert_eq!(prefixes.account("10.0.0.5", 3.0, "alice", "s-new", Some(&alice)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("alice").await[0], txt("released"));
    // The NAS 10.0.0.5 restarts: bob's, which it had, is released; carol's, on another NAS, stays.
    assert_eq!(prefixes.account("10.0.0.5", 7.0, "", "", None).await, Outcome::Accept);
    assert_eq!(prefixes.lease("bob").await[0], txt("released"));
    assert_eq!(prefixes.lease("carol").await[0], txt("active"));
    assert_eq!(prefixes.account("10.0.0.6", 8.0, "", "", None).await, Outcome::Accept);
    assert_eq!(prefixes.lease("carol").await[0], txt("released"));
    // A reserved lease that was never confirmed is not touched by anyone's Stop (the reap takes it when it runs out).
    let dave = prefixes.allocate("dave").await.1.ok_or("no prefix")?;
    assert_eq!(prefixes.account("10.0.0.5", 2.0, "dave", "s-d", Some(&dave)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("dave").await[0], txt("reserved"));
    // A late Interim of an older session does not take the row from the newer one.
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "dave", "s-new", Some(&dave)).await, Outcome::Accept);
    assert_eq!(prefixes.account("10.0.0.5", 3.0, "dave", "s-old", Some(&dave)).await, Outcome::Accept);
    assert_eq!(prefixes.account("10.0.0.5", 2.0, "dave", "s-new", Some(&dave)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("dave").await[0], txt("released"));
    // A quick reconnect: the old session's Stop comes after the new login and before the new Start;
    // the Start brings the prefix back to active.
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "bob", "s-b2", Some(&bob)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("bob").await, [txt("active"), txt("s-b2"), txt("10.0.0.5"), txt(&bob)]);
    Ok(())
}

/// A user whose prefix is released and who logs in again and then confirms it (Start) has it
/// active; the Stop of the session that ended in between does not release the new one.
#[tokio::test]
async fn test_prefix_pool_quick_reconnect_keeps_the_prefix() -> Result<(), String> {
    let prefixes = Prefixes::new("pxl").await;
    let alice = prefixes.allocate("alice").await.1.ok_or("no prefix")?;
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "alice", "s-1", Some(&alice)).await, Outcome::Accept);
    // The new login comes first, finding the prefix active: the same one, no write.
    assert_eq!(prefixes.allocate("alice").await.1, Some(alice.clone()));
    // The old session's Stop comes next: released (it is the session on the row).
    assert_eq!(prefixes.account("10.0.0.5", 2.0, "alice", "s-1", Some(&alice)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("alice").await[0], txt("released"));
    // The new session's Start: the same prefix is hers, active again.
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "alice", "s-2", Some(&alice)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("alice").await, [txt("active"), txt("s-2"), txt("10.0.0.5"), txt(&alice)]);
    Ok(())
}

/// The reap: leases that ran out are taken back, once by one request, and what has not run out
/// stays: reserved and released ones are deleted, an active one whose Interims stopped is released.
#[tokio::test]
async fn test_prefix_pool_takes_back_what_is_not_used() -> Result<(), String> {
    let prefixes = Prefixes::new("pxi").await;
    let alice = prefixes.allocate("alice").await.1.ok_or("no prefix")?;
    let bob = prefixes.allocate("bob").await.1.ok_or("no prefix")?;
    let dave = prefixes.allocate("dave").await.1.ok_or("no prefix")?;
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "bob", "s-b", Some(&bob)).await, Outcome::Accept);
    assert_eq!(prefixes.account("10.0.0.5", 1.0, "dave", "s-d", Some(&dave)).await, Outcome::Accept);
    assert_eq!(prefixes.account("10.0.0.5", 2.0, "dave", "s-d", Some(&dave)).await, Outcome::Accept);
    // Alice never got her Start (reserved, ran out); bob's Interims stopped (active, ran out); dave's
    // hold ran out (released); and carol is a new one.
    prefixes.sql("UPDATE LEASES SET expires = 5").await?;
    prefixes.sql("UPDATE HOUSE SET last_run = 0").await?;
    let carol = prefixes.allocate("carol").await.1.ok_or("no prefix")?;
    assert!(prefixes.lease("alice").await.is_empty(), "reserved that ran out is deleted");
    assert!(prefixes.lease("dave").await.is_empty(), "released that ran out is deleted");
    assert_eq!(prefixes.lease("bob").await, [txt("released"), txt("s-b"), txt("10.0.0.5"), txt(&bob)], "active that ran out is released, with its session");
    // Its NAS was still there, only the Interims did not come: the next one brings it back.
    assert_eq!(prefixes.account("10.0.0.5", 3.0, "bob", "s-b", Some(&bob)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("bob").await[0], txt("active"));
    // An Interim of another session does not.
    prefixes.sql("UPDATE LEASES SET state = 'released' WHERE username = 'bob'").await?;
    assert_eq!(prefixes.account("10.0.0.5", 3.0, "bob", "s-other", Some(&bob)).await, Outcome::Accept);
    assert_eq!(prefixes.lease("bob").await[0], txt("released"));
    assert_eq!(prefixes.count().await, 2, "bob's, kept, and carol's");
    assert_eq!(prefixes.lease("carol").await[3], txt(&carol));
    // The reap ran once: the time of the last run is now, so the next request does not.
    prefixes.sql("INSERT INTO LEASES (pool, n, prefix, state, username, expires, updated) VALUES ('residential', 200, 'x', 'active', 'late', 5, 5)").await?;
    prefixes.allocate("carol").await;
    assert_eq!(prefixes.count().await, 3, "not again within a minute");
    // A prefix is free again: alice, who lost hers, is given one, and gets hers again when the start is free.
    let again = prefixes.allocate("alice").await.1.ok_or("no prefix")?;
    assert_eq!(again, alice, "the same start, free again");
    Ok(())
}
