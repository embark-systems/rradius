//! Spike for the hiqlite-cluster change. Run with `cargo run --release -- <mode>`:
//! `single` (default), `forbidden`, `cluster`.
use hiqlite::{Client, Error, Node, NodeConfig, Row};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// One row with every column, read generically from the underlying rusqlite row.
#[derive(Debug)]
struct GenericRow(Vec<(String, String)>);

impl<'a, 'r> TryFrom<&'a mut Row<'r>> for GenericRow {
    type Error = Error;

    fn try_from(row: &'a mut Row<'r>) -> Result<Self, Self::Error> {
        match row {
            Row::Borrowed(r) => {
                let stmt = r.as_ref();
                let mut cols = Vec::new();
                for i in 0..stmt.column_count() {
                    let name = stmt.column_name(i).map_err(Error::from)?.to_string();
                    let value = format!("{:?}", r.get_ref(i).map_err(Error::from)?);
                    cols.push((name, value));
                }
                Ok(GenericRow(cols))
            }
            Row::Owned(_) => Err(Error::Sqlite("owned row: column names are not enumerable".into())),
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn node_config(id: u64, nodes: &[Node], dir: &std::path::Path) -> NodeConfig {
    NodeConfig {
        node_id: id,
        nodes: nodes.to_vec(),
        listen_addr_api: "127.0.0.1".into(),
        listen_addr_raft: "127.0.0.1".into(),
        data_dir: dir.join(format!("node{id}")).to_string_lossy().into_owned().into(),
        secret_raft: "spike-raft-secret-123".into(),
        secret_api: "spike-api-secret-1234".into(),
        ..Default::default()
    }
}

fn nodes(count: u64) -> Vec<Node> {
    (1..=count)
        .map(|id| Node { id, addr_raft: format!("127.0.0.1:{}", free_port()), addr_api: format!("127.0.0.1:{}", free_port()) })
        .collect()
}

async fn rows(client: &Client, sql: &'static str) -> Result<Vec<GenericRow>, Error> {
    client.query_try_map::<GenericRow, _>(sql, vec![]).await?.into_iter().collect()
}

async fn single() {
    let dir = tempfile::tempdir().unwrap();
    let lua = mlua::Lua::new();
    let lua_ok: i64 = lua.load("return 40 + 2").eval_async().await.unwrap();
    println!("[1] compiles and runs together with mlua (luajit): lua returned {lua_ok}");

    let started = Instant::now();
    let nodes = nodes(1);
    let client = hiqlite::start_node(node_config(1, &nodes, dir.path())).await.expect("start single node");
    client.wait_until_healthy_db().await;
    let start_ms = started.elapsed().as_millis();

    client.execute("CREATE TABLE sessions (id TEXT, ts INTEGER, score REAL, note TEXT, raw BLOB)", vec![]).await.unwrap();
    let inserted = client
        .execute("INSERT INTO sessions VALUES ('s-1', 1700000000, 1.5, NULL, x'00ff'), ('s-2', 2, 2.5, 'n', NULL)", vec![])
        .await
        .unwrap();
    println!("[2] execute INSERT affected {inserted} rows");
    println!("[2] generic rows via query_try_map: {:?}", rows(&client, "SELECT * FROM sessions ORDER BY id").await);
    println!("[2] query_raw row variant: {:?}", client.query_raw("SELECT id FROM sessions LIMIT 1", vec![]).await.map(|r| match &r[0] {
        Row::Borrowed(_) => "Borrowed",
        Row::Owned(_) => "Owned",
    }));

    println!("[4] query_raw with INSERT: {:?}", client.query_raw("INSERT INTO sessions VALUES ('via-read', 0, 0, NULL, NULL)", vec![]).await.map(|r| r.len()));
    println!("[4] query_try_map with INSERT: {:?}", rows(&client, "INSERT INTO sessions VALUES ('via-map', 0, 0, NULL, NULL)").await.map(|r| r.len()));
    println!("[4] rows now: {:?}", rows(&client, "SELECT id FROM sessions ORDER BY id").await);

    println!("[5a] execute with CURRENT_TIMESTAMP: {:?}", client.execute("INSERT INTO sessions (id, note) VALUES ('ts', CURRENT_TIMESTAMP)", vec![]).await);
    let shutdown_started = Instant::now();
    client.shutdown().await.unwrap();
    println!("[2] single node start+healthy {start_ms} ms, shutdown {} ms", shutdown_started.elapsed().as_millis());
    println!("[3] enc_keys: not a field without the s3/dashboard features (this compiles without setting it); listen_addr_* is a bare host, e.g. \"127.0.0.1\"");
}

async fn forbidden() {
    let dir = tempfile::tempdir().unwrap();
    let nodes = nodes(1);
    let client = hiqlite::start_node(node_config(1, &nodes, dir.path())).await.unwrap();
    client.wait_until_healthy_db().await;
    client.execute("CREATE TABLE t (v TEXT)", vec![]).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.execute("INSERT INTO t VALUES (strftime('%s', 1700000000))", vec![]),
    )
    .await;
    println!("[5] execute with strftime(fixed args): {:?}", result);
    let after = tokio::time::timeout(Duration::from_secs(10), client.execute("INSERT INTO t VALUES ('after')", vec![])).await;
    println!("[5] next plain execute after that: {:?}", after);
}

async fn cluster() {
    let dir = tempfile::tempdir().unwrap();
    let nodes = nodes(3);
    let mut clients = Vec::new();
    let started = Instant::now();
    for id in 1..=3 {
        clients.push(hiqlite::start_node(node_config(id, &nodes, dir.path())).await.expect("start node"));
    }
    for c in &clients {
        c.wait_until_healthy_db().await;
    }
    println!("[6] 3 nodes in one process healthy after {} ms", started.elapsed().as_millis());

    clients[0].execute("CREATE TABLE sessions (id TEXT)", vec![]).await.unwrap();
    let write = Instant::now();
    clients[1].execute("INSERT INTO sessions VALUES ('from-node-2')", vec![]).await.unwrap();
    let write_ms = write.elapsed().as_millis();
    let seen = Instant::now();
    loop {
        if !rows(&clients[0], "SELECT id FROM sessions").await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    println!("[6] write on node 2 took {write_ms} ms; visible on node 1 after {} ms more", seen.elapsed().as_millis());

    let mut leader = None;
    for (i, c) in clients.iter().enumerate() {
        if c.is_leader_db().await {
            leader = Some(i);
        }
    }
    let leader = leader.expect("a leader");
    println!("[6] leader is node {}", leader + 1);
    clients[leader].shutdown().await.unwrap();
    let survivor = &clients[(leader + 1) % 3];
    let election = Instant::now();
    let during = tokio::time::timeout(Duration::from_millis(500), survivor.execute("INSERT INTO sessions VALUES ('during')", vec![])).await;
    println!("[6] execute within 500 ms after leader stop: {:?}", during.map(|r| r.map_err(|e| e.to_string())));
    loop {
        match tokio::time::timeout(Duration::from_secs(2), survivor.execute("INSERT INTO sessions VALUES ('after')", vec![])).await {
            Ok(Ok(_)) => break,
            other => {
                if election.elapsed() > Duration::from_secs(30) {
                    println!("[6] no new leader within 30 s: {:?}", other.map(|r| r.map_err(|e| e.to_string())));
                    return;
                }
            }
        }
    }
    println!("[6] writes succeed again {} ms after stopping the leader", election.elapsed().as_millis());

    let other = (leader + 2) % 3;
    clients[other].shutdown().await.unwrap();
    let no_quorum = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(500), survivor.execute("INSERT INTO sessions VALUES ('no-quorum')", vec![])).await;
    println!(
        "[6] with 1 of 3 nodes left, execute under a 500 ms timeout: {:?} after {} ms",
        result.map(|r| r.map_err(|e| e.to_string())),
        no_quorum.elapsed().as_millis()
    );
    survivor.shutdown().await.ok();
}

/// Child process: runs one node and answers line commands on stdin.
async fn node(id: u64, spec: &str, dir: &str) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let nodes: Vec<Node> = spec
        .split(',')
        .enumerate()
        .map(|(i, pair)| {
            let (raft, api) = pair.split_once('/').unwrap();
            Node { id: i as u64 + 1, addr_raft: raft.to_string(), addr_api: api.to_string() }
        })
        .collect();
    let client = hiqlite::start_node(node_config(id, &nodes, std::path::Path::new(dir))).await.unwrap();
    client.wait_until_healthy_db().await;
    println!("ready");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let (cmd, sql) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let sql = sql.to_string();
        match cmd {
            "leader" => println!("{}", client.is_leader_db().await),
            "exec" => match tokio::time::timeout(Duration::from_millis(500), client.execute(sql, vec![])).await {
                Ok(Ok(n)) => println!("ok {n}"),
                Ok(Err(e)) => println!("err {e}"),
                Err(_) => println!("timeout"),
            },
            "count" => println!("{}", client.query_try_map::<GenericRow, _>(sql, vec![]).await.map(|r| r.len()).unwrap_or(0)),
            _ => println!("?"),
        }
    }
}

/// Parent: three child processes; kills the leader with SIGKILL.
async fn processes() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    let spec = nodes(3).iter().map(|n| format!("{}/{}", n.addr_raft, n.addr_api)).collect::<Vec<_>>().join(",");
    let exe = std::env::current_exe().unwrap();
    let started = Instant::now();
    let mut children: Vec<_> = (1..=3)
        .map(|id| {
            let mut child = Command::new(&exe)
                .args(["node", &id.to_string(), &spec, dir.path().to_str().unwrap()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let stdout = BufReader::new(child.stdout.take().unwrap());
            (child, stdout)
        })
        .collect();
    let ask = |c: &mut (std::process::Child, BufReader<std::process::ChildStdout>), line: &str| -> String {
        writeln!(c.0.stdin.as_mut().unwrap(), "{line}").unwrap();
        let mut out = String::new();
        c.1.read_line(&mut out).unwrap();
        out.trim().to_string()
    };
    for c in children.iter_mut() {
        let mut out = String::new();
        c.1.read_line(&mut out).unwrap();
    }
    println!("[7] 3 node processes ready after {} ms", started.elapsed().as_millis());
    println!("[7] create: {}", ask(&mut children[0], "exec CREATE TABLE sessions (id TEXT)"));
    println!("[7] write on node 2: {}", ask(&mut children[1], "exec INSERT INTO sessions VALUES ('from-node-2')"));
    println!("[7] rows on node 1: {}", ask(&mut children[0], "count SELECT id FROM sessions"));
    let leader = (0..3).find(|&i| ask(&mut children[i], "leader") == "true").unwrap();
    println!("[7] leader is node {}; killing it with SIGKILL", leader + 1);
    children[leader].0.kill().unwrap();
    children[leader].0.wait().unwrap();
    let survivor = (leader + 1) % 3;
    let killed = Instant::now();
    let first = ask(&mut children[survivor], "exec INSERT INTO sessions VALUES ('during')");
    println!("[7] first write right after the kill: {first}");
    loop {
        let out = ask(&mut children[survivor], "exec INSERT INTO sessions VALUES ('after')");
        if out.starts_with("ok") {
            println!("[7] writes succeed again {} ms after the kill", killed.elapsed().as_millis());
            break;
        }
        if killed.elapsed() > Duration::from_secs(30) {
            println!("[7] still failing after 30 s: {out}");
            break;
        }
    }
    let other = (leader + 2) % 3;
    children[other].0.kill().unwrap();
    children[other].0.wait().unwrap();
    let alone = Instant::now();
    println!("[7] 1 of 3 left, write with 500 ms timeout: {} after {} ms", ask(&mut children[survivor], "exec INSERT INTO sessions VALUES ('no-quorum')"), alone.elapsed().as_millis());
    children[survivor].0.kill().ok();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("forbidden") => forbidden().await,
        Some("cluster") => cluster().await,
        Some("processes") => processes().await,
        Some("node") => node(args[2].parse().unwrap(), &args[3], &args[4]).await,
        _ => single().await,
    }
}
