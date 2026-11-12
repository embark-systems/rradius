//! `rradius-bench` against this program, started from its executable, which also checks the
//! tool's reading of the protocol against the server's own. The tool only sends; the server is
//! started here. The load generator is `RRADIUS_BENCH_BIN`, or the `rradius-bench` built next
//! to this program (`cargo build -p rradius-bench`).

use rradius_core::test_support::bench::{bench_binary, eap_certificates, number, start_full, step, users, Output, Server, PASSWORD, SECRET, SQL_CHAIN};
use rradius_core::test_support::server::free_port;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

const DUPLICATES: &str = "rradius_radius_duplicates_total";
const DISCARDED_INVALID: &str = "rradius_radius_discarded_total{reason=\"invalid\"}";
const PACKETS_ACCT: &str = "rradius_radius_packets_total{kind=\"acct\"}";
const TACACS_ACCEPTED: &str = "rradius_tacacs_connections_total{result=\"accepted\"}";

/// The load generator run against the server.
fn tool() -> Result<PathBuf, String> {
    bench_binary(Path::new(PROGRAM))
}

/// Starts a single-node server. With `chain`, an `sql` handler authenticates "alice".
fn start(script: &str, chain: bool) -> Result<Server, String> {
    start_with(script, chain, None)
}

/// Like `start`, with `[radius.eap]` offering only `method`, when given. Its certificates
/// are in the server's directory: `ca.pem`, and `alice.pem` and `alice.key` for EAP-TLS.
fn start_with(script: &str, chain: bool, method: Option<&str>) -> Result<Server, String> {
    start_full(Path::new(PROGRAM), &tool()?, script, chain, method, None)
}

/// Runs the tool against the server's RADIUS ports, with `args` after these.
fn bench(server: &Server, args: &[&str]) -> Output {
    server.bench(args)
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
fn bench_at(server: &Server, port: u16, server_name: &str, args: &[&str]) -> Output {
    let port = port.to_string();
    let mut all: Vec<String> = ["run", "--json", "--server", server_name, "--secret", SECRET, "--auth-port", &port].iter().map(|s| s.to_string()).collect();
    all.extend(args.iter().map(|arg| arg.to_string()));
    server.tool(&all)
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
    let output = bench_at(&server, port, "127.0.0.1", &["--load", "peap,concurrency=1", "--requests", "4", "--user", "alice", "--password", PASSWORD, "--timeout", "2"]);
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
    let output = bench_at(&server, port, "127.0.0.1", &["--load", "ttls,concurrency=1", "--requests", "1", "--user", "alice", "--password", PASSWORD, "--timeout", "0.5", "--retries", "1"]);
    assert_eq!(output.code, 0, "{}\n{}", output.stdout, output.stderr);
    let json = output.json();
    let counts = step(&json, 0, 0);
    assert_eq!((counts["responses"]["accept"].as_u64(), counts["retransmits"].as_u64(), counts["timeouts"].as_u64()), (Some(1), Some(1), Some(0)), "{counts}");

    let port = relay(server.auth, |packet| packet[0] != 11);
    let output = bench_at(&server, port, "127.0.0.1", &["--load", "ttls,concurrency=2", "--requests", "4", "--user", "alice", "--password", PASSWORD, "--timeout", "0.3"]);
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
    eap_certificates(other.path())?;
    let (ca, other_ca) = (server.file("ca.pem"), other.path().join("ca.pem").display().to_string());
    let port = server.auth.port();
    let login = ["--load", "peap,concurrency=1", "--requests", "2", "--user", "alice", "--password", PASSWORD, "--timeout", "2"];
    for (tls_name, ca_file, accepted) in [(None, &ca, true), (Some("other.example"), &ca, false), (None, &other_ca, false)] {
        let mut args = login.to_vec();
        args.extend(["--ca-file", ca_file.as_str()]);
        if let Some(name) = tls_name {
            args.extend(["--tls-name", name]);
        }
        let output = bench_at(&server, port, "127.0.0.1", &args);
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
    let mut server = Server::spawn(Path::new(PROGRAM), &tool()?, dir, script, &body, &migrations)?;
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
    Ok(server.tool(&all))
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
    let output = server.tool(&all);
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
