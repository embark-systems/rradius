//! Running `rradius-bench` against a server started from its executable, for the tests of the
//! programs built on this library. The load generator only sends: the tests start the server,
//! wait for it, and read its metrics; the tool gets the address and the ports as arguments.

use super::server::free_port;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The RADIUS secret of the test servers' clients.
pub const SECRET: &str = "test-secret";
/// alice's password in `users()`.
pub const PASSWORD: &str = "a-test-password";
/// How long a server may take to start answering.
const START_WAIT: Duration = Duration::from_secs(10);

/// The `[auth]` section with an `sql` handler that looks up `users`.
pub const SQL_CHAIN: &str = "[auth]\n[[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users WHERE username = ?\"\n";

/// The load generator: `RRADIUS_BENCH_BIN` when set, or else `rradius-bench` next to
/// `program`, where a build of the same workspace puts it.
pub fn bench_binary(program: &Path) -> Result<PathBuf, String> {
    let path = match std::env::var_os("RRADIUS_BENCH_BIN") {
        Some(path) => PathBuf::from(path),
        None => program.with_file_name("rradius-bench"),
    };
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("no rradius-bench at {}: run `cargo build -p rradius-bench` first, or set RRADIUS_BENCH_BIN", path.display()))
    }
}

/// What a run of the tool gave.
pub struct Output {
    /// The exit code; -1 when the tool could not be run or was killed by a signal, with the
    /// reason in `stderr`.
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// The JSON report on standard output. A test that asks for it needs it, so it fails
    /// there, with what the tool printed, when there is none.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| panic!("{e}: {} / {}", self.stdout, self.stderr))
    }
}

/// Runs the tool at `bench` with `args`.
pub fn run(bench: &Path, args: &[String]) -> Output {
    match Command::new(bench).args(args).output() {
        Ok(output) => Output {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(e) => Output { code: -1, stdout: String::new(), stderr: format!("cannot run {}: {e}", bench.display()) },
    }
}

/// The counts of step `step` of load `load` in a report.
pub fn step(json: &Value, load: usize, step: usize) -> &Value {
    &json["loads"][load]["steps"][step]
}

/// A count in a report.
pub fn number(value: &Value) -> Result<u64, String> {
    value.as_u64().ok_or_else(|| format!("not a count: {value}"))
}

/// The migration that makes `users`, with alice and `PASSWORD`.
pub fn users() -> (&'static str, String) {
    ("1_users.sql", format!("CREATE TABLE users (username TEXT, password TEXT);\nINSERT INTO users VALUES ('alice', '{PASSWORD}');\n"))
}

/// Writes `<name>.pem` and `<name>.key`, signed by `issuer`, for `names`.
fn leaf(dir: &Path, name: &str, names: &[&str], issuer: &rcgen::Issuer<'_, rcgen::KeyPair>) -> Result<(), String> {
    let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let params = rcgen::CertificateParams::new(names.iter().map(|name| name.to_string()).collect::<Vec<_>>()).map_err(|e| e.to_string())?;
    let certificate = params.signed_by(&key, issuer).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{name}.pem")), certificate.pem()).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).map_err(|e| e.to_string())
}

/// A CA, the server's certificate for `localhost`, and alice's.
pub fn eap_certificates(dir: &Path) -> Result<(), String> {
    let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = params.clone().self_signed(&key).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("ca.pem"), ca.pem()).map_err(|e| e.to_string())?;
    let issuer = rcgen::Issuer::new(params, key);
    // The address too, as what `--tls-name` is unless given.
    leaf(dir, "server", &["localhost", "127.0.0.1"], &issuer)?;
    leaf(dir, "alice", &["alice"], &issuer)
}

/// A single-node server running from its executable, killed when dropped.
pub struct Server {
    child: Child,
    /// Holds the configuration, the database and what the server writes to standard error.
    /// Declared after `child`, so that it goes once the server has.
    dir: tempfile::TempDir,
    /// The executable it runs, which its commands run too.
    program: PathBuf,
    /// The load generator run against it.
    bench: PathBuf,
    /// The RADIUS authentication address; port 0 without `[radius]`.
    pub auth: SocketAddr,
    /// 0 without `[radius]`.
    pub acct_port: u16,
    pub metrics_port: u16,
    /// With `[radius.tls]`.
    pub radsec_port: Option<u16>,
    /// With `[tacacs]`.
    pub tacacs_port: Option<u16>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// Writes `script`, `migrations` and `rradius.toml` (the parts every test server has,
    /// then `body`) into `dir`, and starts `program` on them; `bench` is the load generator
    /// the test runs against it.
    pub fn spawn(program: &Path, bench: &Path, dir: tempfile::TempDir, script: &str, body: &str, migrations: &[(&str, String)]) -> Result<Server, String> {
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
            dictionary = super::dictionary().display(),
            raft = free_port(),
            api = free_port(),
        );
        let path = dir.path().join("rradius.toml");
        std::fs::write(&path, config).map_err(|e| e.to_string())?;
        let stderr = std::fs::File::create(dir.path().join("server.stderr")).map_err(|e| e.to_string())?;
        let child = Command::new(program).arg("server").arg("--config").arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn().map_err(|e| format!("starting the server: {e}"))?;
        Ok(Server { child, dir, program: program.to_path_buf(), bench: bench.to_path_buf(), auth: SocketAddr::from(([127, 0, 0, 1], 0)), acct_port: 0, metrics_port, radsec_port: None, tacacs_port: None })
    }

    /// What the server has written to standard error so far.
    pub fn stderr(&self) -> String {
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
    pub fn answering(mut self) -> Result<Server, String> {
        let started = Instant::now();
        while self.bench(&["--load", "status-auth,concurrency=1", "--requests", "1", "--timeout", "0.2"]).code != 0 {
            self.still_starting(started)?;
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(self)
    }

    /// Returns the server once `port` takes TCP connections.
    pub fn listening(mut self, port: u16) -> Result<Server, String> {
        self.wait_for_port(port)?;
        Ok(self)
    }

    /// Returns once `port` takes TCP connections.
    pub fn wait_for_port(&mut self, port: u16) -> Result<(), String> {
        let started = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            self.still_starting(started)?;
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// The value of `series`, the whole name with its labels, read from the metrics port.
    pub fn metric(&self, series: &str) -> Result<u64, String> {
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
    pub fn file(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    /// Runs the server executable with `args`, `stdin` on standard input. Returns what it
    /// printed, or what it said on standard error when it fails.
    pub fn command(&self, args: &[&str], stdin: &str) -> Result<String, String> {
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

    /// Runs the tool with `args` as they are.
    pub fn tool(&self, args: &[String]) -> Output {
        run(&self.bench, args)
    }

    /// Runs the tool against the server's RADIUS ports, with `args` after these.
    pub fn bench(&self, args: &[&str]) -> Output {
        let (auth_port, acct_port) = (self.auth.port().to_string(), self.acct_port.to_string());
        let mut all: Vec<String> = ["run", "--json", "--server", "127.0.0.1", "--secret", SECRET, "--auth-port", &auth_port, "--acct-port", &acct_port].iter().map(|s| s.to_string()).collect();
        all.extend(args.iter().map(|arg| arg.to_string()));
        self.tool(&all)
    }
}

/// Starts a single-node RADIUS server from `program`, answering once it does. With `chain`,
/// an `sql` handler authenticates alice. With `method`, `[radius.eap]` offers only it; with
/// `radsec`, a RadSec port as well, `[radius.tls]` with `radsec` added to it. The
/// certificates, when either is given, are in the server's directory: `ca.pem`, and
/// `alice.pem` and `alice.key` for EAP-TLS and RadSec clients.
pub fn start_full(program: &Path, bench: &Path, script: &str, chain: bool, method: Option<&str>, radsec: Option<&str>) -> Result<Server, String> {
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
            eap_certificates(dir.path())?;
            let client_ca = if method == "tls" { "client_ca_file = \"ca.pem\"\n" } else { "" };
            format!("{body}[radius.eap]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\n{client_ca}methods = [\"{method}\"]\n")
        }
        None => body,
    };
    let radsec_port = radsec.map(|_| free_port());
    let body = match (radsec, radsec_port) {
        (Some(tls), Some(port)) => {
            if method.is_none() {
                eap_certificates(dir.path())?;
            }
            format!("{body}[radius.tls]\nbind = \"127.0.0.1\"\nport = {port}\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n{tls}")
        }
        _ => body,
    };
    let migrations = if chain { vec![users()] } else { Vec::new() };
    let mut server = Server::spawn(program, bench, dir, script, &body, &migrations)?;
    server.auth = SocketAddr::from(([127, 0, 0, 1], auth_port));
    server.acct_port = acct_port;
    server.radsec_port = radsec_port;
    server.answering()
}
