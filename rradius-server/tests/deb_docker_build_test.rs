//! `packages/deb/docker-build.sh` always names the platform of the container it builds in. Without
//! one, `docker run` takes whatever `rust` image is on the machine, also one of another
//! architecture, and the package comes out for that architecture. The script runs in a small git
//! repository of its own, with stand-ins for `docker` and `check-release.sh`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../packages/deb/docker-build.sh");

/// `tar`, `mktemp` and the rest come from here, and `git` where it is installed here.
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

/// Writes down each call, answers `docker version` from `$ENGINE` (fails when it is empty), and
/// for `docker run` leaves a package in the directory mounted on `/out`.
const DOCKER: &str = r#"#!/bin/sh
echo "$*" >> "$LOG"
case "$1" in
    version)
        if [ -z "$ENGINE" ]; then echo 'stand-in: cannot connect to the engine' >&2; exit 1; fi
        echo "$ENGINE" ;;
    run)
        for arg in "$@"; do
            case "$arg" in *:/out) touch "${arg%:/out}/rradius_1_test.deb" ;; esac
        done ;;
esac
"#;

struct Repo {
    dir: tempfile::TempDir,
    /// Outside the repository, so that `git add -A` does not take it in.
    _tools: tempfile::TempDir,
    search: String,
}

impl Repo {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let tools = tempfile::tempdir()?;
        let search = search_path(tools.path())?;
        let repo = Self { dir: tempfile::tempdir()?, _tools: tools, search };
        repo.script("packages/deb/docker-build.sh", &std::fs::read_to_string(SCRIPT)?)?;
        repo.script("packages/common/check-release.sh", "#!/bin/sh\nexit 0\n")?;
        repo.git(&["init", "-q"])?;
        repo.git(&["add", "-A"])?;
        repo.git(&["commit", "-q", "-m", "first"])?;
        // Not part of the repository: it only has to be on the PATH.
        repo.script("bin/docker", DOCKER)?;
        Ok(repo)
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn script(&self, relative: &str, text: &str) -> TestResult {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
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

    /// Runs the build script. The engine reports `engine` as its platform.
    fn run(&self, args: &[&str], engine: &str, env: &[(&str, &str)]) -> Result<Output, Box<dyn std::error::Error>> {
        Ok(Command::new("/bin/sh")
            .arg(self.path("packages/deb/docker-build.sh"))
            .arg("--out")
            .arg(self.path("out"))
            .args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", self.path("bin").display(), self.search))
            .env("LOG", self.path("docker.log"))
            .env("ENGINE", engine)
            .envs(env.iter().copied())
            .current_dir(Path::new("/"))
            .output()?)
    }

    /// The calls of `docker`, one line each; empty when there was none.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("docker.log")).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    /// The only `docker run` line. The script passed to the container spans several lines, so
    /// only the first line of the call is looked at.
    fn run_call(&self) -> String {
        let calls: Vec<String> = self.calls().into_iter().filter(|line| line.starts_with("run ")).collect();
        assert_eq!(calls.len(), 1, "{:?}", self.calls());
        calls.concat()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn test_docker_build_names_the_platform_of_the_engine() -> TestResult {
    let repo = Repo::new()?;
    let output = repo.run(&[], "linux/arm64", &[])?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(repo.run_call().starts_with("run --rm --platform linux/arm64 "), "{}", repo.run_call());
    // The crate cache keeps the name it had.
    assert!(repo.run_call().contains(" rradius-deb-cargo-native:"), "{}", repo.run_call());
    Ok(())
}

#[test]
fn test_docker_build_uses_the_given_platform_without_asking_the_engine() -> TestResult {
    let repo = Repo::new()?;
    let output = repo.run(&["--platform", "linux/amd64"], "linux/arm64", &[("DOCKER_DEFAULT_PLATFORM", "linux/arm64")])?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(repo.run_call().starts_with("run --rm --platform linux/amd64 "), "{}", repo.run_call());
    assert!(repo.run_call().contains(" rradius-deb-cargo-linux-amd64:"), "{}", repo.run_call());
    assert_eq!(repo.calls().iter().filter(|line| line.starts_with("version")).count(), 0, "{:?}", repo.calls());
    Ok(())
}

#[test]
fn test_docker_build_keeps_docker_default_platform() -> TestResult {
    let repo = Repo::new()?;
    let output = repo.run(&[], "linux/arm64", &[("DOCKER_DEFAULT_PLATFORM", "linux/amd64")])?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(repo.run_call().starts_with("run --rm --platform linux/amd64 "), "{}", repo.run_call());
    Ok(())
}

#[test]
fn test_docker_build_stops_when_the_engine_does_not_say_its_platform() -> TestResult {
    let repo = Repo::new()?;
    let output = repo.run(&[], "", &[])?;
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("docker-build.sh: cannot ask docker for its platform; give --platform"), "{}", stderr(&output));
    assert!(!repo.calls().iter().any(|line| line.starts_with("run ")), "{:?}", repo.calls());
    Ok(())
}
