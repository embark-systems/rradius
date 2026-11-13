//! A server started from its executable, as the tests of a program start it, and what those
//! tests share around it. Each function that runs the program takes its executable as
//! `program`, which a test names with `env!("CARGO_BIN_EXE_<name>")`.

use crate::radius::RadiusPacket;
use md5::Md5;
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A port for one server to bind later, handed out once, from below the range systems use
/// for outgoing connections (above 32767 on Linux, above 49151 on macOS).
///
/// This does not ask the system for a free port, or check that a port is free, because
/// both open a socket. Closing it leaves the port for anyone to take first, and on macOS,
/// where a socket is not close-on-exec from the moment it exists, a server being spawned
/// by a test alongside can inherit it and hold the port for as long as it runs.
///
/// The start moves with the clock, 20 ports a second, because a server that just ended
/// leaves its TCP ports unusable for a while: a run starting seconds after another stays
/// past the ports that one used, and the range lasts ten minutes before it comes round.
pub fn free_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    let _ = NEXT.compare_exchange(0, 20000 + (seconds % 600) as u16 * 20, Ordering::SeqCst, Ordering::SeqCst);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// Writes a configuration with `script` as `[lua].script` and, when given,
/// `handler` as the script of one `lua` auth handler. Returns its path and the
/// RADIUS authentication port.
pub fn write_config(dir: &Path, script: &str, handler: Option<&str>) -> (PathBuf, u16) {
    write_config_with(dir, script, handler, "")
}

/// Like `write_config`, with more settings for `[cluster]`.
pub fn write_config_with(dir: &Path, script: &str, handler: Option<&str>, cluster: &str) -> (PathBuf, u16) {
    let path = dir.join("rradius.toml");
    std::fs::write(dir.join("top.lua"), script).unwrap();
    let auth = match handler {
        Some(source) => {
            std::fs::write(dir.join("handler.lua"), source).unwrap();
            "[auth]\n[[auth.handlers]]\ntype = \"lua\"\nscript = \"handler.lua\"\n"
        }
        None => "",
    };
    let auth_port = free_port();
    std::fs::write(
        &path,
        format!(
            "dictionary = \"{}\"\n[lua]\nscript = \"top.lua\"\n\
             [cluster]\ndata_dir = \"data\"\nsecret_raft = \"test-raft-secret-0123\"\nsecret_api = \"test-api-secret-01234\"\n{}\
             [[cluster.nodes]]\nid = 1\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n{}\
             [radius]\nauth_port = {}\nacct_port = {}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n",
            super::dictionary().display(),
            cluster,
            free_port(),
            free_port(),
            auth,
            auth_port,
            free_port(),
        ),
    )
    .unwrap();
    (path, auth_port)
}

/// A running server, killed when the test ends. Its working directory is the
/// directory holding its configuration; its stderr goes to a file there.
pub struct Server {
    pub child: Child,
    pub dir: tempfile::TempDir,
    pub config: PathBuf,
    pub auth_port: u16,
}

impl Server {
    pub fn start(program: &str, dir: tempfile::TempDir, script: &str, handler: Option<&str>) -> Self {
        Self::start_with(program, dir, script, handler, "")
    }

    pub fn start_with(program: &str, dir: tempfile::TempDir, script: &str, handler: Option<&str>, cluster: &str) -> Self {
        let (config, auth_port) = write_config_with(dir.path(), script, handler, cluster);
        let child = Self::spawn(program, dir.path(), &config);
        Self { child, dir, config, auth_port }
    }

    pub fn spawn(program: &str, dir: &Path, config: &Path) -> Child {
        let stderr = std::fs::File::create(dir.join("stderr.log")).unwrap();
        let stdout = std::fs::File::create(dir.join("stdout.log")).unwrap();
        Command::new(program)
            .args(["server", "--config", config.to_str().unwrap()])
            .current_dir(dir)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap()
    }

    /// Stops the server and starts `program` again on the same configuration and data.
    pub fn restart(&mut self, program: &str) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.child = Self::spawn(program, self.dir.path(), &self.config);
    }

    /// Sends an Access-Request until the server answers, and returns the code.
    pub fn access_request(&self) -> u8 {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let packet = RadiusPacket::create_access_request(1, [1; 16], vec![(1, b"alice".to_vec())], b"s");
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            socket.send_to(&packet, ("127.0.0.1", self.auth_port)).unwrap();
            if let Ok((size, _)) = socket.recv_from(&mut buf) {
                return RadiusPacket::parse(&buf[..size]).unwrap().code;
            }
        }
        panic!("no answer within 60 s; stderr: {}", self.stderr());
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("stderr.log")).unwrap()
    }

    /// The accounting port in its configuration.
    pub fn port_acct(&self) -> u16 {
        let text = std::fs::read_to_string(&self.config).unwrap();
        text.lines().find_map(|line| line.strip_prefix("acct_port = ")).unwrap().parse().unwrap()
    }

    /// Sends SIGHUP and waits for the log to report one more reload with `outcome` in it.
    pub fn reload(&self, outcome: &str) -> String {
        let reports = |stderr: &str| stderr.matches(outcome).count();
        let before = reports(&self.stderr());
        let status = Command::new("kill").args(["-HUP", &self.child.id().to_string()]).status().unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stderr = self.stderr();
            // `eprintln!` writes a line in pieces, so wait for the end of it too.
            if reports(&stderr) > before && stderr.ends_with('\n') {
                return stderr;
            }
            assert!(Instant::now() < deadline, "no {outcome:?} in the log: {stderr}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn write_config_text(&self, change: impl FnOnce(String) -> String) {
        let text = std::fs::read_to_string(&self.config).unwrap();
        std::fs::write(&self.config, change(text)).unwrap();
    }

    /// Waits for logged text, since lines are written in the background.
    pub fn wait_for_log(&self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stderr = self.stderr();
            if stderr.contains(text) {
                return stderr;
            }
            assert!(Instant::now() < deadline, "no {text:?} in the log: {stderr}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub const RELOADED: &str = "configuration reloaded";

/// The whole of `/metrics` on `port`.
pub fn metrics_text(port: u16) -> Result<String, String> {
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    socket.write_all(b"GET /metrics HTTP/1.1\r\n\r\n").map_err(|e| e.to_string())?;
    let mut response = String::new();
    socket.read_to_string(&mut response).map_err(|e| e.to_string())?;
    Ok(response)
}

/// The value of one sample in `/metrics` on `port`.
pub fn scrape(port: u16, sample: &str) -> u64 {
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.write_all(b"GET /metrics HTTP/1.1\r\n\r\n").unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let line = response.lines().find(|line| line.strip_prefix(sample).is_some_and(|rest| rest.starts_with(' '))).unwrap_or_else(|| panic!("no {sample} in {response}"));
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

/// A User-Password as a NAS with `secret` sends it (RFC 2865 section 5.2).
pub fn hidden_password(password: &[u8], secret: &[u8], authenticator: &[u8; 16]) -> Vec<u8> {
    use md5::Digest;
    let mut data = password.to_vec();
    data.resize(data.len().div_ceil(16).max(1) * 16, 0);
    let mut previous = authenticator.to_vec();
    let mut hidden = Vec::new();
    for block in data.chunks(16) {
        let pad: [u8; 16] = Md5::new().chain_update(secret).chain_update(&previous).finalize().into();
        previous = block.iter().zip(pad).map(|(plain, key)| plain ^ key).collect();
        hidden.extend(&previous);
    }
    hidden
}

/// Runs an admin command; its exit status, standard output and standard error.
pub fn admin(program: &str, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(program).args(args).output().unwrap();
    (output.status.code(), String::from_utf8_lossy(&output.stdout).trim().to_string(), String::from_utf8_lossy(&output.stderr).into_owned())
}

/// Writes `ca.pem` and `server.pem` with `server.key`, and `rradius.toml` holding `section`
/// after the settings every configuration has; returns the configuration's path.
pub fn write_tls_config(dir: &Path, section: &str) -> Result<PathBuf, String> {
    let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    std::fs::write(dir.join("ca.pem"), params.clone().self_signed(&key).map_err(|e| e.to_string())?.pem()).map_err(|e| e.to_string())?;
    let issuer = rcgen::Issuer::new(params, key);
    let server_key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let server = rcgen::CertificateParams::new(vec!["localhost".to_string()]).map_err(|e| e.to_string())?.signed_by(&server_key, &issuer).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("server.pem"), server.pem()).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("server.key"), server_key.serialize_pem()).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("top.lua"), "return 'accept'").map_err(|e| e.to_string())?;
    let dictionary = super::dictionary();
    let path = dir.join("rradius.toml");
    let text = format!(
        "dictionary = \"{}\"\n[lua]\nscript = \"top.lua\"\n\
         [cluster]\ndata_dir = \"data\"\nsecret_raft = \"test-raft-secret-0123\"\nsecret_api = \"test-api-secret-01234\"\n\
         [[cluster.nodes]]\nid = 1\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n{section}",
        dictionary.display(),
        free_port(),
        free_port(),
    );
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

/// What the two programs say of a configuration: the message `rradius-server` exits with, and
/// the one `rradius-enterprise` exits with; `None` loads.
pub struct Said {
    pub in_core: Option<&'static str>,
    pub in_enterprise: Option<&'static str>,
}

/// The table of the server-config spec ("Dynamic Clients Only in the Enterprise Program"):
/// sections for `write_tls_config`, and what each program says of them.
pub fn dynamic_clients_settings_table() -> Vec<(String, Said)> {
    let plain = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
    let radius = |sections: &str| format!("[radius]\nauth_port = {}\nacct_port = {}\n{plain}{sections}", free_port(), free_port());
    let dynamic = "[radius.dynamic_clients]\nnetworks = [\"10.20.0.0/16\"]\nquery = \"SELECT name, secret FROM nas WHERE address = ?\"\n";
    let statements = "[accounts.clients]\nadd = \"INSERT INTO nas (address, name, secret) VALUES (?1, ?2, ?3)\"\n";
    let refused = |section: &'static str| match section {
        "radius.dynamic_clients" => Some("radius.dynamic_clients: not supported by this program (dynamic clients need rradius-enterprise)"),
        _ => Some("accounts.clients: not supported by this program (dynamic clients need rradius-enterprise)"),
    };
    vec![
        (radius(""), Said { in_core: None, in_enterprise: None }),
        (radius(dynamic), Said { in_core: refused("radius.dynamic_clients"), in_enterprise: None }),
        (radius(statements), Said { in_core: refused("accounts.clients"), in_enterprise: None }),
        (radius(&format!("{dynamic}{statements}")), Said { in_core: refused("radius.dynamic_clients"), in_enterprise: None }),
    ]
}

/// The table of the eap spec ("Session Resumption Only in the Enterprise Program"): sections for
/// `write_tls_config`, with `seal_file` as `ticket_seal_file`, and what each program says of them.
pub fn session_resumption_settings_table(seal_file: &str) -> Vec<(String, Said)> {
    let radius = |settings: &str| {
        format!(
            "[radius]\nauth_port = {}\nacct_port = {}\n[radius.eap]\nmethods = [\"tls\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n{settings}\
             [[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n",
            free_port(),
            free_port()
        )
    };
    let refused = |setting: &'static str| match setting {
        "resumption_ms" => Some("radius.eap.resumption_ms: not supported by this program (session resumption needs rradius-enterprise)"),
        "shared_tickets" => Some("radius.eap.shared_tickets: not supported by this program (session resumption needs rradius-enterprise)"),
        _ => Some("radius.eap.previous_ticket_seal_file: not supported by this program (session resumption needs rradius-enterprise)"),
    };
    vec![
        (radius(""), Said { in_core: None, in_enterprise: None }),
        (radius("resumption_ms = 0\n"), Said { in_core: refused("resumption_ms"), in_enterprise: None }),
        (radius("resumption_ms = 3600000\n"), Said { in_core: refused("resumption_ms"), in_enterprise: None }),
        (radius(&format!("resumption_ms = 3600000\nshared_tickets = true\nticket_seal_file = \"{seal_file}\"\n")), Said { in_core: refused("resumption_ms"), in_enterprise: None }),
        (radius(&format!("shared_tickets = true\nticket_seal_file = \"{seal_file}\"\n")), Said { in_core: refused("shared_tickets"), in_enterprise: Some("radius.eap.shared_tickets needs resumption_ms") }),
        (radius(&format!("previous_ticket_seal_file = \"{seal_file}\"\n")), Said { in_core: refused("previous_ticket_seal_file"), in_enterprise: Some("radius.eap.ticket_seal_file and previous_ticket_seal_file need shared_tickets = true") }),
        (radius("resumption_ms = \"x\"\n"), Said { in_core: Some("line 20: `resumption_ms`: invalid type: string, expected u64"), in_enterprise: Some("line 20: `resumption_ms`: invalid type: string, expected u64") }),
    ]
}

/// The table of the tacacs-aaa spec ("TACACS+ over TLS Only in the Enterprise Program"):
/// sections for `write_tls_config`, and what each program says of them. Only `check` reads
/// them, so the port without TLS can be 49.
pub fn tacacs_tls_settings_table() -> Vec<(String, Said)> {
    let tacacs = |client: &str, tls: &str| format!("[tacacs]\nport = 49\n[[tacacs.clients]]\nname = \"core1\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{client}{tls}");
    let files = "cert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n";
    let refused = Some("tacacs.tls: not supported by this program (TACACS+ over TLS needs rradius-enterprise)");
    let needs_tls = Some("tacacs client 'core1': tls_name needs [tacacs.tls]");
    vec![
        (tacacs("", ""), Said { in_core: None, in_enterprise: None }),
        (tacacs("", &format!("[tacacs.tls]\n{files}")), Said { in_core: refused, in_enterprise: None }),
        (tacacs("", &format!("[tacacs.tls]\n{files}tls_only = true\n")), Said { in_core: refused, in_enterprise: None }),
        (tacacs("", &format!("[tacacs.tls]\nport = 49\n{files}")), Said { in_core: refused, in_enterprise: Some("tacacs.tls.port 49 is the port of TACACS+ without TLS; RFC 9887 needs another") }),
        (tacacs("", "[tacacs.tls]\ncert_file = \"missing.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n"), Said { in_core: refused, in_enterprise: Some("tacacs.tls.cert_file") }),
        (tacacs("", &format!("[tacacs.tls]\n{files}ocsp = \"hard\"\n")), Said { in_core: refused, in_enterprise: None }),
        (tacacs("", &format!("[tacacs.tls]\nport = \"x\"\n{files}")), Said { in_core: refused, in_enterprise: Some("line 19: `port`: invalid type: string, expected u16") }),
        (tacacs("tls_name = \"core1.example.com\"\n", ""), Said { in_core: needs_tls, in_enterprise: needs_tls }),
    ]
}

/// The table of the eap spec ("EAP-AKA Prime Only in the Enterprise Program"): sections for
/// `write_tls_config`, with `key_file` as the key of `[radius.eap.aka]`, and what each program
/// says of them.
pub fn aka_settings_table(key_file: &str) -> Vec<(String, Said)> {
    let radius = |eap: &str, client: &str| {
        format!(
            "[auth]\n[[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users WHERE username = ?\"\n\
             [radius]\nauth_port = {}\nacct_port = {}\n{eap}[[radius.clients]]\nname = \"nas1\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{client}",
            free_port(),
            free_port()
        )
    };
    let peap = "[radius.eap]\nmethods = [\"peap\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\n";
    let aka = "[radius.eap]\nmethods = [\"aka-prime\"]\n";
    let section = format!(
        "[radius.eap.aka]\nnetwork_name = \"WLAN\"\nkey_file = \"{key_file}\"\nsubscriber_query = \"SELECT ki, opc FROM s WHERE imsi = ?\"\n\
         next_sqn_query = \"UPDATE s SET sqn = sqn + 32 WHERE imsi = ? RETURNING sqn\"\nresync_query = \"UPDATE s SET sqn = max(sqn, ?) + 32 WHERE imsi = ? RETURNING sqn\"\n"
    );
    vec![
        (radius(peap, ""), Said { in_core: None, in_enterprise: None }),
        (radius(&format!("{aka}{section}"), ""), Said { in_core: Some("radius.eap.aka: not supported by this program (EAP-AKA' needs rradius-enterprise)"), in_enterprise: None }),
        (
            radius(aka, ""),
            Said { in_core: Some("radius.eap.methods: aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)"), in_enterprise: Some("radius.eap.methods has aka-prime, which needs [radius.eap.aka]") },
        ),
        (
            radius(peap, "eap_methods = [\"peap\", \"aka-prime\"]\n"),
            Said {
                in_core: Some("radius client 'nas1': eap_methods: aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)"),
                in_enterprise: Some("radius client 'nas1': eap_methods has a method that radius.eap.methods does not"),
            },
        ),
        (
            radius("[radius.eap]\nmethods = [\"md5\"]\n", ""),
            Said { in_core: Some("'md5' is not a method this server offers; it has tls, peap and ttls"), in_enterprise: Some("'md5' is not a method this server offers; it has tls, peap, ttls and aka-prime") },
        ),
        (
            radius(peap, "") + "[accounts.aka]\nshow = \"SELECT imsi FROM aka_subscribers WHERE imsi = ?1\"\n",
            Said { in_core: Some("accounts.aka: not supported by this program (EAP-AKA' needs rradius-enterprise)"), in_enterprise: None },
        ),
    ]
}

/// The table of the dpsk spec ("DPSK Only in the Enterprise Program"): sections for
/// `write_tls_config`, with `key_file` as the key of `[radius.dpsk]`, and what each program says
/// of them.
pub fn dpsk_settings_table(key_file: &str) -> Vec<(String, Said)> {
    let radius = |sections: &str| format!("[radius]\nauth_port = {}\nacct_port = {}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{sections}", free_port(), free_port());
    let dpsk = format!("[radius.dpsk]\ncandidates_query = \"SELECT identity, pmk FROM dpsk_keys WHERE ssid = ?\"\nkey_file = \"{key_file}\"\n");
    vec![
        (radius(""), Said { in_core: None, in_enterprise: None }),
        (radius(&dpsk), Said { in_core: Some("radius.dpsk: not supported by this program (DPSK needs rradius-enterprise)"), in_enterprise: None }),
        (
            radius("[accounts.dpsk]\ndelete = \"DELETE FROM dpsk_keys WHERE identity = ?1 AND ssid = ?2\"\n"),
            Said { in_core: Some("accounts.dpsk: not supported by this program (DPSK needs rradius-enterprise)"), in_enterprise: None },
        ),
    ]
}

/// The table of the dynamic-authorization spec ("CoA Only in the Enterprise Program"): sections
/// for `write_tls_config`, and what each program says of them.
pub fn coa_settings_table() -> Vec<(String, Said)> {
    let radius = |settings: &str, client: &str, sections: &str| format!("[radius]\nauth_port = {}\nacct_port = {}\n{settings}[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{client}{sections}", free_port(), free_port());
    let relay = format!("[radius.coa]\nport = {}\n[[radius.coa.clients]]\naddress = \"192.0.2.1\"\nsecret = \"coa-secret-0123456789\"\n", free_port());
    vec![
        (radius("", "", ""), Said { in_core: None, in_enterprise: None }),
        (radius("", "", &relay), Said { in_core: Some("radius.coa: not supported by this program (CoA needs rradius-enterprise)"), in_enterprise: None }),
        (radius("coa_timeout_ms = 2000\ncoa_attempts = 2\n", "coa_port = 1700\n", ""), Said { in_core: None, in_enterprise: None }),
    ]
}

/// The table of the radsec spec ("RadSec Only in the Enterprise Program"): sections for
/// `write_tls_config`, with `[radius.tls]` on `radsec_port` where there is one, and what each
/// program says of them.
pub fn radsec_settings_table(radsec_port: u16) -> Vec<(String, Said)> {
    let files = "cert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n";
    let plain = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
    let radius = |sections: &str, clients: &str| format!("[radius]\nauth_port = {}\nacct_port = {}\n{sections}{clients}", free_port(), free_port());
    vec![
        (radius("", plain), Said { in_core: None, in_enterprise: None }),
        (radius(&format!("[radius.tls]\nport = {radsec_port}\n{files}"), plain), Said { in_core: Some("radius.tls: not supported by this program (RadSec needs rradius-enterprise)"), in_enterprise: None }),
        (
            radius("", "[[radius.clients]]\nname = \"branch\"\ntls_name = \"branch.example\"\n"),
            Said { in_core: Some("radius client 'branch' has no address, so it can only use RadSec, which needs rradius-enterprise"), in_enterprise: Some("radius client 'branch' has no address, so it can only use RadSec, which needs [radius.tls]") },
        ),
        (
            radius("", "[[radius.clients]]\nname = \"nas1\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\nmax_radsec_connections = 3\n"),
            Said { in_core: Some("radius client 'nas1': max_radsec_connections applies only to RadSec, which needs rradius-enterprise"), in_enterprise: None },
        ),
        (
            radius(&format!("[radius.coa]\nport = {}\n", free_port()), "[[radius.clients]]\nname = \"hq-proxy\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\ncoa_sender = true\n"),
            Said { in_core: Some("radius.coa: not supported by this program (CoA needs rradius-enterprise)"), in_enterprise: Some("radius client 'hq-proxy': coa_sender needs [radius.tls]") },
        ),
        (radius("", "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\ntls_name = \"nas1.example\"\n"), Said { in_core: None, in_enterprise: None }),
    ]
}

/// The table of the radius-proxy spec ("Proxying Only in the Enterprise Program"): sections for
/// `write_tls_config`, and what each program says of them.
pub fn proxy_settings_table() -> Vec<(String, Said)> {
    let radius = |sections: &str| format!("[radius]\nauth_port = {}\nacct_port = {}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{sections}", free_port(), free_port());
    let server = "[[proxy.servers]]\nname = \"hq\"\naddress = \"192.0.2.1\"\nsecret = \"upstream-secret-0123\"\n";
    let refused = Some("proxy: not supported by this program (proxying needs rradius-enterprise)");
    // A part alone is refused by rradius-enterprise too, as before the split.
    let incomplete = Some("[proxy] needs at least one [[proxy.servers]] and one [[proxy.realms]], or [proxy.discovery]");
    vec![
        (radius(""), Said { in_core: None, in_enterprise: None }),
        (radius(&format!("[proxy]\n{server}[[proxy.realms]]\nname = \"hq.test\"\nservers = [\"hq\"]\n")), Said { in_core: refused, in_enterprise: None }),
        (radius(server), Said { in_core: refused, in_enterprise: incomplete }),
        (radius("[proxy.accounting_spool]\ndir = \"spool\"\n"), Said { in_core: refused, in_enterprise: incomplete }),
    ]
}

/// A temporary directory holding a `migrations` directory with these files.
pub fn dir_with_migrations(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let migrations = dir.path().join("migrations");
    std::fs::create_dir(&migrations).unwrap();
    for (name, sql) in files {
        std::fs::write(migrations.join(name), sql).unwrap();
    }
    dir
}

pub const WITH_MIGRATIONS: &str = "migrations_dir = \"migrations\"\n";

/// The key of the sections of `AKA_RECORDS` and `DPSK_RECORDS`.
pub const RECORD_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// For `record_server_with`: the table and the sections of DPSK with `[accounts.dpsk]` for the
/// `dpsk` commands, which only rradius-enterprise loads.
pub const DPSK_RECORDS: (&str, &str) = (
    "CREATE TABLE dpsk_keys (ssid TEXT NOT NULL, identity TEXT NOT NULL, pmk TEXT NOT NULL, PRIMARY KEY (ssid, identity));\n",
    "[radius.dpsk]\ncandidates_query = \"SELECT identity, pmk FROM dpsk_keys WHERE ssid = ?\"\nkey_file = \"record.key\"\n\
     [accounts.dpsk]\n\
     add = \"INSERT INTO dpsk_keys (identity, ssid, pmk) VALUES (?1, ?2, ?3)\"\n\
     passwd = \"UPDATE dpsk_keys SET pmk = ?3 WHERE identity = ?1 AND ssid = ?2\"\n\
     delete = \"DELETE FROM dpsk_keys WHERE identity = ?1 AND ssid = ?2\"\n\
     show = \"SELECT identity, ssid, pmk FROM dpsk_keys WHERE identity = ?1 AND ssid = ?2\"\n",
);

/// For `record_server_with`: the table and the sections of EAP-AKA' with `[accounts.aka]` for
/// the `aka` commands, which only rradius-enterprise loads.
pub const AKA_RECORDS: (&str, &str) = (
    "CREATE TABLE aka_subscribers (imsi TEXT PRIMARY KEY, ki TEXT NOT NULL, opc TEXT NOT NULL, sqn INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1);\n",
    "[radius.eap]\nmethods = [\"aka-prime\"]\n\
     [radius.eap.aka]\nnetwork_name = \"WLAN\"\nkey_file = \"record.key\"\n\
     subscriber_query = \"SELECT ki, opc FROM aka_subscribers WHERE imsi = ?\"\n\
     next_sqn_query = \"UPDATE aka_subscribers SET sqn = sqn + 32 WHERE imsi = ? RETURNING sqn\"\n\
     resync_query = \"UPDATE aka_subscribers SET sqn = max(sqn, ?) + 32 WHERE imsi = ? RETURNING sqn\"\n\
     [accounts.aka]\n\
     add = \"INSERT INTO aka_subscribers (imsi, ki, opc) VALUES (?1, ?2, ?3)\"\n\
     enable = \"UPDATE aka_subscribers SET enabled = 1 WHERE imsi = ?1\"\n\
     disable = \"UPDATE aka_subscribers SET enabled = 0 WHERE imsi = ?1\"\n\
     delete = \"DELETE FROM aka_subscribers WHERE imsi = ?1\"\n\
     show = \"SELECT ki, opc, enabled FROM aka_subscribers WHERE imsi = ?1\"\n",
);

/// A server with `more` after its sections, such as dynamic clients and `[accounts.clients]`. 127.0.0.1 is still a configured client,
/// named `core`, so that it answers.
pub fn record_server(program: &str, more: &str) -> Result<Server, String> {
    record_server_with(program, ("", ""), more)
}

/// `record_server` with `records.0` among its tables and `records.1` among its sections, such as
/// `DPSK_RECORDS`.
pub fn record_server_with(program: &str, records: (&str, &str), more: &str) -> Result<Server, String> {
    use std::os::unix::fs::PermissionsExt;
    let tables = format!(
        "CREATE TABLE nas (address TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, secret TEXT NOT NULL);\n{}",
        records.0
    );
    let dir = dir_with_migrations(&[("1_records.sql", &tables)]);
    let key = dir.path().join("record.key");
    std::fs::write(&key, format!("{RECORD_KEY}\n")).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let (config, auth_port) = write_config_with(dir.path(), "return 'accept'", None, WITH_MIGRATIONS);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    let text = text.replace("[[radius.clients]]\naddress = \"127.0.0.1\"", "[[radius.clients]]\nname = \"core\"\naddress = \"127.0.0.1\"")
        + records.1
        + more;
    std::fs::write(&config, text).map_err(|e| e.to_string())?;
    let child = Server::spawn(program, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    assert_eq!(server.access_request(), crate::radius::ACCESS_ACCEPT, "the configured client answers");
    Ok(server)
}

/// `<program> <command> <args> --config <config>`, with `stdin` as its input.
pub fn record_command(program: &str, config: &Path, command: &str, args: &[&str], stdin: &str) -> Result<(Option<i32>, String, String), String> {
    let mut child = Command::new(program)
        .arg(command)
        .args(args)
        .arg("--config")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut input = child.stdin.take().ok_or("no stdin")?;
    input.write_all(stdin.as_bytes()).map_err(|e| e.to_string())?;
    drop(input);
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok((output.status.code(), String::from_utf8_lossy(&output.stdout).trim().to_string(), String::from_utf8_lossy(&output.stderr).into_owned()))
}

pub const SHORT_SECRET: &str = "short-secret";

pub fn check_command(program: &str, config: &Path) -> (Option<i32>, String, String) {
    admin(program, &["check", "--config", config.to_str().unwrap()])
}

/// How a server that cannot go on ended, and the files named `lock` it left behind, which a next
/// start takes as not having shut down cleanly.
pub fn how_it_ended(program: &str, dir: &Path, config: &Path) -> (Option<i32>, String, Vec<PathBuf>) {
    let mut child = Server::spawn(program, dir, config);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("still running with its port taken");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    (status.code(), std::fs::read_to_string(dir.join("stderr.log")).unwrap(), lock_files(dir))
}

/// The files named `lock` in the data directory.
pub fn lock_files(dir: &Path) -> Vec<PathBuf> {
    fn locks(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                locks(&path, found);
            } else if path.file_name().is_some_and(|name| name == "lock") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    locks(&dir.join("data"), &mut found);
    found
}
