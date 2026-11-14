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
    // Logins between the two samples: their response times are shown, accounting's are not.
    let child = Command::new(PROGRAM).args(["status", "--config", &config, "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(300));
    for identifier in 4..=13 {
        login(&server, identifier)?;
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    let json: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| format!("{e}: {stdout}"))?;
    let radius = section(&json, "radius").ok_or("no RADIUS section")?;
    for key in ["auth_p50_ms", "auth_p99_ms"] {
        let value = item(radius, key).and_then(|item| item["value"].as_f64()).ok_or(format!("no {key}: {json}"))?;
        assert!(value > 0.0, "{key}: {json}");
    }
    assert!(item(radius, "acct_p50_ms").is_none() && item(radius, "acct_p99_ms").is_none(), "{json}");
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
    for args in [&["--bogus"][..], &["--config"], &["--json", "--json"], &["--cluster", "--cluster"]] {
        let (code, _, stderr) = status(args)?;
        assert_eq!(code, Some(2), "{args:?}");
        assert!(stderr.contains("usage: rradius-server status"), "{args:?}: {stderr}");
    }
    Ok(())
}

#[test]
fn test_cluster_wrong_arguments() -> Result<(), String> {
    let (code, _, stderr) = status(&["--cluster", "--json", "--cluster"])?;
    assert_eq!(code, Some(2));
    assert!(stderr.contains("[--cluster]"), "{stderr}");
    Ok(())
}

/// `--cluster` on the only node of a cluster: one member, the leader, with its own status.
#[test]
fn test_cluster_one_node() -> Result<(), String> {
    let (server, metrics) = server("")?;
    login(&server, 1)?;
    // The configuration `status` reads lists a node 2 the cluster does not have: the members
    // come from the cluster all the same.
    let text = std::fs::read_to_string(&server.config).map_err(|e| e.to_string())?;
    let listed = text.replacen("[radius]", &format!("[[cluster.nodes]]\nid = 2\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n[radius]", free_port(), free_port()), 1);
    let config = server.dir.path().join("status.toml");
    std::fs::write(&config, listed).map_err(|e| e.to_string())?;
    let config = config.to_string_lossy().into_owned();
    // The node leads once its management API answers; until then the members come from the configuration.
    let deadline = Instant::now() + Duration::from_secs(30);
    let json = loop {
        let (code, stdout, stderr) = status(&["--config", &config, "--cluster", "--json"])?;
        assert_eq!(code, Some(0), "{stderr}");
        let json: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| format!("{e}: {stdout}"))?;
        if json["members_from"] == "cluster" && json["nodes"][0]["role"] == "leader" {
            break json;
        }
        if Instant::now() > deadline {
            return Err(format!("not a one-member cluster led by node 1: {json}"));
        }
    };
    assert_eq!(json["cluster"], true);
    let nodes = json["nodes"].as_array().ok_or("no nodes")?;
    assert_eq!(nodes.len(), 1, "{json}");
    assert_eq!(nodes[0]["id"], 1);
    assert_eq!(nodes[0]["reachable"], true);
    assert_eq!(nodes[0]["address"], format!("127.0.0.1:{metrics}"));
    let radius = section(&nodes[0]["status"], "radius").ok_or("no RADIUS section in the member's status")?;
    assert_eq!(item(radius, "auth_requests").map(|item| item["total"].clone()), Some(serde_json::json!(1)), "{json}");
    assert_eq!(json["total"]["nodes"], 1);
    assert_eq!(json["total"]["reachable"], 1);
    // The same as text: one member line and the total.
    let (code, stdout, stderr) = status(&["--config", &config, "--cluster"])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.lines().any(|line| line.starts_with("1 ") && line.contains("leader")), "{stdout}");
    assert!(stdout.lines().any(|line| line.starts_with("total")), "{stdout}");
    Ok(())
}

/// Three nodes on their own loopback addresses, which only Linux has without setting up.
#[cfg(target_os = "linux")]
#[test]
fn test_cluster_three_nodes() -> Result<(), String> {
    use std::path::Path;
    struct Children(Vec<Option<std::process::Child>>);
    impl Drop for Children {
        fn drop(&mut self) {
            for child in self.0.iter_mut().filter_map(Option::take) {
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let ready_at = |host: &str, port: u16| -> Option<u16> {
        let mut stream = std::net::TcpStream::connect((host, port)).ok()?;
        stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: x\r\n\r\n").ok()?;
        let mut text = String::new();
        stream.read_to_string(&mut text).ok()?;
        text.split(' ').nth(1)?.parse().ok()
    };
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::write(dir.path().join("top.lua"), "return 'accept'").map_err(|e| e.to_string())?;
    let hosts = ["127.0.0.11", "127.0.0.12", "127.0.0.13"];
    let (raft, api, metrics) = (free_port(), free_port(), free_port());
    let node_list: String = hosts.iter().enumerate().map(|(i, host)| format!("[[cluster.nodes]]\nid = {}\nraft = \"{host}:{raft}\"\napi = \"{host}:{api}\"\n", i + 1)).collect();
    let mut configs = Vec::new();
    let mut children = Children(Vec::new());
    for (i, host) in hosts.iter().enumerate() {
        let config = format!(
            "dictionary = \"{dictionary}\"\n[lua]\nscript = \"top.lua\"\n\
             [cluster]\nnode_id = {id}\ndata_dir = \"data{id}\"\nlisten = \"{host}\"\n\
             secret_raft = \"cluster-raft-secret-01\"\nsecret_api = \"cluster-api-secret-012\"\n{node_list}\
             [radius]\nbind = \"{host}\"\nauth_port = {auth}\nacct_port = {acct}\n\
             [[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n\
             [metrics]\nbind = \"{host}\"\nport = {metrics}\n",
            dictionary = root.join("../share/dictionary").display(),
            id = i + 1,
            auth = free_port(),
            acct = free_port(),
        );
        let path = dir.path().join(format!("node{}.toml", i + 1));
        std::fs::write(&path, config).map_err(|e| e.to_string())?;
        let log = std::fs::File::create(dir.path().join(format!("node{}.log", i + 1))).map_err(|e| e.to_string())?;
        let child = Command::new(PROGRAM).arg("server").arg("--config").arg(&path).current_dir(dir.path()).stdout(Stdio::null()).stderr(log).spawn().map_err(|e| e.to_string())?;
        children.0.push(Some(child));
        configs.push(path.to_string_lossy().into_owned());
    }
    let logs = || (1..=3).map(|n| std::fs::read_to_string(dir.path().join(format!("node{n}.log"))).unwrap_or_default()).collect::<Vec<_>>().join("\n---\n");
    let deadline = Instant::now() + Duration::from_secs(90);
    while hosts.iter().any(|host| ready_at(host, metrics) != Some(200)) {
        if Instant::now() > deadline {
            return Err(format!("the nodes did not become ready: {}", logs()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let look = || -> Result<(Option<i32>, serde_json::Value, String), String> {
        let (code, stdout, stderr) = status(&["--config", &configs[0], "--cluster", "--json"])?;
        let json = serde_json::from_str(&stdout).map_err(|e| format!("{e}: {stdout} {stderr}"))?;
        Ok((code, json, stderr))
    };
    let (code, json, stderr) = look()?;
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(json["members_from"], "cluster", "{json}");
    assert_eq!(json["total"]["nodes"], 3, "{json}");
    assert_eq!(json["total"]["reachable"], 3, "{json}");
    for (node, host) in json["nodes"].as_array().ok_or("no nodes")?.iter().zip(hosts) {
        assert_eq!(node["address"], format!("{host}:{metrics}"), "{json}");
        assert_eq!(node["reachable"], true, "{json}");
    }
    assert_eq!(json["nodes"].as_array().ok_or("no nodes")?.iter().filter(|node| node["role"] == "leader").count(), 1, "{json}");
    // Node 3 stops: it is shown unreachable, the others still count.
    if let Some(mut child) = children.0[2].take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let (code, json, stderr) = look()?;
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(json["nodes"][2]["reachable"], false, "{json}");
    assert!(json["nodes"][2]["error"].as_str().is_some_and(|e| !e.is_empty()), "{json}");
    assert_eq!(json["total"]["reachable"], 2, "{json}");
    assert_eq!(json["total"]["nodes"], 3, "{json}");
    Ok(())
}

#[test]
fn test_cluster_secret_refused() -> Result<(), String> {
    let (server, _) = server("")?;
    let text = std::fs::read_to_string(&server.config).map_err(|e| e.to_string())?;
    let config = server.dir.path().join("wrong-secret.toml");
    std::fs::write(&config, text.replace("test-api-secret-01234", "another-api-secret-99")).map_err(|e| e.to_string())?;
    let (code, _, stderr) = status(&["--config", &config.to_string_lossy(), "--cluster"])?;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("secret_api"), "{stderr}");
    Ok(())
}

/// Nothing running: the members come from the configuration and none can be sampled.
#[test]
fn test_cluster_nothing_listening() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let port = free_port();
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    std::fs::write(&config, format!("{text}[metrics]\nport = {port}\n")).map_err(|e| e.to_string())?;
    let (code, stdout, stderr) = status(&["--config", &config.to_string_lossy(), "--cluster", "--json"])?;
    assert_eq!(code, Some(3), "{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| format!("{e}: {stdout}"))?;
    assert_eq!(json["members_from"], "configuration", "{json}");
    assert_eq!(json["nodes"][0]["reachable"], false, "{json}");
    assert_eq!(json["total"]["reachable"], 0, "{json}");
    Ok(())
}

#[test]
fn test_cluster_watching() -> Result<(), String> {
    let (server, _) = server("")?;
    let config = server.config.to_string_lossy().into_owned();
    let child = Command::new(PROGRAM).args(["status", "--config", &config, "--cluster", "--watch", "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(3500));
    let killed = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().map_err(|e| e.to_string())?;
    assert!(killed.success());
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    // Two samples for the first print, then one each second.
    assert!(stdout.lines().count() >= 2, "{stdout}");
    for line in stdout.lines() {
        let json: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("{e}: {line}"))?;
        assert_eq!(json["cluster"], true, "{json}");
        assert!((json["interval_seconds"].as_f64().unwrap_or(0.0) - 1.0).abs() < 0.5, "{json}");
    }
    Ok(())
}
