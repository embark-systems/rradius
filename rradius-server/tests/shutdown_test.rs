//! Stopping a serving server with SIGTERM: `/readyz` first, then no more requests taken, then
//! the requests being handled are answered, within `[shutdown]`'s limits.

use rradius_core::radius::{self, RadiusPacket};
use rradius_core::test_support::server::*;
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::process::Command;
use std::time::{Duration, Instant};

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

/// A script that takes a second or more: a query the database has to count through.
const SLOW: &str = "sql_query('WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 3000000) SELECT count(*) AS n FROM c') return 'accept'";

/// A server with `script`, `[shutdown]` set to `shutdown`, `/metrics` and TACACS+ on the returned
/// ports, and scripts allowed to run long.
fn server(script: &str, shutdown: &str) -> Result<(Server, u16, u16), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (metrics, tacacs) = (free_port(), free_port());
    let (config, auth_port) = write_config(dir.path(), script, None);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?.replace("script = \"top.lua\"", "script = \"top.lua\"\nmax_execution_ms = 120000");
    let extra = format!("[metrics]\nport = {metrics}\n[tacacs]\nport = {tacacs}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"t\"\n[shutdown]\n{shutdown}\n");
    std::fs::write(&config, format!("{text}{extra}")).map_err(|e| e.to_string())?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    // Serving once it is ready.
    let deadline = Instant::now() + Duration::from_secs(60);
    while get(metrics, "/readyz").map(|(status, _)| status) != Some(200) {
        if Instant::now() > deadline {
            return Err(format!("did not start: {}", server.stderr()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok((server, metrics, tacacs))
}

/// The status and body of `GET path` on the metrics port, or `None` when it does not answer.
fn get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    let status = text.split(' ').nth(1)?.parse().ok()?;
    Some((status, text.split("\r\n\r\n").nth(1).unwrap_or_default().to_string()))
}

/// Sends an Access-Request from a socket of its own, for `ask` to wait for its answer.
fn send(server: &Server, identifier: u8) -> Result<UdpSocket, String> {
    let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let packet = RadiusPacket::create_access_request(identifier, [identifier; 16], vec![(1, b"alice".to_vec())], b"s");
    socket.send_to(&packet, ("127.0.0.1", server.auth_port)).map_err(|e| e.to_string())?;
    Ok(socket)
}

/// The code of the answer to what `socket` sent, or `None` without one within `wait`.
fn answer(socket: &UdpSocket, wait: Duration) -> Result<Option<u8>, String> {
    socket.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
    let mut buffer = [0u8; 4096];
    match socket.recv_from(&mut buffer) {
        Ok((size, _)) => Ok(Some(RadiusPacket::parse(&buffer[..size]).map_err(|e| format!("{e:?}"))?.code)),
        Err(_) => Ok(None),
    }
}

fn signal(server: &Server, signal: &str) -> Result<(), String> {
    let status = Command::new("kill").args([signal, &server.child.id().to_string()]).status().map_err(|e| e.to_string())?;
    status.success().then_some(()).ok_or_else(|| format!("kill {signal} failed"))
}

/// Waits for the process to end, at most `wait`; its exit status and what it logged.
fn ended(server: &mut Server, wait: Duration) -> Result<(Option<i32>, String), String> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(status) = server.child.try_wait().map_err(|e| e.to_string())? {
            return Ok((status.code(), server.stderr()));
        }
        if Instant::now() > deadline {
            return Err(format!("did not end within {wait:?}: {}", server.stderr()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn test_request_being_handled_is_answered() -> Result<(), String> {
    let (mut server, _, _) = server(SLOW, "ready_delay_ms = 0\ndrain_ms = 30000")?;
    let socket = send(&server, 1)?;
    std::thread::sleep(Duration::from_millis(200));
    signal(&server, "-TERM")?;
    assert_eq!(answer(&socket, Duration::from_secs(30))?, Some(radius::ACCESS_ACCEPT), "{}", server.stderr());
    let (status, stderr) = ended(&mut server, Duration::from_secs(30))?;
    assert_eq!(status, Some(0), "{stderr}");
    assert!(stderr.contains("stopping: every request taken was answered"), "{stderr}");
    Ok(())
}

#[test]
fn test_new_requests_after_the_delay() -> Result<(), String> {
    let (mut server, metrics, tacacs) = server("return 'accept'", "ready_delay_ms = 1500\ndrain_ms = 1000")?;
    signal(&server, "-TERM")?;
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(get(metrics, "/readyz"), Some((503, "stopping\n".to_string())), "{}", server.stderr());
    assert_eq!(get(metrics, "/livez").map(|(status, _)| status), Some(200));
    assert_eq!(answer(&send(&server, 2)?, Duration::from_secs(1))?, Some(radius::ACCESS_ACCEPT), "answered during the delay");
    assert!(std::net::TcpStream::connect(("127.0.0.1", tacacs)).is_ok(), "TACACS+ taken during the delay");
    std::thread::sleep(Duration::from_millis(1600));
    assert_eq!(answer(&send(&server, 3)?, Duration::from_millis(500))?, None, "answered after the delay");
    assert!(std::net::TcpStream::connect(("127.0.0.1", tacacs)).is_err(), "TACACS+ taken after the delay");
    let (status, stderr) = ended(&mut server, Duration::from_secs(30))?;
    assert_eq!(status, Some(0), "{stderr}");
    Ok(())
}

#[test]
fn test_drain_limit() -> Result<(), String> {
    let (mut server, _, _) = server(SLOW, "ready_delay_ms = 0\ndrain_ms = 100")?;
    let socket = send(&server, 4)?;
    std::thread::sleep(Duration::from_millis(200));
    signal(&server, "-TERM")?;
    let signalled = Instant::now();
    let (status, stderr) = ended(&mut server, Duration::from_secs(30))?;
    assert_eq!(status, Some(0), "{stderr}");
    // The 100 ms wait and a one-node cluster's shutdown, not the script's run.
    assert!(signalled.elapsed() < Duration::from_secs(5), "ended after {:?}", signalled.elapsed());
    assert!(stderr.contains("1 RADIUS requests were still being handled"), "{stderr}");
    assert_eq!(answer(&socket, Duration::from_millis(100))?, None, "cut off");
    Ok(())
}

#[test]
fn test_second_signal_ends_the_wait() -> Result<(), String> {
    let (mut server, _, _) = server("return 'accept'", "ready_delay_ms = 30000")?;
    signal(&server, "-TERM")?;
    std::thread::sleep(Duration::from_millis(500));
    let asked = Instant::now();
    signal(&server, "-TERM")?;
    let (status, stderr) = ended(&mut server, Duration::from_secs(20))?;
    assert_eq!(status, Some(0), "{stderr}");
    assert!(asked.elapsed() < Duration::from_secs(20));
    assert!(stderr.contains("stopping: asked again"), "{stderr}");
    Ok(())
}
