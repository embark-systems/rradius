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

const POOL_TABLES: [&str; 6] = ["subscribers", "ip_pool", "housekeeping", "ip_pool_user", "ip_pool_state", "ip_pool_expiry"];

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
