//! Three `rradius-server` processes forming one hiqlite cluster. Nodes run as
//! separate processes because stopping one in-process node does not behave like
//! a node failure (see spikes/hiqlite/FINDINGS.md).
use rradius_core::test_support::server::free_port;
use rradius_core::radius::{self, RadiusPacket};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"cluster-secret";

/// Kills every node process when the test ends, also on failure, and then
/// prints what the nodes logged if the test failed.
struct Nodes(Vec<Option<Child>>, Vec<PathBuf>);

impl Nodes {
    /// Starts node `index + 1` from `node<n>.toml` in `dir`, logging to its log file.
    fn start(&mut self, index: usize, dir: &Path) -> std::io::Result<()> {
        let path = dir.join(format!("node{}.toml", index + 1));
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&self.1[index])?;
        let child = Command::new(env!("CARGO_BIN_EXE_rradius-server")).arg("server").arg("--config").arg(path).stdout(Stdio::null()).stderr(log).spawn()?;
        self.0[index] = Some(child);
        Ok(())
    }

    fn kill(&mut self, index: usize) {
        if let Some(mut child) = self.0[index].take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Nodes {
    fn drop(&mut self) {
        for i in 0..self.0.len() {
            self.kill(i);
        }
        if std::thread::panicking() {
            for log in &self.1 {
                eprintln!("--- {}\n{}", log.display(), std::fs::read_to_string(log).unwrap_or_default());
            }
        }
    }
}

/// Sends one RADIUS packet and waits up to `wait` for the reply.
fn send(port: u16, packet: &[u8], wait: Duration) -> Option<RadiusPacket> {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(wait)).unwrap();
    socket.send_to(packet, ("127.0.0.1", port)).unwrap();
    let mut buf = [0u8; 4096];
    let (size, _) = socket.recv_from(&mut buf).ok()?;
    Some(RadiusPacket::parse(&buf[..size]).unwrap())
}

fn access_request(id: u8, user: &str) -> Vec<u8> {
    RadiusPacket::create_access_request(id, [id; 16], vec![(1, user.as_bytes().to_vec())], SECRET)
}

/// A Status-Server signed with the cluster's client secret.
fn status_server(id: u8) -> Vec<u8> {
    use hmac::{KeyInit, Mac};
    let mut packet = RadiusPacket { code: radius::STATUS_SERVER, identifier: id, authenticator: [id; 16], attributes: vec![(radius::MESSAGE_AUTHENTICATOR, vec![0; 16])] }.encode();
    if let Ok(mut mac) = hmac::Hmac::<md5::Md5>::new_from_slice(SECRET) {
        mac.update(&packet);
        packet[22..38].copy_from_slice(&mac.finalize().into_bytes());
    }
    packet
}

/// One sample of a node's `/metrics`, by its name and labels.
fn metric(port: u16, sample: &str) -> Option<f64> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).ok()?;
    body.lines().find_map(|line| line.strip_prefix(sample).and_then(|rest| rest.strip_prefix(' ')).and_then(|value| value.trim().parse().ok()))
}

/// Repeats `check` every 200 ms until it returns true or `limit` passes.
fn eventually(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[test]
fn test_three_node_cluster() {
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let raft: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let api: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let auth: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let acct: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let metrics: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let node_list: String = (0..3)
        .map(|i| format!("[[cluster.nodes]]\nid = {}\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n", i + 1, raft[i], api[i]))
        .collect();

    let mut nodes = Nodes(Vec::new(), Vec::new());
    for i in 0..3 {
        // Node 1 stops answering without a leader; the others go on as by default.
        let stop = if i == 0 { "stop_without_leader_ms = 3000\n" } else { "" };
        let config = format!(
            "dictionary = \"{dictionary}\"\n[lua]\nscript = \"{script}\"\nquery_timeout_ms = 500\n\
             [cluster]\nnode_id = {id}\ndata_dir = \"data{id}\"\nlisten = \"127.0.0.1\"\n{stop}\
             secret_raft = \"cluster-raft-secret-01\"\nsecret_api = \"cluster-api-secret-012\"\n{node_list}\
             [radius]\nbind = \"127.0.0.1\"\nauth_port = {auth}\nacct_port = {acct}\n\
             [[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"cluster-secret\"\n\
             [metrics]\nbind = \"127.0.0.1\"\nport = {metrics}\n",
            dictionary = root.join("../share/dictionary").display(),
            script = root.join("tests/fixtures/cluster.lua").display(),
            id = i + 1,
            auth = auth[i],
            acct = acct[i],
            metrics = metrics[i],
        );
        let path = dir.path().join(format!("node{}.toml", i + 1));
        std::fs::write(&path, config).unwrap();
        let log = dir.path().join(format!("node{}.log", i + 1));
        nodes.1.push(log);
        nodes.0.push(None);
        assert!(nodes.start(i, dir.path()).is_ok(), "node {} did not start", i + 1);
    }

    // Writes succeed on node 2 once the cluster has a leader.
    let ready = eventually(Duration::from_secs(60), || {
        send(auth[1], &access_request(1, "write"), Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCESS_ACCEPT)
    });
    assert!(ready, "cluster did not accept writes within 60 s");

    // Write on node 2 through accounting, read on node 1 through authentication.
    let acct_request = RadiusPacket::create_accounting_request(2, vec![(40, vec![0, 0, 0, 1]), (44, b"from-node-2".to_vec())], SECRET);
    // Retransmitted like a NAS would: a write that times out gets no response.
    let acknowledged = eventually(Duration::from_secs(5), || {
        send(acct[1], &acct_request, Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCOUNTING_RESPONSE)
    });
    assert!(acknowledged, "no Accounting-Response from node 2 within 5 s");
    let replicated = eventually(Duration::from_secs(5), || {
        send(auth[0], &access_request(3, "lookup"), Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCESS_ACCEPT)
    });
    assert!(replicated, "session written on node 2 was not readable on node 1 within 5 s");

    // A value bound as a parameter is replicated the same way.
    let acct_request = RadiusPacket::create_accounting_request(5, vec![(40, vec![0, 0, 0, 1]), (44, b"o'brien-from-node-2".to_vec())], SECRET);
    let acknowledged = eventually(Duration::from_secs(5), || {
        send(acct[1], &acct_request, Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCOUNTING_RESPONSE)
    });
    assert!(acknowledged, "no Accounting-Response for the quoted session within 5 s");
    for (node, identifier) in [(0, 6), (2, 7)] {
        let replicated = eventually(Duration::from_secs(5), || {
            send(auth[node], &access_request(identifier, "lookup-quoted"), Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCESS_ACCEPT)
        });
        assert!(replicated, "the parameter written on node 2 was not readable on node {} within 5 s", node + 1);
    }

    // Node 3 is down while a session is written; back, it answers nothing before it has it.
    nodes.kill(2);
    let later = RadiusPacket::create_accounting_request(8, vec![(40, vec![0, 0, 0, 1]), (44, b"later-from-node-2".to_vec())], SECRET);
    let acknowledged = eventually(Duration::from_secs(5), || send(acct[1], &later, Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCOUNTING_RESPONSE));
    assert!(acknowledged, "no Accounting-Response for the session written while node 3 was down");
    assert!(nodes.start(2, dir.path()).is_ok(), "node 3 did not start again");
    let mut first = None;
    let answered = eventually(Duration::from_secs(60), || {
        first = send(auth[2], &access_request(9, "lookup-later"), Duration::from_millis(500)).map(|r| r.code);
        first.is_some()
    });
    assert!(answered, "node 3 did not answer within 60 s of its restart");
    assert_eq!(first, Some(radius::ACCESS_ACCEPT), "node 3's first answer did not see what was written while it was down");
    let log = std::fs::read_to_string(&nodes.1[2]).unwrap_or_default();
    let restarted = log.rfind("cluster node 3 started").unwrap_or(0);
    let caught_up = log[restarted..].find("has caught up").map(|at| at + restarted);
    let listening = log[restarted..].find("RADIUS Server listening").map(|at| at + restarted);
    assert!(caught_up.is_some() && caught_up < listening, "node 3 listened before it caught up");

    // Status-Server is answered while node 1 has a leader.
    assert_eq!(send(auth[0], &status_server(10), Duration::from_secs(2)).map(|r| r.code), Some(radius::ACCESS_ACCEPT));
    assert_eq!(metric(metrics[0], "rradius_cluster_leader"), Some(1.0));

    // With only node 1 left there is no quorum: the write in the script times out, and the
    // request goes unanswered rather than refused, so that a NAS asks another server.
    nodes.kill(1);
    nodes.kill(2);
    let reply = send(auth[0], &access_request(4, "write"), Duration::from_secs(2));
    assert!(reply.is_none(), "a write without quorum was answered: {:?}", reply.map(|r| r.code));
    let counted = eventually(Duration::from_secs(5), || metric(metrics[0], "rradius_radius_discarded_total{reason=\"database\"}").is_some_and(|n| n >= 1.0));
    assert!(counted, "the unanswered login was not counted");
    let leaderless = eventually(Duration::from_secs(15), || metric(metrics[0], "rradius_cluster_leader") == Some(0.0));
    assert!(leaderless, "node 1 still shows a leader");
    // Past stop_without_leader_ms: no Status-Server, and no read-only login either.
    let stopped = eventually(Duration::from_secs(15), || metric(metrics[0], "rradius_cluster_seconds_without_leader").is_some_and(|n| n >= 4.0));
    assert!(stopped, "seconds without a leader did not grow");
    assert!(send(auth[0], &status_server(11), Duration::from_secs(1)).is_none(), "Status-Server answered without a leader");
    assert!(send(auth[0], &access_request(12, "lookup"), Duration::from_secs(1)).is_none(), "a login answered without a leader");
    assert!(metric(metrics[0], "rradius_radius_discarded_total{reason=\"no_leader\"}").is_some_and(|n| n >= 2.0));

    drop(nodes);
    assert!(started.elapsed() < Duration::from_secs(90), "cluster test took {:?}", started.elapsed());
}

/// A follower paused while the leader snapshots and drops the log it would need comes back with
/// its Raft core stopped (hiqlite 0.16 cannot read its own log then): the process exits with
/// status 1 instead of answering from a replica that no longer follows, and a restart recovers it.
#[test]
fn test_node_whose_raft_core_stops_exits() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let raft: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let api: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let auth: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let acct: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let node_list: String = (0..3).map(|i| format!("[[cluster.nodes]]\nid = {}\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n", i + 1, raft[i], api[i])).collect();
    let mut nodes = Nodes(Vec::new(), Vec::new());
    for i in 0..3 {
        let config = format!(
            "dictionary = \"{dictionary}\"\n[lua]\nscript = \"{script}\"\nquery_timeout_ms = 2000\n\
             [cluster]\nnode_id = {id}\ndata_dir = \"data{id}\"\nlisten = \"127.0.0.1\"\nsnapshot_every = 50\n\
             secret_raft = \"cluster-raft-secret-01\"\nsecret_api = \"cluster-api-secret-012\"\n{node_list}\
             [radius]\nbind = \"127.0.0.1\"\nauth_port = {auth}\nacct_port = {acct}\n\
             [[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"cluster-secret\"\n",
            dictionary = root.join("../share/dictionary").display(),
            script = root.join("tests/fixtures/cluster.lua").display(),
            id = i + 1,
            auth = auth[i],
            acct = acct[i],
        );
        std::fs::write(dir.path().join(format!("node{}.toml", i + 1)), config).map_err(|e| e.to_string())?;
        nodes.1.push(dir.path().join(format!("node{}.log", i + 1)));
        nodes.0.push(None);
        nodes.start(i, dir.path()).map_err(|e| e.to_string())?;
    }
    let ready = eventually(Duration::from_secs(60), || (0..3).all(|i| send(auth[i], &access_request(1, "lookup"), Duration::from_millis(500)).is_some()));
    assert!(ready, "the cluster did not answer within 60 s");

    let paused = nodes.0[2].as_ref().ok_or("no node 3")?.id().to_string();
    Command::new("kill").args(["-STOP", &paused]).status().map_err(|e| e.to_string())?;
    for n in 0..400u32 {
        let id = (n % 250) as u8;
        let request = RadiusPacket::create_accounting_request(id, vec![(40, vec![0, 0, 0, 1]), (44, format!("while-paused-{n}").into_bytes())], SECRET);
        let acknowledged = eventually(Duration::from_secs(5), || send(acct[0], &request, Duration::from_secs(1)).is_some());
        assert!(acknowledged, "write {n} not acknowledged");
    }
    Command::new("kill").args(["-CONT", &paused]).status().map_err(|e| e.to_string())?;
    let child = nodes.0[2].as_mut().ok_or("no node 3")?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() > deadline {
            return Err(format!("node 3 still runs 90 s after it was let go: {}", std::fs::read_to_string(&nodes.1[2]).unwrap_or_default()));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let log = std::fs::read_to_string(&nodes.1[2]).unwrap_or_default();
    assert_eq!(status.code(), Some(1), "{log}");
    // In a debug build hiqlite's own assertions panic first, and the panic ends it; in a release
    // build its core stops on an error, and the check of the serving loop ends it.
    let by = if cfg!(debug_assertions) { "a panic in its database" } else { "its Raft core stopped" };
    assert!(log.contains(by) && log.contains("exiting so that a restart recovers it"), "{log}");
    nodes.0[2] = None;
    nodes.start(2, dir.path()).map_err(|e| e.to_string())?;
    let back = eventually(Duration::from_secs(60), || send(auth[2], &access_request(2, "lookup"), Duration::from_millis(500)).is_some());
    assert!(back, "node 3 did not come back after a restart");
    let log = std::fs::read_to_string(&nodes.1[2]).unwrap_or_default();
    assert!(log.matches("has caught up with the cluster").count() >= 2, "the restart caught up: {log}");
    Ok(())
}

/// Three server processes sharing one migrations directory and script, which can
/// be changed between restarts.
struct MigratingCluster {
    // Before `dir`: fields drop in order, and this prints the logs kept in that directory.
    nodes: Nodes,
    dir: tempfile::TempDir,
    configs: Vec<PathBuf>,
    auth: Vec<u16>,
    metrics: Vec<u16>,
    env: Vec<(&'static str, PathBuf)>,
}

impl MigratingCluster {
    fn start(files: &[(&str, &str)], script: &str) -> Self {
        Self::start_with_env(files, script, |_| Vec::new())
    }

    /// Like `start`, with environment variables for every node; `env` gets the
    /// cluster's directory.
    fn start_with_env(files: &[(&str, &str)], script: &str, env: impl FnOnce(&Path) -> Vec<(&'static str, PathBuf)>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::create_dir(dir.path().join("migrations")).unwrap();
        std::fs::write(dir.path().join("check.lua"), script).unwrap();
        let raft: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let api: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let auth: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let metrics: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let node_list: String = (0..3)
            .map(|i| format!("[[cluster.nodes]]\nid = {}\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n", i + 1, raft[i], api[i]))
            .collect();
        let configs: Vec<PathBuf> = auth
            .iter()
            .enumerate()
            .map(|(i, auth)| {
                let config = format!(
                    "dictionary = \"{dictionary}\"\n[lua]\nscript = \"check.lua\"\n\
                     [cluster]\nnode_id = {id}\ndata_dir = \"data{id}\"\nlisten = \"127.0.0.1\"\n\
                     secret_raft = \"cluster-raft-secret-01\"\nsecret_api = \"cluster-api-secret-012\"\nmigrations_dir = \"migrations\"\n{node_list}\
                     [radius]\nbind = \"127.0.0.1\"\nauth_port = {auth}\nacct_port = {acct}\n\
                     [[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"cluster-secret\"\n\
                     [metrics]\nbind = \"127.0.0.1\"\nport = {metrics}\n",
                    dictionary = root.join("../share/dictionary").display(),
                    id = i + 1,
                    acct = free_port(),
                    metrics = metrics[i],
                );
                let path = dir.path().join(format!("node{}.toml", i + 1));
                std::fs::write(&path, config).unwrap();
                path
            })
            .collect();
        let env = env(dir.path());
        let mut cluster = Self { dir, nodes: Nodes(vec![None, None, None], Vec::new()), configs, auth, metrics, env };
        cluster.write_migrations(files);
        for i in 0..3 {
            cluster.spawn(i);
        }
        cluster
    }

    fn write_migrations(&self, files: &[(&str, &str)]) {
        for (name, sql) in files {
            std::fs::write(self.dir.path().join("migrations").join(name), sql).unwrap();
        }
    }

    fn write_script(&self, script: &str) {
        std::fs::write(self.dir.path().join("check.lua"), script).unwrap();
    }

    fn spawn(&mut self, index: usize) {
        let log = self.dir.path().join(format!("node{}-{}.log", index + 1, self.nodes.1.len()));
        let log_file = std::fs::File::create(&log).unwrap();
        self.nodes.1.push(log);
        let child = Command::new(env!("CARGO_BIN_EXE_rradius-server"))
            .args(["server", "--config", self.configs[index].to_str().unwrap()])
            .envs(self.env.iter().map(|(name, value)| (*name, value)))
            .stdout(Stdio::null())
            .stderr(log_file)
            .spawn()
            .unwrap();
        self.nodes.0[index] = Some(child);
    }

    /// Stops a node as `docker stop` does and waits for it to exit.
    fn terminate(&mut self, index: usize) {
        if let Some(mut child) = self.nodes.0[index].take() {
            let _ = Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
            let _ = child.wait();
        }
    }

    /// Changes node `index + 1`'s configuration for its next start.
    fn edit_config(&self, index: usize, edit: impl FnOnce(String) -> String) {
        let text = std::fs::read_to_string(&self.configs[index]).unwrap();
        std::fs::write(&self.configs[index], edit(text)).unwrap();
    }

    /// Adds `line` to node `index + 1`'s `[cluster]` for its next start.
    fn add_cluster_option(&self, index: usize, line: &str) {
        self.edit_config(index, |text| text.replacen("[cluster]\n", &format!("[cluster]\n{line}\n"), 1));
    }

    /// The status node `index + 1` exits with, if it does within `limit`.
    fn exit_code(&mut self, index: usize, limit: Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(status) = self.nodes.0[index].as_mut().and_then(|child| child.try_wait().ok().flatten()) {
                self.nodes.0[index] = None;
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }

    /// What node `index + 1` logged since it was last spawned.
    fn last_log(&self, index: usize) -> String {
        let prefix = format!("node{}-", index + 1);
        self.nodes.1.iter().rev().find(|log| log.file_name().is_some_and(|name| name.to_string_lossy().starts_with(&prefix))).map(|log| std::fs::read_to_string(log).unwrap_or_default()).unwrap_or_default()
    }

    /// What the node answers first. Scripts run only once the node serves, so
    /// retrying until the check passes would hide a node serving too early.
    fn first_answer(&mut self, index: usize) -> u8 {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if let Some(child) = self.nodes.0[index].as_mut() {
                if let Some(status) = child.try_wait().unwrap() {
                    panic!("node {} exited with {status} before answering", index + 1);
                }
            }
            if let Some(reply) = send(self.auth[index], &access_request(1, "alice"), Duration::from_millis(500)) {
                return reply.code;
            }
        }
        panic!("node {} did not answer within 90 s", index + 1);
    }
}

/// Three processes applying one directory at the same moment. Whether a node's
/// replica lags behind is up to timing, so passing does not prove the race is gone;
/// it does catch files applied twice, or a node that fails to start.
#[test]
fn test_nodes_starting_together_apply_migrations() {
    let mut cluster = MigratingCluster::start(
        &[
            ("1_users.sql", "CREATE TABLE users (id INTEGER);"),
            ("2_reply.sql", "CREATE TABLE user_reply (id INTEGER);"),
            ("3_acct.sql", "CREATE TABLE acct (id INTEGER);"),
        ],
        // Accepts when this node sees each file recorded once and all three tables.
        "local ids = sql_query('SELECT id FROM _migrations ORDER BY id')
         assert(#ids == 3 and ids[1].id == 1 and ids[2].id == 2 and ids[3].id == 3)
         sql_query('SELECT id FROM users') sql_query('SELECT id FROM user_reply') sql_query('SELECT id FROM acct')
         return 'accept'",
    );
    for i in 0..3 {
        assert_eq!(cluster.first_answer(i), radius::ACCESS_ACCEPT, "node {}", i + 1);
    }
}

/// A host with no CA certificates at all, such as a minimal container. hiqlite builds
/// an HTTP client for the other nodes, and reqwest refuses to build one from an empty
/// set of system roots, which used to panic the node. On Linux these two variables are
/// the only places the system roots are read from; macOS reads its own keychain, so
/// this only catches the problem on Linux.
#[test]
fn test_cluster_starts_without_system_certificates() {
    let mut cluster = MigratingCluster::start_with_env(&[("1_users.sql", "CREATE TABLE users (id INTEGER);")], "return 'accept'", |dir| {
        std::fs::create_dir(dir.join("no-certs")).unwrap();
        vec![("SSL_CERT_FILE", dir.join("no-certs.pem")), ("SSL_CERT_DIR", dir.join("no-certs"))]
    });
    for i in 0..3 {
        assert_eq!(cluster.first_answer(i), radius::ACCESS_ACCEPT, "node {}", i + 1);
    }
}

/// A node that was down while a file was applied: its replica has to catch up
/// after it restarts, and its first answer must already see the table.
#[test]
fn test_node_rejoining_behind_sees_new_tables() {
    let mut cluster = MigratingCluster::start(&[("1_users.sql", "CREATE TABLE users (id INTEGER);")], "return 'accept'");
    for i in 0..3 {
        assert_eq!(cluster.first_answer(i), radius::ACCESS_ACCEPT, "node {} at the first start", i + 1);
    }

    cluster.nodes.kill(2);
    // Enough rows that node 3 is behind for a while after it rejoins. The table is
    // created by the same transaction, so a node serving too early does not see it.
    cluster.write_migrations(&[(
        "2_reply.sql",
        "CREATE TABLE user_reply (id INTEGER);
         INSERT INTO user_reply WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 3000000) SELECT x FROM n;",
    )]);
    cluster.write_script("sql_query('SELECT id FROM users') sql_query('SELECT id FROM user_reply LIMIT 1') return 'accept'");
    cluster.nodes.kill(0);
    cluster.spawn(0);
    assert_eq!(cluster.first_answer(0), radius::ACCESS_ACCEPT, "node 1, which applied the new file");

    cluster.spawn(2);
    assert_eq!(cluster.first_answer(2), radius::ACCESS_ACCEPT, "node 3, which was down");
}

/// Every node handing out addresses at once: no address twice, and rejections only
/// once the pool has run out.
#[test]
fn test_ip_pool_example_across_nodes() {
    const ADDRESSES: usize = 30;
    const REQUESTS: usize = 12; // on each node, at the same time, so 36 for 30 addresses
    let values: Vec<String> = (1..=ADDRESSES).map(|i| format!("('10.0.0.{i}')")).collect();
    let schema = format!("CREATE TABLE ip_pool (ip TEXT PRIMARY KEY, holder TEXT, expires INTEGER NOT NULL DEFAULT 0);\nINSERT INTO ip_pool (ip) VALUES {};", values.join(", "));
    let mut cluster = MigratingCluster::start(&[("1_pool.sql", &schema)], include_str!("../examples/ip-pool.lua"));
    let ports = cluster.auth.clone();
    // Wait for every node with a user the test does not count: this uses up to one
    // address per node. Status-Server would not show that the script can run.
    let mut warmup = 0;
    for i in 0..3 {
        assert_eq!(cluster.first_answer(i), radius::ACCESS_ACCEPT, "node {}", i + 1);
        warmup += 1;
    }

    let threads: Vec<_> = ports
        .iter()
        .enumerate()
        .flat_map(|(node, port)| {
            let port = *port;
            (0..REQUESTS).map(move |n| {
                std::thread::spawn(move || {
                    let id = (node * REQUESTS + n) as u8 + 10;
                    let reply = send(port, &access_request(id, &format!("user{node}x{n}")), Duration::from_secs(30)).expect("no answer");
                    let address = reply.attribute(8).map(<[u8]>::to_vec);
                    (reply.code, address)
                })
            })
        })
        .collect();
    let replies: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();

    let mut addresses: Vec<_> = replies.iter().filter_map(|(_, address)| address.clone()).collect();
    let accepted = replies.iter().filter(|(code, _)| *code == radius::ACCESS_ACCEPT).count();
    assert_eq!(addresses.len(), accepted, "an Access-Accept without an address");
    addresses.sort();
    addresses.dedup();
    assert_eq!(addresses.len(), accepted, "an address was handed out twice");
    assert!(accepted + warmup <= ADDRESSES, "accepted {accepted}, warm-up {warmup}");
    // A copy of the database lagging near the end can turn away a request for the last few.
    assert!(accepted + warmup >= ADDRESSES - 3, "accepted {accepted}, warm-up {warmup}");
}

/// With two of three nodes gone, node 1 keeps taking accounting into its spool, and the
/// requests reach the database once the nodes are back.
#[test]
fn test_spool_across_a_lost_quorum() {
    let dir = tempfile::tempdir().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let raft: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let api: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let auth: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let acct: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let node_list: String = (0..3)
        .map(|i| format!("[[cluster.nodes]]\nid = {}\nraft = \"127.0.0.1:{}\"\napi = \"127.0.0.1:{}\"\n", i + 1, raft[i], api[i]))
        .collect();
    let spawn = |i: usize, nodes: &mut Nodes| {
        let path = dir.path().join(format!("node{}.toml", i + 1));
        let log_file = std::fs::OpenOptions::new().create(true).append(true).open(dir.path().join(format!("node{}.log", i + 1))).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_rradius-server"))
            .args(["server", "--config", path.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(log_file)
            .spawn()
            .unwrap();
        nodes.0[i] = Some(child);
    };
    let mut nodes = Nodes(vec![None, None, None], (0..3).map(|i| dir.path().join(format!("node{}.log", i + 1))).collect());
    for i in 0..3 {
        let config = format!(
            "dictionary = \"{dictionary}\"\n[lua]\nscript = \"{script}\"\nquery_timeout_ms = 500\n\
             [cluster]\nnode_id = {id}\ndata_dir = \"data{id}\"\nlisten = \"127.0.0.1\"\n\
             secret_raft = \"cluster-raft-secret-01\"\nsecret_api = \"cluster-api-secret-012\"\n{node_list}\
             [radius]\nbind = \"127.0.0.1\"\nauth_port = {auth}\nacct_port = {acct}\n\
             [radius.accounting_spool]\ndir = \"spool{id}\"\nreplay_interval_ms = 200\n\
             [[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"cluster-secret\"\n",
            dictionary = root.join("../share/dictionary").display(),
            script = root.join("tests/fixtures/cluster.lua").display(),
            id = i + 1,
            auth = auth[i],
            acct = acct[i],
        );
        std::fs::write(dir.path().join(format!("node{}.toml", i + 1)), config).unwrap();
        spawn(i, &mut nodes);
    }
    let ready = eventually(Duration::from_secs(60), || send(auth[0], &access_request(1, "write"), Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCESS_ACCEPT));
    assert!(ready, "cluster did not accept writes within 60 s");
    // The table exists on every node, written while all are up.
    let first = RadiusPacket::create_accounting_request(2, vec![(40, vec![0, 0, 0, 1]), (44, b"before-the-outage".to_vec())], SECRET);
    assert!(eventually(Duration::from_secs(5), || send(acct[0], &first, Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCOUNTING_RESPONSE)));

    nodes.kill(1);
    nodes.kill(2);
    // The script's write cannot succeed now; the answer means the request is on disk.
    for id in 0..5 {
        let session = format!("spooled-{id}");
        let during = RadiusPacket::create_accounting_request(3 + id, vec![(40, vec![0, 0, 0, 1]), (44, session.into_bytes())], SECRET);
        let answered = eventually(Duration::from_secs(15), || send(acct[0], &during, Duration::from_secs(5)).map(|r| r.code) == Some(radius::ACCOUNTING_RESPONSE));
        assert!(answered, "no answer from node 1 without a quorum");
    }
    let spooled = eventually(Duration::from_secs(5), || std::fs::read_dir(dir.path().join("spool1")).is_ok_and(|entries| entries.flatten().any(|entry| entry.metadata().is_ok_and(|m| m.len() > 0))));
    assert!(spooled, "nothing in the spool");

    spawn(1, &mut nodes);
    spawn(2, &mut nodes);
    let replicated = eventually(Duration::from_secs(90), || send(auth[2], &access_request(9, "spooled-count"), Duration::from_secs(2)).map(|r| r.code) == Some(radius::ACCESS_ACCEPT));
    assert!(replicated, "the five spooled requests did not all reach the database, once each");
}

/// A sequence number taken from any node is one no other node gets: what EAP-AKA' depends on, so
/// that no two authentication vectors for a card share one. Nodes in one process here, since none
/// has to stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_returning_from_any_node_gives_distinct_values() {
    use rradius_core::config::{ClusterNode, ClusterSettings};
    use rradius_core::lua_engine::Db;
    let dir = tempfile::tempdir().unwrap();
    let nodes: Vec<ClusterNode> = (1..=3).map(|id| ClusterNode { id, raft: format!("127.0.0.1:{}", free_port()), api: format!("127.0.0.1:{}", free_port()) }).collect();
    let settings = |id: u64| ClusterSettings {
        node_id: id,
        data_dir: dir.path().join(format!("node{id}")),
        secret_raft: "cluster-raft-secret-01".to_string(),
        secret_api: "cluster-api-secret-012".to_string(),
        listen: "127.0.0.1".parse().unwrap(),
        nodes: nodes.clone(),
        migrations_dir: None,
        migrations_wait: Duration::from_secs(60),
        snapshot_every: rradius_core::config::DEFAULT_SNAPSHOT_EVERY,
        read_connections: 4,
        stop_without_leader: None,
    };
    let (first, second, third) = (settings(1), settings(2), settings(3));
    let (one, two, three) = tokio::join!(Db::start(&first), Db::start(&second), Db::start(&third));
    let dbs = [one.unwrap(), two.unwrap(), three.unwrap()];
    for db in &dbs {
        db.wait_healthy(Duration::from_secs(60)).await.unwrap();
    }
    dbs[0].execute("CREATE TABLE subscribers (imsi TEXT PRIMARY KEY, sqn INTEGER NOT NULL)").await.unwrap();
    dbs[0].execute("INSERT INTO subscribers VALUES ('1', 0)").await.unwrap();
    let mut tasks = Vec::new();
    for round in 0..30 {
        let db = dbs[round % 3].clone();
        tasks.push(tokio::spawn(async move {
            db.execute_returning_integer("UPDATE subscribers SET sqn = sqn + 32 WHERE imsi = ? RETURNING sqn", "sqn", vec![hiqlite::Param::Text("1".to_string())]).await
        }));
    }
    let mut values = Vec::new();
    for task in tasks {
        values.push(task.await.unwrap().unwrap().expect("no row"));
    }
    values.sort_unstable();
    assert_eq!(values, (1..=30).map(|n| n * 32).collect::<Vec<i64>>(), "each one distinct, none lost");
    let missing = dbs[1].execute_returning_integer("UPDATE subscribers SET sqn = sqn + 32 WHERE imsi = ? RETURNING sqn", "sqn", vec![hiqlite::Param::Text("nobody".to_string())]).await.unwrap();
    assert_eq!(missing, None);
    // Only forward, and a number bound as an integer, not as text, which max would rank higher.
    let forward = "UPDATE subscribers SET sqn = max(sqn, ?) + 32 WHERE imsi = ? RETURNING sqn";
    let moved = dbs[2].execute_returning_integer(forward, "sqn", vec![hiqlite::Param::Integer(5), hiqlite::Param::Text("1".to_string())]).await.unwrap();
    assert_eq!(moved, Some(30 * 32 + 32), "an old number does not take it back");
    for db in &dbs {
        db.shutdown().await.unwrap();
    }
}


/// A cluster whose three nodes applied a file with one account, then lost two nodes, with node
/// 1 stopped cleanly as `docker stop` does: what is left of a cluster without a majority.
fn cluster_without_majority() -> MigratingCluster {
    let mut cluster = MigratingCluster::start(
        &[("1_users.sql", USERS_WITH_ALICE)],
        "if #sql_query(\"SELECT name FROM users WHERE name = 'alice'\") == 1 then return 'accept' end return 'reject'",
    );
    for i in 0..3 {
        assert_eq!(cluster.first_answer(i), radius::ACCESS_ACCEPT, "node {} at the first start", i + 1);
    }
    cluster.nodes.kill(1);
    cluster.nodes.kill(2);
    cluster.terminate(0);
    cluster.add_cluster_option(0, "migrations_wait_ms = 10000");
    cluster
}

const USERS_WITH_ALICE: &str = "CREATE TABLE users (name TEXT); INSERT INTO users VALUES ('alice');";

/// Restarted without a majority, a node that has every file applied serves from its replica,
/// and catches up once the others are back.
#[test]
fn test_restart_without_majority_serves_from_replica() {
    let mut cluster = cluster_without_majority();
    cluster.spawn(0);
    assert_eq!(cluster.first_answer(0), radius::ACCESS_ACCEPT, "node 1 without a majority");
    assert!(cluster.last_log(0).contains("serving without a leader"), "{}", cluster.last_log(0));
    assert_eq!(metric(cluster.metrics[0], "rradius_cluster_caught_up"), Some(0.0));

    cluster.spawn(1);
    cluster.spawn(2);
    let caught_up = eventually(Duration::from_secs(60), || metric(cluster.metrics[0], "rradius_cluster_caught_up") == Some(1.0));
    assert!(caught_up, "node 1 did not catch up: {}", cluster.last_log(0));
    assert!(cluster.last_log(0).contains("has caught up with the cluster"), "{}", cluster.last_log(0));
}

/// Restarted without a majority, a node does not serve when it cannot know the tables are
/// right, or is told not to serve without a leader.
#[test]
fn test_restart_without_majority_refused() {
    let mut cluster = cluster_without_majority();

    cluster.write_migrations(&[("2_reply.sql", "CREATE TABLE user_reply (id INTEGER);")]);
    cluster.spawn(0);
    assert_eq!(cluster.exit_code(0, Duration::from_secs(90)), Some(1), "with a file not applied here");
    let log = cluster.last_log(0);
    assert!(log.contains("not ready") && log.contains("2_reply.sql"), "{log}");
    std::fs::remove_file(cluster.dir.path().join("migrations/2_reply.sql")).unwrap();

    cluster.write_migrations(&[("1_users.sql", "CREATE TABLE users (name TEXT, realm TEXT);")]);
    cluster.spawn(0);
    assert_eq!(cluster.exit_code(0, Duration::from_secs(90)), Some(2), "with an applied file changed");
    assert!(cluster.last_log(0).contains("1_users.sql"), "{}", cluster.last_log(0));
    cluster.write_migrations(&[("1_users.sql", USERS_WITH_ALICE)]);

    cluster.add_cluster_option(0, "stop_without_leader_ms = 10000");
    cluster.spawn(0);
    assert_eq!(cluster.exit_code(0, Duration::from_secs(90)), Some(1), "with stop_without_leader_ms");
    assert!(cluster.last_log(0).contains("stop_without_leader_ms"), "{}", cluster.last_log(0));
}

/// Without `migrations_dir`, a node restarted without a majority serves as before, unless
/// told not to serve without a leader.
#[test]
fn test_restart_without_majority_and_migrations() {
    let mut cluster = cluster_without_majority();
    cluster.edit_config(0, |text| text.replace("migrations_dir = \"migrations\"\n", ""));
    cluster.spawn(0);
    let answered = eventually(Duration::from_secs(60), || send(cluster.auth[0], &status_server(3), Duration::from_millis(500)).is_some());
    assert!(answered, "no answer to Status-Server: {}", cluster.last_log(0));
    assert!(cluster.last_log(0).contains("before catching up"), "{}", cluster.last_log(0));
    cluster.terminate(0);

    cluster.add_cluster_option(0, "stop_without_leader_ms = 10000");
    cluster.spawn(0);
    // Asked all the while it starts: it must not answer before it exits.
    let deadline = Instant::now() + Duration::from_secs(90);
    let code = loop {
        assert!(send(cluster.auth[0], &status_server(4), Duration::from_millis(300)).is_none(), "answered before exiting: {}", cluster.last_log(0));
        if let Some(code) = cluster.exit_code(0, Duration::from_millis(200)) {
            break Some(code);
        }
        if Instant::now() > deadline {
            break None;
        }
    };
    assert_eq!(code, Some(1), "with stop_without_leader_ms");
    let log = cluster.last_log(0);
    assert!(log.contains("stop_without_leader_ms") && !log.contains("apply migrations"), "{log}");
}
