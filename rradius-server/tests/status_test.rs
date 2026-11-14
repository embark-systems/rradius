//! `rradius-server status` against a serving server: what it reads from `/metrics` and
//! `/readyz`, `--watch`, `--json`, and its exit statuses.

use rradius_core::radius::RadiusPacket;
use rradius_core::test_support::server::*;
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

/// A serving server with `/metrics` on the returned port, `[shutdown]` set to `shutdown`.
fn server(shutdown: &str) -> Result<(Server, u16), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let metrics = free_port();
    let (config, auth_port) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    std::fs::write(&config, format!("{text}[metrics]\nport = {metrics}\n[shutdown]\n{shutdown}\n")).map_err(|e| e.to_string())?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    let deadline = Instant::now() + Duration::from_secs(60);
    while ready(metrics) != Some(200) {
        if Instant::now() > deadline {
            return Err(format!("did not start: {}", server.stderr()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok((server, metrics))
}

/// The status `/readyz` answers with, or `None` when nothing answers.
fn ready(port: u16) -> Option<u16> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: x\r\n\r\n").ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    text.split(' ').nth(1)?.parse().ok()
}

/// Sends an Access-Request and waits for its answer.
fn login(server: &Server, identifier: u8) -> Result<(), String> {
    let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let packet = RadiusPacket::create_access_request(identifier, [identifier; 16], vec![(1, b"alice".to_vec())], b"s");
    socket.send_to(&packet, ("127.0.0.1", server.auth_port)).map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let mut buffer = [0u8; 4096];
    socket.recv_from(&mut buffer).map(|_| ()).map_err(|e| format!("no answer: {e}"))
}

/// Runs `status` with `args`: its exit status, stdout and stderr.
fn status(args: &[&str]) -> Result<(Option<i32>, String, String), String> {
    let output = Command::new(PROGRAM).arg("status").args(args).output().map_err(|e| e.to_string())?;
    Ok((output.status.code(), String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned()))
}

fn section<'a>(json: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    json["sections"].as_array()?.iter().find(|section| section["key"] == key)
}

fn item<'a>(section: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    section["items"].as_array()?.iter().find(|item| item["key"] == key)
}

#[test]
fn test_one_look() -> Result<(), String> {
    let (server, _) = server("")?;
    for identifier in 1..=3 {
        login(&server, identifier)?;
    }
    let config = server.config.to_string_lossy().into_owned();
    let started = Instant::now();
    let (code, stdout, stderr) = status(&["--config", &config, "--json"])?;
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let json: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| format!("{e}: {stdout}"))?;
    let radius = section(&json, "radius").ok_or("no RADIUS section")?;
    assert_eq!(item(radius, "auth_requests").map(|item| item["total"].clone()), Some(serde_json::json!(3)), "{json}");
    assert_eq!(item(radius, "auth_limit").map(|item| item["value"].clone()), Some(serde_json::json!(4096)), "{json}");
    assert_eq!(json["ready"], serde_json::json!({"status": 200, "text": "ok"}));
    // The version `--version` prints.
    let version = Command::new(PROGRAM).arg("--version").output().map_err(|e| e.to_string())?;
    let version = String::from_utf8_lossy(&version.stdout).lines().next().and_then(|line| line.split(' ').nth(1)).unwrap_or_default().to_string();
    let node = section(&json, "node").ok_or("no node section")?;
    assert_eq!(item(node, "version").map(|item| item["value"].clone()), Some(serde_json::json!(version)));
    // No enterprise sections in the core program.
    for key in ["radsec", "proxy", "coa_relay", "dynamic_clients", "dpsk", "diameter", "accounting_export", "backup"] {
        assert!(section(&json, key).is_none(), "{key}: {json}");
    }
    // The same as text.
    let (code, stdout, stderr) = status(&["--config", &config])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("RADIUS") && stdout.contains("Auth requests"), "{stdout}");
    Ok(())
}

#[test]
fn test_watching() -> Result<(), String> {
    let (server, _) = server("")?;
    let config = server.config.to_string_lossy().into_owned();
    let child = Command::new(PROGRAM).args(["status", "--config", &config, "--watch", "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_secs(3));
    let killed = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().map_err(|e| e.to_string())?;
    assert!(killed.success());
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.lines().count() >= 2, "{stdout}");
    for line in stdout.lines() {
        let json: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("{e}: {line}"))?;
        assert!(json.is_object());
    }
    Ok(())
}

#[test]
fn test_not_ready_is_shown() -> Result<(), String> {
    let (server, metrics) = server("ready_delay_ms = 20000\ndrain_ms = 0")?;
    let killed = Command::new("kill").args(["-TERM", &server.child.id().to_string()]).status().map_err(|e| e.to_string())?;
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while ready(metrics) != Some(503) {
        if Instant::now() > deadline {
            return Err("/readyz did not say stopping".to_string());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let config = server.config.to_string_lossy().into_owned();
    let (code, stdout, stderr) = status(&["--config", &config, "--json"])?;
    assert_eq!(code, Some(0), "{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| format!("{e}: {stdout}"))?;
    assert_eq!(json["ready"], serde_json::json!({"status": 503, "text": "stopping"}));
    Ok(())
}

#[test]
fn test_without_metrics() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let (code, _, stderr) = status(&["--config", &config.to_string_lossy()])?;
    assert_eq!(code, Some(2));
    assert!(stderr.contains("[metrics]"), "{stderr}");
    Ok(())
}

#[test]
fn test_nothing_listening() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let port = free_port();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    std::fs::write(&config, format!("{text}[metrics]\nport = {port}\n")).map_err(|e| e.to_string())?;
    let (code, _, stderr) = status(&["--config", &config.to_string_lossy()])?;
    assert_eq!(code, Some(3));
    assert!(stderr.contains(&format!("127.0.0.1:{port}")), "{stderr}");
    Ok(())
}

#[test]
fn test_wrong_arguments() -> Result<(), String> {
    for args in [&["--bogus"][..], &["--config"], &["--json", "--json"]] {
        let (code, _, stderr) = status(args)?;
        assert_eq!(code, Some(2), "{args:?}");
        assert!(stderr.contains("usage: rradius-server status"), "{args:?}: {stderr}");
    }
    Ok(())
}
