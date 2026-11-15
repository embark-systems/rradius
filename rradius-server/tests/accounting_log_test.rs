//! `[radius.accounting_log]` against a running server: one line per answered accounting request,
//! a new file after SIGHUP, and every answer in the file after SIGTERM.

use rradius_core::radius::{self, RadiusPacket};
use rradius_core::test_support::server::*;
use std::net::UdpSocket;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");
const SCRIPT: &str = "if get_attr('Acct-Session-Id') == 'refused' then return 'reject' end return 'accept'";

/// A serving server with `[radius.accounting_log] file = "acct.jsonl"`.
fn server() -> Result<Server, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, auth_port) = write_config(dir.path(), SCRIPT, None);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    std::fs::write(&config, format!("{text}[radius.accounting_log]\nfile = \"acct.jsonl\"\n")).map_err(|e| e.to_string())?;
    let child = Server::spawn(PROGRAM, dir.path(), &config);
    let server = Server { child, dir, config, auth_port };
    server.access_request();
    Ok(server)
}

/// Sends an Accounting-Request and returns whether it was answered with an Accounting-Response.
fn account(socket: &UdpSocket, port: u16, packet: &[u8]) -> Result<bool, String> {
    socket.send_to(packet, ("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 4096];
    Ok(socket.recv_from(&mut buf).is_ok_and(|(size, _)| RadiusPacket::parse(&buf[..size]).is_ok_and(|reply| reply.code == radius::ACCOUNTING_RESPONSE)))
}

fn request(identifier: u8, status: u32, session: &str) -> Vec<u8> {
    RadiusPacket::create_accounting_request(identifier, vec![(40, status.to_be_bytes().to_vec()), (1, b"alice".to_vec()), (44, session.as_bytes().to_vec())], b"s")
}

fn udp(wait: Duration) -> Result<UdpSocket, String> {
    let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
    Ok(socket)
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).map(|text| text.lines().map(str::to_string).collect()).unwrap_or_default()
}

/// Waits until `path` has `count` lines, as they are written in the background.
fn wait_lines(path: &Path, count: usize) -> Result<Vec<String>, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = lines(path);
        if now.len() >= count {
            return Ok(now);
        }
        if Instant::now() > deadline {
            return Err(format!("{} has {} lines, not {count}", path.display(), now.len()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// spec One line per answered request, and Rejected by the script.
#[test]
fn test_one_line_per_answered_request() -> Result<(), String> {
    let server = server()?;
    let (port, file) = (server.port_acct(), server.dir.path().join("acct.jsonl"));
    let socket = udp(Duration::from_secs(5))?;
    let start = request(1, 1, "s-1");
    assert!(account(&socket, port, &start)?);
    assert!(account(&socket, port, &start)?, "the retransmission is answered");
    assert!(account(&socket, port, &request(2, 2, "s-1"))?);
    let refused = udp(Duration::from_millis(500))?;
    assert!(!account(&refused, port, &request(3, 1, "refused"))?, "a rejected request is not answered");
    let written = wait_lines(&file, 2)?;
    std::thread::sleep(Duration::from_millis(300));
    let written_later = lines(&file);
    assert_eq!(written_later.len(), 2, "{written_later:?}");
    assert!(written[0].contains("\"Acct-Status-Type\":\"Start\"") && written[0].contains("\"handling\":\"script\""), "{}", written[0]);
    assert!(written[1].contains("\"Acct-Status-Type\":\"Stop\""), "{}", written[1]);
    assert!(written[0].starts_with("{\"time\":\"") && written[0].contains("\"source\":\"127.0.0.1\",\"transport\":\"udp\""), "{}", written[0]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).map_err(|e| e.to_string())?.permissions().mode() & 0o777;
        assert_eq!(mode & !0o640, 0, "{mode:o}");
    }
    Ok(())
}

/// spec Rotated by moving the file, New file name on reload, and a reload whose directory does
/// not exist.
#[test]
fn test_reopened_on_reload() -> Result<(), String> {
    let server = server()?;
    let (port, dir) = (server.port_acct(), server.dir.path().to_path_buf());
    let socket = udp(Duration::from_secs(5))?;
    assert!(account(&socket, port, &request(1, 1, "s-1"))?);
    wait_lines(&dir.join("acct.jsonl"), 1)?;
    std::fs::rename(dir.join("acct.jsonl"), dir.join("acct.jsonl.1")).map_err(|e| e.to_string())?;
    server.reload(RELOADED);
    assert!(account(&socket, port, &request(2, 1, "s-2"))?);
    assert_eq!(wait_lines(&dir.join("acct.jsonl"), 1)?.len(), 1);
    assert_eq!(lines(&dir.join("acct.jsonl.1")).len(), 1, "nothing more in the moved file");

    server.write_config_text(|text| text.replace("acct.jsonl", "acct2.jsonl"));
    server.reload(RELOADED);
    assert!(account(&socket, port, &request(3, 1, "s-3"))?);
    assert_eq!(wait_lines(&dir.join("acct2.jsonl"), 1)?.len(), 1);

    server.write_config_text(|text| text.replace("acct2.jsonl", "missing/acct.jsonl"));
    let stderr = server.reload("reload failed");
    assert!(stderr.contains("radius.accounting_log.file"), "{stderr}");
    assert!(account(&socket, port, &request(4, 1, "s-4"))?);
    assert_eq!(wait_lines(&dir.join("acct2.jsonl"), 2)?.len(), 2, "still the running file");
    assert_eq!(lines(&dir.join("acct.jsonl")).len(), 1);
    Ok(())
}

/// spec Every answer is in the file: SIGTERM right after 1000 answers.
#[test]
fn test_every_answer_written_through_shutdown() -> Result<(), String> {
    let mut server = server()?;
    let (port, file) = (server.port_acct(), server.dir.path().join("acct.jsonl"));
    let socket = udp(Duration::from_secs(5))?;
    for n in 0..1000u32 {
        let identifier = u8::try_from(n % 256).map_err(|e| e.to_string())?;
        assert!(account(&socket, port, &request(identifier, 1, &format!("s-{n}")))?, "request {n}");
    }
    let status = Command::new("kill").args(["-TERM", &server.child.id().to_string()]).status().map_err(|e| e.to_string())?;
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(30);
    while server.child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if Instant::now() > deadline {
            return Err(format!("did not stop: {}", server.stderr()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lines(&file).len(), 1000, "{}", server.stderr());
    Ok(())
}
