//! Shared fixtures for the unit tests: one database node and one set of Lua VMs per test
//! binary, and the log lines the server wrote. Also built with the `test-support` feature, for
//! the tests of crates built on this library.
use crate::config::{ClusterNode, ClusterSettings, DEFAULT_SNAPSHOT_EVERY};
use crate::lua_engine::{Db, LuaVms};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub mod bench;
pub mod certs;
pub mod eap;
pub mod radius;
pub mod scenario;
pub mod server;

/// The root of the core's repository, which holds this crate. A crate that depends on this one
/// by git gets the whole repository checked out, so the files below are there too.
pub fn core_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// The core's RADIUS dictionary, `share/dictionary`.
pub fn dictionary() -> PathBuf {
    core_root().join("share").join("dictionary")
}

/// A file of the core's examples, `rradius-server/examples/<name>`.
pub fn core_example(name: &str) -> PathBuf {
    core_root().join("rradius-server").join("examples").join(name)
}

/// A single-node cluster shared by every test in this test binary. Each
/// `#[tokio::test]` has its own runtime, so the node runs on a runtime of its
/// own that lives as long as the process; tests use distinct table names.
pub fn test_db() -> Db {
    static DB: OnceLock<Db> = OnceLock::new();
    DB.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_multi_thread().enable_all().build() else {
                panic!("test database: cannot build its runtime");
            };
            runtime.block_on(async move {
                let data_dir = std::env::temp_dir().join(format!("rradius-test-db-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&data_dir);
                let settings = ClusterSettings {
                    node_id: 1,
                    data_dir,
                    secret_raft: "test-raft-secret-0123".to_string(),
                    secret_api: "test-api-secret-01234".to_string(),
                    listen: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    nodes: vec![ClusterNode { id: 1, raft: free_port(), api: free_port() }],
                    migrations_dir: None,
                    migrations_wait: Duration::from_secs(60),
                    snapshot_every: DEFAULT_SNAPSHOT_EVERY,
                    read_connections: 4,
                    stop_without_leader: None,
                };
                let db = match Db::start(&settings).await {
                    Ok(db) => db,
                    Err(e) => panic!("test database: cannot start the node: {e}"),
                };
                if let Err(e) = db.wait_healthy(Duration::from_secs(60)).await {
                    panic!("test database: the node did not become healthy: {e}");
                }
                if tx.send(db).is_err() {
                    panic!("test database: nothing is waiting for the node");
                }
                std::future::pending::<()>().await;
            });
        });
        match rx.recv() {
            Ok(db) => db,
            Err(_) => panic!("test database: the node's thread ended before the node started"),
        }
    })
    .clone()
}

/// A free local port for the test node, as `127.0.0.1:<port>`.
fn free_port() -> String {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(e) => panic!("test database: cannot find a free port: {e}"),
    };
    match listener.local_addr() {
        Ok(address) => address.to_string(),
        Err(e) => panic!("test database: cannot read the free port: {e}"),
    }
}

/// Whether this process keeps what it logs for tests to look at, in `logged_errors` and
/// `logging::records_for_client`: in a test binary, yes. A program that `cargo test` starts
/// is built with this feature too, as a dev-dependency's features reach it, and would keep
/// every login for good; `logging::init`, which only a running program calls, turns it off.
static KEEPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub(crate) fn keeping() -> bool {
    KEEPING.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn stop_keeping() {
    KEEPING.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Every line `log_script_error` wrote in this test binary.
pub fn logged_errors() -> &'static Mutex<Vec<String>> {
    static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    LINES.get_or_init(Mutex::default)
}

/// A revocation check for tests: refuses a chain whose end-entity certificate was revoked,
/// which can happen while a connection is open.
#[derive(Default)]
pub struct FakeRevocation {
    revoked: parking_lot::Mutex<Vec<Vec<u8>>>,
}

impl FakeRevocation {
    pub fn revoke(&self, certificate: &[u8]) {
        self.revoked.lock().push(certificate.to_vec());
    }

    fn verdict(&self, chain: &[rustls::pki_types::CertificateDer<'static>]) -> Result<(), String> {
        match chain.first() {
            Some(leaf) if self.revoked.lock().iter().any(|revoked| revoked.as_slice() == leaf.as_ref()) => Err("revoked (test)".to_string()),
            _ => Ok(()),
        }
    }
}

impl crate::revocation::RevocationCheck for FakeRevocation {
    fn check<'a>(&'a self, chain: &'a [rustls::pki_types::CertificateDer<'static>]) -> futures::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.verdict(chain) })
    }

    fn recheck(self: Arc<Self>, chain: &[rustls::pki_types::CertificateDer<'static>]) -> Result<(), String> {
        self.verdict(chain)
    }
}

/// Writes `ca.pem`, and certificates with their keys signed by it: `server` and `client` for
/// localhost, `alice` for alice.example.com. Returns the client's.
pub fn write_pki(dir: &std::path::Path) -> Result<rustls::pki_types::CertificateDer<'static>, String> {
    use rustls::pki_types::pem::PemObject;
    let issuer = certs::ca(dir, "ca");
    for (name, dns, serial) in [("server", "localhost", 76), ("alice", "alice.example.com", 78), ("client", "localhost", 77)] {
        certs::named_leaf(dir, name, dns, serial, &issuer);
    }
    rustls::pki_types::CertificateDer::from_pem_file(dir.join("client.pem")).map_err(|e| e.to_string())
}

/// A configuration with only what loading it needs, and the directory that holds it.
pub fn minimal_config() -> Result<(tempfile::TempDir, crate::config::ServerConfig), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("rradius.toml");
    let body = "dictionary = \"dictionary\"\n[lua]\nscript = \"auth.lua\"\n\
        [cluster]\ndata_dir = \"data\"\nsecret_raft = \"raft-secret-0123456\"\nsecret_api = \"api-secret-01234567\"\n\
        [radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    let config = crate::config::load(&path).map_err(|e| e.to_string())?;
    Ok((dir, config))
}

/// A shared set of VMs for tests in this test binary.
pub fn test_vms() -> Arc<LuaVms> {
    static VMS: OnceLock<Arc<LuaVms>> = OnceLock::new();
    VMS.get_or_init(|| match LuaVms::with_count(2) {
        Ok(vms) => Arc::new(vms),
        Err(e) => panic!("test VMs: cannot create them: {e}"),
    })
    .clone()
}
