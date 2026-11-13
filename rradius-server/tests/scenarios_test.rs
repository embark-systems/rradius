//! The scenarios of the user manual (docs/manual), each started as the manual starts it, from
//! the files in examples/scenarios, so that what the manual tells people to copy works.
//!
//! The configuration is copied into a temporary directory with its file paths made absolute,
//! its ports moved to free ones and `data_dir` left relative, so that the database lands in
//! the temporary directory. Nothing else is changed unless a test says so.

use md5::Md5;
use rradius_core::radius::RadiusPacket;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::process::Command;
use std::time::Duration;
use rradius_core::test_support::scenario::*;

/// The program these tests start.
const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

/// The program with the directory of its scenarios.
const SCENARIOS: Scenarios = Scenarios { program: PROGRAM, root: concat!(env!("CARGO_MANIFEST_DIR"), "/examples/scenarios") };

const STATUS_SERVER: u8 = 12;

/// A Status-Server (RFC 5997) with its Message-Authenticator.
fn status_server(secret: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, KeyInit, Mac};
    let mut packet = vec![STATUS_SERVER, next_id(), 0, 38];
    packet.extend_from_slice(&random_authenticator());
    packet.extend_from_slice(&[80, 18]);
    packet.extend_from_slice(&[0; 16]);
    let mut mac = Hmac::<Md5>::new_from_slice(secret).unwrap();
    mac.update(&packet);
    let digest = mac.finalize().into_bytes();
    packet[22..38].copy_from_slice(&digest);
    packet
}

/// A copy of a scenario's migrations with `extra` files added, for data a test needs and the
/// scenario does not ship. Returns the directory and the text to replace in the configuration.
fn migrations_with(name: &str, extra: &[(&str, &str)]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let ours = SCENARIOS.dir(name).join("migrations");
    for entry in std::fs::read_dir(&ours).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
    }
    for (file, text) in extra {
        std::fs::write(dir.path().join(file), text).unwrap();
    }
    (dir, format!("migrations_dir = \"{}\"", ours.display()))
}

/// Chapter 2: the users of the demo migration, PAP and CHAP, the reply attributes, and accounting.
#[test]
fn test_quick_start() {
    let mut server = Server::start(SCENARIOS, "quick-start", "rradius.toml", |text| text);
    server.secret = secret_of(SCENARIOS, "quick-start", "localhost.secret");

    let alice = server.pap("alice", "alice-password");
    assert_eq!(alice.code, ACCESS_ACCEPT, "{}", server.stderr());
    assert_eq!(number(&alice, 27), Some(3600), "Session-Timeout");
    assert_eq!(text(&alice, 11).as_deref(), Some("staff"), "Filter-Id");
    assert_eq!(server.chap("alice", "alice-password").code, ACCESS_ACCEPT, "CHAP with a plaintext password");
    assert_eq!(server.pap("alice", "wrong").code, ACCESS_REJECT);

    let bob = server.pap("bob", "bob-password");
    assert_eq!(bob.code, ACCESS_ACCEPT, "bcrypt");
    assert_eq!(text(&bob, 18).as_deref(), Some("Welcome, bob"));
    assert_eq!(server.chap("bob", "bob-password").code, ACCESS_REJECT, "CHAP cannot use a hash");

    assert_eq!(server.pap("carol", "carol-password").code, ACCESS_REJECT, "disabled");
    assert_eq!(server.pap("nobody", "x").code, ACCESS_REJECT);

    // Status-Server, as the manual's health check sends it.
    let status = status_server(&server.secret);
    assert_eq!(server.exchange(server.port("radius.auth_port"), &status).map(|answer| answer.code), Some(ACCESS_ACCEPT));

    for (status, extra) in [
        (1, vec![]),
        (3, vec![(46, 60u32.to_be_bytes().to_vec()), (42, 1000u32.to_be_bytes().to_vec())]),
        (2, vec![(46, 120u32.to_be_bytes().to_vec()), (42, 2000u32.to_be_bytes().to_vec()), (49, 1u32.to_be_bytes().to_vec())]),
        (2, vec![(46, 120u32.to_be_bytes().to_vec())]), // the Stop again
        (7, vec![]),
    ] {
        let answer = server.accounting(session("alice", "qs-1", status, extra));
        assert_eq!(answer.map(|answer| answer.code), Some(ACCOUNTING_RESPONSE), "status {status}: {}", server.stderr());
    }
    assert!(!server.stderr().contains("Lua script error"), "{}", server.stderr());
}

/// Chapter 3: the certificates from make-certs.sh load, and the chain gives each group its
/// VLAN. EAP itself is covered by the interop tests with eapol_test; here the chain is asked
/// with PAP, which takes the same queries.
#[test]
fn test_enterprise_wifi() {
    let certs = tempfile::tempdir().unwrap();
    let made = Command::new("sh").arg(SCENARIOS.dir("enterprise-wifi").join("make-certs.sh")).arg(certs.path()).output().unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let ours = format!("{}/", SCENARIOS.dir("enterprise-wifi").join("certs").display());
    // A device row that happens to have a password: the password login is still for users only.
    let device = "INSERT INTO accounts (name, kind, password, grp) VALUES ('laptop-002.corp.example.com', 'device', 'device-password', 'corp-devices');";
    let (migrations, migrations_line) = migrations_with("enterprise-wifi", &[("3_test_device.sql", device)]);
    let mut server = Server::start(SCENARIOS, "enterprise-wifi", "rradius.toml", |text| {
        text.replace(&ours, &format!("{}/", certs.path().display()))
            .replacen("address = \"10.30.0.10\"", "address = \"127.0.0.1\"", 1)
            .replace(&migrations_line, &format!("migrations_dir = \"{}\"", migrations.path().display()))
    });
    server.secret = secret_of(SCENARIOS, "enterprise-wifi", "wlc.secret");

    let staff = server.pap("staff1", "staff1-password");
    assert_eq!(staff.code, ACCESS_ACCEPT, "the NT hash: {}", server.stderr());
    assert_eq!(vlan(&staff).as_deref(), Some("20"));
    assert_eq!(server.pap("Staff1@corp.example.com", "staff1-password").code, ACCESS_ACCEPT, "realm and case");
    assert_eq!(server.pap("staff2", "staff2-password").code, ACCESS_REJECT, "disabled");
    assert_eq!(server.pap("laptop-001.corp.example.com", "").code, ACCESS_REJECT, "a device has no password");
    assert_eq!(server.pap("laptop-002.corp.example.com", "device-password").code, ACCESS_REJECT, "devices log in with certificates only");
}

/// Chapter 4: a known device by its MAC address, written as switches write it.
#[test]
fn test_wired_mab() {
    let mut server = Server::start(SCENARIOS, "wired-mab", "rradius.toml", |text| text.replacen("address = \"10.40.0.0/24\"", "address = \"127.0.0.1\"", 1));
    server.secret = secret_of(SCENARIOS, "wired-mab", "switches.secret");
    let mab = |user: &str, station: &str| {
        let authenticator = random_authenticator();
        let attributes = vec![(1, user.as_bytes().to_vec()), (2, hide_password(user.as_bytes(), &server.secret, &authenticator)), (31, station.as_bytes().to_vec())];
        server.login(attributes, authenticator)
    };
    let printer = mab("00-11-22-AA-BB-CC", "00-11-22-AA-BB-CC");
    assert_eq!(printer.code, ACCESS_ACCEPT, "{}", server.stderr());
    assert_eq!(vlan(&printer).as_deref(), Some("30"));
    assert_eq!(vlan(&mab("00a0.c912.3456", "00:a0:c9:12:34:56")).as_deref(), Some("40"));
    assert_eq!(mab("001122aabbcc", "00:00:00:00:00:01").code, ACCESS_REJECT, "another station");
    assert_eq!(mab("00ffffffffff", "00:ff:ff:ff:ff:ff").code, ACCESS_REJECT, "unknown");
}

/// Chapter 6: PAP login against the bcrypt hashes, authorization by role, and accounting.
#[test]
fn test_device_admin() {
    use rradius_core::tacacs::{ACCT_STATUS_SUCCESS, AUTHEN_STATUS_FAIL, AUTHEN_STATUS_PASS, AUTHOR_STATUS_FAIL, AUTHOR_STATUS_PASS_ADD, TYPE_ACCT, TYPE_AUTHEN, TYPE_AUTHOR};
    let server = Server::start(SCENARIOS, "device-admin", "rradius.toml", |text| text.replacen("address = \"10.50.0.0/24\"", "address = \"127.0.0.1\"", 1));
    let secret = secret_of(SCENARIOS, "device-admin", "tacacs.secret");
    let port = server.port("tacacs.port");

    let login = |user: &str, password: &str| {
        let mut body = vec![1, 1, 2, 1, user.len() as u8, 0, 0, password.len() as u8];
        body.extend_from_slice(user.as_bytes());
        body.extend_from_slice(password.as_bytes());
        tacacs(port, &secret, TYPE_AUTHEN, 0xc1, body)[0]
    };
    assert_eq!(login("netadmin1", "netadmin1-password"), AUTHEN_STATUS_PASS, "{}", server.stderr());
    assert_eq!(login("noc1", "noc1-password"), AUTHEN_STATUS_PASS);
    assert_eq!(login("noc1", "wrong"), AUTHEN_STATUS_FAIL);

    // The status, and the AV pairs as text.
    let authorize = |user: &str, args: &[&str]| {
        let answer = tacacs(port, &secret, TYPE_AUTHOR, 0xc0, tacacs_args(user, args));
        let count = answer[1] as usize;
        let lengths: Vec<usize> = answer[6..6 + count].iter().map(|length| *length as usize).collect();
        let mut at = 6 + count + u16::from_be_bytes([answer[2], answer[3]]) as usize + u16::from_be_bytes([answer[4], answer[5]]) as usize;
        let mut pairs = Vec::new();
        for length in lengths {
            pairs.push(String::from_utf8_lossy(&answer[at..at + length]).into_owned());
            at += length;
        }
        (answer[0], pairs)
    };
    assert_eq!(authorize("netadmin1", &["service=shell", "cmd="]), (AUTHOR_STATUS_PASS_ADD, vec!["priv-lvl=15".to_string()]), "{}", server.stderr());
    assert_eq!(authorize("noc1", &["service=shell", "cmd="]), (AUTHOR_STATUS_PASS_ADD, vec!["priv-lvl=1".to_string()]));
    assert_eq!(authorize("netadmin1", &["service=shell", "cmd=configure", "cmd-arg=terminal", "cmd-arg=<cr>"]).0, AUTHOR_STATUS_PASS_ADD);
    assert_eq!(authorize("noc1", &["service=shell", "cmd=show", "cmd-arg=interfaces", "cmd-arg=<cr>"]).0, AUTHOR_STATUS_PASS_ADD);
    assert_eq!(authorize("noc1", &["service=shell", "cmd=show", "cmd-arg=running-config", "cmd-arg=<cr>"]).0, AUTHOR_STATUS_FAIL);
    assert_eq!(authorize("noc1", &["service=shell", "cmd=configure", "cmd-arg=terminal", "cmd-arg=<cr>"]).0, AUTHOR_STATUS_FAIL);
    assert_eq!(authorize("nobody", &["service=shell", "cmd="]).0, AUTHOR_STATUS_FAIL);

    for _ in 0..2 {
        let mut body = vec![0x04]; // STOP, sent twice
        body.extend(tacacs_args("noc1", &["task_id=42", "stop_time=1700000000", "service=shell", "cmd=show", "cmd-arg=interfaces", "cmd-arg=<cr>"]));
        let answer = tacacs(port, &secret, TYPE_ACCT, 0xc0, body);
        assert_eq!(answer[4], ACCT_STATUS_SUCCESS, "{}", server.stderr());
    }
    assert!(!server.stderr().contains("Lua script error"), "{}", server.stderr());
}

/// The TOTP code (RFC 6238: HMAC-SHA-1, 6 digits, 30 seconds) of a Base32 secret, now.
fn totp(secret: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut bits = 0u64;
    let mut count = 0;
    let mut key = Vec::new();
    for c in secret.chars() {
        bits = (bits << 5) | "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".find(c).unwrap() as u64;
        count += 5;
        if count >= 8 {
            key.push((bits >> (count - 8)) as u8);
            count -= 8;
        }
    }
    let step = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() / 30;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&key).unwrap();
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let number = u32::from_be_bytes(digest[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
    format!("{:06}", number % 1_000_000)
}

/// Chapter 5: the jump host's two steps. The VPN chain needs a directory, which the LDAP
/// tests cover; here its settings only have to pass check and start.
#[test]
fn test_remote_access() {
    let mut server = Server::start(SCENARIOS, "remote-access", "rradius.toml", |text| text.replacen("address = \"10.60.0.20\"", "address = \"127.0.0.1\"", 1));
    server.secret = secret_of(SCENARIOS, "remote-access", "jump-host.secret");

    assert_eq!(server.pap("ops1", "wrong").code, ACCESS_REJECT);
    let challenge = server.pap("ops1", "ops1-password");
    assert_eq!(challenge.code, 11, "Access-Challenge: {}", server.stderr());
    let state = challenge.attribute(24).unwrap().to_vec();
    let authenticator = random_authenticator();
    let code = totp("JBSWY3DPEHPK3PXP");
    let answer = server.login(vec![(1, b"ops1".to_vec()), (2, hide_password(code.as_bytes(), &server.secret, &authenticator)), (24, state.clone())], authenticator);
    assert_eq!(answer.code, ACCESS_ACCEPT, "{}", server.stderr());
    let again = random_authenticator();
    let replayed = server.login(vec![(1, b"ops1".to_vec()), (2, hide_password(code.as_bytes(), &server.secret, &again)), (24, state)], again);
    assert_eq!(replayed.code, ACCESS_REJECT, "a challenge answers once");
}

/// Chapter 8: three nodes on this host, with the node list each of them has. A user logs in
/// at each node, and a session started at one node is stopped at another.
#[test]
fn test_ha_cluster() {
    let mut nodes = HashMap::new();
    let mut servers: Vec<Server> = (1..=3)
        .map(|n| {
            Server::spawn(SCENARIOS, "ha-cluster", &format!("node{n}.toml"), &mut nodes, |text| {
                text.replace(&format!("\"10.99.0.{n}\""), "\"127.0.0.1\"").replacen("address = \"10.80.0.0/16\"", "address = \"127.0.0.1\"", 1)
            })
        })
        .collect();
    let secret = secret_of(SCENARIOS, "ha-cluster", "nas.secret");
    for server in &mut servers {
        server.wait_ready();
        server.secret = secret.clone();
    }
    for server in &servers {
        assert_eq!(server.pap("alice", "alice-password").code, ACCESS_ACCEPT, "{}", server.stderr());
        assert_eq!(server.pap("carol", "carol-password").code, ACCESS_REJECT);
    }
    let start = servers[0].accounting(session("alice", "ha-1", 1, vec![]));
    assert_eq!(start.map(|answer| answer.code), Some(ACCOUNTING_RESPONSE), "{}", servers[0].stderr());
    let stop = servers[2].accounting(session("alice", "ha-1", 2, vec![(46, 10u32.to_be_bytes().to_vec())]));
    assert_eq!(stop.map(|answer| answer.code), Some(ACCOUNTING_RESPONSE), "{}", servers[2].stderr());
}

/// Chapter 17: accounts brought over with the hashes their old systems kept, each logging in
/// with the password it always had; a password changed here is stored the new way.
#[test]
fn test_migrate_users() -> TestResult {
    let mut server = Server::start(SCENARIOS, "migrate-users", "rradius.toml", |text| text.replacen("address = \"10.90.0.0/24\"", "address = \"127.0.0.1\"", 1));
    server.secret = secret_of(SCENARIOS, "migrate-users", "nas.secret");
    for user in ["ldap1", "ldap2", "ldap3", "ldap4", "shell1", "mail1", "old1", "win1"] {
        assert_eq!(server.pap(user, &format!("{user}-password")).code, ACCESS_ACCEPT, "{user}: {}", server.stderr());
        assert_eq!(server.pap(user, "wrong").code, ACCESS_REJECT, "{user}");
    }
    let ldap1 = server.pap("EXAMPLE\\LDAP1@example.com", "ldap1-password");
    assert_eq!((ldap1.code, text(&ldap1, 11).as_deref()), (ACCESS_ACCEPT, Some("staff")), "the name as an old client wrote it");
    assert_eq!(server.chap("ldap1", "ldap1-password").code, ACCESS_REJECT, "CHAP cannot use a hash");

    let config = config_of(&server)?;
    assert_eq!(command(PROGRAM, &["user", "show", "--config", &config, "mail1"], "")?, "username=mail1\nenabled=1\norigin=mail");
    assert_eq!(command(PROGRAM, &["user", "passwd", "--config", &config, "Mail1"], "a-new-password\n")?, "password changed for mail1");
    assert_eq!(server.pap("mail1", "a-new-password").code, ACCESS_ACCEPT, "{}", server.stderr());
    assert_eq!(server.pap("mail1", "mail1-password").code, ACCESS_REJECT, "the old password is gone");
    assert_eq!(command(PROGRAM, &["user", "show", "--config", &config, "mail1"], "")?, "username=mail1\nenabled=1\norigin=rradius");
    Ok(())
}

/// A member system for chapter 18: answers each `POST /v1/radius/login` that carries the API
/// key with 200 and a plan for dave's password, 401 for another, and nothing at all for `slow`.
fn member_api() -> Result<u16, Box<dyn std::error::Error>> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                // The headers, then as much body as Content-Length says.
                loop {
                    let Ok(size) = stream.read(&mut buffer) else { return };
                    if size == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..size]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                    let length = head.lines().filter_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())).next().unwrap_or(0);
                    if body.len() < length {
                        continue;
                    }
                    let keyed = head.starts_with("POST /v1/radius/login ") && head.to_ascii_lowercase().contains("authorization: bearer member-api-key-change-me");
                    let answer = match body {
                        _ if !keyed => "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        // "d&ve" and its password, as url_encode writes them.
                        "username=dave&password=dave-password" | "username=d%26ve&password=p%3Dss%20word" => "HTTP/1.1 200 OK\r\nX-Plan: gold\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        _ if body.starts_with("username=slow&") => {
                            std::thread::sleep(Duration::from_secs(3));
                            return;
                        }
                        _ => "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    };
                    let _ = stream.write_all(answer.as_bytes());
                    return;
                }
            });
        }
    });
    Ok(port)
}

/// Chapter 18: the accounts are in another system, asked over HTTP at each login. The script is
/// the scenario's own, with the API's address changed to a stand-in on this host.
#[test]
fn test_external_api() -> TestResult {
    let api = member_api()?;
    let scripts = tempfile::tempdir()?;
    let script = std::fs::read_to_string(SCENARIOS.dir("external-api").join("member-api.lua"))?;
    assert!(script.contains("'https://members.example.com/v1/radius/login'"));
    std::fs::write(scripts.path().join("member-api.lua"), script.replace("https://members.example.com/", &format!("http://127.0.0.1:{api}/")))?;
    let ours = SCENARIOS.dir("external-api").join("member-api.lua");
    let mut server = Server::start(SCENARIOS, "external-api", "rradius.toml", |text| {
        text.replacen("address = \"10.95.0.0/24\"", "address = \"127.0.0.1\"", 1)
            .replacen("allowed_hosts = [\"members.example.com\"]", &format!("allowed_hosts = [\"http://127.0.0.1:{api}\"]"), 1)
            .replacen("timeout_ms = 1500", "timeout_ms = 500", 1)
            .replace(&ours.display().to_string(), &scripts.path().join("member-api.lua").display().to_string())
    });
    server.secret = secret_of(SCENARIOS, "external-api", "hotspot.secret");

    let dave = server.pap("dave", "dave-password");
    assert_eq!(dave.code, ACCESS_ACCEPT, "{}", server.stderr());
    assert_eq!((text(&dave, 11).as_deref(), number(&dave, 27)), (Some("gold"), Some(14400)), "the plan from the API's header");
    assert_eq!(server.pap("d&ve", "p=ss word").code, ACCESS_ACCEPT, "names and passwords are encoded: {}", server.stderr());
    assert_eq!(server.pap("dave", "wrong").code, ACCESS_REJECT);
    assert_eq!(server.chap("dave", "dave-password").code, ACCESS_REJECT, "CHAP has no password to send to the API");
    assert_eq!(server.pap("slow", "anything").code, ACCESS_REJECT, "no answer from the API is a refusal");
    assert!(server.stderr().contains("member API gave no answer"), "{}", server.stderr());
    assert!(!server.stderr().contains("dave-password"), "no password in the log");
    assert_eq!(metric(&server, "rradius_script_http_requests_total{result=\"ok\"}")?, 3.0);
    assert_eq!(metric(&server, "rradius_script_http_requests_total{result=\"timeout\"}")?, 1.0);

    let start = server.accounting(session("dave", "ea-1", 1, vec![]));
    assert_eq!(start.map(|answer| answer.code), Some(ACCOUNTING_RESPONSE), "accounting stays local: {}", server.stderr());
    Ok(())
}

/// Chapter 15: a laptop logs in with its certificate, with a full handshake each time, since
/// this program resumes no session; the laptop on the revocation list is refused; a device
/// disabled in the table is refused.
#[test]
fn test_wifi_certs() -> TestResult {
    let made = set_up(SCENARIOS, "wifi-certs")?;
    let mut server = Server::start(SCENARIOS, "wifi-certs", "rradius.toml", |text| with_set_up(SCENARIOS, text, "wifi-certs", made.path(), "certs").replacen("address = \"10.30.0.10\"", "address = \"127.0.0.1\"", 1));
    server.secret = secret_of(SCENARIOS, "wifi-certs", "wlc.secret");
    let laptop = "laptop-001.corp.example.com";

    let device = device_tls(&made.path().join("certs"), "laptop-001")?;
    let (first, kind) = eap_tls_login(&server, &device, laptop)?;
    assert_eq!((first.code, vlan(&first).as_deref()), (ACCESS_ACCEPT, Some("10")), "{}", server.stderr());
    assert_eq!(kind, Some(rustls::HandshakeKind::Full));
    let (again, kind) = eap_tls_login(&server, &device, laptop)?;
    assert_eq!(again.code, ACCESS_ACCEPT, "{}", server.stderr());
    assert_eq!(kind, Some(rustls::HandshakeKind::Full), "this program resumes no session");

    let lost = device_tls(&made.path().join("certs"), "laptop-002")?;
    assert_eq!(eap_tls_login(&server, &lost, "laptop-002.corp.example.com")?.0.code, ACCESS_REJECT, "on the revocation list");
    assert_eq!(eap_tls_login(&server, &device, "laptop-002.corp.example.com")?.0.code, ACCESS_REJECT, "a name the certificate does not have");

    let config = config_of(&server)?;
    assert_eq!(command(PROGRAM, &["user", "disable", "--config", &config, laptop], "")?, format!("disabled {laptop}"));
    assert_eq!(eap_tls_login(&server, &device, laptop)?.0.code, ACCESS_REJECT, "disabled in the table: {}", server.stderr());
    assert_eq!(command(PROGRAM, &["user", "enable", "--config", &config, laptop], "")?, format!("enabled {laptop}"));
    assert_eq!(eap_tls_login(&server, &device, laptop)?.0.code, ACCESS_ACCEPT, "{}", server.stderr());
    assert_eq!(metric(&server, "rradius_eap_sessions_total{method=\"tls\",result=\"accept\"}")?, 3.0);
    Ok(())
}

/// The server's resident memory, in KiB.
fn resident_kib(pid: u32) -> u64 {
    let out = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// The "Steady load" scenario of Heap Allocation Alignment: a minute of logins, and the
/// resident memory stops growing. Each login runs a script (the MAC authentication of
/// chapter 4), as the growth on macOS came from the Lua VMs' memory. It takes a minute and wants a quiet machine, so it only runs when
/// asked: `cargo test --release --test scenarios_test -- --ignored`.
#[test]
#[ignore]
fn test_memory_stops_growing_under_steady_load() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    let mut server = Server::start(SCENARIOS, "wired-mab", "rradius.toml", |text| text.replacen("address = \"10.40.0.0/24\"", "address = \"127.0.0.1\"", 1));
    server.secret = secret_of(SCENARIOS, "wired-mab", "switches.secret");
    let (port, pid) = (server.port("radius.auth_port"), server.child.id());
    let stop = Arc::new(AtomicBool::new(false));
    let answered = Arc::new(AtomicU64::new(0));
    let accepted = Arc::new(AtomicU64::new(0));
    let senders: Vec<_> = (0..8)
        .map(|_| {
            let (stop, answered, accepted, secret) = (stop.clone(), answered.clone(), accepted.clone(), server.secret.clone());
            std::thread::spawn(move || {
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
                let mut buffer = [0u8; 4096];
                while !stop.load(Ordering::Relaxed) {
                    let authenticator = random_authenticator();
                    let mac = b"00-11-22-AA-BB-CC";
                    let attributes = vec![(1, mac.to_vec()), (2, hide_password(mac, &secret, &authenticator)), (31, mac.to_vec())];
                    let packet = RadiusPacket::create_access_request(next_id(), authenticator, attributes, &secret);
                    socket.send_to(&packet, ("127.0.0.1", port)).unwrap();
                    if socket.recv_from(&mut buffer).is_ok() {
                        answered.fetch_add(1, Ordering::Relaxed);
                        if buffer[0] == ACCESS_ACCEPT {
                            accepted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();
    let mut samples = Vec::new();
    for _ in 0..6 {
        std::thread::sleep(Duration::from_secs(10));
        samples.push(resident_kib(pid));
    }
    stop.store(true, Ordering::Relaxed);
    for sender in senders {
        sender.join().unwrap();
    }
    let logins = answered.load(Ordering::Relaxed);
    eprintln!("{logins} logins ({} accepted); resident memory every 10 s, KiB: {samples:?}", accepted.load(Ordering::Relaxed));
    assert!(logins > 100_000, "too little load to say anything: {logins}");
    assert_eq!(accepted.load(Ordering::Relaxed), logins, "every login is the known printer");
    // Warmed up after 30 seconds; from then on it may wander, not climb.
    let (settled, last) = (samples[2], samples[5]);
    assert!(last <= settled + settled / 5 + 16 * 1024, "still growing: {samples:?}");
}
