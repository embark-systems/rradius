//! `packages/common/check-release.sh`, which the deb and rpm build scripts run before they build:
//! the Release workflow must have passed for the commit (unless `--no-ci-check`), and the version
//! of `rradius-server/Cargo.toml` must be the version of the changelog, the spec and the FreeBSD
//! Makefile. The script runs in a small git repository of its own, with a stand-in for
//! `.github/scripts/require-release-run.sh` (the real one needs `gh` and the network). The real
//! gate is tested at the end, with a stand-in `gh`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../packages/common/check-release.sh");

/// `sed`, `head` and `cut` come from here, and `git` where it is installed here.
const SYSTEM_PATH: &str = "/usr/bin:/bin";

/// `PATH` of the script under test: `SYSTEM_PATH`, with `git` in front when it is installed
/// somewhere else (FreeBSD has it in `/usr/local/bin`). Only `git` is linked into `tools`, so no
/// other program of that directory, such as a real `gh` or `docker`, comes along.
fn search_path(tools: &Path) -> Result<String, Box<dyn std::error::Error>> {
    if std::env::split_paths(SYSTEM_PATH).any(|dir| dir.join("git").is_file()) {
        return Ok(SYSTEM_PATH.to_string());
    }
    let found = std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).map(|dir| dir.join("git")).find(|git| git.is_file()))
        .ok_or("git is not on the PATH")?;
    std::fs::create_dir_all(tools)?;
    match std::os::unix::fs::symlink(found, tools.join("git")) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(error.into()),
        _ => Ok(format!("{}:{SYSTEM_PATH}", tools.display())),
    }
}

const VERSION: &str = "1.2.3";

/// A git repository with the files the script reads, and the stand-in for the workflow check.
struct Repo {
    dir: tempfile::TempDir,
    /// Outside the repository, so that the working tree stays clean.
    _tools: tempfile::TempDir,
    search: String,
}

impl Repo {
    /// The stand-in exits with `gate_exit` and leaves `.called` behind.
    fn new(gate_exit: i32) -> Result<Self, Box<dyn std::error::Error>> {
        let tools = tempfile::tempdir()?;
        let search = search_path(tools.path())?;
        let repo = Self { dir: tempfile::tempdir()?, _tools: tools, search };
        repo.write("packages/common/check-release.sh", &std::fs::read_to_string(SCRIPT)?)?;
        repo.make_executable("packages/common/check-release.sh")?;
        repo.write(
            ".github/scripts/require-release-run.sh",
            &format!("#!/bin/sh\ntouch \"$(dirname \"$0\")/../../.called\"\necho 'stand-in: gate said no' >&2\nexit {gate_exit}\n"),
        )?;
        repo.make_executable(".github/scripts/require-release-run.sh")?;
        repo.write("rradius-server/Cargo.toml", &format!("[package]\nname = \"rradius-server\"\nversion = \"{VERSION}\"\nedition = \"2021\"\n"))?;
        repo.write("packages/deb/debian/changelog", &format!("rradius ({VERSION}) unstable; urgency=medium\n\n  * Package of rradius {VERSION}.\n"))?;
        repo.write("packages/rpm/rradius.spec", &format!("Name:           rradius\nVersion:        {VERSION}\nRelease:        1%{{?dist}}\n"))?;
        repo.write("packages/freebsd/net/rradius/Makefile", &format!("PORTNAME=\trradius\nDISTVERSION=\t{VERSION}\nCATEGORIES=\tnet\n"))?;
        repo.git(&["init", "-q"])?;
        repo.commit("first")?;
        Ok(repo)
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, text: &str) -> TestResult {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    fn make_executable(&self, relative: &str) -> TestResult {
        std::fs::set_permissions(self.path(relative), std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    /// The text of a file in the working tree with `from` replaced by `to`.
    fn replace(&self, relative: &str, from: &str, to: &str) -> TestResult {
        let text = std::fs::read_to_string(self.path(relative))?;
        assert!(text.contains(from), "{relative} has no {from}");
        self.write(relative, &text.replace(from, to))
    }

    fn git(&self, args: &[&str]) -> TestResult {
        let output = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(self.dir.path())
            .env("PATH", &self.search)
            .output()?;
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        Ok(())
    }

    fn commit(&self, message: &str) -> TestResult {
        // The stand-in's marker is not part of the repository.
        self.write(".gitignore", ".called\n")?;
        self.git(&["add", "-A"])?;
        self.git(&["commit", "-q", "-m", message])
    }

    fn called(&self) -> bool {
        self.path(".called").exists()
    }

    /// Runs the script with only `PATH` in its environment.
    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn std::error::Error>> {
        Ok(Command::new("/bin/sh")
            .arg(self.path("packages/common/check-release.sh"))
            .args(args)
            .env_clear()
            .env("PATH", &self.search)
            .current_dir(Path::new("/"))
            .output()?)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn test_check_release_passes_when_the_gate_and_the_versions_agree() -> TestResult {
    let repo = Repo::new(0)?;
    let output = repo.run(&["HEAD"])?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("release check passed for"), "{}", stdout(&output));
    assert!(stdout(&output).contains(VERSION), "{}", stdout(&output));
    assert!(repo.called(), "the gate was not asked");
    Ok(())
}

#[test]
fn test_check_release_stops_when_the_gate_fails() -> TestResult {
    let repo = Repo::new(1)?;
    let output = repo.run(&["HEAD"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("stand-in: gate said no"), "{}", stderr(&output));
    assert!(!stdout(&output).contains("passed"), "{}", stdout(&output));
    Ok(())
}

#[test]
fn test_check_release_no_ci_check_skips_only_the_gate() -> TestResult {
    let repo = Repo::new(1)?;
    let output = repo.run(&["HEAD", "--no-ci-check"])?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!repo.called(), "the gate was asked although it was skipped");

    // The flag may come first, and a wrong version still stops it.
    repo.replace("packages/rpm/rradius.spec", VERSION, "9.9.9")?;
    repo.commit("wrong spec")?;
    let output = repo.run(&["--no-ci-check", "HEAD"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("packages/rpm/rradius.spec: 9.9.9"), "{}", stderr(&output));
    Ok(())
}

/// Each of the three package files with a version of its own is named when it differs.
fn version_differs(relative: &str, from: &str, to: &str) -> TestResult {
    let repo = Repo::new(0)?;
    repo.replace(relative, from, to)?;
    repo.commit("wrong version")?;
    let output = repo.run(&["HEAD"])?;
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains(&format!("{relative}: 9.9.9 (Cargo.toml: {VERSION})")), "{message}");
    assert_eq!(message.matches("(Cargo.toml:").count(), 1, "only the file that differs is named: {message}");
    assert!(!stdout(&output).contains("passed"), "{}", stdout(&output));
    Ok(())
}

#[test]
fn test_check_release_names_the_changelog_when_it_differs() -> TestResult {
    version_differs("packages/deb/debian/changelog", &format!("rradius ({VERSION})"), "rradius (9.9.9)")
}

#[test]
fn test_check_release_names_the_spec_when_it_differs() -> TestResult {
    version_differs("packages/rpm/rradius.spec", &format!("Version:        {VERSION}"), "Version:        9.9.9")
}

#[test]
fn test_check_release_names_the_makefile_when_it_differs() -> TestResult {
    version_differs("packages/freebsd/net/rradius/Makefile", &format!("DISTVERSION=\t{VERSION}"), "DISTVERSION=\t9.9.9")
}

#[test]
fn test_check_release_names_every_file_that_differs() -> TestResult {
    let repo = Repo::new(0)?;
    repo.replace("packages/rpm/rradius.spec", VERSION, "9.9.9")?;
    repo.replace("packages/freebsd/net/rradius/Makefile", VERSION, "8.8.8")?;
    repo.commit("two wrong versions")?;
    let output = repo.run(&["HEAD"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("packages/rpm/rradius.spec: 9.9.9"), "{}", stderr(&output));
    assert!(stderr(&output).contains("packages/freebsd/net/rradius/Makefile: 8.8.8"), "{}", stderr(&output));
    Ok(())
}

#[test]
fn test_check_release_fails_when_a_version_cannot_be_read() -> TestResult {
    let repo = Repo::new(0)?;
    repo.replace("packages/rpm/rradius.spec", "Version:", "Versions:")?;
    repo.commit("no version in the spec")?;
    let output = repo.run(&["HEAD"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("cannot read the version from packages/rpm/rradius.spec"), "{}", stderr(&output));

    let repo = Repo::new(0)?;
    std::fs::remove_file(repo.path("rradius-server/Cargo.toml"))?;
    repo.commit("no Cargo.toml")?;
    let output = repo.run(&["HEAD"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("cannot read the version from rradius-server/Cargo.toml"), "{}", stderr(&output));
    Ok(())
}

#[test]
fn test_check_release_fails_for_a_commit_that_does_not_exist() -> TestResult {
    let repo = Repo::new(0)?;
    let output = repo.run(&["no-such-commit"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no commit 'no-such-commit'"), "{}", stderr(&output));
    assert!(!repo.called(), "the gate was asked for a commit that does not exist");
    Ok(())
}

#[test]
fn test_check_release_reads_the_commit_and_not_the_working_tree() -> TestResult {
    let repo = Repo::new(0)?;
    // The working tree has a wrong version that is not committed: the commit is right.
    repo.replace("packages/rpm/rradius.spec", VERSION, "9.9.9")?;
    let output = repo.run(&["HEAD"])?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("uncommitted changes"), "no warning: {}", stderr(&output));
    Ok(())
}

#[test]
fn test_check_release_does_not_warn_for_a_clean_working_tree() -> TestResult {
    let repo = Repo::new(0)?;
    let output = repo.run(&["HEAD"])?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!stderr(&output).contains("uncommitted"), "{}", stderr(&output));
    Ok(())
}

#[test]
fn test_check_release_usage_errors_exit_with_2() -> TestResult {
    let repo = Repo::new(0)?;
    for args in [&[][..], &["--no-ci-check"][..], &["HEAD", "--nope"][..], &["HEAD", "other"][..]] {
        let output = repo.run(args)?;
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", stderr(&output));
        assert!(stderr(&output).contains("usage:"), "{args:?}: {}", stderr(&output));
    }
    assert!(!repo.called());
    Ok(())
}

const GATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../.github/scripts/require-release-run.sh");

/// The real gate script in a git repository with an `origin`, and a stand-in `gh` in front of the
/// system `PATH`: it prints `stdout` and `stderr` and exits with `code`.
fn run_gate(code: i32, stdout: &str, stderr: &str) -> Result<Output, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join(".github/scripts/require-release-run.sh");
    std::fs::create_dir_all(script.parent().ok_or("no parent")?)?;
    std::fs::copy(GATE, &script)?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    let gh = dir.path().join("bin/gh");
    std::fs::create_dir_all(gh.parent().ok_or("no parent")?)?;
    std::fs::write(&gh, format!("#!/bin/sh\n[ -z '{stdout}' ] || echo '{stdout}'\n[ -z '{stderr}' ] || echo '{stderr}' >&2\nexit {code}\n"))?;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))?;
    let system = search_path(&dir.path().join("tools"))?;
    for args in [&["init", "-q"][..], &["remote", "add", "origin", "https://github.com/example/repo.git"][..]] {
        let output = Command::new("git").args(args).current_dir(dir.path()).env("PATH", &system).output()?;
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }
    let path = format!("{}:{system}", dir.path().join("bin").display());
    Ok(Command::new("/bin/sh").arg(&script).arg("abc123").env_clear().env("PATH", path).current_dir(Path::new("/")).output()?)
}

#[test]
fn test_gate_reports_a_gh_failure_apart_from_a_missing_run() -> TestResult {
    let output = run_gate(4, "", "HTTP 401: Bad credentials")?;
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("gh could not look up the runs of release.yml in example/repo"), "{message}");
    assert!(message.contains("HTTP 401: Bad credentials"), "{message}");
    assert!(message.contains("gh auth status"), "{message}");
    assert!(!message.contains("no passed run"), "{message}");
    Ok(())
}

#[test]
fn test_gate_reports_a_missing_run_when_gh_works() -> TestResult {
    let output = run_gate(0, "0", "")?;
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("no passed run of release.yml for abc123 in example/repo"), "{message}");
    assert!(!message.contains("could not look up"), "{message}");
    Ok(())
}

#[test]
fn test_gate_passes_when_a_run_passed() -> TestResult {
    let output = run_gate(0, "1", "")?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("release.yml passed for abc123"), "{}", stdout(&output));
    Ok(())
}

#[test]
fn test_gate_treats_output_that_is_not_a_number_as_a_failure() -> TestResult {
    let output = run_gate(0, "oops", "")?;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("could not look up"), "{}", stderr(&output));
    Ok(())
}
