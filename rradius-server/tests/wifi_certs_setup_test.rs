//! The `setup.sh` of the `wifi-certs` scenario takes the directory to write into as its first
//! argument. The path may be relative to where the script is started from: the script works from
//! inside the directory it writes, so it has to make the path absolute first.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/scenarios/wifi-certs/setup.sh");

/// Runs the script in `from` with `argument`, and checks that everything it promises is in `out`.
fn set_up_in(from: &Path, argument: &str, out: &Path) -> TestResult {
    let made = Command::new("sh").arg(SCRIPT).arg(argument).current_dir(from).output()?;
    assert!(made.status.success(), "setup.sh {argument}: {:?}\n{}{}", made.status, String::from_utf8_lossy(&made.stdout), String::from_utf8_lossy(&made.stderr));
    let certs = out.join("certs");
    for name in ["ca.pem", "ca.key", "ca.crl", "server.pem", "server.key", "laptop-001.pem", "laptop-001.key", "laptop-002.pem", "laptop-002.key"] {
        assert!(certs.join(name).is_file(), "setup.sh {argument}: {name} is missing");
    }
    for name in ["ext.cnf", "ca.srl"] {
        assert!(!certs.join(name).exists(), "setup.sh {argument}: {name} was left behind");
    }
    Ok(())
}

#[test]
fn the_current_directory_as_a_relative_path() -> TestResult {
    let dir = tempfile::tempdir()?;
    set_up_in(dir.path(), ".", dir.path())
}

#[test]
fn a_relative_directory_that_does_not_exist_yet() -> TestResult {
    let dir = tempfile::tempdir()?;
    set_up_in(dir.path(), "lab/wifi", &dir.path().join("lab/wifi"))
}
