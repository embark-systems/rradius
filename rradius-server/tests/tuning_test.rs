//! `rradius-server tuning`: reads the system and the configuration, prints advice, changes nothing.

use rradius_core::test_support::server::write_config;
use std::path::Path;
use std::process::Command;

const PROGRAM: &str = env!("CARGO_BIN_EXE_rradius-server");

fn tuning(args: &[&str]) -> Result<(Option<i32>, String, String), String> {
    let output = Command::new(PROGRAM).arg("tuning").args(args).output().map_err(|e| e.to_string())?;
    Ok((output.status.code(), String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned()))
}

fn platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "FreeBSD"
    }
}

#[test]
fn test_report_from_a_configuration() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let path = config.to_str().ok_or("path not UTF-8")?;
    let (code, stdout, stderr) = tuning(&["--config", path])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.starts_with(&format!("rradius tuning advice for {}\n", platform())), "{stdout}");
    assert!(stdout.contains("UDP packets dropped because a receive buffer was full, since the system started: "), "{stdout}");
    assert!(stdout.contains("Open files of the process (RLIMIT_NOFILE)"), "{stdout}");
    assert!(stdout.contains("UDP receive buffer"), "the configuration has [radius]: {stdout}");
    assert!(stdout.trim_end().ends_with("confirm with the dropped packet counts above."), "{stdout}");
    // It started nothing: no data directory.
    assert!(!dir.path().join("data").exists());
    Ok(())
}

#[test]
fn test_report_with_recv_buffer_bytes() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (config, _) = write_config(dir.path(), "return 'accept'", None);
    let text = std::fs::read_to_string(&config).map_err(|e| e.to_string())?;
    if !text.contains("[radius]\n") {
        return Err(format!("no [radius] in {text}"));
    }
    std::fs::write(&config, text.replacen("[radius]\n", "[radius]\nrecv_buffer_bytes = 2097152\n", 1)).map_err(|e| e.to_string())?;
    let path = config.to_str().ok_or("path not UTF-8")?;
    let (code, stdout, stderr) = tuning(&["--config", path])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("Receive buffer of each RADIUS socket ([radius] recv_buffer_bytes)\n  current:   2097152\n"), "{stdout}");
    assert!(!stdout.contains("rmem_default") && !stdout.contains("recvspace"), "the system default is not used: {stdout}");
    Ok(())
}

#[test]
fn test_unreadable_configuration() -> Result<(), String> {
    let (code, stdout, stderr) = tuning(&["--config", "/nonexistent/missing.toml"])?;
    assert_eq!(code, Some(2));
    assert!(stderr.contains("/nonexistent/missing.toml") && stdout.is_empty(), "{stderr}");
    Ok(())
}

#[test]
fn test_no_configuration_file() -> Result<(), String> {
    let default = rradius_core::config::default_config_path();
    if Path::new(default).exists() {
        return Ok(());
    }
    let (code, stdout, stderr) = tuning(&[])?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.starts_with(&format!("{default}: ")) && stdout.lines().next().is_some_and(|line| line.ends_with("; the default settings are used")), "{stdout}");
    assert!(stdout.contains(&format!("rradius tuning advice for {}", platform())), "{stdout}");
    Ok(())
}

#[test]
fn test_unknown_argument() -> Result<(), String> {
    let (code, _, stderr) = tuning(&["--apply"])?;
    assert_eq!(code, Some(2));
    assert_eq!(stderr.trim_end(), "usage: rradius-server tuning [--config <path>]");
    Ok(())
}
