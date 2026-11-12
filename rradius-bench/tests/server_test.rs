//! `run` against a real server, started from its executable, which also checks this tool's
//! reading of the protocol against the server's own.
mod common;

use common::{free_port, step, Output, SECRET};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PASSWORD: &str = "a-test-password";

/// The server's counters the tests read, as Prometheus series.
const RADSEC_ACCEPTED: &str = "rradius_radsec_connections_total{result=\"accepted\"}";
const DUPLICATES: &str = "rradius_radius_duplicates_total";
const DISCARDED_INVALID: &str = "rradius_radius_discarded_total{reason=\"invalid\"}";
const PACKETS_ACCT: &str = "rradius_radius_packets_total{kind=\"acct\"}";
const TACACS_ACCEPTED: &str = "rradius_tacacs_connections_total{result=\"accepted\"}";

/// How long a server has to start answering.
const START_WAIT: Duration = Duration::from_secs(10);

/// The server executable: `RRADIUS_SERVER_BIN` when set, or else the one built next to
/// this tool, as `cargo test --workspace` builds both.
fn server_binary() -> Result<PathBuf, String> {
    program("rradius-server", "RRADIUS_SERVER_BIN")
}

/// The server executable with RadSec and EAP-AKA', which only `rradius-enterprise` has:
/// `RRADIUS_ENTERPRISE_BIN` when set, or else the one built next to this tool.
fn enterprise_binary() -> Result<PathBuf, String> {
    program("rradius-enterprise", "RRADIUS_ENTERPRISE_BIN")
}

/// Whether the enterprise executable is missing. The tests of RadSec and EAP-AKA' need it; in a
/// repository with only the core they say so and pass, and the enterprise's CI runs them with
/// `RRADIUS_ENTERPRISE_BIN`. When that variable is set, the executable it names must be there:
/// a wrong path fails the test rather than skipping it.
fn without_enterprise() -> Result<bool, String> {
    match enterprise_binary() {
        Ok(_) => Ok(false),
        Err(e) if std::env::var_os("RRADIUS_ENTERPRISE_BIN").is_some() => Err(e),
        Err(e) => {
            eprintln!("skipped: needs rradius-enterprise ({e})");
            Ok(true)
        }
    }
}

/// The executable `name`: the one `variable` names when set, or else the one built next to
/// this tool.
fn program(name: &str, variable: &str) -> Result<PathBuf, String> {
    let path = match std::env::var_os(variable) {
        Some(path) => PathBuf::from(path),
        None => Path::new(env!("CARGO_BIN_EXE_rradius-bench")).with_file_name(name),
    };
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("no server executable at {}: run `cargo build -p {name}` first, or set {variable}", path.display()))
    }
}

/// A single-node server running from its executable, killed when dropped.
struct Server {
    child: Child,
    /// Holds the configuration, the database and what the server writes to standard error.
    /// Declared after `child`, so that it goes once the server has.
    dir: tempfile::TempDir,
    /// The executable it runs, which its commands run too.
    program: PathBuf,
    /// The RADIUS authentication address; port 0 without `[radius]`.
    auth: SocketAddr,
    /// 0 without `[radius]`.
    acct_port: u16,
    metrics_port: u16,
    /// With `[radius.tls]`.
    radsec_port: Option<u16>,
    /// With `[tacacs]`.
    tacacs_port: Option<u16>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// Writes `script`, `migrations` and `rradius.toml` (the parts every test server has,
    /// then `body`) into `dir`, and starts the server on them.
    fn spawn(dir: tempfile::TempDir, script: &str, body: &str, migrations: &[(&str, String)]) -> Result<Server, String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::write(dir.path().join("script.lua"), script).map_err(|e| e.to_string())?;
        let migrations_dir = if migrations.is_empty() {
            ""
        } else {
            std::fs::create_dir(dir.path().join("migrations")).map_err(|e| e.to_string())?;
            for (name, sql) in migrations {
                std::fs::write(dir.path().join("migrations").join(name), sql).map_err(|e| e.to_string())?;
            }
            "migrations_dir = \"migrations\"\n"
        };
        let metrics_port = free_port();
        let config = format!(
            "dictionary = \"{dictionary}\"\n[lua]\nscript = \"script.lua\"\n\
             [cluster]\ndata_dir = \"data\"\nsecret_raft = \"test-raft-secret-0123\"\nsecret_api = \"test-api-secret-01234\"\nlisten = \"127.0.0.1\"\n{migrations_dir}\
             [[cluster.nodes]]\nid = 1\nraft = \"127.0.0.1:{raft}\"\napi = \"127.0.0.1:{api}\"\n\
             [metrics]\nbind = \"127.0.0.1\"\nport = {metrics_port}\n\
             {body}",
            dictionary = root.join("../share/dictionary").display(),
            raft = free_port(),
            api = free_port(),
        );
        let path = dir.path().join("rradius.toml");
        std::fs::write(&path, config).map_err(|e| e.to_string())?;
        let stderr = std::fs::File::create(dir.path().join("server.stderr")).map_err(|e| e.to_string())?;
        // RadSec and EAP-AKA' are in rradius-enterprise only.
        let program = if body.contains("[radius.tls]") || body.contains("[radius.eap.aka]") { enterprise_binary()? } else { server_binary()? };
        let child = Command::new(&program).arg("--config").arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn().map_err(|e| format!("starting the server: {e}"))?;
        Ok(Server { child, dir, program, auth: SocketAddr::from(([127, 0, 0, 1], 0)), acct_port: 0, metrics_port, radsec_port: None, tacacs_port: None })
    }

    /// What the server has written to standard error so far.
    fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("server.stderr")).unwrap_or_default()
    }

    /// Fails, with what the server said, once it has exited or `started` is too long ago.
    fn still_starting(&mut self, started: Instant) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(None) if started.elapsed() < START_WAIT => Ok(()),
            Ok(None) => Err(format!("the server did not start within {START_WAIT:?}: {}", self.stderr())),
            Ok(Some(status)) => Err(format!("the server exited with {status}: {}", self.stderr())),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Returns the server once it answers a Status-Server on its authentication port.
    fn answering(mut self) -> Result<Server, String> {
        let started = Instant::now();
        while bench(&self, &["--load", "status-auth,concurrency=1", "--requests", "1", "--timeout", "0.2"]).code != 0 {
            self.still_starting(started)?;
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(self)
    }

    /// Returns the server once `port` takes TCP connections.
    fn listening(mut self, port: u16) -> Result<Server, String> {
        self.wait_for_port(port)?;
        Ok(self)
    }

    /// Returns once `port` takes TCP connections.
    fn wait_for_port(&mut self, port: u16) -> Result<(), String> {
        let started = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            self.still_starting(started)?;
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// The value of `series`, the whole name with its labels, read from the metrics port.
    fn metric(&self, series: &str) -> Result<u64, String> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.metrics_port)).map_err(|e| format!("metrics port: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
        let mut response = String::new();
        stream.read_to_string(&mut response).map_err(|e| e.to_string())?;
        let prefix = format!("{series} ");
        let line = response.lines().find(|line| line.starts_with(&prefix)).ok_or_else(|| format!("no {series} among the metrics: {response}"))?;
        line[prefix.len()..].trim().parse().map_err(|e| format!("{line}: {e}"))
    }

    /// The path of a file in the server's directory.
    fn file(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    /// Runs the server executable with `args`, `stdin` on standard input. Returns what it
    /// printed, or what it said on standard error when it fails.
    fn command(&self, args: &[&str], stdin: &str) -> Result<String, String> {
        let mut child = Command::new(&self.program).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
        let mut input = child.stdin.take().ok_or("no standard input")?;
        input.write_all(stdin.as_bytes()).map_err(|e| e.to_string())?;
        drop(input);
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!("{args:?}: {}", String::from_utf8_lossy(&output.stderr)));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// The `[auth]` section with an `sql` handler that looks up `users`.
const SQL_CHAIN: &str = "[auth]\n[[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users WHERE username = ?\"\n";

/// The migration that makes `users`, with alice and `PASSWORD`.
fn users() -> (&'static str, String) {
    ("1_users.sql", format!("CREATE TABLE users (username TEXT, password TEXT);\nINSERT INTO users VALUES ('alice', '{PASSWORD}');\n"))
}

/// Starts a single-node server. With `chain`, an `sql` handler authenticates "alice".
fn start(script: &str, chain: bool) -> Result<Server, String> {
    start_with(script, chain, None)
}

/// Writes `<name>.pem` and `<name>.key`, signed by `issuer`, for `names`.
fn leaf(dir: &std::path::Path, name: &str, names: &[&str], issuer: &rcgen::Issuer<'_, rcgen::KeyPair>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(names.iter().map(|name| name.to_string()).collect::<Vec<_>>()).unwrap();
    std::fs::write(dir.join(format!("{name}.pem")), params.signed_by(&key, issuer).unwrap().pem()).unwrap();
    std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).unwrap();
}

/// A CA, the server's certificate for `localhost`, and alice's.
fn eap_certificates(dir: &std::path::Path) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    std::fs::write(dir.join("ca.pem"), params.clone().self_signed(&key).unwrap().pem()).unwrap();
    let issuer = rcgen::Issuer::new(params, key);
    // The address too, as what `--tls-name` is unless given.
    leaf(dir, "server", &["localhost", "127.0.0.1"], &issuer);
    leaf(dir, "alice", &["alice"], &issuer);
}

/// Like `start`, with `[radius.eap]` offering only `method`, when given. Its certificates
/// are in the server's directory: `ca.pem`, and `alice.pem` and `alice.key` for EAP-TLS.
fn start_with(script: &str, chain: bool, method: Option<&str>) -> Result<Server, String> {
    start_full(script, chain, method, None)
}

/// Like `start_with`, with a RadSec port as well: `[radius.tls]` with the certificates of
/// `eap_certificates`, and `tls` added to it.
fn start_radsec(script: &str, chain: bool, method: Option<&str>, tls: &str) -> Result<Server, String> {
    start_full(script, chain, method, Some(tls))
}

fn start_full(script: &str, chain: bool, method: Option<&str>, radsec: Option<&str>) -> Result<Server, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (auth_port, acct_port) = (free_port(), free_port());
    let auth = if chain { SQL_CHAIN } else { "" };
    let body = format!(
        "{auth}\
         [radius]\nbind = \"127.0.0.1\"\nauth_port = {auth_port}\nacct_port = {acct_port}\n\
         [[radius.clients]]\nname = \"local\"\naddress = \"127.0.0.1\"\nsecret = \"{SECRET}\"\n",
    );
    let body = match method {
        Some(method) => {
            eap_certificates(dir.path());
            let client_ca = if method == "tls" { "client_ca_file = \"ca.pem\"\n" } else { "" };
            format!("{body}[radius.eap]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\n{client_ca}methods = [\"{method}\"]\n")
        }
        None => body,
    };
    let radsec_port = radsec.map(|_| free_port());
    let body = match (radsec, radsec_port) {
        (Some(tls), Some(port)) => {
            if method.is_none() {
                eap_certificates(dir.path());
            }
            format!("{body}[radius.tls]\nbind = \"127.0.0.1\"\nport = {port}\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n{tls}")
        }
        _ => body,
    };
    let migrations = if chain { vec![users()] } else { Vec::new() };
    let mut server = Server::spawn(dir, script, &body, &migrations)?;
    server.auth = SocketAddr::from(([127, 0, 0, 1], auth_port));
    server.acct_port = acct_port;
    server.radsec_port = radsec_port;
    server.answering()
}

/// Runs the tool against the server's RADIUS ports, with `args` after these.
fn bench(server: &Server, args: &[&str]) -> Output {
    let (auth_port, acct_port) = (server.auth.port().to_string(), server.acct_port.to_string());
    let mut all: Vec<String> = ["run", "--json", "--server", "127.0.0.1", "--secret", SECRET, "--auth-port", &auth_port, "--acct-port", &acct_port].iter().map(|s| s.to_string()).collect();
    all.extend(args.iter().map(|arg| arg.to_string()));
    common::bench(&all)
}

#[test]
fn test_eap_wrong_password_rejected() -> Result<(), String> {
    for (kind, method) in [("peap", "peap"), ("ttls", "ttls")] {
        let server = start_with("return 'accept'", true, Some(method))?;
        let load = format!("{kind},concurrency=4");
        let output = bench(&server, &["--load", &load, "--requests", "8", "--user", "alice", "--password", "wrong", "--timeout", "2"]);
        assert_eq!(output.code, 1, "{kind}: {}\n{}", output.stdout, output.stderr);
        let json = output.json();
        let step = step(&json, 0, 0);
        assert_eq!((step["responses"]["reject"].as_u64(), step["invalid"].as_u64()), (Some(8), Some(0)), "{kind}");
    }
    Ok(())
}

/// Passes packets between the tool and `server`, handing each answer to `change` with the
/// Request Authenticator of its request: it can change the answer, which is then signed
/// again, or return false to drop it.
fn relay(server: SocketAddr, mut change: impl FnMut(&mut Vec<u8>) -> bool + Send + 'static) -> u16 {
    use hmac::{Hmac, KeyInit, Mac};
    use md5::{Digest, Md5};
    let outside = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let inside = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    inside.connect(server).unwrap();
    let port = outside.local_addr().unwrap().port();
    let (outside_in, inside_out) = (outside.try_clone().unwrap(), inside.try_clone().unwrap());
    let authenticators = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let client = Arc::new(std::sync::Mutex::new(None));
    let (seen, to) = (authenticators.clone(), client.clone());
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok((length, from)) = outside_in.recv_from(&mut buffer) {
            *to.lock().unwrap() = Some(from);
            seen.lock().unwrap().insert(buffer[1], buffer[4..20].to_vec());
            let _ = inside_out.send(&buffer[..length]);
        }
    });
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok(length) = inside.recv(&mut buffer) {
            let mut packet = buffer[..length].to_vec();
            if !change(&mut packet) {
                continue;
            }
            let request_authenticator = authenticators.lock().unwrap()[&packet[1]].clone();
            let (mut at, mut signature) = (20, None);
            while at + 2 <= packet.len() {
                if packet[at] == 80 {
                    signature = Some(at + 2);
                }
                at += usize::from(packet[at + 1]);
            }
            if let Some(signature) = signature {
                packet[signature..signature + 16].fill(0);
                let mut unsigned = packet.clone();
                unsigned[4..20].copy_from_slice(&request_authenticator);
                let mut mac = <Hmac<md5::Md5> as KeyInit>::new_from_slice(SECRET.as_bytes()).unwrap();
                mac.update(&unsigned);
                packet[signature..signature + 16].copy_from_slice(&mac.finalize().into_bytes());
            }
            let mut unsigned = packet.clone();
            unsigned[4..20].copy_from_slice(&request_authenticator);
            let digest: [u8; 16] = Md5::new().chain_update(&unsigned).chain_update(SECRET.as_bytes()).finalize().into();
            packet[4..20].copy_from_slice(&digest);
            if let Some(client) = *client.lock().unwrap() {
                let _ = outside.send_to(&packet, client);
            }
        }
    });
    port
}

/// The tool, with `args` after these, against 127.0.0.1 on `port`.
fn bench_at(port: u16, server_name: &str, args: &[&str]) -> Output {
    let port = port.to_string();
    let mut all: Vec<String> = ["run", "--json", "--server", server_name, "--secret", SECRET, "--auth-port", &port].iter().map(|s| s.to_string()).collect();
    all.extend(args.iter().map(|arg| arg.to_string()));
    common::bench(&all)
}

#[test]
fn test_eap_wrong_keys_are_invalid() -> Result<(), String> {
    let server = start_with("return 'accept'", true, Some("peap"))?;
    // Changes a key octet after the salt and the first, which is the key's length.
    let port = relay(server.auth, |packet| {
        let mut at = 20;
        while packet[0] == 2 && at + 2 <= packet.len() {
            if packet[at] == 26 && packet[at + 2..at + 6] == 311u32.to_be_bytes() && packet[at + 6] == 17 {
                packet[at + 12] ^= 0xff;
            }
            at += usize::from(packet[at + 1]);
        }
        true
    });
    let output = bench_at(port, "127.0.0.1", &["--load", "peap,concurrency=1", "--requests", "4", "--user", "alice", "--password", PASSWORD, "--timeout", "2"]);
    assert_eq!(output.code, 2, "{}\n{}", output.stdout, output.stderr);
    let json = output.json();
    let step = step(&json, 0, 0);
    assert_eq!((step["responses"]["accept"].as_u64(), step["invalid"].as_u64()), (Some(0), Some(4)), "{step}");
    assert_eq!(output.stderr.matches("MS-MPPE keys that differ").count(), 1, "said once: {}", output.stderr);
    Ok(())
}

/// A round whose answer is lost is sent again with `--retries`, and the login goes on;
/// without, the conversation ends as a timeout.
#[test]
fn test_eap_lost_round() -> Result<(), String> {
    let server = start_with("return 'accept'", true, Some("ttls"))?;
    let mut challenges = 0;
    let port = relay(server.auth, move |packet| {
        if packet[0] == 11 {
            challenges += 1;
            return challenges != 2;
        }
        true
    });
    let output = bench_at(port, "127.0.0.1", &["--load", "ttls,concurrency=1", "--requests", "1", "--user", "alice", "--password", PASSWORD, "--timeout", "0.5", "--retries", "1"]);
    assert_eq!(output.code, 0, "{}\n{}", output.stdout, output.stderr);
    let json = output.json();
    let counts = step(&json, 0, 0);
    assert_eq!((counts["responses"]["accept"].as_u64(), counts["retransmits"].as_u64(), counts["timeouts"].as_u64()), (Some(1), Some(1), Some(0)), "{counts}");

    let port = relay(server.auth, |packet| packet[0] != 11);
    let output = bench_at(port, "127.0.0.1", &["--load", "ttls,concurrency=2", "--requests", "4", "--user", "alice", "--password", PASSWORD, "--timeout", "0.3"]);
    assert_eq!(output.code, 2, "{}\n{}", output.stdout, output.stderr);
    let json = output.json();
    let counts = step(&json, 0, 0);
    assert_eq!((counts["sent"].as_u64(), counts["timeouts"].as_u64(), counts["responses"]["accept"].as_u64()), (Some(4), Some(4), Some(0)), "one timeout per conversation: {counts}");
    Ok(())
}

/// With `--ca-file`, a server certificate from another CA, or for another name than
/// `--tls-name`, which is `--server` unless given, ends the login.
#[test]
fn test_eap_server_certificate_checked() -> Result<(), String> {
    let server = start_with("return 'accept'", true, Some("peap"))?;
    let other = tempfile::tempdir().unwrap();
    eap_certificates(other.path());
    let (ca, other_ca) = (server.file("ca.pem"), other.path().join("ca.pem").display().to_string());
    let port = server.auth.port();
    let login = ["--load", "peap,concurrency=1", "--requests", "2", "--user", "alice", "--password", PASSWORD, "--timeout", "2"];
    for (tls_name, ca_file, accepted) in [(None, &ca, true), (Some("other.example"), &ca, false), (None, &other_ca, false)] {
        let mut args = login.to_vec();
        args.extend(["--ca-file", ca_file.as_str()]);
        if let Some(name) = tls_name {
            args.extend(["--tls-name", name]);
        }
        let output = bench_at(port, "127.0.0.1", &args);
        let server_name = tls_name.unwrap_or("(--server)");
        let json = output.json();
        let counts = step(&json, 0, 0);
        if accepted {
            assert_eq!((output.code, counts["responses"]["accept"].as_u64()), (0, Some(2)), "{server_name} {ca_file}: {}", output.stderr);
        } else {
            assert_eq!((output.code, counts["invalid"].as_u64()), (2, Some(2)), "{server_name} {ca_file}: {counts}");
            assert!(output.stderr.contains("TLS"), "{}", output.stderr);
        }
    }
    Ok(())
}

/// The tool over RadSec to `server`, as alice, with `args` after these.
fn bench_radsec(server: &Server, args: &[&str]) -> Result<Output, String> {
    let port = server.radsec_port.ok_or("no RadSec port")?.to_string();
    let (cert, key) = (server.file("alice.pem"), server.file("alice.key"));
    let mut all: Vec<String> = ["run", "--json", "--server", "127.0.0.1", "--secret", "radsec", "--transport", "tls", "--tls-port", &port, "--cert-file", &cert, "--key-file", &key].iter().map(|s| s.to_string()).collect();
    all.extend(args.iter().map(|arg| arg.to_string()));
    Ok(common::bench(&all))
}

/// The RadSec port takes connections once its listener is up.
fn radsec_ready(server: &mut Server) -> Result<(), String> {
    let port = server.radsec_port.ok_or("no RadSec port")?;
    server.wait_for_port(port)
}

fn number(value: &serde_json::Value) -> Result<u64, String> {
    value.as_u64().ok_or_else(|| format!("not a count: {value}"))
}

/// Every kind of load over RadSec, one connection per socket.
#[test]
fn test_radsec_loads() -> Result<(), String> {
    if without_enterprise()? {
        return Ok(());
    }
    let mut server = start_radsec("return 'accept'", true, Some("peap"), "")?;
    radsec_ready(&mut server)?;
    let before = server.metric(RADSEC_ACCEPTED)?;
    let output = bench_radsec(&server, &["--load", "pap,concurrency=8,window=2", "--requests", "40", "--user", "alice", "--password", PASSWORD, "--timeout", "2"])?;
    assert_eq!(output.code, 0, "{}\n{}", output.stdout, output.stderr);
    let json = output.json();
    let load = &json["loads"][0];
    assert_eq!((load["transport"].as_str(), number(&load["sockets"])?, number(&load["reconnects"])?), (Some("tls"), 4, 0), "{load}");
    assert_eq!(number(&step(&json, 0, 0)["responses"]["accept"])?, 40);
    assert_eq!(server.metric(RADSEC_ACCEPTED)? - before, 4, "one connection per socket");
    let before = server.metric(RADSEC_ACCEPTED)?;
    let output = bench_radsec(&server, &["--load", "pap,concurrency=64", "--requests", "64", "--user", "alice", "--password", PASSWORD, "--timeout", "2"])?;
    assert_eq!(output.code, 0, "{}\n{}", output.stdout, output.stderr);
    assert_eq!(server.metric(RADSEC_ACCEPTED)? - before, 1, "64 in flight on one connection with the default window");

    for (load, user_args, expected) in [
        ("acct-start,concurrency=4", &[][..], ("accounting", 20)),
        ("peap,concurrency=4", &["--user", "alice", "--password", PASSWORD][..], ("accept", 20)),
    ] {
        let mut args = vec!["--load", load, "--requests", "20", "--timeout", "2"];
        args.extend_from_slice(user_args);
        let output = bench_radsec(&server, &args)?;
        assert_eq!(output.code, 0, "{load}: {}\n{}", output.stdout, output.stderr);
        let json = output.json();
        let counts = step(&json, 0, 0);
        assert_eq!(number(&counts["responses"][expected.0])?, expected.1, "{load}: {counts}");
        assert_eq!((number(&counts["invalid"])?, number(&counts["timeouts"])?, number(&counts["socket_errors"])?), (0, 0, 0), "{load}");
    }
    let output = bench_radsec(&server, &["--load", "pap,concurrency=2", "--requests", "4", "--bind", "127.0.0.1", "--user", "alice", "--password", PASSWORD, "--timeout", "2"])?;
    assert_eq!(output.code, 0, "--bind with RadSec: {}\n{}", output.stdout, output.stderr);
    // 64 challenges of about a kilobyte each arrive at once on one connection.
    let output = bench_radsec(&server, &["--load", "peap,concurrency=64", "--requests", "128", "--user", "alice", "--password", PASSWORD, "--timeout", "5"])?;
    assert_eq!(output.code, 0, "peap on one connection: {}\n{}", output.stdout, output.stderr);
    let json = output.json();
    assert_eq!((number(&step(&json, 0, 0)["responses"]["accept"])?, number(&json["loads"][0]["reconnects"])?), (128, 0), "{}", output.stderr);
    let output = bench_radsec(&server, &["--load", "session,concurrency=2", "--requests", "5", "--user", "alice", "--password", PASSWORD, "--timeout", "2"])?;
    assert_eq!(output.code, 0, "session: {}\n{}", output.stdout, output.stderr);
    let json = output.json();
    for index in 0..3 {
        let counts = step(&json, 0, index);
        assert_eq!((number(&counts["sent"])?, number(&counts["timeouts"])?, number(&counts["socket_errors"])?), (5, 0, 0), "session step {index}: {counts}");
    }
    Ok(())
}

#[test]
fn test_radsec_untrusted_server() -> Result<(), String> {
    if without_enterprise()? {
        return Ok(());
    }
    let mut server = start_radsec("return 'accept'", true, None, "")?;
    radsec_ready(&mut server)?;
    let other = tempfile::tempdir().map_err(|e| e.to_string())?;
    eap_certificates(other.path());
    let other_ca = other.path().join("ca.pem").display().to_string();
    let output = bench_radsec(&server, &["--load", "pap,concurrency=1", "--requests", "1", "--user", "alice", "--password", PASSWORD, "--ca-file", &other_ca, "--timeout", "2"])?;
    assert_eq!(output.code, 3, "{}\n{}", output.stdout, output.stderr);
    assert!(output.stderr.contains("TLS handshake"), "{}", output.stderr);
    Ok(())
}

/// A server that closes every connection soon after it opens: the run goes on.
#[test]
fn test_radsec_reconnects() -> Result<(), String> {
    if without_enterprise()? {
        return Ok(());
    }
    let mut server = start_radsec("return 'accept'", true, None, "max_lifetime_ms = 300\n")?;
    radsec_ready(&mut server)?;
    let output = bench_radsec(&server, &["--load", "pap,concurrency=4,window=2", "--warmup", "0.2", "--duration", "2", "--user", "alice", "--password", PASSWORD, "--timeout", "1"])?;
    let json = output.json();
    let load = &json["loads"][0];
    assert!(number(&load["reconnects"])? > 0, "{load}\n{}", output.stderr);
    let counts = step(&json, 0, 0);
    assert!(number(&counts["responses"]["accept"])? > 0, "{load}");
    // The load never stops, so every close finds requests waiting.
    let socket_errors = number(&counts["socket_errors"])?;
    assert!(socket_errors > 0, "requests waiting at a close are socket errors: {counts}");
    assert_eq!(output.code, 2, "socket errors make the exit code 2");
    assert!(output.stderr.contains("RadSec connection ended"), "{}", output.stderr);
    Ok(())
}

const ALL_KINDS: [&str; 4] = ["pap", "chap", "chap-authenticator", "mschapv2"];

/// Whole logins of each method, as a server sees them: accepted, with the keys the TLS
/// session gives, in several rounds.
#[test]
fn test_eap_against_server() -> Result<(), String> {
    for (kind, method, extra) in [("eap-tls", "tls", &["--tls-version", "1.2"][..]), ("eap-tls", "tls", &["--tls-version", "1.3"]), ("peap", "peap", &[]), ("ttls", "ttls", &[])] {
        let server = start_with("return 'accept'", true, Some(method))?;
        let (cert, key, ca) = (server.file("alice.pem"), server.file("alice.key"), server.file("ca.pem"));
        let load = format!("{kind},concurrency=4");
        let mut args = vec!["--load", &load, "--requests", "20", "--user", "alice", "--password", PASSWORD, "--timeout", "2", "--cert-file", &cert, "--key-file", &key, "--ca-file", &ca, "--tls-name", "localhost"];
        args.extend_from_slice(extra);
        let output = bench(&server, &args);
        assert_eq!(output.code, 0, "{kind} {extra:?}: {}\n{}", output.stdout, output.stderr);
        let json = output.json();
        let step = step(&json, 0, 0);
        assert_eq!((step["responses"]["accept"].as_u64(), step["invalid"].as_u64(), step["sent"].as_u64()), (Some(20), Some(0), Some(20)), "{kind} {extra:?}");
        assert_eq!(step["responses"]["challenge"], 0, "{kind}: challenges are rounds of a login, not answers");
        assert!(step["rounds"].as_f64().unwrap() > 1.0, "{kind}: {step}");
        assert_eq!(server.metric(DISCARDED_INVALID)?, 0, "{kind}");
    }
    Ok(())
}

/// 3GPP TS 35.208 test set 1, as every card's keys.
const CARD: &str = "465b5ce8b199b49faa5f0a2ee238a6bc:cd63cb71954a9f4e48a5994e37a02baf";

/// `--metrics-url` reads the names the server writes: what it reports of the server's
/// counters matches the requests sent.
#[test]
fn test_metrics_url_against_server() -> Result<(), String> {
    let server = start("return 'accept'", false)?;
    let url = format!("http://127.0.0.1:{}/metrics", server.metrics_port);
    for (args, auth, acct, invalid, code) in [
        (&["--load", "pap,concurrency=2", "--requests", "10", "--timeout", "2"][..], 10, 0, 0, 0),
        (&["--load", "acct-start,concurrency=2", "--requests", "10", "--timeout", "2"][..], 0, 10, 0, 0),
        (&["--load", "pap,concurrency=1", "--requests", "1", "--timeout", "0.2", "--no-message-authenticator"][..], 0, 0, 1, 2),
    ] {
        let mut all = vec!["--metrics-url", url.as_str()];
        all.extend_from_slice(args);
        let output = bench(&server, &all);
        assert_eq!(output.code, code, "{args:?}: {}\n{}", output.stdout, output.stderr);
        let json = output.json();
        let change = &json["server_metrics"];
        let counted = (number(&change["packets"]["auth"])?, number(&change["packets"]["acct"])?, number(&change["discarded"]["invalid"])?);
        assert_eq!(counted, (auth, acct, invalid), "{args:?}: {change}");
        assert_eq!((number(&change["duplicates"])?, number(&change["discarded"]["overload"])?, number(&change["discarded"]["unknown_client"])?), (0, 0, 0), "{change}");
    }
    // The counters that stay 0 here are still among the server's, under the names read.
    for series in [DUPLICATES, "rradius_radius_discarded_total{reason=\"overload\"}", "rradius_radius_discarded_total{reason=\"unknown_client\"}"] {
        server.metric(series)?;
    }
    Ok(())
}

/// A single-node server offering only EAP-AKA', with `cards` subscribers whose IMSIs are
/// `0010100000000` followed by 0, 1, and so on, all with the keys of `CARD`.
fn start_aka(cards: u32) -> Result<Server, String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (auth_port, acct_port) = (free_port(), free_port());
    let key_path = dir.path().join("aka.key");
    std::fs::write(&key_path, "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\n").map_err(|e| e.to_string())?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let body = format!(
        "[radius]\nbind = \"127.0.0.1\"\nauth_port = {auth_port}\nacct_port = {acct_port}\n\
         [radius.eap]\nmethods = [\"aka-prime\"]\n\
         [radius.eap.aka]\nnetwork_name = \"WLAN\"\nkey_file = \"aka.key\"\n\
         subscriber_query = \"SELECT ki, opc FROM aka_subscribers WHERE imsi = ?\"\n\
         next_sqn_query = \"UPDATE aka_subscribers SET sqn = sqn + 32 WHERE imsi = ? RETURNING sqn\"\n\
         resync_query = \"UPDATE aka_subscribers SET sqn = max(sqn, ?) + 32 WHERE imsi = ? RETURNING sqn\"\n\
         [[radius.clients]]\nname = \"local\"\naddress = \"127.0.0.1\"\nsecret = \"{SECRET}\"\n\
         [accounts.aka]\nadd = \"INSERT INTO aka_subscribers (imsi, ki, opc, sqn) VALUES (?1, ?2, ?3, 0)\"\n",
    );
    let table = ("1_aka.sql", "CREATE TABLE aka_subscribers (imsi TEXT PRIMARY KEY, ki TEXT NOT NULL, opc TEXT NOT NULL, sqn INTEGER NOT NULL);\n".to_string());
    let mut server = Server::spawn(dir, "return 'accept'", &body, &[table])?;
    server.auth = SocketAddr::from(([127, 0, 0, 1], auth_port));
    server.acct_port = acct_port;
    let server = server.answering()?;
    // The server seals each card's Ki and OPc under aka.key as it adds it.
    let (ki, opc) = CARD.split_once(':').ok_or("CARD")?;
    let config = server.file("rradius.toml");
    for index in 0..cards {
        let imsi = format!("0010100000000{index}");
        server.command(&["aka", "add", "--config", &config, &imsi], &format!("{ki}\n{opc}\n"))?;
    }
    Ok(server)
}

/// Whole EAP-AKA' logins against the server: accepted, with the keys of the Challenge.
#[test]
fn test_aka_prime_against_server() -> Result<(), String> {
    if without_enterprise()? {
        return Ok(());
    }
    let server = start_aka(4)?;
    let output = bench(&server, &["--load", "aka-prime,concurrency=4", "--requests", "20", "--user", "60010100000000", "--users", "4", "--password", CARD, "--timeout", "2"]);
    assert_eq!(output.code, 0, "{}\n{}", output.stdout, output.stderr);
    let json = output.json();
    let step = step(&json, 0, 0);
    assert_eq!((step["responses"]["accept"].as_u64(), step["invalid"].as_u64()), (Some(20), Some(0)), "{step}");
    // Identity, AKA-Identity, Challenge.
    assert_eq!(step["rounds"].as_f64(), Some(3.0), "{step}");
    // Another card's keys do not answer the Challenge.
    let wrong = "465b5ce8b199b49faa5f0a2ee238a6bd:cd63cb71954a9f4e48a5994e37a02baf";
    let output = bench(&server, &["--load", "aka-prime,concurrency=1", "--requests", "2", "--user", "60010100000000", "--users", "4", "--password", wrong, "--timeout", "2"]);
    assert_ne!(output.code, 0, "{}", output.stdout);
    let json = output.json();
    assert_eq!(common::step(&json, 0, 0)["responses"]["accept"].as_u64().unwrap_or(0), 0, "{}", output.stdout);
    Ok(())
}

#[test]
fn test_auth_kinds_against_server() -> Result<(), String> {
    let server = start("return 'accept'", true)?;
    for kind in ALL_KINDS {
        let load = format!("{kind},concurrency=4");
        let output = bench(&server, &["--load", &load, "--requests", "20", "--user", "alice", "--password", PASSWORD, "--timeout", "2"]);
        assert_eq!(output.code, 0, "{kind}: {}", output.stdout);
        assert_eq!(step(&output.json(), 0, 0)["responses"]["accept"], 20, "{kind}");
        assert_eq!(step(&output.json(), 0, 0)["rounds"], 1.0, "{kind}: one request each");
        let json = output.json();
        assert_eq!((json["loads"][0]["transport"].as_str(), json["loads"][0]["reconnects"].as_u64()), (Some("udp"), Some(0)), "{kind}");

        let output = bench(&server, &["--load", &load, "--requests", "20", "--user", "alice", "--password", "wrong", "--timeout", "2"]);
        assert_eq!(output.code, 1, "{kind}: {}", output.stdout);
        assert_eq!(step(&output.json(), 0, 0)["responses"]["reject"], 20, "{kind}");
    }
    assert_eq!(server.metric(DUPLICATES)?, 0);
    assert_eq!(server.metric(DISCARDED_INVALID)?, 0);
    Ok(())
}

#[test]
fn test_accounting_and_status_against_server() -> Result<(), String> {
    let server = start("return 'accept'", false)?;
    for kind in ["acct-start", "acct-alive", "acct-stop", "acct-on", "acct-off", "status-acct"] {
        let load = format!("{kind},concurrency=4");
        let output = bench(&server, &["--load", &load, "--requests", "20", "--timeout", "2"]);
        assert_eq!(output.code, 0, "{kind}: {}", output.stdout);
        assert_eq!(step(&output.json(), 0, 0)["responses"]["accounting"], 20, "{kind}");
    }
    let output = bench(&server, &["--load", "status-auth,concurrency=4", "--requests", "20", "--timeout", "2"]);
    assert_eq!(output.code, 0, "{}", output.stdout);
    assert_eq!(step(&output.json(), 0, 0)["responses"]["accept"], 20);
    assert_eq!(server.metric(DISCARDED_INVALID)?, 0);
    Ok(())
}

#[test]
fn test_accepted_under_default_policy() -> Result<(), String> {
    let server = start("return 'accept'", false)?;
    let accepted = bench(&server, &["--load", "pap,concurrency=1", "--requests", "1"]);
    assert_eq!(accepted.code, 0, "{}", accepted.stdout);
    Ok(())
}

#[test]
fn test_policy_test_times_out() -> Result<(), String> {
    let server = start("return 'accept'", false)?;
    let before = server.metric(DISCARDED_INVALID)?;
    let refused = bench(&server, &["--load", "pap,concurrency=1", "--requests", "1", "--timeout", "0.2", "--no-message-authenticator"]);
    assert_eq!(refused.code, 2, "{}", refused.stdout);
    assert_eq!(step(&refused.json(), 0, 0)["timeouts"], 1);
    assert_eq!(server.metric(DISCARDED_INVALID)?, before + 1);
    Ok(())
}

#[test]
fn test_identifier_wrap_stays_off_duplicate_path() -> Result<(), String> {
    let server = start("return 'accept'", false)?;
    let duplicates = server.metric(DUPLICATES)?;
    let output = bench(&server, &["--load", "acct-start,concurrency=1", "--requests", "600", "--timeout", "2"]);
    assert_eq!(output.code, 0, "{}", output.stdout);
    assert_eq!(server.metric(DUPLICATES)?, duplicates);
    assert_eq!(server.metric(PACKETS_ACCT)?, 600);
    Ok(())
}

#[test]
fn test_session_against_server() -> Result<(), String> {
    // Accounting is only answered when it carries what the Access-Accept gave, as an
    // accounting script that fails gets no response.
    let script = "\
        if phase == 'accounting' then \
          if get_attr('Class') ~= 'gold' or get_attr('Framed-IP-Address') ~= '10.0.0.9' then error('not copied') end \
          return 'accept' \
        end \
        set_attr('Class', 'gold') set_attr('Framed-IP-Address', '10.0.0.9') \
        return 'accept'";
    let server = start(script, false)?;
    let output = bench(&server, &["--load", "session,concurrency=4,alive=2", "--requests", "20", "--timeout", "2"]);
    assert_eq!(output.code, 0, "{}", output.stdout);
    let json = output.json();
    assert_eq!(step(&json, 0, 0)["responses"]["accept"], 20);
    assert_eq!(step(&json, 0, 1)["responses"]["accounting"], 20);
    assert_eq!(step(&json, 0, 2)["responses"]["accounting"], 40);
    assert_eq!(step(&json, 0, 3)["responses"]["accounting"], 20);
    assert_eq!(server.metric(DUPLICATES)?, 0);
    Ok(())
}

/// More Alive requests than Identifiers on one socket: only their contents can tell them apart.
#[test]
fn test_long_session_stays_off_the_duplicate_path() -> Result<(), String> {
    let server = start("return 'accept'", false)?;
    let output = bench(&server, &["--load", "session,concurrency=1,alive=300", "--requests", "1", "--timeout", "2"]);
    assert_eq!(output.code, 0, "{}", output.stdout);
    assert_eq!(step(&output.json(), 0, 2)["responses"]["accounting"], 300);
    assert_eq!(server.metric(DUPLICATES)?, 0);
    Ok(())
}

/// Starts a TACACS+ server whose script decides everything. With `chain`, an `sql` handler
/// authenticates "alice" with `PASSWORD` instead.
fn start_tacacs(script: &str, chain: bool) -> Result<Server, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let port = free_port();
    let auth = if chain { SQL_CHAIN } else { "" };
    let body = format!(
        "{auth}\
         [tacacs]\nbind = \"127.0.0.1\"\nport = {port}\n\
         [[tacacs.clients]]\nname = \"local\"\naddress = \"127.0.0.1\"\nsecret = \"{SECRET}\"\n",
    );
    let migrations = if chain { vec![users()] } else { Vec::new() };
    let mut server = Server::spawn(dir, script, &body, &migrations)?;
    server.tacacs_port = Some(port);
    let server = server.listening(port)?;
    // The connection made just now counts; tests read the counter from here.
    std::thread::sleep(Duration::from_millis(50));
    Ok(server)
}

/// Runs the tool against the TACACS+ port.
fn bench_tacacs(server: &Server, args: &[&str]) -> Result<Output, String> {
    let port = server.tacacs_port.ok_or("no TACACS+ port")?.to_string();
    let mut all: Vec<String> = ["run", "--json", "--server", "127.0.0.1", "--secret", SECRET, "--tacacs-port", &port, "--timeout", "5"].iter().map(|s| s.to_string()).collect();
    all.extend(args.iter().map(|arg| arg.to_string()));
    Ok(common::bench(&all))
}

/// Every TACACS+ kind against a server that accepts it: only accepts or accounting.
fn check_tacacs_loads(server: &Server, connect: &str) -> Result<(), String> {
    for kind in ["tacacs-pap", "tacacs-ascii", "tacacs-author", "tacacs-acct"] {
        let load = format!("{kind},concurrency=4");
        let output = bench_tacacs(server, &["--load", &load, "--requests", "40", "--tacacs-connect", connect])?;
        let json = output.json();
        let counts = step(&json, 0, 0);
        let (good, other) = if kind == "tacacs-acct" { ("accounting", "accept") } else { ("accept", "accounting") };
        let checks = [
            (output.code == 0, "exit code 0"),
            (counts["responses"][good] == 40, "40 good responses"),
            (counts["responses"][other] == 0 && counts["responses"]["reject"] == 0, "nothing else"),
            (counts["invalid"] == 0 && counts["timeouts"] == 0 && counts["socket_errors"] == 0, "no failures"),
            (json["loads"][0]["transport"] == "tcp" && json["loads"][0]["sockets"] == 4, "transport tcp, 4 sockets"),
            (counts["rounds"] == if kind == "tacacs-ascii" { 3.0 } else { 1.0 }, "rounds"),
            (json["loads"][0]["reconnects"] == 0, "no reconnects"),
        ];
        for (ok, what) in checks {
            if !ok {
                return Err(format!("{kind} {connect}: {what}: {} / {}", output.stdout, output.stderr));
            }
        }
    }
    Ok(())
}

#[test]
fn test_tacacs_loads_keep() -> Result<(), String> {
    let server = start_tacacs("return 'accept'", false)?;
    check_tacacs_loads(&server, "keep")?;
    let before = server.metric(TACACS_ACCEPTED)?;
    let output = bench_tacacs(&server, &["--load", "tacacs-pap,concurrency=8", "--requests", "80"])?;
    let opened = server.metric(TACACS_ACCEPTED)? - before;
    if output.code != 0 || opened != 8 || output.json()["loads"][0]["sockets"] != 8 {
        return Err(format!("keep: {opened} connections for 8 sockets: {} / {}", output.stdout, output.stderr));
    }
    Ok(())
}

#[test]
fn test_tacacs_loads_per_request() -> Result<(), String> {
    let server = start_tacacs("return 'accept'", false)?;
    check_tacacs_loads(&server, "per-request")?;
    let before = server.metric(TACACS_ACCEPTED)?;
    let output = bench_tacacs(&server, &["--load", "tacacs-pap,concurrency=4", "--requests", "20", "--tacacs-connect", "per-request"])?;
    let opened = server.metric(TACACS_ACCEPTED)? - before;
    if output.code != 0 || opened != 20 {
        return Err(format!("per-request: {opened} connections for 20 requests: {} / {}", output.stdout, output.stderr));
    }
    Ok(())
}

#[test]
fn test_tacacs_wrong_password_rejected() -> Result<(), String> {
    let server = start_tacacs("return 'reject'", true)?;
    for kind in ["tacacs-pap", "tacacs-ascii"] {
        let load = format!("{kind},concurrency=2");
        let right = bench_tacacs(&server, &["--load", &load, "--requests", "4", "--user", "alice", "--password", PASSWORD])?;
        if right.code != 0 || step(&right.json(), 0, 0)["responses"]["accept"] != 4 {
            return Err(format!("{kind} right password: exit {}: {} / {}", right.code, right.stdout, right.stderr));
        }
        let wrong = bench_tacacs(&server, &["--load", &load, "--requests", "4", "--user", "alice", "--password", "wrong"])?;
        let json = wrong.json();
        let counts = step(&json, 0, 0);
        if wrong.code != 1 || counts["responses"]["reject"] != 4 || counts["responses"]["accept"] != 0 || counts["invalid"] != 0 {
            return Err(format!("{kind} wrong password: exit {}: {} / {}", wrong.code, wrong.stdout, wrong.stderr));
        }
    }
    Ok(())
}

#[test]
fn test_tacacs_wrong_secret_invalid() -> Result<(), String> {
    let server = start_tacacs("return 'accept'", false)?;
    let port = server.tacacs_port.ok_or("no TACACS+ port")?.to_string();
    let all: Vec<String> = ["run", "--json", "--server", "127.0.0.1", "--secret", "not-the-secret", "--tacacs-port", &port, "--timeout", "5", "--load", "tacacs-pap,concurrency=2", "--requests", "10"].iter().map(|s| s.to_string()).collect();
    let output = common::bench(&all);
    let json = output.json();
    let counts = step(&json, 0, 0);
    // The server closes a connection after a body it cannot decode (RFC 8907 section 10.5.2),
    // so a request sent before the tool sees the close ends as a socket error instead.
    let (invalid, lost) = (counts["invalid"].as_u64().unwrap_or(0), counts["socket_errors"].as_u64().unwrap_or(0));
    if output.code != 2 || invalid == 0 || invalid + lost != 10 || counts["responses"]["accept"] != 0 || counts["responses"]["reject"] != 0 {
        return Err(format!("exit {}: {} / {}", output.code, output.stdout, output.stderr));
    }
    if output.stderr.matches("tacacs-pap: lengths").count() != 1 {
        return Err(format!("the error should be printed once: {}", output.stderr));
    }
    Ok(())
}
