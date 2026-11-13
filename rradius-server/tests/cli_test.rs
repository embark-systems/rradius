use hmac::{Hmac, KeyInit, Mac};
use md5::Md5;
use rradius_core::radius::{self, RadiusPacket};
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use rradius_core::test_support::server::*;

/// The program these tests start.
const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

fn run(args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_rradius-server")).args(args).output().unwrap();
    (output.status.code(), String::from_utf8_lossy(&output.stderr).into_owned())
}

/// Without `--config`, `server` and `check` read the default configuration, which is not
/// there when the tests run unless a prefix pointed it at one.
#[test]
fn test_missing_config_argument() {
    let default = rradius_core::config::default_config_path();
    if Path::new(default).exists() {
        return;
    }
    let (code, stderr) = run(&["server"]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains(default) && stderr.contains("give --config <path> for another"), "unexpected stderr: {stderr}");
    let (code, stderr) = run(&["check"]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains(default), "unexpected stderr: {stderr}");
    // So does a user command.
    let (code, stderr) = run(&["user", "show", "alice"]);
    assert_ne!(code, Some(0));
    assert!(stderr.contains(default), "unexpected stderr: {stderr}");
    let (code, stderr) = run(&["--confg", "x"]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("usage: rradius-server server [--config <path>]") && stderr.contains(&format!("without --config, the configuration is {default}")), "unexpected stderr: {stderr}");
}

/// The exit status, standard output and standard error of the program run with `args`.
fn output_of(args: &[&str]) -> Result<(Option<i32>, String, String), String> {
    let output = Command::new(PROGRAM).args(args).output().map_err(|e| e.to_string())?;
    Ok((output.status.code(), String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned()))
}

/// `help`, `--help` and `-h` print every command on standard output, read nothing and succeed.
#[test]
fn test_help_prints_the_usage() -> Result<(), String> {
    let default = rradius_core::config::default_config_path();
    for help in ["help", "--help", "-h"] {
        let (code, stdout, stderr) = output_of(&[help])?;
        assert_eq!(code, Some(0), "{help}: {stderr}");
        for line in ["usage: rradius-server server [--config <path>]", "usage: rradius-server check [--config <path>]", "usage: rradius-server --version", "usage: rradius-server config show", "usage: rradius-server tuning", "usage: rradius-server user"] {
            assert!(stdout.contains(line), "{help}: {line} missing from {stdout}");
        }
        assert!(stdout.ends_with(&format!("without --config, the configuration is {default}\n")), "{help}: {stdout}");
        assert!(stderr.is_empty(), "{help}: {stderr}");
    }
    Ok(())
}

/// Without arguments the program prints its usage and reads nothing.
#[test]
fn test_no_arguments_print_the_usage() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let output = Command::new(PROGRAM).current_dir(dir.path()).output().map_err(|e| e.to_string())?;
    let (stdout, stderr) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stdout.starts_with("usage: rradius-server server [--config <path>]\nusage: rradius-server check [--config <path>]\n"), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");
    let (_, help, _) = output_of(&["--help"])?;
    assert_eq!(stdout, help);
    assert_eq!(std::fs::read_dir(dir.path()).map_err(|e| e.to_string())?.count(), 0, "it created something");
    Ok(())
}

/// The flags that started and checked before the subcommands now print the usage and the
/// subcommand to use, and start nothing.
#[test]
fn test_old_flags_name_the_subcommands() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let config = config.to_str().ok_or("path not UTF-8")?;
    let start = "to start the server: rradius-server server --config <path>";
    let check = "to check a configuration: rradius-server check [--config <path>]";
    for (args, hint) in [
        (&["--config", config][..], Some(start)),
        (&["--config", config, "--check"], Some(check)),
        (&["--check"], Some(check)),
        (&["--check", "--config", config], Some(check)),
        (&["--confg", config], None),
    ] {
        let (code, stdout, stderr) = output_of(args)?;
        assert_eq!(code, Some(2), "{args:?}");
        assert!(stdout.is_empty() && stderr.contains("usage: rradius-server server [--config <path>]"), "{args:?}: {stderr}");
        match hint {
            Some(hint) => {
                assert!(stderr.trim_end().ends_with(hint), "{args:?}: {stderr}");
                // An old check is never told to start the server.
                assert!(hint == start || !stderr.contains("to start the server"), "{args:?}: {stderr}");
            }
            None => assert!(!stderr.contains("to start the server") && !stderr.contains("to check a configuration"), "{args:?}: {stderr}"),
        }
    }
    assert!(!dir.path().join("data").exists(), "the server started");
    Ok(())
}

/// After `server` and `check` only `--config <path>` is taken.
#[test]
fn test_server_and_check_arguments() -> Result<(), String> {
    for (args, usage) in [
        (&["server", "extra"][..], "usage: rradius-server server [--config <path>]"),
        (&["server", "--config"], "usage: rradius-server server [--config <path>]"),
        (&["server", "--config", "a.toml", "extra"], "usage: rradius-server server [--config <path>]"),
        (&["check", "extra"], "usage: rradius-server check [--config <path>]"),
        (&["check", "--config", "a.toml", "extra"], "usage: rradius-server check [--config <path>]"),
    ] {
        let (code, stdout, stderr) = output_of(args)?;
        assert_eq!(code, Some(2), "{args:?}");
        assert!(stdout.is_empty() && stderr.trim_end() == usage, "{args:?}: {stderr}");
    }
    Ok(())
}

#[test]
fn test_nonexistent_config_file() {
    let (code, stderr) = run(&["server", "--config", "/nonexistent.toml"]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("/nonexistent.toml"), "unexpected stderr: {stderr}");
}

#[test]
fn test_unknown_auth_handler_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rradius.toml");
    std::fs::write(
        &path,
        "dictionary = \"d\"\n[lua]\nscript = \"s\"\n\
         [cluster]\ndata_dir = \"data\"\nsecret_raft = \"test-raft-secret-0123\"\nsecret_api = \"test-api-secret-01234\"\n\
         [auth]\n[[auth.handlers]]\ntype = \"kerberos\"\n\
         [radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n",
    )
    .unwrap();
    let (code, stderr) = run(&["server", "--config", path.to_str().unwrap()]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("unknown variant") && stderr.contains("expected") && !stderr.contains("kerberos"), "unexpected stderr: {stderr}");
}

#[test]
fn test_missing_auth_handler_script() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rradius.toml");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::write(
        &path,
        format!(
            "dictionary = \"{}\"\n[lua]\nscript = \"{}\"\n\
             [cluster]\ndata_dir = \"data\"\nsecret_raft = \"test-raft-secret-0123\"\nsecret_api = \"test-api-secret-01234\"\n\
             [[cluster.nodes]]\nid = 1\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n\
             [auth]\n[[auth.handlers]]\ntype = \"lua\"\nscript = \"missing.lua\"\n\
             [radius]\nauth_port = {}\nacct_port = {}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n",
            root.join("../share/dictionary").display(),
            root.join("tests/fixtures/auth.lua").display(),
            free_port(),
            free_port(),
            free_port(),
            free_port(),
        ),
    )
    .unwrap();
    let (code, stderr) = run(&["server", "--config", path.to_str().unwrap()]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("missing.lua"), "unexpected stderr: {stderr}");
}

fn run_with_scripts(script: &str, handler: Option<&str>) -> (Option<i32>, String) {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_config(dir.path(), script, handler);
    run(&["server", "--config", path.to_str().unwrap()])
}

/// The outcome end to end, on a real server with a real file. mlua also refuses
/// `require` from chunks named with `=`, so this still passes if `require` comes
/// back: `test_removed_globals_are_nil` and `test_visible_globals` guard that.
#[test]
fn test_require_cannot_load_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mod.luau"), "return { loaded = true }").unwrap();
    let script = "local ok = pcall(require, './mod') return ok and 'accept' or 'reject'";
    let server = Server::start(PROGRAM, dir, script, None);
    assert_eq!(server.access_request(), radius::ACCESS_REJECT);
}

/// The message and the traceback both locate the error by the script's path.
fn assert_names_script(stderr: &str, script: &Path) {
    let location = format!("{}:2:", script.display());
    assert!(stderr.contains(&format!("{location} attempt to index nil")), "unexpected stderr: {stderr}");
    assert!(stderr.contains(&format!("{location} in function")), "unexpected stderr: {stderr}");
    assert!(!stderr.contains("lua_engine.rs"), "unexpected stderr: {stderr}");
}

/// Starts a server whose script fails on line 2 and returns what it logged.
fn stderr_after_script_error(script: &str, handler: Option<&str>) -> (String, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let server = Server::start(PROGRAM, dir, script, handler);
    assert_eq!(server.access_request(), radius::ACCESS_REJECT);
    (server.wait_for_log("Lua script error"), root)
}

const FAILS_ON_LINE_2: &str = "local x = nil\nreturn x.y";

#[test]
fn test_script_error_names_top_level_script() {
    let (stderr, dir) = stderr_after_script_error(FAILS_ON_LINE_2, None);
    assert_names_script(&stderr, &dir.join("top.lua"));
}

#[test]
fn test_script_error_names_handler_script() {
    let (stderr, dir) = stderr_after_script_error("return 'accept'", Some(FAILS_ON_LINE_2));
    assert_names_script(&stderr, &dir.join("handler.lua"));
}

#[test]
fn test_migrations_applied_before_serving() {
    let dir = dir_with_migrations(&[("1_users.sql", "CREATE TABLE users (id INTEGER);"), ("2_reply.sql", "CREATE TABLE user_reply (id INTEGER);")]);
    // A write-free script that only succeeds with both tables: an error rejects the request.
    let script = "sql_query('SELECT id FROM users') sql_query('SELECT id FROM user_reply') return 'accept'";
    let server = Server::start_with(PROGRAM, dir, script, None, WITH_MIGRATIONS);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
}

#[test]
fn test_migrations_survive_restart() {
    // Without IF NOT EXISTS: running a file again would stop the server with status 2.
    let dir = dir_with_migrations(&[("1_users.sql", "CREATE TABLE users (id INTEGER);"), ("2_reply.sql", "CREATE TABLE user_reply (id INTEGER);")]);
    let two_applied = "return #sql_query('SELECT id FROM _migrations') == 2 and 'accept' or 'reject'";
    let mut server = Server::start_with(PROGRAM, dir, two_applied, None, WITH_MIGRATIONS);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "first start");

    server.restart(PROGRAM);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "restart: {}", server.stderr());

    let dir = server.dir.path();
    std::fs::write(dir.join("migrations/3_acct.sql"), "CREATE TABLE acct (id INTEGER);").unwrap();
    std::fs::write(dir.join("top.lua"), "sql_query('SELECT id FROM acct') return #sql_query('SELECT id FROM _migrations') == 3 and 'accept' or 'reject'").unwrap();
    server.restart(PROGRAM);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "restart with a new file: {}", server.stderr());
}

fn exit_with_migrations(files: &[(&str, &str)]) -> (Option<i32>, String) {
    let dir = dir_with_migrations(files);
    let (path, _) = write_config_with(dir.path(), "return 'accept'", None, WITH_MIGRATIONS);
    run(&["server", "--config", path.to_str().unwrap()])
}

#[test]
fn test_invalid_migration_file_name_stops_startup() {
    let (code, stderr) = exit_with_migrations(&[("users.sql", "SELECT 1;")]);
    assert_eq!(code, Some(2), "unexpected stderr: {stderr}");
    assert!(stderr.contains("users.sql"), "unexpected stderr: {stderr}");
}

#[test]
fn test_failing_migration_stops_startup() {
    let (code, stderr) = exit_with_migrations(&[("1_users.sql", "CREATE TABLE users (id INTEGER);"), ("2_reply.sql", "CREATE TABLE user_reply (")]);
    assert_eq!(code, Some(2), "unexpected stderr: {stderr}");
    assert!(stderr.contains("2_reply.sql"), "unexpected stderr: {stderr}");
}

#[test]
fn test_missing_migrations_directory_stops_startup() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_config_with(dir.path(), "return 'accept'", None, WITH_MIGRATIONS);
    let (code, stderr) = run(&["server", "--config", path.to_str().unwrap()]);
    assert_eq!(code, Some(2), "unexpected stderr: {stderr}");
    assert!(stderr.contains(&dir.path().join("migrations").display().to_string()), "unexpected stderr: {stderr}");
}

/// The file finishes in the cluster after the node gave up on it, so the restart must
/// find it recorded once rather than run it again.
#[test]
fn test_file_slower_than_the_limit() {
    let dir = dir_with_migrations(&[(
        "1_big.sql",
        "CREATE TABLE big (id INTEGER);
         INSERT INTO big WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 10000000) SELECT x FROM n;",
    )]);
    let script = "sql_query('SELECT id FROM big LIMIT 1') return #sql_query('SELECT id FROM _migrations') == 1 and 'accept' or 'reject'";
    let (config, auth_port) = write_config_with(dir.path(), script, None, "migrations_dir = \"migrations\"\nmigrations_wait_ms = 3000\n");
    let (code, stderr) = run_within(Duration::from_secs(60), &config);
    assert_eq!(code, Some(1), "unexpected stderr: {stderr}");
    assert!(stderr.contains("migrations_wait_ms"), "unexpected stderr: {stderr}");

    let text = std::fs::read_to_string(&config).unwrap().replace("migrations_wait_ms = 3000", "migrations_wait_ms = 120000");
    std::fs::write(&config, text).unwrap();
    let server = Server { child: Server::spawn(PROGRAM, dir.path(), &config), dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "after raising the limit: {}", server.stderr());
}

/// The usage document tells operators to look for this text.
#[test]
fn test_discard_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(PROGRAM, dir, "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "the server is up");

    let bare = RadiusPacket { code: radius::ACCESS_REQUEST, identifier: 2, authenticator: [2; 16], attributes: vec![(1, b"alice".to_vec())] }.encode();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.send_to(&bare, ("127.0.0.1", server.auth_port)).unwrap();
    socket.send_to(&bare, ("127.0.0.1", server.auth_port)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let stderr = server.stderr();
    assert_eq!(stderr.matches("lacking a Message-Authenticator").count(), 1, "unexpected stderr: {stderr}");
}

/// `main.rs` hands the setting to the server: without that the default would stay on.
#[test]
fn test_status_server_can_be_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap().replace("[radius]\n", "[radius]\nstatus_server = false\n");
    std::fs::write(&config, text).unwrap();
    let server = Server { child: Server::spawn(PROGRAM, dir.path(), &config), dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "the server is up");

    let mut status = RadiusPacket { code: radius::STATUS_SERVER, identifier: 9, authenticator: [9; 16], attributes: vec![(radius::MESSAGE_AUTHENTICATOR, vec![0; 16])] }.encode();
    let mut mac = Hmac::<Md5>::new_from_slice(b"s").unwrap();
    mac.update(&status);
    status[22..38].copy_from_slice(&mac.finalize().into_bytes());
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    socket.send_to(&status, ("127.0.0.1", server.auth_port)).unwrap();
    assert!(socket.recv_from(&mut [0u8; 4096]).is_err(), "answered with status_server = false");
}

/// Writes a configuration for node 1 of three, with the other two never started.
fn lone_node_config(dir: &Path) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::write(dir.join("top.lua"), "return 'accept'").unwrap();
    let nodes: String = (1..=3)
        .map(|id| format!("[[cluster.nodes]]\nid = {id}\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n", free_port(), free_port()))
        .collect();
    let path = dir.join("rradius.toml");
    std::fs::write(
        &path,
        format!(
            "dictionary = \"{}\"\n[lua]\nscript = \"top.lua\"\n\
             [cluster]\ndata_dir = \"data\"\nsecret_raft = \"test-raft-secret-0123\"\nsecret_api = \"test-api-secret-01234\"\n\
             migrations_dir = \"migrations\"\nmigrations_wait_ms = 2000\n{nodes}\
             [radius]\nauth_port = {}\nacct_port = {}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n",
            root.join("../share/dictionary").display(),
            free_port(),
            free_port(),
        ),
    )
    .unwrap();
    path
}

/// Like `run`, but a server that does not exit within `limit` fails the test rather
/// than hanging it.
fn run_within(limit: Duration, config: &Path) -> (Option<i32>, String) {
    let stderr_path = config.with_extension("stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rradius-server"))
        .args(["server", "--config", config.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("still running after {limit:?}: {}", std::fs::read_to_string(&stderr_path).unwrap());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    (status.code(), std::fs::read_to_string(&stderr_path).unwrap())
}

#[test]
fn test_cluster_not_ready_for_migrations() {
    let dir = dir_with_migrations(&[("1_users.sql", "CREATE TABLE users (id INTEGER);")]);
    let (code, stderr) = run_within(Duration::from_secs(10), &lone_node_config(dir.path()));
    assert_eq!(code, Some(1), "unexpected stderr: {stderr}");
    assert!(stderr.contains("not ready"), "unexpected stderr: {stderr}");
}

/// The cluster is not ready here, which must not hide a mistake in the directory.
/// The setting in `lone_node_config` is 2 s, so raise it past the limit of this test.
#[test]
fn test_invalid_migrations_stop_startup_before_joining() {
    let wait_long = |config: &Path| {
        let text = std::fs::read_to_string(config).unwrap().replace("migrations_wait_ms = 2000", "migrations_wait_ms = 60000");
        std::fs::write(config, text).unwrap();
    };
    let dir = dir_with_migrations(&[("users.sql", "SELECT 1;")]);
    let config = lone_node_config(dir.path());
    wait_long(&config);
    let (code, stderr) = run_within(Duration::from_secs(5), &config);
    assert_eq!(code, Some(2), "unexpected stderr: {stderr}");
    assert!(stderr.contains("users.sql"), "unexpected stderr: {stderr}");

    let dir = tempfile::tempdir().unwrap();
    let config = lone_node_config(dir.path());
    wait_long(&config);
    let (code, stderr) = run_within(Duration::from_secs(5), &config);
    assert_eq!(code, Some(2), "unexpected stderr: {stderr}");
    assert!(stderr.contains(&dir.path().join("migrations").display().to_string()), "unexpected stderr: {stderr}");
}

#[test]
fn test_script_syntax_error() {
    let (code, stderr) = run_with_scripts("return 'accept", None);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("top.lua"), "unexpected stderr: {stderr}");
}

#[test]
fn test_auth_handler_script_syntax_error() {
    let (code, stderr) = run_with_scripts("return 'accept'", Some("if then"));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("handler.lua"), "unexpected stderr: {stderr}");
}

/// Access-Requests with a secret, and the scripts of a running server.
trait Asking {
    fn ask(&self, secret: &[u8]) -> Option<u8>;
    fn ask_as(&self, name: &[u8], secret: &[u8]) -> Option<u8>;
    fn write_script(&self, name: &str, source: &str);
}

impl Asking for Server {
    /// One Access-Request signed with `secret`, never a retransmission; the code of the
    /// response, or `None` without one.
    fn ask(&self, secret: &[u8]) -> Option<u8> {
        self.ask_as(b"alice", secret)
    }

    fn ask_as(&self, name: &[u8], secret: &[u8]) -> Option<u8> {
        static SENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        let sent = SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let packet = RadiusPacket::create_access_request(sent, [sent; 16], vec![(1, name.to_vec())], secret);
        socket.send_to(&packet, ("127.0.0.1", self.auth_port)).unwrap();
        let mut buf = [0u8; 4096];
        let (size, _) = socket.recv_from(&mut buf).ok()?;
        Some(RadiusPacket::parse(&buf[..size]).unwrap().code)
    }

    fn write_script(&self, name: &str, source: &str) {
        std::fs::write(self.dir.path().join(name), source).unwrap();
    }
}

const RELOAD_FAILED: &str = "reload failed";

#[test]
fn test_reload_script() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'reject'", None);
    assert_eq!(server.access_request(), radius::ACCESS_REJECT);
    server.write_script("top.lua", "return 'accept'");
    server.reload(RELOADED);
    assert_eq!(server.ask(b"s"), Some(radius::ACCESS_ACCEPT));
}

#[test]
fn test_reload_chain_script() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'reject'", Some("return 'reject'"));
    assert_eq!(server.access_request(), radius::ACCESS_REJECT);
    server.write_script("handler.lua", "return 'accept'");
    server.reload(RELOADED);
    assert_eq!(server.ask(b"s"), Some(radius::ACCESS_ACCEPT));
}

#[test]
fn test_reload_username_settings() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'reject'", Some("return auth_user == 'alice' and 'accept' or 'reject'"));
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    assert_eq!(server.ask_as(b"ALICE", b"s"), Some(radius::ACCESS_REJECT));

    server.write_config_text(|text| text.replace("[auth]\n", "[auth]\n[auth.username]\nlowercase = false\n"));
    server.reload(RELOADED);
    assert_eq!(server.ask_as(b"ALICE", b"s"), Some(radius::ACCESS_REJECT));

    server.write_config_text(|text| text.replace("lowercase = false", "lowercase = true"));
    let stderr = server.reload(RELOADED);
    assert!(!stderr.to_lowercase().contains("restart"), "{stderr}");
    assert_eq!(server.ask_as(b"ALICE", b"s"), Some(radius::ACCESS_ACCEPT));
}

#[test]
fn test_reload_client_list() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);

    // Removed: from now on an unknown client.
    server.write_config_text(|text| text.replace("address = \"127.0.0.1\"", "address = \"192.0.2.1\""));
    server.reload(RELOADED);
    assert_eq!(server.ask(b"s"), None);
    assert!(server.stderr().contains("unknown client 127.0.0.1"), "{}", server.stderr());

    // Added.
    server.write_config_text(|text| text.replace("address = \"192.0.2.1\"", "address = \"127.0.0.1\""));
    server.reload(RELOADED);
    assert_eq!(server.ask(b"s"), Some(radius::ACCESS_ACCEPT));
}

#[test]
fn test_reload_secret_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("client.secret"), "s\n").unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap().replace("secret = \"s\"", "secret_file = \"client.secret\"");
    std::fs::write(&config, text).unwrap();
    let server = Server { child: Server::spawn(PROGRAM, dir.path(), &config), dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);

    server.write_script("client.secret", "changed\n");
    server.reload(RELOADED);
    assert_eq!(server.ask(b"changed"), Some(radius::ACCESS_ACCEPT));
    assert_eq!(server.ask(b"s"), None, "signed with the old secret");
}

#[test]
fn test_failed_reload_changes_nothing() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);

    // A valid new client list, but a syntax error in the script.
    server.write_config_text(|text| text.replace("secret = \"s\"", "secret = \"changed\""));
    server.write_script("top.lua", "return 'reject");
    let log = server.reload(RELOAD_FAILED);
    assert!(log.contains("top.lua"), "{log}");
    assert_eq!(server.ask(b"s"), Some(radius::ACCESS_ACCEPT), "the old client list and script stay");
    assert_eq!(server.ask(b"changed"), None);

    // Recovery.
    server.write_script("top.lua", "return 'reject'");
    server.reload(RELOADED);
    assert_eq!(server.ask(b"changed"), Some(radius::ACCESS_REJECT));
}

#[test]
fn test_metrics_across_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let metrics_port = free_port();
    let text = std::fs::read_to_string(&config).unwrap().replace("[radius]\n", &format!("[metrics]\nport = {metrics_port}\n[radius]\n"));
    std::fs::write(&config, text).unwrap();
    let server = Server { child: Server::spawn(PROGRAM, dir.path(), &config), dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    assert_eq!(scrape(metrics_port, "rradius_reload_total{result=\"ok\"}"), 0);
    // RadSec, dynamic clients and CoA, and their metrics, are in rradius-enterprise only.
    let body = metrics_text(metrics_port).unwrap_or_default();
    assert!(body.contains("rradius_reload_total"), "{body}");
    for name in ["rradius_radsec_", "rradius_dynamic_client", "rradius_dynauth_", "rradius_coa_relay"] {
        assert!(!body.contains(name), "{name}: {body}");
    }
    assert_eq!(scrape(metrics_port, "rradius_reload_last_success_timestamp_seconds"), 0);
    let handled = scrape(metrics_port, "rradius_radius_packets_total{kind=\"auth\"}");
    assert!(handled >= 1);

    server.reload(RELOADED);
    assert_eq!(scrape(metrics_port, "rradius_reload_total{result=\"ok\"}"), 1);
    assert!(scrape(metrics_port, "rradius_reload_last_success_timestamp_seconds") > 1_000_000_000);
    assert_eq!(scrape(metrics_port, "rradius_radius_packets_total{kind=\"auth\"}"), handled, "a reload does not reset the counters");

    server.write_script("top.lua", "return 'accept");
    server.reload(RELOAD_FAILED);
    assert_eq!(scrape(metrics_port, "rradius_reload_total{result=\"error\"}"), 1);
    assert_eq!(scrape(metrics_port, "rradius_reload_total{result=\"ok\"}"), 1);
}

/// Whether a connection to `port` on this machine is refused.
fn tcp_refused(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1)).is_err()
}

/// With these sections absent their default ports stay closed. A port something else
/// on this machine already holds proves nothing, so it is left out.
#[test]
fn test_unconfigured_ports_stay_closed() {
    let ports: Vec<u16> = [2083, 9812].into_iter().filter(|port| tcp_refused(*port)).collect();
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "the server is up");
    for port in ports {
        assert!(tcp_refused(port), "port {port} is open");
    }
}

#[test]
fn test_reload_does_not_move_ports() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let new_port = free_port();
    let old_port = server.auth_port;
    server.write_config_text(|text| text.replace(&format!("auth_port = {old_port}"), &format!("auth_port = {new_port}")));
    server.write_script("top.lua", "return 'reject'");
    let log = server.reload("need a restart");
    assert!(log.contains("radius.auth_port"), "{log}");

    assert_eq!(server.ask(b"s"), Some(radius::ACCESS_REJECT), "still on the old port, with the new script");
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    socket.send_to(&RadiusPacket::create_access_request(1, [1; 16], vec![(1, b"alice".to_vec())], b"s"), ("127.0.0.1", new_port)).unwrap();
    assert!(socket.recv_from(&mut [0u8; 4096]).is_err(), "answered on the new port");
}

#[test]
fn test_reload_does_not_open_a_section() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let port = free_port();
    server.write_config_text(|text| format!("{text}[tacacs]\nport = {port}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"t\"\n"));
    let log = server.reload("need a restart");
    assert!(log.contains("[tacacs]"), "{log}");
    assert!(tcp_refused(port), "the TACACS+ port was opened");
}

#[test]
fn test_invalid_toml_logged_in_one_line_without_the_value() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    server.write_config_text(|text| format!("{text}secret = \"hunter2\" x\n"));
    let log = server.reload(RELOAD_FAILED);
    assert!(!log.contains("hunter2"), "{log}");
    // Other lines can arrive at any time; this one has to be a single line.
    let failures: Vec<&str> = log.lines().filter(|line| line.contains(RELOAD_FAILED)).collect();
    assert_eq!(failures.len(), 1, "{log}");
    assert!(failures[0].contains("`secret`"), "{log}");
}

/// SIGHUP once the process is running but likely before it serves: it must not end the
/// process, and the reload follows. A fixed delay will not do, since a process that has
/// not reached `main` yet has no handler and dies: the data directory appearing shows it has.
#[test]
fn test_sighup_during_startup() {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let server = Server { child: Server::spawn(PROGRAM, dir.path(), &config), dir, config, auth_port };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !server.dir.path().join("data").exists() {
        assert!(Instant::now() < deadline, "no data directory: {}", server.stderr());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(Command::new("kill").args(["-HUP", &server.child.id().to_string()]).status().unwrap().success());
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "the process lived");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !server.stderr().contains(RELOADED) {
        assert!(Instant::now() < deadline, "no reload followed: {}", server.stderr());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A signal arriving while a reload runs still leads to the file being read after it.
#[test]
fn test_sighup_during_reload_is_not_lost() {
    let server = Server::start(PROGRAM, tempfile::tempdir().unwrap(), "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let hup = || assert!(Command::new("kill").args(["-HUP", &server.child.id().to_string()]).status().unwrap().success());
    for _ in 0..3 {
        hup();
    }
    server.write_script("top.lua", "return 'reject'");
    hup();
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.ask(b"s") != Some(radius::ACCESS_REJECT) {
        assert!(Instant::now() < deadline, "the last signal was lost: {}", server.stderr());
    }
}

/// A server with `settings` added to its configuration file.
fn logging_server(script: &str, handler: Option<&str>, settings: &str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), script, handler);
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}{settings}\n")).unwrap();
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    Server { child, dir, config, auth_port }
}

/// Stopping a server with a signal.
trait Stopping {
    fn stop_with(&mut self, signal: &str) -> String;
}

impl Stopping for Server {
    /// Ends the server the way `docker stop` does, or as Ctrl+C does, and returns what it wrote.
    fn stop_with(&mut self, signal: &str) -> String {
        let status = Command::new("kill").args([signal, &self.child.id().to_string()]).status().unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if self.child.try_wait().unwrap().is_some() {
                return self.stderr();
            }
            assert!(Instant::now() < deadline, "did not stop: {}", self.stderr());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

}

fn json_lines(stderr: &str) -> Vec<serde_json::Value> {
    stderr.lines().map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"))).collect()
}

#[test]
fn test_text_format() {
    let server = logging_server("log('hello \\27[31m') return 'accept'", None, "");
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let stderr = server.wait_for_log("rradius_auth");
    for line in stderr.lines() {
        let (timestamp, rest) = line.split_once(' ').unwrap_or_else(|| panic!("{line}"));
        assert!(timestamp.len() >= 20 && timestamp.ends_with('Z') && timestamp.as_bytes()[10] == b'T', "{line}");
        assert!(["ERROR", "WARN", "INFO", "DEBUG"].iter().any(|level| rest.trim_start().starts_with(level)), "{line}");
    }
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(stderr.contains("Servers started"), "{stderr}");
    assert_eq!(std::fs::read_to_string(server.dir.path().join("stdout.log")).unwrap(), "");
}

/// `[log.trace]` asks for its lines whatever `level` says, also as JSON, and only for those listed.
#[test]
fn test_trace_in_a_real_server_at_level_error() {
    let server = logging_server("return 'accept'", None, "[log]\nlevel = \"error\"\nformat = \"json\"\n[log.trace]\nusers = [\"alice\"]\nminutes = 1");
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    assert_eq!(server.ask_as(b"bob", b"s"), Some(radius::ACCESS_ACCEPT));
    server.wait_for_log("rradius_trace");
    std::thread::sleep(Duration::from_millis(200));
    let stderr = server.stderr();
    let lines = json_lines(&stderr);
    let traced: Vec<_> = lines.iter().filter(|line| line["target"] == "rradius_trace").collect();
    assert_eq!(traced.len(), 2, "a request and its response: {stderr}");
    assert!(traced.iter().all(|line| line["user"] == "alice"), "{stderr}");
    assert!(traced.iter().any(|line| line.to_string().contains("User-Name")), "{stderr}");
    assert!(lines.iter().any(|line| line["target"] == "rradius_auth" && line["user"] == "bob"), "bob was handled, and has no trace: {stderr}");
    assert!(lines.iter().all(|line| line["target"] == "rradius_trace" || line["target"] == "rradius_auth" || line["level"] == "ERROR" || line["level"] == "WARN"), "level still holds for the rest: {stderr}");
}

#[test]
fn test_json_format() {
    let server = logging_server("log('hello') return 'accept'", None, "[log]\nformat = \"json\"");
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let stderr = server.wait_for_log("rradius_auth");
    let lines = json_lines(&stderr);
    for line in &lines {
        for field in ["timestamp", "level", "target", "message"] {
            assert!(line.get(field).is_some(), "no {field}: {line}");
        }
    }
    let record = lines.iter().find(|line| line["target"] == "rradius_auth").unwrap();
    let mut fields: Vec<&str> = record.as_object().unwrap().keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(fields, ["account", "chain", "client", "decision", "handler", "level", "message", "protocol", "source", "target", "timestamp", "user"]);
    assert_eq!((record["protocol"].as_str(), record["user"].as_str(), record["decision"].as_str()), (Some("radius"), Some("alice"), Some("accept")));
    assert_eq!((record["chain"].as_str(), record["handler"].as_str(), record["source"].as_str()), (Some("script"), Some("script"), Some("127.0.0.1")));
}

#[test]
fn test_invalid_log_settings_stop_startup() {
    for (setting, expected) in [("level = \"loud\"", "log.level"), ("format = \"xml\"", "log.format"), ("color = true", "color")] {
        let dir = tempfile::tempdir().unwrap();
        let (config, _) = write_config(dir.path(), "return 'accept'", None);
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, format!("{text}[log]\n{setting}\n")).unwrap();
        let (code, stderr) = run(&["server", "--config", config.to_str().unwrap()]);
        assert_eq!(code, Some(2), "{stderr}");
        assert!(stderr.contains(expected), "{setting}: {stderr}");
    }
}

#[test]
fn test_level_filters_messages() {
    let quiet = logging_server("log('hello') return 'accept'", None, "[log]\nlevel = \"warn\"");
    assert_eq!(quiet.access_request(), radius::ACCESS_ACCEPT);
    let stderr = quiet.wait_for_log("rradius_auth");
    assert!(!stderr.contains("hello") && !stderr.contains("Servers started"), "{stderr}");

    let default = logging_server("log('hello') return 'accept'", None, "");
    assert_eq!(default.access_request(), radius::ACCESS_ACCEPT);
    default.wait_for_log("hello");
}

#[test]
fn test_debug_level_shows_tacacs_connections() {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let tacacs = free_port();
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[log]\nlevel = \"debug\"\n[tacacs]\nport = {tacacs}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n")).unwrap();
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let stream = std::net::TcpStream::connect(("127.0.0.1", tacacs)).unwrap();
    server.wait_for_log("New TACACS+ connection");
    drop(stream);
}

#[test]
fn test_other_libraries_are_quiet_at_debug() {
    let server = logging_server("return 'accept'", None, "[log]\nlevel = \"debug\"\nformat = \"json\"");
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    for line in json_lines(&server.wait_for_log("rradius_auth")) {
        let target = line["target"].as_str().unwrap();
        if !target.starts_with("rradius") {
            assert!(matches!(line["level"].as_str(), Some("WARN" | "ERROR")), "{line}");
        }
    }
}

#[test]
fn test_level_does_not_hide_records() {
    let server = logging_server("return 'reject'", None, "[log]\nlevel = \"error\"");
    assert_eq!(server.access_request(), radius::ACCESS_REJECT);
    let stderr = server.wait_for_log("rradius_auth");
    assert!(stderr.contains("decision=\"reject\""), "{stderr}");
}

#[test]
fn test_records_turned_off() {
    let mut server = logging_server("log('script-ran') return 'accept'", None, "[log]\nauth = false");
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    server.wait_for_log("script-ran");
    let stderr = server.stop_with("-TERM");
    assert!(!stderr.contains("rradius_auth"), "{stderr}");
}

#[test]
fn test_line_break_in_a_user_name() {
    for (settings, is_json) in [("", false), ("[log]\nformat = \"json\"", true)] {
        let server = logging_server("return 'accept'", None, settings);
        assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
        assert_eq!(server.ask_as(b"alice\nINFO forged", b"s"), Some(radius::ACCESS_ACCEPT));
        let stderr = server.wait_for_log("forged");
        assert!(!stderr.lines().any(|line| line.trim_start().starts_with("INFO forged")), "{stderr}");
        assert_eq!(stderr.lines().filter(|line| line.contains("forged")).count(), 1, "{stderr}");
        if is_json {
            let lines = json_lines(&stderr);
            assert!(lines.iter().any(|line| line["user"] == "alice\nINFO forged"), "{stderr}");
        }
    }
}

#[test]
fn test_script_cannot_forge_a_line() {
    let script = "log('a\\nINFO forged') error('b\\nINFO forged-error')";
    let server = logging_server(script, None, "");
    assert_eq!(server.access_request(), radius::ACCESS_REJECT);
    let stderr = server.wait_for_log("forged-error");
    assert!(!stderr.lines().any(|line| line.trim_start().starts_with("INFO forged")), "{stderr}");
}

#[test]
fn test_output_is_flushed_at_shutdown() {
    for signal in ["-TERM", "-INT"] {
        let mut server = logging_server("return 'accept'", None, "");
        assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
        let stderr = server.stop_with(signal);
        assert!(stderr.contains("user=\"alice\""), "{signal}: {stderr}");
    }
}

fn metrics_body(port: u16) -> String {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).unwrap();
    body
}

#[test]
fn test_dropped_lines_metric_present() {
    let port = free_port();
    let server = logging_server("return 'accept'", None, &format!("[metrics]\nport = {port}"));
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let body = metrics_body(port);
    assert!(body.contains("rradius_log_dropped_lines_total 0"), "{body}");
}

/// A server whose stderr is a pipe left to the test, with `/metrics` on the returned port
/// and scripts allowed to run long enough to log a lot.
fn piped_server(script: &str) -> (Server, u16) {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let (config, auth_port) = write_config(dir.path(), script, None);
    let text = std::fs::read_to_string(&config).unwrap().replace("script = \"top.lua\"", "script = \"top.lua\"\nmax_execution_ms = 120000");
    std::fs::write(&config, format!("{text}[metrics]\nport = {port}\n")).unwrap();
    let stdout = std::fs::File::create(dir.path().join("stdout.log")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_rradius-server"))
        .args(["server", "--config", config.to_str().unwrap()])
        .current_dir(dir.path())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    (Server { child, dir, config, auth_port }, port)
}

fn dropped_lines(port: u16) -> u64 {
    let body = metrics_body(port);
    let line = body.lines().find(|line| line.starts_with("rradius_log_dropped_lines_total ")).unwrap_or_else(|| panic!("{body}"));
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

/// An Access-Request that waits for its answer.
trait AskingSlowly {
    fn ask_slowly(&self, id: u8, name: &[u8]) -> u8;
}

impl AskingSlowly for Server {
    /// One Access-Request for `name`, given up to a minute for a script that logs a lot.
    fn ask_slowly(&self, id: u8, name: &[u8]) -> u8 {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        let packet = RadiusPacket::create_access_request(id, [id; 16], vec![(1, name.to_vec())], b"s");
        socket.send_to(&packet, ("127.0.0.1", self.auth_port)).unwrap();
        let mut buf = [0u8; 4096];
        let (size, _) = socket.recv_from(&mut buf).expect("no answer");
        RadiusPacket::parse(&buf[..size]).unwrap().code
    }
}

const NOISY: &str = "if get_attr('User-Name') == 'noisy' then for i = 1, 200000 do log('x') end end return 'accept'";

/// Nobody reads the pipe, so the background thread gets stuck on a write and its queue
/// fills. Requests go on, the lines are counted as dropped, and the server still stops,
/// without complaining on stdout.
#[test]
fn test_blocked_output_does_not_delay_requests() {
    let (mut server, port) = piped_server(NOISY);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    assert_eq!(server.ask_slowly(200, b"noisy"), radius::ACCESS_ACCEPT);
    assert_eq!(server.ask(b"s"), Some(radius::ACCESS_ACCEPT), "a request after the output blocked");
    assert!(dropped_lines(port) > 0);

    let status = Command::new("kill").args(["-TERM", &server.child.id().to_string()]).status().unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(30);
    while server.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(server.dir.path().join("stdout.log")).unwrap(), "");
}

/// What the reader of a broken pipe or a full disk would never see has to show in the metric.
#[test]
fn test_failing_output_is_counted() {
    let (mut server, port) = piped_server("return 'accept'");
    drop(server.child.stderr.take());
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let deadline = Instant::now() + Duration::from_secs(10);
    while dropped_lines(port) == 0 {
        assert!(Instant::now() < deadline, "writes that failed were not counted");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A reader that only comes back after the server was told to stop still gets everything.
#[test]
fn test_backlog_is_written_at_shutdown() {
    let (mut server, _) = piped_server("if get_attr('User-Name') == 'noisy' then for i = 1, 20000 do log('x') end end return 'accept'");
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    assert_eq!(server.ask_slowly(201, b"noisy"), radius::ACCESS_ACCEPT);
    assert_eq!(server.ask_slowly(202, b"last-user"), radius::ACCESS_ACCEPT);
    let mut reader = server.child.stderr.take().unwrap();
    let status = Command::new("kill").args(["-TERM", &server.child.id().to_string()]).status().unwrap();
    assert!(status.success());
    std::thread::sleep(Duration::from_secs(4));
    let mut stderr = String::new();
    reader.read_to_string(&mut stderr).unwrap();
    assert!(stderr.contains("user=\"last-user\""), "{} lines: {}", stderr.lines().count(), stderr.lines().last().unwrap_or(""));
}

#[test]
fn test_fatal_error_after_logging_began() {
    let (code, stderr) = exit_with_migrations(&[("1_broken.sql", "THIS IS NOT SQL;")]);
    assert_eq!(code, Some(2), "{stderr}");
    assert_eq!(stderr.lines().last().map(|line| line.starts_with("migration file '1_broken.sql'")), Some(true), "{stderr}");
}

/// User-Password per RFC 2865 section 5.2.
fn encrypted_password(password: &[u8], authenticator: &[u8; 16], secret: &[u8]) -> Vec<u8> {
    use md5::Digest;
    let mut padded = password.to_vec();
    padded.resize(password.len().div_ceil(16).max(1) * 16, 0);
    let mut out: Vec<u8> = Vec::with_capacity(padded.len());
    for block in padded.chunks(16) {
        let previous = if out.is_empty() { &authenticator[..] } else { &out[out.len() - 16..] };
        let pad = Md5::new().chain_update(secret).chain_update(previous).finalize();
        let cipher: Vec<u8> = block.iter().zip(pad.iter()).map(|(a, b)| a ^ b).collect();
        out.extend(cipher);
    }
    out
}

/// One TACACS+ request and its reply, whose first body octet is the status.
fn tacacs_ask(stream: &mut std::net::TcpStream, session_id: u32, seq_num: u8, body: &[u8]) -> u8 {
    use rradius_core::tacacs::TacacsPacket;
    let packet = TacacsPacket { version: 0xc0, type_: 1, seq_num, flags: 0, session_id, body: TacacsPacket::encrypt_decrypt(body, b"s", session_id, 0xc0, seq_num) };
    stream.write_all(&packet.encode()).unwrap();
    let mut header = [0u8; 12];
    stream.read_exact(&mut header).unwrap();
    let mut reply = vec![0u8; u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize];
    stream.read_exact(&mut reply).unwrap();
    TacacsPacket::encrypt_decrypt(&reply, b"s", session_id, 0xc0, header[2])[0]
}

fn tacacs_stream(port: u16) -> std::net::TcpStream {
    let stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream
}

/// The status of a PAP login, whose password travels in the START's `data`.
fn tacacs_pap_login(port: u16, user: &[u8], password: &[u8], session_id: u32) -> u8 {
    let mut body = vec![1u8, 1, 2, 1, user.len() as u8, 0, 0, password.len() as u8];
    body.extend_from_slice(user);
    body.extend_from_slice(password);
    tacacs_ask(&mut tacacs_stream(port), session_id, 1, &body)
}

/// The status of an ASCII login, whose password travels in a CONTINUE.
fn tacacs_ascii_login(port: u16, user: &[u8], password: &[u8], session_id: u32) -> u8 {
    let mut stream = tacacs_stream(port);
    let mut start = vec![1u8, 1, 1, 1, user.len() as u8, 0, 0, 0];
    start.extend_from_slice(user);
    assert_eq!(tacacs_ask(&mut stream, session_id, 1, &start), 5, "asks for the password");
    let mut answer = (password.len() as u16).to_be_bytes().to_vec();
    answer.extend_from_slice(&[0, 0, 0]);
    answer.extend_from_slice(password);
    tacacs_ask(&mut stream, session_id, 3, &answer)
}

const PASS: u8 = 1;
const FAIL: u8 = 2;

/// A marker in every secret, through real processes, every kind of login and both ways of
/// deciding: a script, and a SQL handler, for which the server decrypts and compares.
#[test]
fn test_passwords_are_not_logged() {
    use md5::Digest;
    const MARKER: &str = "pw-marker-7f3a";
    let password = format!("{MARKER}-secret");
    let script = "local name = get_attr('User-Name') or get_attr('user') or '' return name:sub(-3) == '-no' and 'reject' or 'accept'";
    let accepted = ["pap-yes", "chap-yes", "tac-yes", "ascii-yes"];
    let rejected = ["pap-no", "chap-no", "tac-no", "ascii-no"];
    let rows: String = accepted.iter().map(|name| format!("INSERT INTO users VALUES ('{name}', '{{clear}}{password}');\n")).collect();
    let schema = format!("CREATE TABLE users (username TEXT, password TEXT);\n{rows}INSERT INTO users VALUES ('pap-no', '{{clear}}other');\nINSERT INTO users VALUES ('chap-no', '{{clear}}other');\nINSERT INTO users VALUES ('mschap', '{{nthash}}00112233445566778899aabbccddeeff');\nINSERT INTO users VALUES ('tac-no', '{{clear}}other');\nINSERT INTO users VALUES ('ascii-no', '{{clear}}other');\n");
    for (format, chain) in [("text", false), ("json", false), ("text", true), ("json", true)] {
        let dir = dir_with_migrations(&[("1_users.sql", &schema)]);
        let (config, auth_port) = write_config_with(dir.path(), script, None, WITH_MIGRATIONS);
        let tacacs = free_port();
        let text = std::fs::read_to_string(&config).unwrap();
        let handlers = if chain { "[auth]\n[[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users WHERE username = ?\"\n" } else { "" };
        std::fs::write(&config, format!("{text}[log]\nlevel = \"debug\"\nformat = \"{format}\"\n[tacacs]\nport = {tacacs}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{handlers}")).unwrap();
        let child = Server::spawn(PROGRAM, dir.path(), &config);
        let mut server = Server { child, dir, config, auth_port };
        server.access_request();

        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let ask = |id: u8, name: &[u8], attributes: Vec<(u8, Vec<u8>)>, authenticator: [u8; 16]| {
            let mut all = vec![(1u8, name.to_vec())];
            all.extend(attributes);
            let packet = RadiusPacket::create_access_request(id, authenticator, all, b"s");
            socket.send_to(&packet, ("127.0.0.1", auth_port)).unwrap();
            let mut buf = [0u8; 4096];
            let (size, _) = socket.recv_from(&mut buf).unwrap();
            RadiusPacket::parse(&buf[..size]).unwrap().code
        };
        let pap = |authenticator: [u8; 16]| vec![(radius::USER_PASSWORD, encrypted_password(password.as_bytes(), &authenticator, b"s"))];
        assert_eq!(ask(90, b"pap-yes", pap([33; 16]), [33; 16]), radius::ACCESS_ACCEPT, "{format} {chain}");
        assert_eq!(ask(91, b"pap-no", pap([34; 16]), [34; 16]), radius::ACCESS_REJECT, "{format} {chain}");

        let chap = |authenticator: [u8; 16]| {
            let mut value = vec![7u8];
            value.extend_from_slice(&Md5::new().chain_update([7u8]).chain_update(password.as_bytes()).chain_update(authenticator).finalize());
            vec![(radius::CHAP_PASSWORD, value)]
        };
        assert_eq!(ask(92, b"chap-yes", chap([35; 16]), [35; 16]), radius::ACCESS_ACCEPT, "{format} {chain}");
        // A response holding the marker itself, the one thing a log could show of it.
        let mut marked = vec![1u8];
        marked.extend_from_slice(MARKER.as_bytes());
        marked.extend_from_slice(b"xx");
        assert_eq!(ask(93, b"chap-no", vec![(radius::CHAP_PASSWORD, marked)], [36; 16]), radius::ACCESS_REJECT, "{format} {chain}");

        // MS-CHAPv2 with the marker in its challenge and response.
        let microsoft = |kind: u8, value: &[u8]| {
            let mut attribute = 311u32.to_be_bytes().to_vec();
            attribute.extend_from_slice(&[kind, value.len() as u8 + 2]);
            attribute.extend_from_slice(value);
            (radius::VENDOR_SPECIFIC, attribute)
        };
        let mut challenge = MARKER.as_bytes().to_vec();
        challenge.resize(16, b'x');
        let mut response = vec![1u8, 0];
        response.extend_from_slice(&challenge);
        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&challenge);
        response.extend_from_slice(&[b'y'; 8]);
        ask(94, b"mschap", vec![microsoft(11, &challenge), microsoft(25, &response)], [37; 16]);

        assert_eq!(tacacs_pap_login(tacacs, b"tac-yes", password.as_bytes(), 0x11), PASS, "{format} {chain}");
        assert_eq!(tacacs_pap_login(tacacs, b"tac-no", password.as_bytes(), 0x12), FAIL, "{format} {chain}");
        assert_eq!(tacacs_ascii_login(tacacs, b"ascii-yes", password.as_bytes(), 0x13), PASS, "{format} {chain}");
        assert_eq!(tacacs_ascii_login(tacacs, b"ascii-no", password.as_bytes(), 0x14), FAIL, "{format} {chain}");

        server.wait_for_log("ascii-no");
        let stderr = server.stop_with("-TERM");
        for user in accepted.iter().chain(&rejected).chain(&["mschap"]) {
            assert!(stderr.contains(user), "{user}: {stderr}");
        }
        assert!(!stderr.contains(MARKER), "{stderr}");
    }
}

#[test]
fn test_check_passes_without_touching_anything() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", Some("return 'accept'"));
    let (code, stdout, stderr) = check_command(PROGRAM, &config);
    assert_eq!((code, stdout.as_str()), (Some(0), "configuration ok"), "{stderr}");
    assert!(!dir.path().join("data").exists(), "the data directory was created");
}

/// A valid LDAP handler passes without its server hearing anything: a listener on the
/// handler's URL still has nothing to accept afterwards.
#[test]
fn test_check_does_not_contact_ldap() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[auth]\n[[auth.handlers]]\ntype = \"ldap\"\nurls = [\"ldaps://127.0.0.1:{port}\"]\nbase_dn = \"dc=example,dc=com\"\nbind_dn = \"cn=svc\"\nbind_password = \"pw\"\nfilter = \"(uid={{user}})\"\n")).unwrap();
    let (code, stdout, stderr) = check_command(PROGRAM, &config);
    assert_eq!((code, stdout.as_str()), (Some(0), "configuration ok"), "{stderr}");
    assert_eq!(listener.accept().map(|_| ()).unwrap_err().kind(), std::io::ErrorKind::WouldBlock, "the check connected");
}

#[test]
fn test_check_names_a_bad_eap_setting() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}\n[radius.eap]\nmethods = []\n")).unwrap();
    let (code, stdout, stderr) = check_command(PROGRAM, &config);
    assert_ne!(code, Some(0), "{stdout}");
    assert!(stderr.contains("radius.eap.methods"), "{stderr}");
}

#[test]
fn test_check_finds_problems() {
    let edit = |config: &Path, change: &dyn Fn(String) -> String| {
        let text = std::fs::read_to_string(config).unwrap();
        std::fs::write(config, change(text)).unwrap();
    };
    let expect = |config: &Path, expected: &str| {
        let (code, stdout, stderr) = check_command(PROGRAM, config);
        assert_eq!(code, Some(2), "{expected}: {stdout} {stderr}");
        assert!(stderr.contains(expected), "{expected}: {stderr}");
        assert!(stdout.is_empty(), "{expected}: {stdout}");
    };

    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    edit(&config, &|text| text.replace("script = \"top.lua\"", "script = \"top.lua\"\ncolour = \"red\""));
    expect(&config, "colour");

    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    edit(&config, &|text| text.lines().map(|line| if line.starts_with("dictionary") { "dictionary = \"missing\"".to_string() } else { line.to_string() }).collect::<Vec<_>>().join("\n") + "\n");
    expect(&config, "dictionary");

    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept", None);
    expect(&config, "top.lua");

    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", Some("return 'accept"));
    expect(&config, "handler.lua");

    let dir = dir_with_migrations(&[("users.sql", "CREATE TABLE users (id INTEGER);")]);
    let (config, _) = write_config_with(dir.path(), "return 'accept'", None, WITH_MIGRATIONS);
    expect(&config, "users.sql");

    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    edit(&config, &|text| format!("{text}[auth]\n[[auth.handlers]]\ntype = \"ldap\"\nurls = [\"ldaps://dc1.example.com\"]\nbase_dn = \"dc=example,dc=com\"\nbind_dn = \"cn=svc\"\nbind_password = \"pw\"\nfilter = \"(uid={{user}})\"\nca_file = \"missing.pem\"\n"));
    expect(&config, "ca_file");

    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    edit(&config, &|text| format!("{text}[auth]\n[[auth.handlers]]\ntype = \"ldap\"\nurls = [\"ldaps://dc1.example.com\"]\nbase_dn = \"dc=example,dc=com\"\nbind_dn = \"cn=svc\"\nbind_password = \"pw\"\nfilter = \"(uid={{user}})\"\n[[auth.handlers.group_reply]]\ngroup = \"cn=staff\"\nattributes = {{ \"Filter_Id\" = \"staff\" }}\n"));
    expect(&config, "Filter_Id");
}

#[test]
fn test_script_that_does_not_compile_leaves_a_clean_node() {
    for (script, handler, named) in [("return 'accept", None, "top.lua"), ("return 'accept'", Some("if then"), "handler.lua")] {
        let dir = tempfile::tempdir().unwrap();
        let (config, _) = write_config(dir.path(), script, handler);
        let (code, stderr, locks) = how_it_ended(PROGRAM, dir.path(), &config);
        assert_eq!(code, Some(2), "{stderr}");
        assert!(stderr.contains(named), "{stderr}");
        assert!(locks.is_empty(), "not a clean shutdown: {locks:?}");
    }
}

#[test]
fn test_unusable_spool_leaves_a_clean_node() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    std::fs::write(dir.path().join("taken"), "").unwrap();
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}\n[radius.accounting_spool]\ndir = \"{}\"\n", dir.path().join("taken").display())).unwrap();
    let (code, stderr, locks) = how_it_ended(PROGRAM, dir.path(), &config);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("radius.accounting_spool.dir"), "{stderr}");
    assert!(locks.is_empty(), "not a clean shutdown: {locks:?}");
}

/// What `check` says of this `[radius.accounting_spool] dir`.
fn check_spool(dir: &Path, spool: &Path) -> (Option<i32>, String) {
    let (config, _) = write_config(dir, "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}\n[radius.accounting_spool]\ndir = \"{}\"\n", spool.display())).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rradius-server")).args(["check", "--config", config.to_str().unwrap()]).output().unwrap();
    (output.status.code(), String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn test_check_spool_that_cannot_be_used() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("taken"), "").unwrap();
    let (code, stderr) = check_spool(dir.path(), &dir.path().join("taken"));
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("radius.accounting_spool.dir"), "{stderr}");
    let (code, stderr) = check_spool(dir.path(), &dir.path().join("taken/under"));
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("radius.accounting_spool.dir"), "{stderr}");
    // A link to nothing: the server could not make the directory.
    std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("dangling")).unwrap();
    let (code, stderr) = check_spool(dir.path(), &dir.path().join("dangling"));
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("radius.accounting_spool.dir"), "{stderr}");
    let closed = dir.path().join("closed");
    std::fs::create_dir(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Root writes anywhere, so there the mode says nothing.
    if std::fs::write(closed.join("probe"), "").is_err() {
        for spool in [closed.clone(), closed.join("under/more")] {
            let (code, stderr) = check_spool(dir.path(), &spool);
            assert_eq!(code, Some(2), "{stderr}");
            assert!(stderr.contains("radius.accounting_spool.dir"), "{stderr}");
        }
    }
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Written to but not listed: the server has to read what is waiting there.
    let blind = dir.path().join("blind");
    std::fs::create_dir(&blind).unwrap();
    std::fs::set_permissions(&blind, std::fs::Permissions::from_mode(0o300)).unwrap();
    if std::fs::read_dir(&blind).is_err() {
        let (code, stderr) = check_spool(dir.path(), &blind);
        assert_eq!(code, Some(2), "{stderr}");
        assert!(stderr.contains("radius.accounting_spool.dir"), "{stderr}");
    }
    std::fs::set_permissions(&blind, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn test_check_spool_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not/yet");
    let (code, stderr) = check_spool(dir.path(), &missing);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(!dir.path().join("not").exists(), "made by a check");
    let spool = dir.path().join("spool");
    std::fs::create_dir(&spool).unwrap();
    let segment = spool.join("segment-00000000000000000001.log");
    std::fs::write(&segment, [0, 0, 1, 0, 9, 9, 9]).unwrap();
    let (code, stderr) = check_spool(dir.path(), &spool);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(std::fs::read(&segment).unwrap(), [0, 0, 1, 0, 9, 9, 9]);
    assert_eq!(std::fs::read_dir(&spool).unwrap().count(), 1, "something left behind");
}

#[test]
fn test_stopping_during_startup() {
    for signal in ["-TERM", "-INT"] {
        let dir = dir_with_migrations(&[("1_users.sql", "CREATE TABLE users (id INTEGER);")]);
        let config = lone_node_config(dir.path());
        let text = std::fs::read_to_string(&config).unwrap().replace("migrations_wait_ms = 2000", "migrations_wait_ms = 60000");
        std::fs::write(&config, text).unwrap();
        let mut child = Server::spawn(PROGRAM, dir.path(), &config);
        std::thread::sleep(Duration::from_secs(2));
        assert!(child.try_wait().unwrap().is_none(), "it should still be waiting");
        assert!(Command::new("kill").args([signal, &child.id().to_string()]).status().unwrap().success());
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{signal}: still running");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let stderr = std::fs::read_to_string(dir.path().join("stderr.log")).unwrap();
        assert_eq!(status.code(), Some(0), "{signal}: {stderr}");
    }
}

/// Returns once the log says this node of the cluster has started.
fn wait_for_node_started(dir: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_to_string(dir.join("stderr.log")).unwrap_or_default().contains("cluster node 1 started") {
        assert!(child.try_wait().unwrap().is_none(), "it ended");
        assert!(Instant::now() < deadline, "the node did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts a server applying `sql` and sends it SIGTERM as soon as its node has started.
/// Returns how it ended, what it logged, how long it took to stop, and its directory and configuration.
fn stopped_while_applying(sql: &str) -> (Option<i32>, String, Duration, tempfile::TempDir, PathBuf, u16) {
    let dir = dir_with_migrations(&[("1_big.sql", sql)]);
    let (config, auth_port) = write_config_with(dir.path(), "return #sql_query('SELECT id FROM _migrations') == 1 and 'accept' or 'reject'", None, WITH_MIGRATIONS);
    let mut child = Server::spawn(PROGRAM, dir.path(), &config);
    wait_for_node_started(dir.path(), &mut child);
    assert!(Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap().success());
    let signalled = Instant::now();
    let deadline = signalled + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("still running");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (status.code(), std::fs::read_to_string(dir.path().join("stderr.log")).unwrap(), signalled.elapsed(), dir, config, auth_port)
}

fn big_table(rows: u64) -> String {
    format!("CREATE TABLE big AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT {rows}) SELECT x FROM c;")
}

#[test]
fn test_stopping_while_a_migration_runs_shuts_the_node_down() {
    let (code, stderr, _, dir, config, _) = stopped_while_applying(&big_table(5_000_000));
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stderr.contains("stopped during startup") && !stderr.contains("Servers started"), "{stderr}");
    assert!(lock_files(dir.path()).is_empty(), "not a clean shutdown: {:?}", lock_files(dir.path()));
    // And the next start goes on.
    let mut again = Server::spawn(PROGRAM, dir.path(), &config);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut served = false;
    while Instant::now() < deadline && again.try_wait().unwrap().is_none() {
        served = std::fs::read_to_string(dir.path().join("stderr.log")).unwrap().contains("Servers started");
        if served {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = again.kill();
    let _ = again.wait();
    assert!(served, "the next start did not serve");
}

#[test]
fn test_stopping_during_a_migration_that_outlasts_the_wait() {
    // About 15 seconds when this was written, within the 25 a stop waits for a clean shutdown.
    let (code, stderr, elapsed, dir, config, auth_port) = stopped_while_applying(&big_table(40_000_000));
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stderr.contains("stopped during startup"), "{stderr}");
    assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    // The next start finishes the file, and has it on record: the script looks for that.
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
}

/// With no file descriptors left the port stays open, and takes connections again when there are.
#[test]
fn test_out_of_file_descriptors() {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let (tacacs_port, metrics_port) = (free_port(), free_port());
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[metrics]\nport = {metrics_port}\n[tacacs]\nport = {tacacs_port}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n")).unwrap();
    let child = Command::new("sh")
        .args(["-c", "ulimit -n 128 && exec \"$0\" server --config \"$1\"", env!("CARGO_BIN_EXE_rradius-server"), config.to_str().unwrap()])
        .current_dir(dir.path())
        .stdout(std::fs::File::create(dir.path().join("stdout.log")).unwrap())
        .stderr(std::fs::File::create(dir.path().join("stderr.log")).unwrap())
        .spawn()
        .unwrap();
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "{}", server.stderr());
    const FAILED: &str = "TACACS+: accepting a connection failed";
    let mut held = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !server.stderr().contains(FAILED) {
        assert!(Instant::now() < deadline && held.len() < 150, "no failure after {} connections: {}", held.len(), server.stderr());
        for _ in 0..5 {
            held.push(std::net::TcpStream::connect(("127.0.0.1", tacacs_port)).unwrap());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(held);
    // What waited in the backlog is taken and closed first.
    let accepted = "rradius_tacacs_connections_total{result=\"accepted\"}";
    std::thread::sleep(Duration::from_millis(500));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut before = scrape(metrics_port, accepted);
    let _late = loop {
        let socket = std::net::TcpStream::connect(("127.0.0.1", tacacs_port)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let now = scrape(metrics_port, accepted);
        if now > before {
            break socket;
        }
        before = now;
        assert!(Instant::now() < deadline, "the port stopped taking connections: {}", server.stderr());
    };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "the server goes on");
}

/// With one socket a port is not shared: a socket already on it stops the start, even one that
/// would share it.
#[test]
fn test_address_in_use_with_one_socket() {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap().replace("[radius]\n", "[radius]\nsockets = 1\n");
    std::fs::write(&config, text).unwrap();
    let taken = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)).unwrap();
    taken.set_reuse_port(true).unwrap();
    taken.bind(&std::net::SocketAddr::from(([0, 0, 0, 0], auth_port)).into()).unwrap();
    let (code, stderr, _) = how_it_ended(PROGRAM, dir.path(), &config);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains(&format!("cannot listen on 0.0.0.0:{auth_port}")), "{stderr}");
}

#[test]
fn test_address_in_use_ends_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let _taken = std::net::UdpSocket::bind(("0.0.0.0", auth_port)).unwrap();
    let (code, stderr, locks) = how_it_ended(PROGRAM, dir.path(), &config);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains(&format!("cannot listen on 0.0.0.0:{auth_port}")), "{stderr}");
    assert_eq!(stderr.matches("cannot listen on").count(), 1, "once: {stderr}");
    assert!(locks.is_empty(), "not a clean shutdown: {locks:?}");
}

#[test]
fn test_address_in_use_in_a_list() {
    let Ok(probe) = std::net::UdpSocket::bind("[::1]:0") else { return };
    drop(probe);
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap().replacen("[radius]\n", "[radius]\nbind = [\"127.0.0.1\", \"::1\"]\n", 1);
    std::fs::write(&config, text).unwrap();
    let _taken = std::net::UdpSocket::bind(("::1", auth_port)).unwrap();
    let (code, stderr, locks) = how_it_ended(PROGRAM, dir.path(), &config);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains(&format!("cannot listen on [::1]:{auth_port}")), "{stderr}");
    assert!(locks.is_empty(), "not a clean shutdown: {locks:?}");
}

#[test]
fn test_check_takes_a_group_reply_of_several_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap();
    let value = "a".repeat(300);
    std::fs::write(&config, format!("{text}[auth]\n[[auth.handlers]]\ntype = \"ldap\"\nurls = [\"ldaps://dc1.example.com\"]\nbase_dn = \"dc=example,dc=com\"\nbind_dn = \"cn=svc\"\nbind_password = \"pw\"\nfilter = \"(uid={{user}})\"\n[[auth.handlers.group_reply]]\ngroup = \"cn=staff\"\nattributes = {{ \"FreeRADIUS-802.1X-EAPoL-Key-Msg\" = \"{value}\" }}\n")).unwrap();
    let (code, _, stderr) = check_command(PROGRAM, &config);
    assert_eq!(code, Some(0), "{stderr}");
}

/// A server with one client whose secret is `secret`.
fn server_with_secret(secret: &str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap().replace("secret = \"s\"", &format!("name = \"nas1\"\nsecret = \"{secret}\""));
    std::fs::write(&config, text).unwrap();
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    Server { child, dir, config, auth_port }
}

#[test]
fn test_short_secret_warning_at_startup() {
    assert_eq!(SHORT_SECRET.len(), 12);
    let server = server_with_secret(SHORT_SECRET);
    let stderr = server.wait_for_log("Servers started");
    let warning = stderr.lines().find(|line| line.contains("shared secret")).unwrap_or_else(|| panic!("{stderr}"));
    assert!(warning.contains("WARN") && warning.contains("nas1") && warning.contains("shorter than 16 octets"), "{warning}");
    assert!(!stderr.contains(SHORT_SECRET), "{stderr}");
}

#[test]
fn test_no_warning_for_a_long_enough_secret() {
    let server = server_with_secret("sixteen-octets-1");
    let stderr = server.wait_for_log("Servers started");
    assert!(!stderr.contains("shared secret"), "{stderr}");
}

/// RFC 8907 and RFC 2865 ask for at least 16: one octet less is warned about.
#[test]
fn test_warning_just_short_of_sixteen_octets() {
    let server = server_with_secret("fifteen-octets!");
    let stderr = server.wait_for_log("shorter than 16 octets");
    assert!(stderr.contains("nas1") && !stderr.contains("fifteen-octets!"), "{stderr}");
}

#[test]
fn test_short_tacacs_secret_warning() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap();
    let tacacs = free_port();
    std::fs::write(&config, format!("{text}[tacacs]\nport = {tacacs}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nname = \"router1\"\nsecret = \"{SHORT_SECRET}\"\n")).unwrap();
    let (code, _, stderr) = check_command(PROGRAM, &config);
    assert_eq!(code, Some(0), "{stderr}");
    let warning = stderr.lines().find(|line| line.contains("shared secret") && line.contains("TACACS+")).unwrap_or_else(|| panic!("{stderr}"));
    assert!(warning.contains("WARN") && warning.contains("router1"), "{warning}");
    assert!(!stderr.contains(SHORT_SECRET), "{stderr}");
}

#[test]
fn test_short_secret_warning_on_reload() {
    // Long enough at startup, so that only the reload can write the warning.
    let server = server_with_secret("sixteen-octets-1");
    let stderr = server.wait_for_log("Servers started");
    assert!(!stderr.contains("shared secret"), "{stderr}");
    server.write_config_text(|text| text.replace("sixteen-octets-1", SHORT_SECRET));
    server.reload(RELOADED);
    server.wait_for_log("shorter than 16 octets");
}

#[test]
fn test_short_secret_warning_in_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap().replace("secret = \"s\"", &format!("name = \"nas1\"\nsecret = \"{SHORT_SECRET}\""));
    std::fs::write(&config, text).unwrap();
    let (code, stdout, stderr) = check_command(PROGRAM, &config);
    assert_eq!((code, stdout.as_str()), (Some(0), "configuration ok"), "{stderr}");
    assert!(stderr.contains("shared secret") && stderr.contains("nas1"), "{stderr}");
    assert!(!stderr.contains(SHORT_SECRET), "{stderr}");
}

/// Allowed hosts apply on SIGHUP, with no restart: a host removed gets no request.
#[test]
fn test_reload_script_http_hosts() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let script = format!("local r = http_request{{url = 'http://{address}/'}} return r and 'accept' or 'reject'");
    let (config, auth_port) = write_config(dir.path(), &script, None);
    let text = std::fs::read_to_string(&config).unwrap().replace("script = \"top.lua\"", &format!("script = \"top.lua\"\n[lua.http]\nallowed_hosts = [\"http://{address}\"]"));
    std::fs::write(&config, text).unwrap();
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);

    server.write_config_text(|text| text.replace(&address, "other.example:1"));
    let stderr = server.reload(RELOADED);
    assert!(!stderr.to_lowercase().contains("need a restart"), "{stderr}");
    assert_eq!(server.ask(b"s"), Some(radius::ACCESS_REJECT));
}

/// A request taken during an outage comes back after the process is killed and started again,
/// with a script that works. A slow query with a tiny limit stands in for the outage.
#[test]
fn test_spool_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let outage = "sql_query([[WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 3000000) SELECT count(*) AS n FROM c]]) return 'accept'";
    let (config, auth_port) = write_config(dir.path(), outage, None);
    let acct_port: u16 = std::fs::read_to_string(&config).unwrap().lines().find_map(|line| line.strip_prefix("acct_port = ")?.parse().ok()).unwrap();
    let text = std::fs::read_to_string(&config).unwrap()
        .replace("script = \"top.lua\"", "script = \"top.lua\"\nquery_timeout_ms = 1")
        .replace("[[radius.clients]]", "[radius.accounting_spool]\ndir = \"spool\"\nreplay_interval_ms = 50\n[[radius.clients]]");
    std::fs::write(&config, text).unwrap();
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let mut server = Server { child, dir, config, auth_port };
    server.wait_for_log("Servers started");

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let packet = RadiusPacket::create_accounting_request(9, vec![(40, vec![0, 0, 0, 1]), (44, b"spooled-session".to_vec())], b"s");
    socket.send_to(&packet, ("127.0.0.1", acct_port)).unwrap();
    let mut buf = [0u8; 4096];
    let (size, _) = socket.recv_from(&mut buf).expect("answered once on disk");
    assert_eq!(RadiusPacket::parse(&buf[..size]).unwrap().code, radius::ACCOUNTING_RESPONSE);
    let spool = server.dir.path().join("spool");
    let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&spool).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o700);

    // SIGKILL, so nothing gets to finish. The next start replays with a script that works.
    server.write_script("top.lua", "log('replayed session', get_attr('Acct-Session-Id'), replayed) return 'accept'");
    server.write_config_text(|text| text.replace("query_timeout_ms = 1\n", ""));
    server.restart(PROGRAM);
    server.wait_for_log("replayed session spooled-session true");
}

#[test]
fn test_spool_directory_that_cannot_be_used() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a-file"), "").unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).unwrap().replace("[[radius.clients]]", "[radius.accounting_spool]\ndir = \"a-file/spool\"\n[[radius.clients]]");
    std::fs::write(&config, text).unwrap();
    let (code, stderr) = run(&["server", "--config", config.to_str().unwrap()]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("accounting_spool.dir"), "{stderr}");
    assert!(!dir.path().join("spool").exists());
}

#[test]
fn test_check_covers_script_http() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let base = std::fs::read_to_string(&config).unwrap();
    let with_http = |extra: &str| {
        let text = base.replace("[lua]\n", &format!("[lua.http]\nallowed_hosts = [\"api.example.com\"]\n{extra}\n[lua]\n"));
        std::fs::write(&config, text).unwrap();
        run(&["check", "--config", config.to_str().unwrap()])
    };
    let (code, stderr) = with_http("ca_file = \"missing.pem\"");
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("lua.http.ca_file"), "{stderr}");
    let key = rcgen::KeyPair::generate().unwrap();
    let ca = rcgen::CertificateParams::new(vec!["ca.example.com".to_string()]).unwrap().self_signed(&key).unwrap();
    std::fs::write(dir.path().join("ca.pem"), ca.pem()).unwrap();
    let (code, stderr) = with_http("ca_file = \"ca.pem\"");
    assert_eq!(code, Some(0), "{stderr}");
    std::fs::write(dir.path().join("empty.pem"), "").unwrap();
    let (code, stderr) = with_http("ca_file = \"empty.pem\"");
    assert_ne!(code, Some(0), "no certificates in it: {stderr}");
}

/// A server whose default chain checks passwords in `users`, with `[accounts]` for the `user`
/// commands. Waits until it answers.
fn account_server() -> Server {
    let dir = dir_with_migrations(&[("1_users.sql", "CREATE TABLE users (username TEXT PRIMARY KEY, password TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1);")]);
    let (config, auth_port) = write_config_with(dir.path(), "return 'reject'", None, WITH_MIGRATIONS);
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(
        "[auth]\n[[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users WHERE username = ? AND enabled = 1\"\n\
         [auth.username]\nstrip_realm = \"suffix\"\nlowercase = true\nallowed = \"[a-z0-9]+\"\n\
         [accounts]\nscheme = \"sha512-crypt\"\n\
         add = \"INSERT INTO users (username, password) VALUES (?1, ?2)\"\n\
         passwd = \"UPDATE users SET password = ?2 WHERE username = ?1\"\n\
         enable = \"UPDATE users SET enabled = 1 WHERE username = ?1\"\n\
         disable = \"UPDATE users SET enabled = 0 WHERE username = ?1\"\n\
         delete = \"DELETE FROM users WHERE username = ?1\"\n\
         show = \"SELECT username, enabled FROM users WHERE username = ?1\"\n",
    );
    std::fs::write(&config, text).unwrap();
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_REJECT, "no account yet");
    server
}

/// Logins and the `user` command of the account server.
trait Accounts {
    fn pap(&self, name: &str, password: &str) -> u8;
    fn user(&self, args: &[&str], stdin: &str) -> (Option<i32>, String, String);
}

impl Accounts for Server {
    /// One PAP Access-Request; the code of the response.
    fn pap(&self, name: &str, password: &str) -> u8 {
        static SENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(100);
        let sent = SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let authenticator = [sent; 16];
        let mut padded = password.as_bytes().to_vec();
        padded.resize(padded.len().div_ceil(16).max(1) * 16, 0);
        let (mut hidden, mut previous) = (Vec::new(), authenticator.to_vec());
        for block in padded.chunks(16) {
            let mut hasher = <Md5 as md5::Digest>::new();
            md5::Digest::update(&mut hasher, b"s");
            md5::Digest::update(&mut hasher, &previous);
            let key = md5::Digest::finalize(hasher);
            previous = block.iter().zip(key.iter()).map(|(a, b)| a ^ b).collect();
            hidden.extend_from_slice(&previous);
        }
        let packet = RadiusPacket::create_access_request(sent, authenticator, vec![(1, name.as_bytes().to_vec()), (2, hidden)], b"s");
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        socket.send_to(&packet, ("127.0.0.1", self.auth_port)).unwrap();
        let mut buf = [0u8; 4096];
        let (size, _) = socket.recv_from(&mut buf).expect("no answer");
        RadiusPacket::parse(&buf[..size]).unwrap().code
    }

    /// `rradius-server user <args> --config <this server's>`, with `stdin` as its input.
    fn user(&self, args: &[&str], stdin: &str) -> (Option<i32>, String, String) {
        user_command(&self.config, args, stdin)
    }
}

fn user_command(config: &Path, args: &[&str], stdin: &str) -> (Option<i32>, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rradius-server"))
        .arg("user")
        .args(args)
        .args(["--config", config.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    (output.status.code(), String::from_utf8_lossy(&output.stdout).trim().to_string(), String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn test_user_commands_change_what_logins_see() {
    let server = account_server();
    let (code, stdout, stderr) = server.user(&["add", "Alice@Corp"], "s3cret\n");
    assert_eq!((code, stdout.as_str()), (Some(0), "added alice"), "{stderr}");
    assert!(!stderr.contains("s3cret"), "{stderr}");
    assert_eq!(server.pap("alice", "s3cret"), radius::ACCESS_ACCEPT);
    assert_eq!(server.pap("alice", "wrong"), radius::ACCESS_REJECT);

    let (code, stdout, _) = server.user(&["show", "alice"], "");
    assert_eq!((code, stdout.as_str()), (Some(0), "username=alice\nenabled=1"));
    assert_eq!(server.user(&["show", "nobody"], ""), (Some(1), "no account nobody".to_string(), String::new()));
    // The configuration may come before the name too.
    let output = Command::new(env!("CARGO_BIN_EXE_rradius-server")).args(["user", "show", "--config", server.config.to_str().unwrap(), "alice"]).output().unwrap();
    assert_eq!((output.status.code(), String::from_utf8_lossy(&output.stdout).trim()), (Some(0), "username=alice\nenabled=1"));

    assert_eq!(server.user(&["passwd", "alice"], "n3w\n").1, "password changed for alice");
    assert_eq!(server.pap("alice", "s3cret"), radius::ACCESS_REJECT);
    assert_eq!(server.pap("alice", "n3w"), radius::ACCESS_ACCEPT);

    assert_eq!(server.user(&["disable", "alice"], "").1, "disabled alice");
    assert_eq!(server.pap("alice", "n3w"), radius::ACCESS_REJECT);
    assert_eq!(server.user(&["enable", "alice"], "").1, "enabled alice");
    assert_eq!(server.pap("alice", "n3w"), radius::ACCESS_ACCEPT);

    let (code, stdout, stderr) = server.user(&["add", "alice"], "again\n");
    assert_eq!((code, stdout.as_str()), (Some(1), ""), "an existing account: {stderr}");
    assert!(!stderr.contains("again") && !stderr.contains("$6$"), "{stderr}");
    assert!(stderr.contains("UNIQUE"), "the database's reason: {stderr}");
    assert_eq!(server.pap("alice", "n3w"), radius::ACCESS_ACCEPT, "the old password still works");

    assert_eq!(server.user(&["delete", "alice"], "").1, "deleted alice");
    assert_eq!(server.pap("alice", "n3w"), radius::ACCESS_REJECT);
    for action in ["show", "passwd", "enable", "disable", "delete"] {
        let (code, stdout, stderr) = server.user(&[action, "alice"], "x\n");
        assert_eq!((code, stdout.as_str()), (Some(1), "no account alice"), "{action}: {stderr}");
    }
}

#[test]
fn test_user_command_mistakes_send_nothing() {
    let server = account_server();
    for (args, stdin, expected) in [
        (&["add", "bad name"][..], "s3cret\n", "allowed"),
        (&["add", "bob"][..], "\n", "empty"),
        (&["rename", "bob"][..], "", "usage"),
        (&["add"][..], "s3cret\n", "name"),
        (&["add", "bob", "extra"][..], "s3cret\n", "extra"),
    ] {
        let (code, stdout, stderr) = server.user(args, stdin);
        assert_eq!((code, stdout.as_str()), (Some(2), ""), "{args:?}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    assert_eq!(server.user(&["show", "bob"], "").0, Some(1), "nothing was added");

    server.write_config_text(|text| text.replace("delete = ", "# delete = "));
    let (code, _, stderr) = server.user(&["delete", "bob"], "");
    assert_eq!(code, Some(2));
    assert!(stderr.contains("accounts.delete"), "{stderr}");
}

#[test]
fn test_user_command_without_the_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'reject'", None);
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[accounts]\nshow = \"SELECT username FROM users WHERE username = ?1\"\n");
    std::fs::write(&config, text).unwrap();
    let (code, stdout, stderr) = user_command(&config, &["show", "alice"], "");
    assert_eq!((code, stdout.as_str()), (Some(3), ""), "{stderr}");
    assert!(!dir.path().join("data").exists(), "the data directory was created");
}

#[test]
fn test_user_commands_without_accounts_section() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'reject'", None);
    for action in ["add", "passwd", "enable", "disable", "delete", "show"] {
        let (code, stdout, stderr) = user_command(&config, &[action, "alice"], "s3cret\n");
        assert_eq!((code, stdout.as_str()), (Some(2), ""), "{action}");
        assert!(stderr.contains(&format!("accounts.{action}")), "{action}: {stderr}");
    }
}

/// Ctrl-C at the password prompt, where the echo is off: the terminal gets it back.
#[test]
fn test_interrupted_prompt_puts_the_echo_back() {
    use std::os::fd::FromRawFd;
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = write_config(dir.path(), "return 'reject'", None);
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[accounts]\nadd = \"INSERT INTO users (username, password) VALUES (?1, ?2)\"\n");
    std::fs::write(&config, text).unwrap();

    let (mut leader, mut follower) = (0, 0);
    // SAFETY: openpty writes two descriptors we own; the name and settings are not asked for.
    assert_eq!(unsafe { libc::openpty(&mut leader, &mut follower, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, 0);
    let echoes = || {
        // SAFETY: tcgetattr writes into storage we own.
        let mut settings: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(follower, &mut settings) }, 0);
        settings.c_lflag & libc::ECHO != 0
    };
    assert!(echoes());
    // SAFETY: a fresh duplicate, owned by the child's standard input from here on.
    let input = unsafe { Stdio::from_raw_fd(libc::dup(follower)) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_rradius-server"))
        .args(["user", "add", "alice", "--config", config.to_str().unwrap()])
        .stdin(input)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while echoes() {
        assert!(Instant::now() < deadline, "the prompt never turned the echo off");
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: a signal to our own child.
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) }, 0);
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGINT));
    assert!(echoes(), "the echo is still off");
    // SAFETY: both descriptors are ours and no longer used.
    unsafe {
        libc::close(follower);
        libc::close(leader);
    }
}

#[test]
fn test_version() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rradius-server")).arg("--version").current_dir(dir.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let expected = format!("rradius-server {}\ndefault config: {}\n", env!("CARGO_PKG_VERSION"), rradius_core::config::default_config_path());
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing was created");
    let (code, stderr) = run(&["--version", "extra"]);
    assert_eq!(code, Some(2), "{stderr}");
}

/// With `pin_threads`, each worker thread has one CPU of its own, and every other thread,
/// such as those running database queries, keeps all the CPUs the process had.
#[cfg(target_os = "linux")]
#[test]
fn test_pinned_worker_threads() {
    let cpus_of = |status: &str| -> Vec<usize> {
        let list = status.lines().find_map(|line| line.strip_prefix("Cpus_allowed_list:")).unwrap().trim().to_string();
        list.split(',').flat_map(|part| match part.split_once('-') {
            Some((a, b)) => (a.parse::<usize>().unwrap()..=b.parse().unwrap()).collect::<Vec<_>>(),
            None => vec![part.parse().unwrap()],
        }).collect()
    };
    let all = cpus_of(&std::fs::read_to_string("/proc/self/status").unwrap());
    if all.len() < 2 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A query slow enough that 32 at once need more threads than started with the server.
    let slow = "sql_query('WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 300000) SELECT count(*) FROM c')";
    let (config, _) = write_config(dir.path(), &format!("{slow} return 'accept'"), None);
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[runtime]\npin_threads = true\n");
    std::fs::write(&config, text).unwrap();
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let auth_port = std::fs::read_to_string(&config).unwrap().lines().find_map(|line| line.strip_prefix("auth_port = ").map(|port| port.parse().unwrap())).unwrap();
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    let threads_before = std::fs::read_dir(format!("/proc/{}/task", server.child.id())).unwrap().count();
    // Pinned by now: the threads these start come from pinned workers.
    let asks: Vec<_> = (0..32u8).map(|i| {
        let port = server.auth_port;
        std::thread::spawn(move || {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let packet = RadiusPacket::create_access_request(i, [i; 16], vec![(1, b"alice".to_vec())], b"s");
            socket.send_to(&packet, ("127.0.0.1", port)).unwrap();
            let mut buf = [0u8; 4096];
            socket.recv_from(&mut buf).is_ok()
        })
    }).collect();
    assert!(asks.into_iter().all(|ask| ask.join().unwrap()), "every request answered");
    let threads_after = std::fs::read_dir(format!("/proc/{}/task", server.child.id())).unwrap().count();
    assert!(threads_after > threads_before, "no thread started after pinning: {threads_before} then {threads_after}");
    // As many workers as the server counts CPUs, which a cgroup limit can make fewer than `all`.
    let stderr = server.stderr();
    let workers: usize = stderr.split("pinned ").nth(1).and_then(|rest| rest.split(' ').next()).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("not pinned: {stderr}"));
    let tasks = std::fs::read_dir(format!("/proc/{}/task", server.child.id())).unwrap();
    let mut pinned = Vec::new();
    for task in tasks {
        let status = std::fs::read_to_string(task.unwrap().path().join("status")).unwrap();
        let cpus = cpus_of(&status);
        if cpus.len() == 1 {
            pinned.push(cpus[0]);
        } else {
            assert_eq!(cpus, all, "a thread neither pinned nor free: {status}");
        }
    }
    let mut distinct = pinned.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(pinned.len(), workers, "one CPU for each worker, and none for any other thread");
    assert_eq!(distinct.len(), workers.min(all.len()), "each worker on a CPU of its own: {pinned:?}");
    assert!(pinned.iter().all(|cpu| all.contains(cpu)), "{pinned:?} not within {all:?}");
}

/// How many snapshot files the node keeps after 300 writes, one log entry each.
fn snapshots_after_writes(cluster: &str) -> usize {
    let dir = dir_with_migrations(&[("1_t.sql", "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT);")]);
    let server = Server::start_with(PROGRAM, dir, "sql_execute('INSERT INTO t DEFAULT VALUES') return 'accept'", None, &format!("{WITH_MIGRATIONS}{cluster}"));
    for _ in 0..300 {
        assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "{}", server.stderr());
    }
    let snapshots = server.dir.path().join("data/state_machine/snapshots");
    // A snapshot is built after the entry that calls for it, so give it a moment.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = std::fs::read_dir(&snapshots).map_or(0, |files| files.filter(|file| file.as_ref().is_ok_and(|file| !file.file_name().to_string_lossy().ends_with(".tmp"))).count());
        // By default, look again after a while, so a snapshot still being written is counted too.
        if count > 0 || Instant::now() > deadline || (cluster.is_empty() && Instant::now() + Duration::from_secs(8) > deadline) {
            return count;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn test_snapshot_every() {
    assert!(snapshots_after_writes("snapshot_every = 100\n") > 0, "no snapshot after 300 entries with snapshot_every = 100");
    assert_eq!(snapshots_after_writes(""), 0, "a snapshot within 300 entries by default");
}

/// How long four requests take that arrive together, each running one slow read. The limit for
/// a query is raised, so that waiting for a connection cannot end in a timeout.
fn four_slow_reads(read_connections: usize) -> Duration {
    let dir = tempfile::tempdir().unwrap();
    let slow = "sql_query('WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 400000) SELECT count(*) FROM c')";
    let (config, auth_port) = write_config_with(dir.path(), &format!("{slow} return 'accept'"), None, &format!("read_connections = {read_connections}\n"));
    let text = std::fs::read_to_string(&config).unwrap().replace("[lua]\n", "[lua]\nquery_timeout_ms = 120000\nmax_execution_ms = 120000\n");
    std::fs::write(&config, text).unwrap();
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT, "{}", server.stderr());
    let started = Instant::now();
    let asks: Vec<_> = (0..4u8).map(|i| {
        let port = server.auth_port;
        std::thread::spawn(move || {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
            let packet = RadiusPacket::create_access_request(10 + i, [10 + i; 16], vec![(1, b"alice".to_vec())], b"s");
            socket.send_to(&packet, ("127.0.0.1", port)).unwrap();
            let mut buf = [0u8; 4096];
            let (size, _) = socket.recv_from(&mut buf).expect("no answer");
            RadiusPacket::parse(&buf[..size]).unwrap().code
        })
    }).collect();
    for ask in asks {
        assert_eq!(ask.join().unwrap(), radius::ACCESS_ACCEPT, "a read failed: {}", server.stderr());
    }
    started.elapsed()
}

/// With one connection the four reads take turns; with two, two at a time. Two reads at once
/// beside the server's own threads and these four clients need more than two CPUs to finish
/// sooner: a runner with two showed no difference (607 ms against 558 ms), so the times are only
/// compared with four or more. Both settings have to answer every read either way.
///
/// The other tests in this binary run at the same time and take CPU from the reads, so one
/// comparison can come out short: a four-CPU runner once measured 478 ms against 340 ms (1.4x).
/// A setting that did not limit the reads comes out near 1.0x every time (406 ms against 427 ms,
/// 409 against 420, 424 against 405 with one connection on both sides), so one of three
/// comparisons reaching 1.3x passes.
#[test]
fn test_read_connections() {
    let cpus = std::thread::available_parallelism().map_or(1, |cpus| cpus.get());
    let mut measured = Vec::new();
    for _ in 0..3 {
        let one = four_slow_reads(1);
        let two = four_slow_reads(2);
        if cpus < 4 {
            eprintln!("test_read_connections: {cpus} CPUs, too few to compare one connection ({one:?}) with two ({two:?})");
            return;
        }
        if one.as_secs_f64() >= 1.3 * two.as_secs_f64() {
            return;
        }
        measured.push((one, two));
    }
    panic!("one connection against two, three times: {measured:?}: the reads did not wait for each other");
}

/// One TACACS+ Authorization REQUEST for `alice` with `args` on `stream`: the reply's status
/// and its AV pairs.
fn tacacs_authorize(stream: &mut std::net::TcpStream, session_id: u32, args: &[&[u8]]) -> std::io::Result<(u8, Vec<Vec<u8>>)> {
    use rradius_core::tacacs::{TacacsPacket, TYPE_AUTHOR};
    let user = b"alice";
    let mut body = vec![0x06, 0x01, 0x02, 0x01, user.len() as u8, 0, 0, args.len() as u8];
    body.extend(args.iter().map(|arg| arg.len() as u8));
    body.extend_from_slice(user);
    for arg in args {
        body.extend_from_slice(arg);
    }
    let packet = TacacsPacket { version: 0xc0, type_: TYPE_AUTHOR, seq_num: 1, flags: 0, session_id, body: TacacsPacket::encrypt_decrypt(&body, b"s", session_id, 0xc0, 1) };
    stream.write_all(&packet.encode())?;
    let mut header = [0u8; 12];
    stream.read_exact(&mut header)?;
    let mut reply = vec![0u8; u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize];
    stream.read_exact(&mut reply)?;
    let reply = TacacsPacket::encrypt_decrypt(&reply, b"s", session_id, 0xc0, header[2]);
    let (Some(&status), Some(&count)) = (reply.first(), reply.get(1)) else {
        return Err(std::io::Error::other("short reply"));
    };
    let lengths = reply.get(6..6 + usize::from(count)).ok_or_else(|| std::io::Error::other("short reply"))?.to_vec();
    let mut rest = reply.get(6 + usize::from(count)..).unwrap_or_default();
    let mut pairs = Vec::new();
    for length in lengths {
        let (pair, after) = rest.split_at_checked(usize::from(length)).ok_or_else(|| std::io::Error::other("short reply"))?;
        pairs.push(pair.to_vec());
        rest = after;
    }
    Ok((status, pairs))
}

/// Authorization groups decide without the script, and SIGHUP replaces their rules on a
/// connection already open; rules that do not compile leave the running ones in place.
#[test]
fn test_tacacs_authorize_groups() -> Result<(), Box<dyn std::error::Error>> {
    use rradius_core::tacacs::{AUTHOR_STATUS_FAIL, AUTHOR_STATUS_PASS_ADD};
    let dir = tempfile::tempdir()?;
    // The script refuses everything: what passes, the rules let through.
    let (config, auth_port) = write_config(dir.path(), "return 'reject'", None);
    let tacacs = free_port();
    let text = std::fs::read_to_string(&config)?;
    let rules = r"['permit service=shell cmd\* {priv-lvl=15}', 'deny cmd=show cmd-arg=running-config', 'permit service=shell cmd=show']";
    std::fs::write(&config, format!("{text}[tacacs]\nport = {tacacs}\n[tacacs.authorize_groups]\ngroup15 = {rules}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\nauthorize_group = \"group15\"\n"))?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_REJECT);

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", tacacs))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    assert_eq!(tacacs_authorize(&mut stream, 1, &[b"service=shell", b"cmd*"])?, (AUTHOR_STATUS_PASS_ADD, vec![b"priv-lvl=15".to_vec()]));
    assert_eq!(tacacs_authorize(&mut stream, 2, &[b"service=shell", b"cmd=show", b"cmd-arg=version"])?, (AUTHOR_STATUS_PASS_ADD, vec![]));
    assert_eq!(tacacs_authorize(&mut stream, 3, &[b"service=shell", b"cmd=show", b"cmd-arg=running-config"])?, (AUTHOR_STATUS_FAIL, vec![]));

    server.write_config_text(|text| text.replace(rules, "['deny service=shell']"));
    server.reload(RELOADED);
    assert_eq!(tacacs_authorize(&mut stream, 4, &[b"service=shell", b"cmd*"])?, (AUTHOR_STATUS_FAIL, vec![]), "the new rules, on the open connection");

    server.write_config_text(|text| text.replace("['deny service=shell']", "['permit cmd=(show']"));
    let log = server.reload(RELOAD_FAILED);
    assert!(log.contains("tacacs.authorize_groups.group15: rule 1"), "{log}");
    assert_eq!(tacacs_authorize(&mut stream, 5, &[b"service=shell", b"cmd*"])?, (AUTHOR_STATUS_FAIL, vec![]), "the previous rules stay");

    // A reply whose double quotes are not paired is refused the same way.
    server.write_config_text(|text| text.replace("['permit cmd=(show']", r#"['permit {a="x y}']"#));
    let log = server.reload(RELOAD_FAILED);
    assert!(log.contains("tacacs.authorize_groups.group15: rule 1: the reply's double quotes are not paired"), "{log}");
    assert_eq!(tacacs_authorize(&mut stream, 6, &[b"service=shell", b"cmd*"])?, (AUTHOR_STATUS_FAIL, vec![]), "the previous rules stay");
    Ok(())
}

/// Enable requests fail until `[tacacs] enable = "login"`, which SIGHUP applies.
#[test]
fn test_tacacs_enable_setting() -> Result<(), Box<dyn std::error::Error>> {
    use rradius_core::tacacs::{AUTHEN_STATUS_FAIL, AUTHEN_STATUS_PASS};
    let dir = tempfile::tempdir()?;
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let tacacs = free_port();
    let text = std::fs::read_to_string(&config)?;
    std::fs::write(&config, format!("{text}[tacacs]\nport = {tacacs}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"))?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);

    // A PAP enable request to level 15: action LOGIN, priv_lvl 15, PAP, service ENABLE.
    let (user, password) = (b"alice", b"secret");
    let mut enable = vec![1u8, 15, 2, 2, user.len() as u8, 0, 0, password.len() as u8];
    enable.extend_from_slice(user);
    enable.extend_from_slice(password);
    assert_eq!(tacacs_ask(&mut tacacs_stream(tacacs), 0x21, 1, &enable), AUTHEN_STATUS_FAIL, "refused by default");
    assert_eq!(tacacs_pap_login(tacacs, user, password, 0x22), AUTHEN_STATUS_PASS, "a login still passes");
    server.wait_for_log("TACACS+ enable request refused");

    server.write_config_text(|text| text.replace("[tacacs]\n", "[tacacs]\nenable = \"login\"\n"));
    server.reload(RELOADED);
    assert_eq!(tacacs_ask(&mut tacacs_stream(tacacs), 0x23, 1, &enable), AUTHEN_STATUS_PASS, "decided as a login");
    Ok(())
}

/// SIGTERM while the node is still starting: the start is waited for, briefly, so that the node
/// shuts down cleanly and the next start keeps its local database.
#[test]
fn test_stopping_while_the_node_starts_shuts_it_down() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let mut child = Server::spawn(PROGRAM, dir.path(), &config);
    let deadline = Instant::now() + Duration::from_secs(30);
    // As soon as the node holds its files, before it says it started.
    while lock_files(dir.path()).is_empty() && matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let started_before = std::fs::read_to_string(dir.path().join("stderr.log")).unwrap_or_default().contains("cluster node 1 started");
    let signalled = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().is_ok_and(|status| status.success());
    // Waited for whatever happened above, so that no process is left behind.
    let status = child.wait()?;
    assert!(signalled, "no SIGTERM sent");
    let stderr = std::fs::read_to_string(dir.path().join("stderr.log"))?;
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(lock_files(dir.path()).is_empty(), "not a clean shutdown (started before the signal: {started_before}): {stderr}");
    Ok(())
}

/// OCSP is only in rradius-enterprise: this program refuses it, at a start and in a check, in
/// each section that could have it, even when it is "off".
#[test]
fn test_ocsp_needs_the_enterprise_program() -> Result<(), String> {
    let files = "cert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n";
    let radius_client = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
    for (section, expected) in [
        (format!("[radius]\n[radius.eap]\n{files}ocsp = \"hard\"\n{radius_client}"), "radius.eap.ocsp: not supported by this program (OCSP needs rradius-enterprise)"),
        (format!("[tacacs]\nport = {}\n[tacacs.tls]\nport = {}\n{files}ocsp_url = \"http://ocsp.example.com/\"\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n", free_port(), free_port()), "tacacs.tls: not supported by this program (TACACS+ over TLS needs rradius-enterprise)"),
    ] {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let config = write_tls_config(dir.path(), &section)?;
        let config = config.to_str().ok_or("a path that is not UTF-8")?;
        for args in [vec!["server", "--config", config], vec!["check", "--config", config]] {
            let output = Command::new(env!("CARGO_BIN_EXE_rradius-server")).args(&args).output().map_err(|e| e.to_string())?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
            assert!(stderr.contains(expected), "{args:?}: {stderr}");
        }
    }
    Ok(())
}

/// Backups are only in rradius-enterprise: this program refuses `[cluster.backup]` at a start and
/// in a check, as it refuses the other sections of that program.
#[test]
fn test_backups_need_the_enterprise_program() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let mut text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    text.push_str("\n[cluster.backup]\ninterval_minutes = 60\n");
    std::fs::write(&config, text).map_err(|e| e.to_string())?;
    let config = config.to_str().ok_or("a path that is not UTF-8")?;
    for args in [vec!["server", "--config", config], vec!["check", "--config", config]] {
        let output = Command::new(env!("CARGO_BIN_EXE_rradius-server")).args(&args).output().map_err(|e| e.to_string())?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains("cluster.backup: not supported by this program (backups need rradius-enterprise)"), "{args:?}: {stderr}");
    }
    Ok(())
}

/// Accounting export is only in rradius-enterprise: this program refuses `[accounting_export]`
/// at a start and in a check (the Accounting Export Needs the Enterprise Program requirement).
#[test]
fn test_accounting_export_needs_enterprise() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let mut text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    text.push_str("\n[accounting_export]\ndirectory = \"exports\"\n[accounting_export.table]\nquery = \"SELECT 1\"\n");
    std::fs::write(&config, text).map_err(|e| e.to_string())?;
    let config = config.to_str().ok_or("a path that is not UTF-8")?;
    for args in [vec!["server", "--config", config], vec!["check", "--config", config]] {
        let output = Command::new(env!("CARGO_BIN_EXE_rradius-server")).args(&args).output().map_err(|e| e.to_string())?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains("accounting_export: not supported by this program (accounting export needs rradius-enterprise)"), "{args:?}: {stderr}");
    }
    Ok(())
}

/// `[accounting_export]` added while the program runs is refused at SIGHUP, and the program goes on
/// with what it had.
#[test]
fn test_accounting_export_refused_at_reload() -> Result<(), String> {
    let server = Server::start(PROGRAM, tempfile::tempdir().map_err(|e| e.to_string())?, "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    server.write_config_text(|text| format!("{text}[accounting_export]\ndirectory = \"exports\"\n[accounting_export.table]\nquery = \"SELECT 1\"\n"));
    let stderr = server.reload(RELOAD_FAILED);
    assert!(stderr.contains("accounting export needs rradius-enterprise"), "{stderr}");
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    Ok(())
}

/// `mfa` on a client needs rradius-enterprise: `check` ends with status 2 and says so, and the
/// setting added while the program runs is refused at SIGHUP (Second Factor Settings: Core
/// program).
#[test]
fn test_mfa_needs_the_enterprise_program() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    std::fs::write(&config, format!("{text}mfa = \"challenge\"\n")).map_err(|e| e.to_string())?;
    let output = Command::new(PROGRAM).arg("check").arg("--config").arg(&config).output().map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("mfa needs rradius-enterprise"), "{stderr}");
    let server = Server::start(PROGRAM, tempfile::tempdir().map_err(|e| e.to_string())?, "return 'accept'", None);
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    server.write_config_text(|text| format!("{text}mfa = \"challenge\"\n"));
    let stderr = server.reload(RELOAD_FAILED);
    assert!(stderr.contains("mfa needs rradius-enterprise"), "{stderr}");
    assert_eq!(server.access_request(), radius::ACCESS_ACCEPT);
    Ok(())
}

/// The settings RadSec, and only it, uses, as the program without it takes them: at a start and
/// in a check (the table of the radsec spec, this program's column).
#[test]
fn test_radsec_needs_the_enterprise_program() -> Result<(), String> {
    core_column(radsec_settings_table(free_port()))
}

/// The same for proxying (the table of the radius-proxy spec, this program's column).
#[test]
fn test_proxy_needs_the_enterprise_program() -> Result<(), String> {
    core_column(proxy_settings_table())
}

/// The same for CoA (the table of the dynamic-authorization spec, this program's column).
#[test]
fn test_coa_needs_the_enterprise_program() -> Result<(), String> {
    core_column(coa_settings_table())
}

/// `disconnect` and `coa` are commands of rradius-enterprise: this program says so before
/// reading anything, and its usage does not list them.
#[test]
fn test_coa_commands_need_the_enterprise_program() -> Result<(), String> {
    for command in ["disconnect", "coa"] {
        let output = Command::new(PROGRAM).args([command, "--config", "/nonexistent.toml", "--target", "nas1", "User-Name=alice"]).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{command}: {stderr}");
        assert!(stderr.contains(&format!("{command}: not supported by this program (CoA needs rradius-enterprise)")) && !stderr.contains("nonexistent"), "{command}: {stderr}");
    }
    let usage = String::from_utf8_lossy(&Command::new(PROGRAM).arg("--help").output().map_err(|e| e.to_string())?.stdout).into_owned();
    assert!(!usage.contains("disconnect|coa"), "{usage}");
    Ok(())
}

/// The same for DPSK (the table of the dpsk spec, this program's column): its sections at a start
/// and in a check; its commands, before reading anything; and its usage, which does not list them.
#[test]
fn test_dpsk_needs_the_enterprise_program() -> Result<(), String> {
    core_column(dpsk_settings_table("/nonexistent/dpsk.key"))?;
    for args in [&["dpsk", "show", "--config", "/nonexistent.toml", "--ssid", "office", "bob"][..], &["dpsk-seal", "--key-file", "/nonexistent.key", "--ssid", "office", "--identity", "bob"][..]] {
        // Standard input stays open: a command that read it would wait, not exit.
        let mut child = Command::new(PROGRAM).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if started.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                return Err(format!("{args:?} waited for its input"));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        child.stderr.take().ok_or("no standard error")?.read_to_string(&mut stderr).map_err(|e| e.to_string())?;
        assert_eq!(status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains(&format!("{}: not supported by this program (DPSK needs rradius-enterprise)", args[0])) && !stderr.contains("nonexistent"), "{args:?}: {stderr}");
    }
    let usage = String::from_utf8_lossy(&Command::new(PROGRAM).arg("--help").output().map_err(|e| e.to_string())?.stdout).into_owned();
    assert!(usage.contains("usage: rradius-server user") && !usage.contains("dpsk"), "{usage}");
    Ok(())
}

/// EAP-AKA' is only in rradius-enterprise: this program refuses its sections and method (the
/// table of the eap spec, this program's column), and its commands before reading anything.
#[test]
fn test_aka_needs_the_enterprise_program() -> Result<(), String> {
    core_column(aka_settings_table("/nonexistent/aka.key"))?;
    for args in [&["aka", "show", "--config", "/nonexistent.toml", "001010000000001"][..], &["aka-seal", "--key-file", "/nonexistent.key", "--imsi", "1"][..]] {
        // Standard input stays open: a command that read it would wait, not exit.
        let mut child = Command::new(PROGRAM).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if started.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                return Err(format!("{args:?} waited for its input"));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        child.stderr.take().ok_or("no standard error")?.read_to_string(&mut stderr).map_err(|e| e.to_string())?;
        assert_eq!(status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains(&format!("{}: not supported by this program (EAP-AKA' needs rradius-enterprise)", args[0])) && !stderr.contains("nonexistent"), "{args:?}: {stderr}");
    }
    let usage = String::from_utf8_lossy(&Command::new(PROGRAM).arg("--help").output().map_err(|e| e.to_string())?.stdout).into_owned();
    assert!(usage.contains("usage: rradius-server user") && !usage.contains("aka"), "{usage}");
    Ok(())
}

/// Session resumption is only in rradius-enterprise (the table of the eap spec, this program's
/// column): the seal file named does not exist, and the message is the same, since it is never read.
#[test]
fn test_session_resumption_needs_the_enterprise_program() -> Result<(), String> {
    core_column(session_resumption_settings_table("/nonexistent/ticket.seal"))
}

/// The table of the tacacs-aaa spec ("TACACS+ over TLS Only in the Enterprise Program"), this
/// program's column: `[tacacs.tls]` is refused whatever it holds, also when its files are missing.
#[test]
fn test_tacacs_tls_needs_the_enterprise_program() -> Result<(), String> {
    core_column(tacacs_tls_settings_table())
}

/// The same for dynamic clients (the table of the server-config spec, this program's column).
#[test]
fn test_dynamic_clients_need_the_enterprise_program() -> Result<(), String> {
    core_column(dynamic_clients_settings_table())
}

/// `client` is a command of rradius-enterprise: this program says so before reading anything.
#[test]
fn test_client_command_needs_the_enterprise_program() -> Result<(), String> {
    let output = Command::new(PROGRAM).args(["client", "show", "--config", "/nonexistent.toml", "10.20.1.5"]).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("client: not supported by this program (dynamic clients need rradius-enterprise)") && !stderr.contains("nonexistent"), "{stderr}");
    // Nor does its usage list it.
    let usage = String::from_utf8_lossy(&Command::new(PROGRAM).arg("--help").output().map_err(|e| e.to_string())?.stdout).into_owned();
    assert!(usage.contains("usage: rradius-server user") && !usage.contains("client add"), "{usage}");
    // A section that needs it is no command: the usage, not its message.
    let section = String::from_utf8_lossy(&Command::new(PROGRAM).arg("radius.tls").output().map_err(|e| e.to_string())?.stderr).into_owned();
    assert!(section.contains("usage: rradius-server user") && !section.contains("not supported"), "{section}");
    Ok(())
}

/// Starts and checks each configuration of `table` with this program, as its `in_core` says.
fn core_column(table: Vec<(String, Said)>) -> Result<(), String> {
    for (section, expected) in table {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let config = write_tls_config(dir.path(), &section)?;
        let config = config.to_str().ok_or("a path that is not UTF-8")?;
        match expected.in_core {
            Some(message) => {
                for args in [vec!["server", "--config", config], vec!["check", "--config", config]] {
                    let output = Command::new(PROGRAM).args(&args).output().map_err(|e| e.to_string())?;
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    assert_eq!(output.status.code(), Some(2), "{section}: {args:?}: {stderr}");
                    assert!(stderr.contains(message), "{section}: {args:?}: {stderr}");
                }
            }
            None => {
                let output = Command::new(PROGRAM).args(["check", "--config", config]).output().map_err(|e| e.to_string())?;
                assert_eq!(output.status.code(), Some(0), "{section}: {}", String::from_utf8_lossy(&output.stderr));
            }
        }
    }
    Ok(())
}

#[test]
fn test_cluster_command_usage() -> Result<(), String> {
    for args in [&["cluster"][..], &["cluster", "join"], &["cluster", "remove"], &["cluster", "remove", "abc"]] {
        let (code, stdout, stderr) = output_of(args)?;
        assert_eq!((code, stdout.as_str()), (Some(2), ""), "{args:?}: {stderr}");
        assert!(stderr.contains("usage: rradius-server cluster members|remove") || stderr.contains("positive integer"), "{args:?}: {stderr}");
    }
    let (_, stdout, _) = output_of(&["help"])?;
    assert!(stdout.contains("usage: rradius-server cluster members|remove"), "{stdout}");
    Ok(())
}

/// With no node running, the command gives up after its 30 s with status 3.
#[test]
fn test_cluster_command_without_the_cluster() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'reject'", None);
    let config = config.to_str().ok_or("config path")?;
    let started = Instant::now();
    let (code, stdout, stderr) = output_of(&["cluster", "members", "--config", config])?;
    assert_eq!((code, stdout.as_str()), (Some(3), ""), "{stderr}");
    assert!(stderr.contains("did not answer within 30 s"), "{stderr}");
    assert!(started.elapsed() < Duration::from_secs(40), "took {:?}", started.elapsed());
    assert!(!dir.path().join("data").exists(), "the data directory was created");
    Ok(())
}

/// A single node: listed as the leader, never removed while it runs, and a wrong api secret
/// ends the command at once.
#[test]
fn test_cluster_command_on_one_node() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let server = Server::start(PROGRAM, dir, "return 'reject'", None);
    let config = server.config.to_str().ok_or("config path")?.to_string();
    let mut members = (None, String::new(), String::new());
    let up = (0..60).any(|_| {
        members = output_of(&["cluster", "members", "--config", &config]).unwrap_or_default();
        let listed = members.0 == Some(0);
        if !listed {
            std::thread::sleep(Duration::from_millis(500));
        }
        listed
    });
    assert!(up, "{members:?}");
    assert!(members.1.trim_end().starts_with("node 1  voter    leader  raft 127.0.0.1:") && members.1.trim_end().lines().count() == 1, "{members:?}");

    let (code, stdout, stderr) = output_of(&["cluster", "remove", "--config", &config, "1"])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""), "{stderr}");
    assert!(stderr.contains("node 1 still answers"), "{stderr}");

    let text = std::fs::read_to_string(&server.config).map_err(|e| e.to_string())?;
    let wrong = server.dir.path().join("wrong-secret.toml");
    std::fs::write(&wrong, text.replace("test-api-secret-01234", "another-api-secret-99")).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let (code, stdout, stderr) = output_of(&["cluster", "members", "--config", wrong.to_str().ok_or("path")?])?;
    assert_eq!((code, stdout.as_str()), (Some(2), ""), "{stderr}");
    assert!(stderr.contains("refused the api secret") && !stderr.contains("api-secret-"), "{stderr}");
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    Ok(())
}
