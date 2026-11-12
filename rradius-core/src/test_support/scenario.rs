//! The scenarios of the user manual started as the manual starts them, for the tests of a
//! program: a server from its executable on a copy of a scenario's files, and what those tests
//! share around it. A test names the program and the directory of its scenarios with
//! `Scenarios`.

use super::server::free_port;
use crate::radius::RadiusPacket;
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const ACCESS_ACCEPT: u8 = 2;
pub const ACCESS_REJECT: u8 = 3;
pub const ACCOUNTING_RESPONSE: u8 = 5;
/// Settings whose value is a file or directory, made absolute. `data_dir` and the spool's
/// `dir` stay relative, so they land in the temporary directory.
pub const PATH_KEYS: &[&str] = &[
    "dictionary",
    "script",
    "migrations_dir",
    "secret_file",
    "secret_raft_file",
    "secret_api_file",
    "bind_password_file",
    "cert_file",
    "key_file",
    "client_ca_file",
    "crl_file",
    "ca_file",
    "previous_key_file",
    "ticket_seal_file",
    "previous_ticket_seal_file",
];

/// Sections whose ports are this server's own.
pub const OWN_PORTS: &[&str] = &["radius", "tacacs", "tacacs.tls", "metrics", "radius.tls", "radius.coa"];

/// The program a test runs and the directory that holds its scenarios, one directory each.
#[derive(Clone, Copy, Debug)]
pub struct Scenarios {
    /// The executable, as `env!("CARGO_BIN_EXE_<name>")` names it.
    pub program: &'static str,
    /// The directory that holds the scenarios, such as `<crate>/examples/scenarios`.
    pub root: &'static str,
}

impl Scenarios {
    /// The directory of scenario `name`.
    pub fn dir(&self, name: &str) -> PathBuf {
        Path::new(self.root).join(name)
    }
}

/// A scenario's configuration file, rewritten to run from a temporary directory. Cluster node
/// addresses go to 127.0.0.1 through `nodes`, which nodes of one cluster share so that they
/// agree. Returns the text and the ports given out, by `section.key`.
pub fn rewrite(scenario: &Path, file: &str, nodes: &mut HashMap<String, String>) -> (String, HashMap<String, u16>) {
    let text = std::fs::read_to_string(scenario.join(file)).unwrap();
    let mut section = String::new();
    let mut ports = HashMap::new();
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.trim_matches(|c| c == '[' || c == ']').to_string();
        }
        let Some((key, value)) = trimmed.split_once('=').filter(|_| !trimmed.starts_with('#')) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let quoted = value.strip_prefix('"').and_then(|rest| rest.split_once('"')).map(|(inner, _)| inner);
        if key == "dictionary" {
            // The core's, as a repository built on the core has no `share` of its own.
            out.push_str(&format!("dictionary = \"{}\"\n", super::dictionary().display()));
        } else if let (true, Some(path)) = (PATH_KEYS.contains(&key), quoted) {
            let absolute = if Path::new(path).is_absolute() { PathBuf::from(path) } else { scenario.join(path) };
            out.push_str(&format!("{key} = \"{}\"\n", absolute.display()));
        } else if OWN_PORTS.contains(&section.as_str()) && ["port", "auth_port", "acct_port"].contains(&key) {
            let port = free_port();
            ports.insert(format!("{section}.{key}"), port);
            out.push_str(&format!("{key} = {port}\n"));
        } else if section == "cluster.nodes" && (key == "raft" || key == "api") {
            let address = nodes.entry(quoted.unwrap().to_string()).or_insert_with(|| format!("127.0.0.1:{}", free_port()));
            out.push_str(&format!("{key} = \"{address}\"\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    // A single node takes 8100 and 8200 by default, which tests running at once would share.
    // Listing the node also moves `listen` to 0.0.0.0, so keep it where the default had it.
    if !text.contains("[[cluster.nodes]]") {
        out = out.replacen("[cluster]\n", "[cluster]\nlisten = \"127.0.0.1\"\n", 1);
        out.push_str(&format!("\n[[cluster.nodes]]\nid = 1\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n", free_port(), free_port()));
    }
    (out, ports)
}

/// A server started from a scenario, killed when the test ends.
pub struct Server {
    pub child: Child,
    pub dir: tempfile::TempDir,
    pub ports: HashMap<String, u16>,
    pub secret: Vec<u8>,
}

impl Server {
    /// Starts `file` of scenario `name`, after `change` has had its say on the rewritten text,
    /// and waits until it serves.
    pub fn start(scenarios: Scenarios, name: &str, file: &str, change: impl FnOnce(String) -> String) -> Self {
        let mut server = Self::spawn(scenarios, name, file, &mut HashMap::new(), change);
        server.wait_ready();
        server
    }

    /// Starts it without waiting: a cluster node waits for the others.
    pub fn spawn(scenarios: Scenarios, name: &str, file: &str, nodes: &mut HashMap<String, String>, change: impl FnOnce(String) -> String) -> Self {
        let scenario = scenarios.dir(name);
        let dir = tempfile::tempdir().unwrap();
        let (text, ports) = rewrite(&scenario, file, nodes);
        let config = dir.path().join("rradius.toml");
        std::fs::write(&config, change(text)).unwrap();
        let check = Command::new(scenarios.program).args(["--check", "--config", config.to_str().unwrap()]).output().unwrap();
        assert!(check.status.success(), "{name}/{file}: {}", String::from_utf8_lossy(&check.stderr));
        let stderr = std::fs::File::create(dir.path().join("stderr.log")).unwrap();
        let child = Command::new(scenarios.program)
            .args(["--config", config.to_str().unwrap()])
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        Self { child, dir, ports, secret: Vec::new() }
    }

    pub fn port(&self, key: &str) -> u16 {
        *self.ports.get(key).unwrap_or_else(|| panic!("no {key} in {:?}", self.ports))
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("stderr.log")).unwrap_or_default()
    }

    /// Waits for the log line that says the RADIUS ports are open, after the migrations.
    pub fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("exited with {status}: {}", self.stderr());
            }
            let ready = if self.ports.contains_key("radius.auth_port") { "RADIUS Server listening" } else { "TACACS+ Server listening" };
            if self.stderr().contains(ready) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("not ready within 60 s: {}", self.stderr());
    }

    /// Sends `packet` to `port` until an answer comes, at most a few times.
    pub fn exchange(&self, port: u16, packet: &[u8]) -> Option<RadiusPacket> {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(1000))).unwrap();
        let mut buffer = [0u8; 4096];
        for _ in 0..5 {
            socket.send_to(packet, ("127.0.0.1", port)).unwrap();
            if let Ok((size, _)) = socket.recv_from(&mut buffer) {
                return Some(RadiusPacket::parse(&buffer[..size]).unwrap());
            }
        }
        None
    }

    pub fn login(&self, attributes: Vec<(u8, Vec<u8>)>, authenticator: [u8; 16]) -> RadiusPacket {
        let packet = RadiusPacket::create_access_request(next_id(), authenticator, attributes, &self.secret);
        self.exchange(self.port("radius.auth_port"), &packet).unwrap_or_else(|| panic!("no answer: {}", self.stderr()))
    }

    /// A PAP login.
    pub fn pap(&self, user: &str, password: &str) -> RadiusPacket {
        let authenticator = random_authenticator();
        self.login(vec![(1, user.as_bytes().to_vec()), (2, hide_password(password.as_bytes(), &self.secret, &authenticator))], authenticator)
    }

    /// A CHAP login, with the Request Authenticator as the challenge.
    pub fn chap(&self, user: &str, password: &str) -> RadiusPacket {
        let authenticator = random_authenticator();
        let ident = 7u8;
        let mut hash = Md5::new();
        hash.update([ident]);
        hash.update(password.as_bytes());
        hash.update(authenticator);
        let mut value = vec![ident];
        value.extend_from_slice(&hash.finalize());
        self.login(vec![(1, user.as_bytes().to_vec()), (3, value)], authenticator)
    }

    pub fn accounting(&self, attributes: Vec<(u8, Vec<u8>)>) -> Option<RadiusPacket> {
        let packet = RadiusPacket::create_accounting_request(next_id(), attributes, &self.secret);
        self.exchange(self.port("radius.acct_port"), &packet)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn next_id() -> u8 {
    static NEXT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn random_authenticator() -> [u8; 16] {
    let mut authenticator = [0u8; 16];
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    authenticator.copy_from_slice(&Md5::digest(format!("{nanos}-{}", next_id()).as_bytes()));
    authenticator
}

/// User-Password as RFC 2865 section 5.2 hides it.
pub fn hide_password(password: &[u8], secret: &[u8], authenticator: &[u8; 16]) -> Vec<u8> {
    let mut padded = password.to_vec();
    padded.resize(password.len().div_ceil(16).max(1) * 16, 0);
    let mut out = Vec::new();
    let mut previous = authenticator.to_vec();
    for block in padded.chunks(16) {
        let key = Md5::new().chain_update(secret).chain_update(&previous).finalize();
        let hidden: Vec<u8> = block.iter().zip(key.iter()).map(|(a, b)| a ^ b).collect();
        previous = hidden.clone();
        out.extend(hidden);
    }
    out
}

pub fn text(packet: &RadiusPacket, attr_type: u8) -> Option<String> {
    packet.attribute(attr_type).map(|value| String::from_utf8_lossy(value).into_owned())
}

pub fn number(packet: &RadiusPacket, attr_type: u8) -> Option<u32> {
    packet.attribute(attr_type).map(|value| u32::from_be_bytes(value.try_into().unwrap()))
}

pub fn secret_of(scenarios: Scenarios, name: &str, file: &str) -> Vec<u8> {
    let text = std::fs::read(scenarios.dir(name).join(file)).unwrap();
    text.strip_suffix(b"\n").unwrap_or(&text).to_vec()
}

pub fn session(user: &str, id: &str, status: u32, extra: Vec<(u8, Vec<u8>)>) -> Vec<(u8, Vec<u8>)> {
    let mut attributes = vec![(1, user.as_bytes().to_vec()), (44, id.as_bytes().to_vec()), (40, status.to_be_bytes().to_vec())];
    attributes.extend(extra);
    attributes
}

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Waits up to `seconds` for `done`, looking five times a second.
pub fn wait_for(seconds: u64, what: &str, mut done: impl FnMut() -> bool) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if done() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("{what}: not within {seconds} s").into())
}

/// The value of one sample of `/metrics`, by the text before its value; 0 when it is not there.
pub fn metric(server: &Server, sample: &str) -> Result<f64, Box<dyn std::error::Error>> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", server.port("metrics.port")))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut body = String::new();
    stream.read_to_string(&mut body)?;
    let Some(line) = body.lines().find(|line| line.strip_prefix(sample).is_some_and(|rest| rest.starts_with(' '))) else {
        return Ok(0.0);
    };
    Ok(line[sample.len()..].trim().parse()?)
}

pub type TlsStream = rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>;

/// The demonstration certificates of a scenario, made by its own `script` in a new directory.
pub fn make_certs(scenarios: Scenarios, name: &str, script: &str) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let certs = tempfile::tempdir()?;
    let made = Command::new("sh").arg(scenarios.dir(name).join(script)).arg(certs.path()).output()?;
    if !made.status.success() {
        return Err(format!("{name}/{script}: {}", String::from_utf8_lossy(&made.stderr)).into());
    }
    Ok(certs)
}

/// A TLS connection to `port` as a device holding `client.pem` and `client.key` of `certs`,
/// that checks the server's certificate against `ca` there and `server_name`.
pub fn tls_connect(certs: &Path, ca: &str, port: u16, server_name: &str, client: &str, versions: &[&'static rustls::SupportedProtocolVersion]) -> Result<TlsStream, Box<dyn std::error::Error>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_file(certs.join(ca))?)?;
    let certificate = CertificateDer::from_pem_file(certs.join(format!("{client}.pem")))?;
    let key = PrivateKeyDer::from_pem_file(certs.join(format!("{client}.key")))?;
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(versions)?
        .with_root_certificates(roots)
        .with_client_auth_cert(vec![certificate], key)?;
    let connection = rustls::ClientConnection::new(std::sync::Arc::new(config), ServerName::try_from(server_name.to_string())?)?;
    let socket = std::net::TcpStream::connect(("127.0.0.1", port))?;
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    Ok(rustls::StreamOwned::new(connection, socket))
}

/// Rewrites the configuration a running server was started from and has it read again with
/// SIGHUP, as chapter 19 tells people to.
pub fn reload(server: &Server, change: impl FnOnce(String) -> String) -> TestResult {
    let config = server.dir.path().join("rradius.toml");
    std::fs::write(&config, change(std::fs::read_to_string(&config)?))?;
    let reloads = |server: &Server| server.stderr().matches("configuration reloaded").count();
    let before = reloads(server);
    let sent = Command::new("kill").args(["-HUP", &server.child.id().to_string()]).status()?;
    if !sent.success() {
        return Err("kill -HUP failed".into());
    }
    wait_for(20, "the reload", || reloads(server) > before).map_err(|e| format!("{e}: {}", server.stderr()).into())
}

/// One of the server's own commands (`user`, `dpsk-seal`, `aka-seal`), given `stdin`: what it
/// printed, or what it complained of.
pub fn command(program: &str, args: &[&str], stdin: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut child = Command::new(program).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let Some(mut input) = child.stdin.take() else { return Err("no standard input".into()) };
    std::io::Write::write_all(&mut input, stdin.as_bytes())?;
    drop(input);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!("{args:?}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The path of the configuration a server runs from, as text.
pub fn config_of(server: &Server) -> Result<String, Box<dyn std::error::Error>> {
    Ok(server.dir.path().join("rradius.toml").to_str().ok_or("a path that is not UTF-8")?.to_string())
}

/// The VLAN of a reply: Tunnel-Private-Group-Id with tag 1.
pub fn vlan(packet: &RadiusPacket) -> Option<String> {
    let value = packet.attribute(81)?;
    (value.first() == Some(&1)).then(|| String::from_utf8_lossy(&value[1..]).into_owned())
}

/// What a scenario's `setup.sh` makes (a key file and the demonstration rows sealed under it),
/// in a new directory that also has the scenario's own migrations. The script runs the commands of
/// the program of `scenarios`.
pub fn set_up(scenarios: Scenarios, name: &str) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir(dir.path().join("migrations"))?;
    for entry in std::fs::read_dir(scenarios.dir(name).join("migrations"))? {
        let entry = entry?;
        std::fs::copy(entry.path(), dir.path().join("migrations").join(entry.file_name()))?;
    }
    let made = Command::new("sh").arg(scenarios.dir(name).join("setup.sh")).arg(dir.path()).env("RRADIUS_SERVER", scenarios.program).output()?;
    if !made.status.success() {
        return Err(format!("{name}/setup.sh: {}", String::from_utf8_lossy(&made.stderr)).into());
    }
    Ok(dir)
}

/// A scenario's configuration with the files of `set_up` in place of the scenario's own.
pub fn with_set_up(scenarios: Scenarios, text: String, name: &str, made: &Path, key: &str) -> String {
    let ours = scenarios.dir(name);
    text.replace(&ours.join(key).display().to_string(), &made.join(key).display().to_string()).replace(&ours.join("migrations").display().to_string(), &made.join("migrations").display().to_string())
}

/// What a device with the certificate `name` of `certs` brings to EAP-TLS. Logins that share
/// one of these share its TLS sessions, as one device's logins do.
pub fn device_tls(certs: &Path, name: &str) -> Result<std::sync::Arc<rustls::ClientConfig>, Box<dyn std::error::Error>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_file(certs.join("ca.pem"))?)?;
    let certificate = CertificateDer::from_pem_file(certs.join(format!("{name}.pem")))?;
    let key = PrivateKeyDer::from_pem_file(certs.join(format!("{name}.key")))?;
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions()?.with_root_certificates(roots).with_client_auth_cert(vec![certificate], key)?;
    Ok(std::sync::Arc::new(config))
}

/// An EAP-TLS login as `identity`, to the packet that ends it, and whether the TLS handshake
/// was a full one or resumed an earlier session.
pub fn eap_tls_login(server: &Server, device: &std::sync::Arc<rustls::ClientConfig>, identity: &str) -> Result<(RadiusPacket, Option<rustls::HandshakeKind>), Box<dyn std::error::Error>> {
    use crate::eap::{encode_tls_fragment, join_message, parse_tls_fragment, split_message, EapPacket};
    const EAP_TLS: u8 = 13;
    let mut connection = rustls::ClientConnection::new(device.clone(), rustls::pki_types::ServerName::try_from("radius.corp.example.com")?)?;
    let mut eap = EapPacket { code: 2, id: 0, body: [&[1u8][..], identity.as_bytes()].concat() }.encode();
    let mut state: Option<Vec<u8>> = None;
    // TLS records waiting to go out, a fragment at a time.
    let mut outgoing: Vec<u8> = Vec::new();
    for _ in 0..40 {
        let mut attributes = vec![(1, identity.as_bytes().to_vec()), (31, b"02-00-00-00-02-01".to_vec())];
        attributes.extend(split_message(&eap));
        attributes.extend(state.take().map(|state| (24, state)));
        let answer = server.login(attributes, random_authenticator());
        if answer.code != 11 {
            return Ok((answer, connection.handshake_kind()));
        }
        state = answer.attribute(24).map(<[u8]>::to_vec);
        let request = join_message(answer.attributes.iter().filter(|(kind, _)| *kind == 79).map(|(_, value)| value.as_slice()));
        let packet = EapPacket::parse(&request).ok_or("the challenge carries no EAP packet")?;
        if packet.body.first() != Some(&EAP_TLS) {
            return Err(format!("EAP type {:?} offered", packet.body.first()).into());
        }
        let fragment = parse_tls_fragment(&packet.body[1..]).ok_or("not an EAP-TLS fragment")?;
        let mut data = fragment.data;
        while !data.is_empty() {
            connection.read_tls(&mut data)?;
            // An alert from the server ends the handshake; its Access-Reject follows.
            if connection.process_new_packets().is_err() {
                break;
            }
        }
        let reply = if fragment.more {
            encode_tls_fragment(&[], None, false)
        } else {
            let first = outgoing.is_empty();
            while first && connection.wants_write() {
                connection.write_tls(&mut outgoing)?;
            }
            let size = outgoing.len().min(900);
            let total = (first && outgoing.len() > size).then_some(outgoing.len() as u32);
            let chunk: Vec<u8> = outgoing.drain(..size).collect();
            encode_tls_fragment(&chunk, total, !outgoing.is_empty())
        };
        eap = EapPacket { code: 2, id: packet.id, body: [&[EAP_TLS][..], &reply].concat() }.encode();
    }
    Err("the login did not end".into())
}

/// One TACACS+ exchange on a connection of its own: the decrypted body of the answer.
pub fn tacacs(port: u16, secret: &[u8], type_: u8, version: u8, body: Vec<u8>) -> Vec<u8> {
    use crate::tacacs::TacacsPacket;
    use std::io::{Read, Write};
    let session_id = u32::from_be_bytes(random_authenticator()[..4].try_into().unwrap());
    let request = TacacsPacket { version, type_, seq_num: 1, flags: 0, session_id, body: TacacsPacket::encrypt_decrypt(&body, secret, session_id, version, 1) };
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(&request.encode()).unwrap();
    let mut header = [0u8; 12];
    stream.read_exact(&mut header).unwrap();
    let mut answer = vec![0u8; u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize];
    stream.read_exact(&mut answer).unwrap();
    TacacsPacket::encrypt_decrypt(&answer, secret, session_id, header[0], header[2])
}

/// The user, port and rem_addr fields and the arguments of an authorization or accounting
/// request, after its first bytes.
pub fn tacacs_args(user: &str, args: &[&str]) -> Vec<u8> {
    let (port, remote) = (b"tty1", b"192.0.2.50");
    let mut body = vec![6, 1, 1, 1, user.len() as u8, port.len() as u8, remote.len() as u8, args.len() as u8];
    body.extend(args.iter().map(|arg| arg.len() as u8));
    body.extend_from_slice(user.as_bytes());
    body.extend_from_slice(port);
    body.extend_from_slice(remote);
    for arg in args {
        body.extend_from_slice(arg.as_bytes());
    }
    body
}

/// One TACACS+ request as RFC 9887 has it on a TLS connection, where nothing is obfuscated and
/// `flags` says so: the flags and the body of the answer.
pub fn tacacs_over_tls(stream: &mut TlsStream, type_: u8, version: u8, flags: u8, body: Vec<u8>) -> Result<(u8, Vec<u8>), Box<dyn std::error::Error>> {
    use crate::tacacs::TacacsPacket;
    use std::io::{Read, Write};
    let session_id = u32::from_be_bytes([random_authenticator()[0], random_authenticator()[1], random_authenticator()[2], random_authenticator()[3]]);
    stream.write_all(&TacacsPacket { version, type_, seq_num: 1, flags, session_id, body }.encode())?;
    let mut header = [0u8; 12];
    stream.read_exact(&mut header)?;
    let mut answer = vec![0u8; u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize];
    stream.read_exact(&mut answer)?;
    Ok((header[3], answer))
}

/// The status of an authorization answer, and its AV pairs as text.
pub fn author_answer(answer: &[u8]) -> Result<(u8, Vec<String>), Box<dyn std::error::Error>> {
    let [status, count, message_high, message_low, data_high, data_low, rest @ ..] = answer else { return Err("a short authorization answer".into()) };
    let lengths = rest.get(..usize::from(*count)).ok_or("no argument lengths")?;
    let mut at = usize::from(*count) + usize::from(u16::from_be_bytes([*message_high, *message_low])) + usize::from(u16::from_be_bytes([*data_high, *data_low]));
    let mut pairs = Vec::new();
    for length in lengths {
        pairs.push(String::from_utf8_lossy(rest.get(at..at + usize::from(*length)).ok_or("a short argument")?).into_owned());
        at += usize::from(*length);
    }
    Ok((*status, pairs))
}
