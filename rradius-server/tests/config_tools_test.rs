//! `rradius-server config show|diff|init`: the effective configuration, the differences between
//! two, and a starting one. None of them starts anything or writes a file.

use rradius_core::test_support::server::write_config;
use std::path::{Path, PathBuf};
use std::process::Command;

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

fn config(args: &[&str]) -> Result<(Option<i32>, String, String), String> {
    let output = Command::new(PROGRAM).arg("config").args(args).output().map_err(|e| e.to_string())?;
    Ok((output.status.code(), String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned()))
}

fn text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

fn path(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| "path not UTF-8".to_string())
}

/// A configuration in `dir`, and a copy of it named `name` with `change` made to its text.
fn with_copy(dir: &Path, name: &str, change: impl FnOnce(String) -> String) -> Result<(PathBuf, PathBuf), String> {
    let (original, _) = write_config(dir, "return 'accept'", None);
    let copy = dir.join(name);
    std::fs::write(&copy, change(text(&original)?)).map_err(|e| e.to_string())?;
    Ok((original, copy))
}

#[test]
fn test_show_fills_defaults_and_hides_secrets() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (file, _) = write_config(dir.path(), "return 'accept'", None);
    let (code, stdout, stderr) = config(&["show", "--config", path(&file)?])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.starts_with("# computed on this machine: radius.sockets = "), "{stdout}");
    assert!(stdout.contains("\nmax_auth_inflight = 4096\n") && stdout.contains("\nrequire_message_authenticator = \"yes\"\n"), "{stdout}");
    assert!(stdout.contains("\nsecret = \"<hidden>\"\n") && stdout.contains("\nsecret_raft = \"<hidden>\"\n"), "{stdout}");
    assert!(!stdout.contains("test-raft-secret") && !stdout.contains("secret = \"s\""), "{stdout}");
    assert!(!stdout.contains("[tacacs]"), "a section the file lacks is not added: {stdout}");
    // `[shutdown]` is in effect without the section: one node does not wait for a load balancer.
    assert!(stdout.contains("\n[shutdown]\n") && stdout.contains("\nready_delay_ms = 0\n") && stdout.contains("\ndrain_ms = 5000\n"), "{stdout}");
    // It started nothing.
    assert!(!dir.path().join("data").exists());
    Ok(())
}

/// With more than one node, `[shutdown]` waits 5 s for a load balancer by default (Defaults).
#[test]
fn test_show_of_shutdown_with_three_nodes() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let others = "[[cluster.nodes]]\nid = 2\nraft = \"127.0.0.2:8100\"\napi = \"127.0.0.2:8200\"\n[[cluster.nodes]]\nid = 3\nraft = \"127.0.0.3:8100\"\napi = \"127.0.0.3:8200\"\n[radius]\n";
    let (_, three) = with_copy(dir.path(), "three.toml", |text| text.replacen("[radius]\n", others, 1))?;
    let (code, stdout, stderr) = config(&["show", "--config", path(&three)?])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("\n[shutdown]\n") && stdout.contains("\nready_delay_ms = 5000\n") && stdout.contains("\ndrain_ms = 5000\n"), "{stdout}");
    Ok(())
}

/// A wait over 60000 ms fails `check` with status 2, naming the field (Out of range).
#[test]
fn test_check_of_shutdown_out_of_range() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (_, long) = with_copy(dir.path(), "long.toml", |text| format!("{text}[shutdown]\ndrain_ms = 60001\n"))?;
    let output = Command::new(PROGRAM).args(["check", "--config", path(&long)?]).output().map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("shutdown.drain_ms"), "{stderr}");
    Ok(())
}

#[test]
fn test_show_of_a_bad_configuration() -> Result<(), String> {
    let (code, stdout, stderr) = config(&["show", "--config", "/nonexistent/missing.toml"])?;
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty() && stderr.contains("/nonexistent/missing.toml"), "{stderr}");
    Ok(())
}

#[test]
fn test_diff_of_the_same_configuration() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    // The copy writes out a default the original leaves out.
    let (a, b) = with_copy(dir.path(), "b.toml", |text| text.replacen("[radius]\n", "[radius]\nmax_auth_inflight = 4096\n", 1))?;
    let (code, stdout, stderr) = config(&["diff", path(&a)?, path(&b)?])?;
    assert_eq!((code, stdout.as_str()), (Some(0), "no differences\n"), "{stderr}");
    Ok(())
}

#[test]
fn test_diff_of_one_setting() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (a, b) = with_copy(dir.path(), "b.toml", |text| text.replacen("[radius]\n", "[radius]\nmax_auth_inflight = 8192\n", 1).replacen("secret = \"s\"", "secret = \"other\"", 1))?;
    let (code, stdout, stderr) = config(&["diff", path(&a)?, path(&b)?])?;
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stdout.contains("radius.max_auth_inflight: 4096 | 8192\n"), "{stdout}");
    assert!(stdout.contains("radius.clients[127.0.0.1].secret: differs (hidden)\n") && !stdout.contains("other"), "{stdout}");
    Ok(())
}

#[test]
fn test_diff_of_two_nodes() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let second = "[[cluster.nodes]]\nid = 2\nraft = \"127.0.0.2:8100\"\napi = \"127.0.0.2:8200\"\n[radius]\n";
    let (one, two) = with_copy(dir.path(), "two.toml", |text| text.replacen("[radius]\n", second, 1).replacen("[cluster]\n", "[cluster]\nnode_id = 2\n", 1))?;
    std::fs::write(&one, text(&one)?.replacen("[radius]\n", second, 1)).map_err(|e| e.to_string())?;
    let (code, stdout, stderr) = config(&["diff", path(&one)?, path(&two)?])?;
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert_eq!(stdout, "no differences\nnode-specific:\n  cluster.node_id: 1 | 2\n");
    Ok(())
}

#[test]
fn test_diff_of_clients_in_another_order() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let first = "[[radius.clients]]\naddress = \"10.0.0.0/8\"\nsecret = \"t\"\n";
    let (a, b) = with_copy(dir.path(), "b.toml", |text| format!("{text}{first}"))?;
    std::fs::write(&a, text(&b)?).map_err(|e| e.to_string())?;
    // In b the 10.0.0.0/8 client comes first.
    let b_text = text(&b)?;
    let client = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
    std::fs::write(&b, b_text.replacen(client, "", 1) + client).map_err(|e| e.to_string())?;
    let (code, stdout, stderr) = config(&["diff", path(&a)?, path(&b)?])?;
    assert_eq!((code, stdout.as_str()), (Some(0), "no differences\n"), "{stderr}");
    Ok(())
}

#[test]
fn test_init_passes_the_check() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (code, first, stderr) = config(&["init"])?;
    assert_eq!(code, Some(0), "{stderr}");
    let (_, second, _) = config(&["init", "--lang", "zh-TW"])?;
    let secret = |text: &str| text.lines().find_map(|line| line.strip_prefix("secret_raft = \"")).map(|rest| rest.trim_end_matches('"').to_string());
    let (one, other) = (secret(&first).ok_or("no secret_raft")?, secret(&second).ok_or("no secret_raft")?);
    assert!(one != other && one.len() == 32 && one.chars().all(|c| c.is_ascii_alphanumeric()), "{one} {other}");
    assert!(second.contains("# 叢集的每個節點都要用相同的 secret_raft"), "{second}");
    let dictionary = rradius_core::test_support::dictionary();
    let file = dir.path().join("rradius.toml");
    std::fs::write(&file, first.replacen("/usr/share/rradius/dictionary", path(&dictionary)?, 1)).map_err(|e| e.to_string())?;
    std::fs::write(dir.path().join("auth.lua"), "return 'accept'").map_err(|e| e.to_string())?;
    let output = Command::new(PROGRAM).args(["check", "--config", path(&file)?]).output().map_err(|e| e.to_string())?;
    assert_eq!(String::from_utf8_lossy(&output.stdout), "configuration ok\n", "{}", String::from_utf8_lossy(&output.stderr));
    Ok(())
}

#[test]
fn test_wrong_arguments() -> Result<(), String> {
    for args in [&[][..], &["show", "extra"], &["diff", "a.toml"], &["init", "--lang", "fr"], &["unknown"]] {
        let (code, _, stderr) = config(args)?;
        assert_eq!(code, Some(2), "{args:?}");
        assert!(stderr.contains("usage: rradius-server config show"), "{args:?}: {stderr}");
    }
    Ok(())
}
