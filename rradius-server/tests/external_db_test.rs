//! Runs `[databases.<name>]` against real MySQL and PostgreSQL servers in Docker: the `sql`
//! handler's `database`, the scripts' `db_query` and `db_execute`, TLS, and what happens when
//! the database stops.
//!
//! Needs Docker and the `mysql:8.4` and `postgres:17` images, so it is ignored by default:
//! `cargo test --test external_db_test -- --ignored`.

use rradius_core::common::Outcome;
use rradius_core::config::{DatabaseKind, DatabaseSettings, DatabaseTls};
use rradius_core::external_db::ExternalDatabases;
use rradius_core::lua_engine::{LuaAuthEngine, LuaRequest};
use rradius_core::radius::{self, RadiusPacket};
use rradius_core::test_support::server::{free_port, hidden_password, write_config, Server};
use rradius_core::test_support::{database_settings, test_db, test_vms};
use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradiusd");
const PASSWORD: &str = "test-db-password";
/// `{SSHA}` of `secret` with the salt `rr-salt1`.
const ALICE_SSHA: &str = "{SSHA}78cdSUfS/6uxGK+hBLB0UWTNMtdyci1zYWx0MQ==";

const OPENSSL_CNF: &str = "[req]\ndistinguished_name = dn\n[dn]\n\
    [v3_ca]\nbasicConstraints = critical,CA:TRUE\nkeyUsage = critical,keyCertSign,cRLSign\nsubjectKeyIdentifier = hash\n\
    [server]\nbasicConstraints = CA:FALSE\nkeyUsage = critical,digitalSignature,keyEncipherment\n\
    extendedKeyUsage = serverAuth\nsubjectAltName = DNS:localhost,IP:127.0.0.1\n";

fn run(program: &str, args: &[&str], dir: &Path) -> Result<String, String> {
    let output = Command::new(program).args(args).current_dir(dir).output().map_err(|e| format!("{program}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{program} {args:?} failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Two CAs in `dir`, `ca.pem` and `other-ca.pem`, and a certificate for 127.0.0.1 and
/// localhost that `ca.pem` signed, `server.pem` with `server.key`.
fn certificates(dir: &Path) -> Result<(), String> {
    std::fs::write(dir.join("openssl.cnf"), OPENSSL_CNF).map_err(|e| e.to_string())?;
    for (name, subject) in [("ca", "/CN=rradius-test-ca"), ("other-ca", "/CN=rradius-other-ca")] {
        run("openssl", &["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", &format!("{name}.key"), "-out", &format!("{name}.pem"), "-days", "2", "-subj", subject, "-config", "openssl.cnf", "-extensions", "v3_ca"], dir)?;
    }
    run("openssl", &["req", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", "server.key", "-out", "server.csr", "-subj", "/CN=localhost", "-config", "openssl.cnf"], dir)?;
    run("openssl", &["x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-out", "server.pem", "-days", "2", "-extfile", "openssl.cnf", "-extensions", "server"], dir)?;
    Ok(())
}

/// A database container on a fixed port of 127.0.0.1, so that it keeps the port when it is
/// stopped and started again. Removed on drop.
struct Container {
    id: String,
    port: u16,
}

impl Container {
    fn stop(&self) -> Result<(), String> {
        run("docker", &["stop", "-t", "1", &self.id], Path::new(".")).map(drop)
    }

    fn start(&self) -> Result<(), String> {
        run("docker", &["start", &self.id], Path::new(".")).map(drop)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).output();
    }
}

/// PostgreSQL 17 with TLS from `server.pem`, the user `rradius` and the tables of `init.sql`.
/// The server wants its key owned by its own user, so the files are copied first.
fn start_postgres(dir: &Path, init: &str) -> Result<Container, String> {
    std::fs::create_dir_all(dir.join("initdb")).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("initdb/init.sql"), init).map_err(|e| e.to_string())?;
    let port = free_port();
    let start = "cp /work/server.pem /work/server.key /tmp/ && chown postgres /tmp/server.key && chmod 600 /tmp/server.key && \
        exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tmp/server.pem -c ssl_key_file=/tmp/server.key";
    let id = run(
        "docker",
        &["run", "-d", "-p", &format!("127.0.0.1:{port}:5432"), "-v", &format!("{}:/work", dir.display()), "-v", &format!("{}:/docker-entrypoint-initdb.d", dir.join("initdb").display()),
            "-e", "POSTGRES_USER=rradius", "-e", &format!("POSTGRES_PASSWORD={PASSWORD}"), "-e", "POSTGRES_DB=radius", "--entrypoint", "sh", "postgres:17", "-c", start],
        dir,
    )?;
    Ok(Container { id, port })
}

/// MySQL 8.4 with TLS from `server.pem`: `rradius` must use TLS, `plain` uses
/// `mysql_native_password`, which works without TLS and without the RSA key exchange.
fn start_mysql(dir: &Path, init: &str) -> Result<Container, String> {
    std::fs::create_dir_all(dir.join("initdb")).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("initdb/init.sql"), init).map_err(|e| e.to_string())?;
    let port = free_port();
    let start = "cp /work/ca.pem /work/server.pem /work/server.key /tmp/ && chown mysql /tmp/server.key && \
        exec docker-entrypoint.sh mysqld --mysql-native-password=ON --ssl-ca=/tmp/ca.pem --ssl-cert=/tmp/server.pem --ssl-key=/tmp/server.key";
    let id = run(
        "docker",
        &["run", "-d", "-p", &format!("127.0.0.1:{port}:3306"), "-v", &format!("{}:/work", dir.display()), "-v", &format!("{}:/docker-entrypoint-initdb.d", dir.join("initdb").display()),
            "-e", "MYSQL_ROOT_PASSWORD=root-password", "-e", "MYSQL_DATABASE=radius", "-e", "MYSQL_USER=rradius", "-e", &format!("MYSQL_PASSWORD={PASSWORD}"),
            "--entrypoint", "sh", "mysql:8.4", "-c", start],
        dir,
    )?;
    Ok(Container { id, port })
}

fn settings(kind: DatabaseKind, port: u16, user: &str, tls: DatabaseTls, ca_file: Option<PathBuf>) -> DatabaseSettings {
    database_settings(kind, "127.0.0.1", port, user, PASSWORD, tls, ca_file)
}

/// Waits until `name` answers a query: the images start a temporary server first, without
/// TCP, to run the init scripts.
async fn wait_ready(databases: &ExternalDatabases, name: &str) -> Result<(), String> {
    let database = databases.get(name).ok_or_else(|| format!("no database {name}"))?;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match database.query("SELECT 1", &[]).await {
            Ok(_) => return Ok(()),
            Err(e) if Instant::now() > deadline => return Err(format!("{name} never answered: {e}")),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Runs `script` as an authentication script with `databases`; it says what was wrong with
/// `Reply-Message` and returns `reject`, or returns `accept`.
async fn script_passes(databases: &ExternalDatabases, script: &str) -> Result<(), String> {
    let engine = LuaAuthEngine::new(test_vms(), test_db(), script)?.with_databases(databases.clone()).with_query_timeout(Duration::from_secs(20));
    let decision = engine.authenticate(LuaRequest::new("radius", "authentication", vec![])).await;
    match decision.outcome {
        Outcome::Accept => Ok(()),
        _ => Err(format!("{:?}", decision.response_attributes)),
    }
}

/// A server with `databases_and_auth` before `[radius]`, `script` as the top-level script and
/// an accounting spool when `spool` is set. Waits until it serves.
fn start_server(databases_and_auth: &str, script: &str, spool: bool) -> Result<Server, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, auth_port) = write_config(dir.path(), script, None);
    let mut text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?.replace("[radius]\n", &format!("{databases_and_auth}[radius]\n"));
    if spool {
        text = text.replace("[[radius.clients]]", "[radius.accounting_spool]\ndir = \"spool\"\nreplay_interval_ms = 200\n[[radius.clients]]");
    }
    std::fs::write(&config, text).map_err(|e| e.to_string())?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    server.wait_for_log("Servers started");
    Ok(server)
}

/// The answer to a PAP Access-Request, or `None` when there is none within `wait`.
fn login(server: &Server, user: &str, password: &str, wait: Duration) -> Result<Option<RadiusPacket>, String> {
    let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
    let authenticator: [u8; 16] = std::array::from_fn(|i| i as u8 + user.len() as u8 + password.len() as u8);
    let packet = RadiusPacket::create_access_request(authenticator[0], authenticator, vec![(1, user.as_bytes().to_vec()), (2, hidden_password(password.as_bytes(), b"s", &authenticator))], b"s");
    socket.send_to(&packet, ("127.0.0.1", server.auth_port)).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 4096];
    match socket.recv_from(&mut buf) {
        Ok((size, _)) => RadiusPacket::parse(&buf[..size]).map(Some),
        Err(_) => Ok(None),
    }
}

/// The answer to a CHAP Access-Request, the Request Authenticator as the challenge.
fn chap_login(server: &Server, user: &str, password: &str) -> Result<RadiusPacket, String> {
    use md5::{Digest, Md5};
    let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let authenticator: [u8; 16] = std::array::from_fn(|i| 0x50 + i as u8);
    let ident = 7u8;
    let mut hash = Md5::new();
    hash.update([ident]);
    hash.update(password.as_bytes());
    hash.update(authenticator);
    let mut value = vec![ident];
    value.extend_from_slice(&hash.finalize());
    let packet = RadiusPacket::create_access_request(authenticator[0], authenticator, vec![(1, user.as_bytes().to_vec()), (3, value)], b"s");
    socket.send_to(&packet, ("127.0.0.1", server.auth_port)).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 4096];
    let (size, _) = socket.recv_from(&mut buf).map_err(|e| format!("no answer to CHAP: {e}"))?;
    RadiusPacket::parse(&buf[..size])
}

/// Sends an Accounting-Request for `session` and returns whether it was answered.
fn account(server: &Server, identifier: u8, session: &str, octets: u32, wait: Duration) -> Result<bool, String> {
    let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
    let packet = RadiusPacket::create_accounting_request(identifier, vec![(40, vec![0, 0, 0, 2]), (44, session.as_bytes().to_vec()), (43, octets.to_be_bytes().to_vec())], b"s");
    socket.send_to(&packet, ("127.0.0.1", server.port_acct())).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 4096];
    Ok(socket.recv_from(&mut buf).is_ok_and(|(size, _)| RadiusPacket::parse(&buf[..size]).is_ok_and(|reply| reply.code == radius::ACCOUNTING_RESPONSE)))
}

async fn wait_for(what: &str, mut done: impl AsyncFnMut() -> Result<bool, String>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done().await? {
        if Instant::now() > deadline {
            return Err(format!("{what} did not happen within 30 s"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

const POSTGRES_INIT: &str = "CREATE TABLE radcheck (username TEXT PRIMARY KEY, password TEXT);\n\
    INSERT INTO radcheck VALUES ('alice', '{SSHA}78cdSUfS/6uxGK+hBLB0UWTNMtdyci1zYWx0MQ=='), ('bob', '{clear}bob-secret');\n\
    CREATE TABLE acct (id TEXT PRIMARY KEY, octets BIGINT);\n\
    CREATE TABLE users (username TEXT, plan TEXT, realm TEXT);\n\
    INSERT INTO users VALUES ('alice', 'gold', 'corp');\n\
    CREATE TABLE kinds (id BIGINT, rate DOUBLE PRECISION, name TEXT, active BOOLEAN, note TEXT, shape POINT);\n\
    INSERT INTO kinds VALUES (7, 1.5, 'alice', true, NULL, point(1, 2));\n";

/// spec Login against PostgreSQL, Database stopped during login and during accounting,
/// Constraint violation in accounting, and the scripts' calls and value types.
#[tokio::test]
#[ignore]
async fn test_postgresql() -> Result<(), String> {
    assert!(POSTGRES_INIT.contains(ALICE_SSHA));
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    certificates(dir.path())?;
    let postgres = start_postgres(dir.path(), POSTGRES_INIT)?;
    let ca = dir.path().join("ca.pem");
    let mut all = BTreeMap::new();
    all.insert("pg".to_string(), settings(DatabaseKind::Postgresql, postgres.port, "rradius", DatabaseTls::Verify, Some(ca.clone())));
    all.insert("other_ca".to_string(), settings(DatabaseKind::Postgresql, postgres.port, "rradius", DatabaseTls::Verify, Some(dir.path().join("other-ca.pem"))));
    let databases = ExternalDatabases::open(&all);
    wait_ready(&databases, "pg").await?;

    // spec Certificate from another CA: the database is unavailable, not the statement wrong.
    let failure = databases.get("other_ca").ok_or("no other_ca")?.query("SELECT 1", &[]).await.err().ok_or("trusted another CA")?;
    assert!(failure.unavailable(), "{failure}");

    // spec Placeholder rewriting, Write and read back, Types of one row, Type with no text form.
    script_passes(&databases, r#"
        local function check(ok, what) if not ok then set_attr('Reply-Message', what) error(what) end end
        local rows = db_query('pg', "SELECT '?' AS mark, plan FROM users WHERE username = ? AND realm = ?", {'alice', 'corp'})
        check(#rows == 1 and rows[1].mark == '?' and rows[1].plan == 'gold', 'placeholders')
        check(db_execute('pg', 'INSERT INTO acct (id, octets) VALUES (?, ?)', {'s-1', 42}) == 1, 'insert')
        rows = db_query('pg', 'SELECT octets FROM acct WHERE id = ?', {'s-1'})
        check(#rows == 1 and rows[1].octets == 42, 'read back')
        rows = db_query('pg', 'SELECT SUM(octets) AS total, 12.50::numeric AS price, (-0.001)::numeric AS tiny FROM acct WHERE id = ?', {'s-1'})
        check(rows[1].total == '42' and rows[1].price == '12.50' and rows[1].tiny == '-0.001', 'numeric: ' .. tostring(rows[1].total) .. ' ' .. tostring(rows[1].price) .. ' ' .. tostring(rows[1].tiny))
        rows = db_query('pg', 'SELECT id, rate, name, active, note FROM kinds')
        local row = rows[1]
        check(row.id == 7 and row.rate == 1.5 and row.name == 'alice' and row.active == 1 and row.note == nil, 'types')
        rows = db_query('pg', 'SELECT shape::text AS shape FROM kinds WHERE name = ?::text', {'alice'})
        check(#rows == 1 and rows[1].shape == '(1,2)', 'cast with ::')
        local ok, e = pcall(db_query, 'pg', 'SELECT shape FROM kinds')
        check(not ok and string.find(tostring(e), 'shape', 1, true) ~= nil, 'no text form: ' .. tostring(e))
        check(db_execute('pg', 'DELETE FROM acct WHERE id = ?', {'s-1'}) == 1, 'delete')
        return 'accept'
    "#).await?;

    let databases_and_auth = format!(
        "[databases.billing]\nkind = \"postgresql\"\nhost = \"127.0.0.1\"\nport = {}\ndatabase = \"radius\"\nuser = \"rradius\"\npassword = \"{PASSWORD}\"\nca_file = \"{}\"\nconnect_timeout_ms = 1000\n\
         [auth]\n[[auth.handlers]]\ntype = \"sql\"\ndatabase = \"billing\"\nuser_query = \"SELECT password FROM radcheck WHERE username = ?\"\ntimeout_ms = 2000\n",
        postgres.port,
        ca.display()
    );
    let accounting = "db_execute('billing', 'INSERT INTO acct (id, octets) VALUES (?, ?)', {get_attr('Acct-Session-Id'), get_attr('Acct-Output-Octets')}) return 'accept'";
    let server = start_server(&databases_and_auth, accounting, true)?;

    // spec Login against PostgreSQL.
    let reply = login(&server, "alice", "secret", Duration::from_secs(10))?.ok_or("no answer")?;
    assert_eq!(reply.code, radius::ACCESS_ACCEPT);
    let reply = login(&server, "alice", "wrong", Duration::from_secs(10))?.ok_or("no answer")?;
    assert_eq!(reply.code, radius::ACCESS_REJECT);
    // CHAP needs the stored password itself, read the same way.
    assert_eq!(chap_login(&server, "bob", "bob-secret")?.code, radius::ACCESS_ACCEPT);
    assert_eq!(chap_login(&server, "bob", "wrong")?.code, radius::ACCESS_REJECT);

    // spec Database stopped during login and during accounting, and Constraint violation in
    // accounting: `dup-1` is written now, and again from the spool, where it is a mistake.
    let wait = Duration::from_secs(10);
    assert!(account(&server, 1, "dup-1", 10, wait)?);
    // spec Constraint violation while the database is up: not answered, as on the embedded one.
    assert!(!account(&server, 4, "dup-1", 12, Duration::from_secs(3))?, "a duplicate was acknowledged");
    postgres.stop()?;
    assert!(login(&server, "alice", "secret", Duration::from_secs(4))?.is_none(), "answered without the database");
    assert!(account(&server, 2, "while-down", 30, wait)?, "accounting is answered from the spool");
    assert!(account(&server, 3, "dup-1", 11, wait)?, "accounting is answered from the spool");
    postgres.start()?;
    wait_ready(&databases, "pg").await?;
    let pg = databases.get("pg").ok_or("no pg")?;
    wait_for("the spooled record written", async || Ok(!pg.query("SELECT octets FROM acct WHERE id = 'while-down'", &[]).await.map_err(|e| e.to_string())?.is_empty())).await?;
    let rejected = server.dir.path().join("spool/rejected.log");
    wait_for("the duplicate in rejected.log", async || Ok(std::fs::read(&rejected).is_ok_and(|text| text.windows(5).any(|w| w == b"dup-1")))).await?;
    let octets = pg.query("SELECT octets FROM acct WHERE id = 'dup-1'", &[]).await.map_err(|e| e.to_string())?;
    assert_eq!(octets.len(), 1, "written once");
    let reply = login(&server, "alice", "secret", Duration::from_secs(10))?.ok_or("no answer after the restart")?;
    assert_eq!(reply.code, radius::ACCESS_ACCEPT);
    let stderr = server.stderr();
    assert!(!stderr.contains(PASSWORD), "the password is in the log");
    Ok(())
}

const MYSQL_INIT: &str = "CREATE TABLE radcheck (username VARCHAR(64) PRIMARY KEY, password VARCHAR(128));\n\
    INSERT INTO radcheck VALUES ('alice', '{SSHA}78cdSUfS/6uxGK+hBLB0UWTNMtdyci1zYWx0MQ==');\n\
    CREATE TABLE radreply (username VARCHAR(64), attribute VARCHAR(64), value VARCHAR(253));\n\
    INSERT INTO radreply VALUES ('alice', 'Filter-Id', 'std.ppp');\n\
    CREATE TABLE users (username VARCHAR(64));\n\
    INSERT INTO users VALUES ('alice'), ('bob');\n\
    CREATE TABLE kinds (id BIGINT, small TINYINT, rate DOUBLE, name VARCHAR(16), data VARBINARY(4), note TEXT, u_small TINYINT UNSIGNED, u_int INT UNSIGNED, u_big BIGINT UNSIGNED);\n\
    INSERT INTO kinds VALUES (7, 3, 1.5, 'alice', 0x01020304, NULL, 5, 4000000000, 18446744073709551615);\n\
    ALTER USER 'rradius'@'%' REQUIRE SSL;\n\
    CREATE USER 'plain'@'%' IDENTIFIED WITH mysql_native_password BY 'test-db-password';\n\
    GRANT ALL ON radius.* TO 'plain'@'%';\n";

/// spec Reply attributes from MySQL and Value is not interpreted as SQL, over TLS and, with
/// `mysql_native_password`, without it.
#[tokio::test]
#[ignore]
async fn test_mysql() -> Result<(), String> {
    assert!(MYSQL_INIT.contains(ALICE_SSHA) && MYSQL_INIT.contains(PASSWORD));
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    certificates(dir.path())?;
    let mysql = start_mysql(dir.path(), MYSQL_INIT)?;
    let ca = dir.path().join("ca.pem");
    let mut all = BTreeMap::new();
    all.insert("m".to_string(), settings(DatabaseKind::Mysql, mysql.port, "rradius", DatabaseTls::Verify, Some(ca.clone())));
    all.insert("plain".to_string(), settings(DatabaseKind::Mysql, mysql.port, "plain", DatabaseTls::Off, None));
    all.insert("other_ca".to_string(), settings(DatabaseKind::Mysql, mysql.port, "rradius", DatabaseTls::Verify, Some(dir.path().join("other-ca.pem"))));
    let databases = ExternalDatabases::open(&all);
    wait_ready(&databases, "m").await?;
    wait_ready(&databases, "plain").await?;

    let failure = databases.get("other_ca").ok_or("no other_ca")?.query("SELECT 1", &[]).await.err().ok_or("trusted another CA")?;
    assert!(failure.unavailable(), "{failure}");

    script_passes(&databases, r#"
        local function check(ok, what) if not ok then set_attr('Reply-Message', what) error(what) end end
        check(#db_query('m', 'SELECT username FROM users WHERE username = ?', {"x' OR '1'='1"}) == 0, 'injection')
        check(#db_query('plain', 'SELECT username FROM users WHERE username = ?', {'bob'}) == 1, 'without TLS')
        local row = db_query('m', 'SELECT id, small, rate, name, data, note FROM kinds')[1]
        check(row.id == 7 and row.small == 3 and row.rate == 1.5 and row.name == 'alice' and row.data == '\1\2\3\4' and row.note == nil, 'types')
        row = db_query('m', 'SELECT u_small, u_int FROM kinds')[1]
        check(row.u_small == 5 and row.u_int == 4000000000, 'unsigned: ' .. tostring(row.u_small) .. ' ' .. tostring(row.u_int))
        local ok, e = pcall(db_query, 'm', 'SELECT u_big FROM kinds')
        check(not ok and string.find(tostring(e), 'u_big', 1, true) ~= nil, 'too big: ' .. tostring(e))
        row = db_query('m', 'SELECT SUM(id) AS total, CAST(12.50 AS DECIMAL(6,2)) AS price FROM kinds')[1]
        check(row.total == '7' and row.price == '12.50', 'decimal: ' .. tostring(row.total) .. ' ' .. tostring(row.price))
        check(db_execute('m', 'UPDATE users SET username = ? WHERE username = ?', {'carol', 'nobody'}) == 0, 'no rows changed')
        return 'accept'
    "#).await?;

    let databases_and_auth = format!(
        "[databases.my]\nkind = \"mysql\"\nhost = \"127.0.0.1\"\nport = {}\ndatabase = \"radius\"\nuser = \"rradius\"\npassword = \"{PASSWORD}\"\nca_file = \"{}\"\n\
         [auth]\n[[auth.handlers]]\ntype = \"sql\"\ndatabase = \"my\"\nuser_query = \"SELECT password FROM radcheck WHERE username = ?\"\n\
         reply_query = \"SELECT attribute, value FROM radreply WHERE username = ?\"\n",
        mysql.port,
        ca.display()
    );
    let server = start_server(&databases_and_auth, "return 'accept'", false)?;
    let reply = login(&server, "alice", "secret", Duration::from_secs(10))?.ok_or("no answer")?;
    assert_eq!(reply.code, radius::ACCESS_ACCEPT);
    assert!(reply.attributes.contains(&(11, b"std.ppp".to_vec())), "{:?}", reply.attributes);
    let reply = login(&server, "mallory", "secret", Duration::from_secs(10))?.ok_or("no answer")?;
    assert_ne!(reply.code, radius::ACCESS_ACCEPT);
    Ok(())
}

/// `GET path` on the metrics port: the status code and the body.
fn http_get(port: u16, path: &str) -> Result<(u16, String), String> {
    use std::io::{Read, Write};
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    socket.write_all(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut response = String::new();
    socket.read_to_string(&mut response).map_err(|e| e.to_string())?;
    let status = response.get(9..12).and_then(|code| code.parse().ok()).ok_or_else(|| format!("no status: {response}"))?;
    let body = response.split_once("\r\n\r\n").map_or("", |(_, body)| body).to_string();
    Ok((status, body))
}

/// The value of `sample` in `/metrics` on `port`.
fn metric(port: u16, sample: &str) -> Result<f64, String> {
    let (_, body) = http_get(port, "/metrics")?;
    let line = body.lines().find_map(|line| line.strip_prefix(sample)?.strip_prefix(' ')).ok_or_else(|| format!("no {sample}"))?;
    line.trim().parse().map_err(|e| format!("{sample}: {e}"))
}

/// Waits up to `limit` for `/readyz` to answer `status`; how long that took.
fn wait_readyz(port: u16, status: u16, limit: Duration) -> Result<Duration, String> {
    let started = Instant::now();
    loop {
        let (now, body) = http_get(port, "/readyz")?;
        if now == status {
            return Ok(started.elapsed());
        }
        if started.elapsed() > limit {
            return Err(format!("/readyz still {now} {body:?} after {limit:?}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// spec Required database stopped, Pool in use, Busy pool is not unreachable, and Failures by
/// class against a real server:
/// `/readyz` follows the database with `required_for_ready`, not the one without.
#[tokio::test]
#[ignore]
async fn test_postgresql_readiness() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    certificates(dir.path())?;
    let postgres = start_postgres(dir.path(), POSTGRES_INIT)?;
    let mut all = BTreeMap::new();
    all.insert("pg".to_string(), DatabaseSettings { pool_size: 8, required_for_ready: true, ..settings(DatabaseKind::Postgresql, postgres.port, "rradius", DatabaseTls::Off, None) });
    let databases = ExternalDatabases::open(&all);
    wait_ready(&databases, "pg").await?;

    // spec Pool in use: connections opened and freed, then two held.
    let pg = databases.get("pg").ok_or("no pg")?.clone();
    let sleep = move |seconds: &'static str| {
        let pg = pg.clone();
        async move { pg.query(&format!("SELECT 1 FROM pg_sleep({seconds})"), &[]).await.map(drop).map_err(|e| e.to_string()) }
    };
    let (a, b, c) = tokio::join!(sleep("0.3"), sleep("0.3"), sleep("0.3"));
    a?;
    b?;
    c?;
    let held = tokio::spawn(async move { tokio::join!(sleep("2"), sleep("2")) });
    tokio::time::sleep(Duration::from_millis(700)).await;
    let now = databases.get("pg").ok_or("no pg")?.connections();
    // The pool opened what it needed before, `wait_ready`'s connection among them: those not
    // held are idle.
    assert!(now.in_use == 2 && now.idle >= 1 && now.in_use + now.idle <= now.max && now.max == 8, "{now:?}");
    let (a, b) = held.await.map_err(|e| e.to_string())?;
    a?;
    b?;

    // spec Busy pool is not unreachable: every connection held across two checks, which use a
    // connection of their own.
    databases.start_checks();
    let pg = databases.get("pg").ok_or("no pg")?.clone();
    wait_for("the first check", async || Ok(pg.reachable() == Some(true))).await?;
    let hold = (check_interval() * 2 + Duration::from_secs(2)).as_secs_f64();
    let all = (0..8).map(|_| {
        let pg = pg.clone();
        tokio::spawn(async move { pg.query(&format!("SELECT 1 FROM pg_sleep({hold})"), &[]).await.map(drop).map_err(|e| e.to_string()) })
    });
    let all: Vec<_> = all.collect();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(pg.connections().in_use, 8, "every connection held");
    let until = Instant::now() + check_interval() * 2 + Duration::from_secs(1);
    while Instant::now() < until {
        assert_eq!(pg.reachable(), Some(true), "a busy pool was taken as unreachable");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    for held in all {
        held.await.map_err(|e| e.to_string())??;
    }

    let metrics_port = free_port();
    let connect_timeout = Duration::from_secs(1);
    let databases_and_auth = format!(
        "[metrics]\nport = {metrics_port}\n\
         [databases.billing]\nkind = \"postgresql\"\nhost = \"127.0.0.1\"\nport = {}\ndatabase = \"radius\"\nuser = \"rradius\"\npassword = \"{PASSWORD}\"\ntls = \"off\"\nconnect_timeout_ms = 1000\nrequired_for_ready = true\n\
         [databases.legacy]\nkind = \"postgresql\"\nhost = \"127.0.0.1\"\nport = {}\ndatabase = \"radius\"\nuser = \"rradius\"\ntls = \"off\"\n\
         [auth]\n[[auth.handlers]]\ntype = \"sql\"\ndatabase = \"billing\"\nuser_query = \"SELECT password FROM radcheck WHERE username = ?\"\ntimeout_ms = 2000\n",
        postgres.port,
        free_port()
    );
    let server = start_server(&databases_and_auth, "return 'accept'", false)?;
    // The first check runs at once; `legacy`, which nothing listens for, is not checked.
    wait_readyz(metrics_port, 200, Duration::from_secs(10))?;
    assert_eq!(metric(metrics_port, "rradius_external_db_reachable{database=\"billing\"}")?, 1.0);
    assert!(metric(metrics_port, "rradius_external_db_reachable{database=\"legacy\"}").is_err(), "legacy is not required");
    let reply = login(&server, "alice", "secret", Duration::from_secs(10))?.ok_or("no answer")?;
    assert_eq!(reply.code, radius::ACCESS_ACCEPT);

    // spec Required database stopped.
    let unavailable = "rradius_external_db_failures_total{database=\"billing\",reason=\"unavailable\"}";
    let before = metric(metrics_port, unavailable)?;
    postgres.stop()?;
    let worst = 2 * (check_interval() + connect_timeout);
    let took = wait_readyz(metrics_port, 503, worst)?;
    let (_, body) = http_get(metrics_port, "/readyz")?;
    assert_eq!(body, "database billing unreachable\n");
    assert_eq!(http_get(metrics_port, "/livez")?.0, 200);
    assert_eq!(metric(metrics_port, "rradius_external_db_reachable{database=\"billing\"}")?, 0.0);
    assert!(login(&server, "alice", "secret", Duration::from_secs(4))?.is_none(), "answered without the database");
    assert!(metric(metrics_port, unavailable)? > before, "the failed login is counted");
    eprintln!("unreachable after {took:?} (at most {worst:?})");

    postgres.start()?;
    wait_ready(&databases, "pg").await?;
    let took = wait_readyz(metrics_port, 200, check_interval() + connect_timeout)?;
    eprintln!("ready again {took:?} after the database answered");
    Ok(())
}

fn check_interval() -> Duration {
    rradius_core::external_db::CHECK_INTERVAL
}

/// spec Database not reachable at startup: the server starts and serves; only what uses the
/// database fails. Needs no Docker, so it is not ignored.
#[test]
fn test_starts_without_the_database() -> Result<(), String> {
    let port = free_port();
    let databases = format!("[databases.billing]\nkind = \"postgresql\"\nhost = \"127.0.0.1\"\nport = {port}\ndatabase = \"radius\"\nuser = \"rradius\"\ntls = \"off\"\n");
    let server = start_server(&databases, "if get_attr('User-Name') == 'billing' then db_query('billing', 'SELECT 1') end return 'accept'", false)?;
    assert_eq!(login(&server, "alice", "secret", Duration::from_secs(10))?.ok_or("no answer")?.code, radius::ACCESS_ACCEPT);
    let failed = login(&server, "billing", "secret", Duration::from_secs(4))?;
    assert!(failed.is_none_or(|reply| reply.code != radius::ACCESS_ACCEPT), "a login that needs the database got in");
    Ok(())
}
