//! The server program: the commands, and starting and running the servers. The programs built
//! on this library each call `main` with what tells them apart.

use crate::accounts;
use crate::admin;
use crate::config;
use crate::dictionary::Dictionary;
use crate::logging;
use crate::lua_engine::{self, LuaVms, MigrationError};
use crate::metrics::{self, Metrics};
use crate::radius::RadiusServer;
use crate::spool::Spool;
use crate::reload::{self, Serving, Shared};
use crate::tacacs::TacacsServer;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::signal::unix::{signal, Signal, SignalKind};

/// What a program built on this library tells `main`.
pub struct Program {
    /// Its name, as `--version` prints it.
    pub name: &'static str,
    /// Its version, as `--version` prints it.
    pub version: &'static str,
    /// What it adds to the server.
    pub extensions: crate::extension::Extensions,
}

const USAGE: &str = "usage: rradius-server server [--config <path>]";
const SCHEMA_USAGE: &str = "usage: rradius-server schema [--lang en|zh-TW]";

/// The name of the program that runs, once `main` has it.
static PROGRAM_NAME: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// The name of the program that runs: `rradius-server` until `main` says otherwise.
pub fn program_name() -> &'static str {
    PROGRAM_NAME.get().copied().unwrap_or("rradius-server")
}

/// A usage text of the core, written for `rradius-server`, for the program that runs.
pub fn named(usage: &str) -> String {
    usage.replace("rradius-server", program_name())
}

/// The `schema` command: prints the JSON Schema of the configuration file; the exit status.
fn print_schema(program: &str, extensions: &crate::extension::Extensions, rest: &[String]) -> i32 {
    write_schema(program, extensions, rest, &mut std::io::stdout().lock(), &mut std::io::stderr().lock())
}

/// Writes what the `schema` command prints to `out` and `err`. A failed write, such as a
/// closed pipe after `| head`, ends it with status 1 instead of a panic.
fn write_schema(program: &str, extensions: &crate::extension::Extensions, rest: &[String], out: &mut impl std::io::Write, err: &mut impl std::io::Write) -> i32 {
    let lang = match rest {
        [] => Some(crate::schema::Lang::En),
        [flag, name] if flag == "--lang" => crate::schema::Lang::parse(name),
        _ => None,
    };
    let Some(lang) = lang else {
        let _ = writeln!(err, "{}", SCHEMA_USAGE.replace("rradius-server", program));
        return 2;
    };
    let text = crate::schema::build_in(program, extensions, lang).and_then(|schema| serde_json::to_string_pretty(&schema).map_err(|e| e.to_string()));
    let written = match text {
        Ok(text) => writeln!(out, "{text}").and_then(|()| out.flush()).map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    match written {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "schema: {e}");
            1
        }
    }
}

/// Every command of the program, one per line, and where the configuration is read from without
/// `--config`: what no arguments and `help` print, and what a mistake prints.
fn full_usage(extensions: &crate::extension::Extensions, default: &str) -> String {
    named(&format!(
        "{USAGE}\n{}\nusage: rradius-server --version\n{SCHEMA_USAGE}\n{}\n{}\n{}\n{}",
        admin::CHECK_USAGE,
        crate::config_tools::USAGE,
        crate::tuning::USAGE,
        accounts::USAGE,
        crate::cluster_commands::USAGE
    )) + &extensions.usage().iter().map(|usage| format!("\n{usage}")).collect::<String>()
        + &format!("\nwithout --config, the configuration is {default}")
}

/// Prints `message` and exits with status 2, the status for usage and configuration errors.
/// Directly: this has to be written before the process ends, in whatever state logging is,
/// and after what logging has waiting.
fn exit_with(message: impl std::fmt::Display) -> ! {
    logging::flush();
    eprintln!("{}", message);
    std::process::exit(2);
}

/// How long a node that cannot start waits to shut down cleanly. A member of a cluster of more
/// than one waits 9.5 s before it begins, and hiqlite gives the whole of it 20 s.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(25);

/// `exit_with` once the node has started: one that exits without this has its local database
/// rebuilt on the next start.
async fn stop_node_and_exit_with(db: &lua_engine::Db, message: impl std::fmt::Display) -> ! {
    let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
    exit_with(message)
}

/// Returns when SIGTERM or SIGINT arrives.
async fn stop_asked(terminate: &mut Signal, interrupt: &mut Signal) {
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
}

fn stopped_during_startup() -> ! {
    tracing::info!("stopped during startup");
    logging::flush();
    std::process::exit(0);
}

/// Runs the program: one of its commands, or the servers until they are asked to stop.
pub fn main(program: Program) -> Result<(), Box<dyn std::error::Error>> {
    exit_when_the_cluster_node_panics();
    crate::runtime::build()?.block_on(run(program))
}

/// A panic inside hiqlite or openraft takes the node's Raft core with it while the process goes
/// on, answering from a replica that no longer follows the cluster: the process ends instead,
/// and its next start rebuilds the local database from the others. Other panics stay with their
/// task, as before.
fn exit_when_the_cluster_node_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let in_cluster_node = info.location().is_some_and(|location| ["/hiqlite", "/openraft"].iter().any(|part| location.file().contains(part)));
        if in_cluster_node {
            let at = info.location().map(|location| format!("{}:{}", location.file(), location.line())).unwrap_or_default();
            tracing::error!("cluster node: a panic in its database ({at}); exiting so that a restart recovers it");
            logging::flush();
            std::process::exit(1);
        }
    }));
}

async fn run(program: Program) -> Result<(), Box<dyn std::error::Error>> {
    // First of all: SIGHUP ends a process that has no handler. One arriving during
    // startup takes effect once serving begins.
    let mut hangup = signal(SignalKind::hangup())?;
    let _ = PROGRAM_NAME.set(program.name);
    crate::extension::install(program.extensions)?;
    let extensions = crate::extension::installed();

    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--version" {
        println!("{} {}", program.name, program.version);
        println!("default config: {}", config::default_config_path());
        return Ok(());
    }
    if args.get(1).is_some_and(|command| command == "schema") {
        std::process::exit(print_schema(program.name, extensions, &args[2..]));
    }
    if args.get(1).is_some_and(|command| command == "config") {
        std::process::exit(crate::config_tools::run(&args[2..], extensions));
    }
    if args.get(1).is_some_and(|command| command == "tuning") {
        std::process::exit(crate::tuning::run(&args[2..], extensions));
    }
    if args.get(1).is_some_and(|command| command == "user") {
        std::process::exit(accounts::run(&args[2..]).await);
    }
    if args.get(1).is_some_and(|command| command == "cluster") {
        std::process::exit(crate::cluster_commands::run(&args[2..]).await);
    }
    if let Some(command) = args.get(1).and_then(|name| extensions.command(name)) {
        std::process::exit((command.run)(args[2..].to_vec()).await);
    }
    // A command only rradius-enterprise has: say so, before reading anything.
    if let Some(hint) = args.get(1).and_then(|name| crate::extension::command_needs_enterprise(name)) {
        exit_with(format!("{}: not supported by this program ({hint})", args[1]));
    }
    let default = config::default_config_path();
    let config_path = match args.as_slice() {
        [_] => {
            println!("{}", full_usage(extensions, default));
            return Ok(());
        }
        [_, help] if help == "help" || help == "--help" || help == "-h" => {
            println!("{}", full_usage(extensions, default));
            return Ok(());
        }
        [_, server] if server == "server" => default,
        [_, server, flag, path] if server == "server" && flag == "--config" => path.as_str(),
        [_, server, ..] if server == "server" => exit_with(named(USAGE)),
        [_, check] if check == "check" => std::process::exit(admin::check(default)),
        [_, check, flag, path] if check == "check" && flag == "--config" => std::process::exit(admin::check(path)),
        [_, check, ..] if check == "check" => exit_with(named(admin::CHECK_USAGE)),
        // Starting and checking are subcommands: the old flags say which. With `--check` anywhere
        // the old command only checked, so it is never told to start the server.
        [_, flag, ..] if (flag == "--config" || flag == "--check") && args.iter().any(|arg| arg == "--check") => {
            exit_with(full_usage(extensions, default) + &format!("\nto check a configuration: {} check [--config <path>]", program.name))
        }
        [_, flag, ..] if flag == "--config" => exit_with(full_usage(extensions, default) + &format!("\nto start the server: {} server --config <path>", program.name)),
        _ => exit_with(full_usage(extensions, default)),
    };
    // Only now: the commands above do not listen for these, and must end on them. As PID 1 the
    // server would ignore them until the end of startup, and be killed.
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;

    let config = config::load(Path::new(config_path)).unwrap_or_else(|e| match args.iter().any(|arg| arg == "--config") {
        false => exit_with(format!("{e}\n(the default configuration; give --config <path> for another)")),
        true => exit_with(e),
    });
    logging::init(&config.log);
    let mut dictionary = Dictionary::new();
    if let Err(e) = dictionary.load_from_file(&config.dictionary) {
        exit_with(e);
    }
    // Before starting the cluster node: a missing script should not leave it half started.
    if let Err(e) = std::fs::File::open(&config.lua.script) {
        exit_with(format!("cannot read script '{}': {}", config.lua.script.display(), e));
    }

    // Before joining anything: a mistake in the directory must not look like a cluster problem.
    let cluster = &config.cluster;
    let migrations = cluster.migrations_dir.as_deref().map(lua_engine::read_migrations).transpose().unwrap_or_else(|e| {
        logging::flush();
        eprintln!("{e}");
        std::process::exit(e.exit_code());
    });

    // Start this node of the hiqlite cluster. On a node's first start this returns only
    // once the node has joined, so with migrations the limit has to cover it too.
    let stop = |error: MigrationError| -> ! {
        logging::flush();
        eprintln!("{error}");
        std::process::exit(error.exit_code());
    };
    lua_engine::set_stop_without_leader(cluster.stop_without_leader);
    let deadline = Instant::now() + cluster.migrations_wait;
    // In a block of its own, so that what it borrows is free again once the node has started.
    let started = {
        let start = lua_engine::Db::start_with(cluster, &config.extensions);
        tokio::pin!(start);
        let limit = migrations.is_some().then_some(deadline);
        let started = tokio::select! {
            started = async {
                match limit {
                    Some(limit) => tokio::time::timeout_at(limit.into(), &mut start).await.ok(),
                    None => Some((&mut start).await),
                }
            } => started,
            _ = stop_asked(&mut terminate, &mut interrupt) => {
                // A node that exits while it starts has its local database rebuilt next time. One
                // about to be up is waited for, within the time a stop is given, and shut down
                // cleanly; one that cannot join is left as it is. Stopped, either way.
                let stop_by = tokio::time::Instant::now() + SHUTDOWN_WAIT;
                if let Ok(Ok(db)) = tokio::time::timeout_at(stop_by, &mut start).await {
                    let _ = tokio::time::timeout_at(stop_by, db.shutdown()).await;
                }
                stopped_during_startup()
            }
        };
        started.unwrap_or_else(|| stop(MigrationError::Cluster(format!("node {} did not join within {:?}", cluster.node_id, cluster.migrations_wait))))
    };
    let db = started.unwrap_or_else(|e| exit_with(format!("cannot start cluster node {}: {}", cluster.node_id, e)));
    tracing::info!("cluster node {} started", cluster.node_id);
    let cluster_node_id = cluster.node_id;
    db.serve_for();

    let migrations_dir_set = migrations.is_some();
    // A leader this node can write through. hiqlite's health is not enough: a node restarted
    // without a majority takes itself for the leader.
    let leader_wait = if migrations_dir_set { deadline.saturating_duration_since(Instant::now()) } else { cluster.migrations_wait };
    let leader_found = tokio::select! {
        found = db.wait_usable_leader(leader_wait) => found,
        _ = stop_asked(&mut terminate, &mut interrupt) => {
            let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
            stopped_during_startup()
        }
    };
    if !leader_found {
        // Without a majority: serve from this node's replica, unless told not to serve without a
        // leader, or the directory has files this replica has not applied.
        let refused = if cluster.stop_without_leader.is_some() {
            Some(MigrationError::Unavailable(format!("no usable leader within {leader_wait:?}, and stop_without_leader_ms is set")))
        } else if let Some(migrations) = &migrations {
            db.check_local_migrations(migrations).await.err()
        } else {
            None
        };
        if let Some(e) = refused {
            // A node that exits without this has its local database rebuilt on the next start.
            let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
            stop(e);
        }
        tracing::warn!(
            "cluster node {}: no usable leader within {:?}; serving without a leader before catching up, so answers come from this node's replica, which may be behind",
            cluster.node_id,
            leader_wait
        );
        db.catch_up_in_background(cluster.node_id, cluster.migrations_wait);
    } else {
        // Before anything can be served: scripts and handlers rely on these tables.
        if let Some(migrations) = migrations {
            let applying = async {
                match db.wait_healthy(deadline.saturating_duration_since(Instant::now())).await {
                    Ok(()) => db.migrate(migrations, deadline.saturating_duration_since(Instant::now()), cluster.migrations_wait).await,
                    Err(e) => Err(MigrationError::Cluster(e)),
                }
            };
            let applied = tokio::select! {
                applied = applying => applied,
                _ = stop_asked(&mut terminate, &mut interrupt) => {
                    let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
                    stopped_during_startup()
                }
            };
            if let Err(e) = applied {
                // A node that exits without this has its local database rebuilt on the next start.
                let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
                stop(e);
            }
        }

        // Not a request before this node has what the cluster committed while it was away: a
        // leader alone says nothing of how far behind its replica is.
        let catching_up = async {
            db.wait_healthy(cluster.migrations_wait).await?;
            db.catch_up(cluster.node_id, cluster.migrations_wait).await
        };
        let caught_up = tokio::select! {
            caught_up = catching_up => caught_up,
            _ = stop_asked(&mut terminate, &mut interrupt) => {
                let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
                stopped_during_startup()
            }
        };
        match caught_up {
            Ok(()) => {
                db.note_caught_up();
                tracing::info!("cluster node {} has caught up with the cluster", cluster.node_id);
            }
            // As with migrations: a node that exits without this has its local database rebuilt.
            Err(e) if migrations_dir_set => {
                let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
                stop(MigrationError::Cluster(e));
            }
            Err(e) if cluster.stop_without_leader.is_some() => {
                let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
                stop(MigrationError::Unavailable(format!("{e}, and stop_without_leader_ms is set")));
            }
            Err(e) => {
                tracing::warn!("cluster node {}: {e}; serving before catching up, so answers may come from old data", cluster.node_id);
                db.catch_up_in_background(cluster.node_id, cluster.migrations_wait);
            }
        }
    }

    // A core that stopped while the node started does not start again: no serving from it.
    if let Some(e) = db.cluster_failure() {
        tracing::error!("cluster node {}: its Raft core stopped ({e}); exiting so that a restart recovers it", cluster.node_id);
        let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
        logging::flush();
        std::process::exit(1);
    }

    // What the program's extensions do before the servers start, such as making the tables the
    // first logins would otherwise find missing.
    let context = crate::extension::StartContext { config: &config, db: &db };
    let started = tokio::select! {
        started = extensions.start(&context) => started,
        _ = stop_asked(&mut terminate, &mut interrupt) => {
            let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
            stopped_during_startup()
        }
    };
    if let Err(e) = started {
        stop_node_and_exit_with(&db, e).await;
    }

    // Shared sandboxed Luau VMs; every script is compiled once here, so syntax errors stop startup.
    let vms = match LuaVms::new(config.lua.max_memory) {
        Ok(vms) => Arc::new(vms),
        Err(e) => stop_node_and_exit_with(&db, format!("cannot create Lua VMs: {e}")).await,
    };
    // Now that the cluster's own threads have started: a thread that std::thread::spawn starts
    // from a pinned worker would share that worker's one CPU.
    if config.runtime.pin_threads {
        match crate::runtime::pin_workers() {
            Ok(cpus) => tracing::info!("pinned {} runtime worker threads to CPUs {:?}", cpus.len(), cpus),
            Err(e) => tracing::warn!("runtime.pin_threads: {e}; running without pinning"),
        }
    }
    let metrics = Arc::new(Metrics::for_extensions(extensions));
    let dictionary = Arc::new(dictionary);
    let shared = Shared { db: db.clone(), vms, dictionary: dictionary.clone(), metrics: metrics.clone(), extensions: extensions.clone() };
    let generation = match reload::build(&config, &shared) {
        Ok(generation) => generation,
        Err(e) => stop_node_and_exit_with(&db, e).await,
    };

    let mut radius_running = None;
    let mut spool_replay = None;
    let mut radius_serving = None;
    if let (Some(radius), Some(state)) = (&config.radius, generation.radius) {
        let radius_server = RadiusServer::from_state(radius.auth_addr, radius.acct_addr, state, dictionary)
            .with_more_binds(radius.more_binds.clone())
            .with_sockets(radius.sockets)
            .with_recv_buffer(radius.recv_buffer_bytes)
            .with_limits(radius.max_auth_inflight, radius.max_acct_inflight)
            .with_status_server(radius.status_server)
            .with_eap(radius.eap.clone())
            .with_accounting_observer(extensions.accounting_observer(&config))
            .with_metrics(metrics.clone());
        let radius_server = match &radius.accounting_spool {
            Some(spool) => match Spool::open(&spool.dir, spool.max_bytes, metrics.acct_spool.clone()) {
                Ok(opened) => radius_server.with_spool(Arc::new(opened)),
                Err(e) => stop_node_and_exit_with(&db, format!("radius.accounting_spool.dir: cannot use '{}': {}", spool.dir.display(), e)).await,
            },
            None => radius_server,
        };
        let radius_server = Arc::new(radius_server);
        // Before anything else starts working: a server that cannot listen should not go on as if it
        // served, and with nothing else running, there is nothing to stop first.
        let sockets = match radius_server.bind_sockets().await {
            Ok(sockets) => sockets,
            Err(e) => {
                let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
                logging::flush();
                eprintln!("RADIUS server error: {e}");
                std::process::exit(1);
            }
        };
        radius_running = Some(radius_server.clone());
        // The extensions' work once a minute, with traffic or without.
        let minutely = radius_server.clone();
        let minutely_db = db.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                every.tick().await;
                let context = crate::extension::JobContext { db: minutely_db.clone(), radius: minutely.clone() };
                extensions.every_minute(&context).await;
            }
        });
        if let Some(spool) = &radius.accounting_spool {
            spool_replay = Some(tokio::spawn(radius_server.clone().replay_spool(spool.replay_interval, spool.replay_window)));
        }
        radius_serving = Some((radius_server, sockets));
    }

    let mut tacacs_running = None;
    if let (Some(tacacs), Some(state)) = (&config.tacacs, generation.tacacs) {
        let tacacs_server = TacacsServer::from_state(tacacs.addr, state, tacacs.max_body_len)
            .with_max_connections(tacacs.max_connections)
            .with_idle_timeout(tacacs.idle_timeout)
            .with_tls_served(config.extension::<crate::tacacs_transport::TacacsTlsServed>().copied())
            .with_metrics(metrics.clone());
        tacacs_running = Some(Arc::new(tacacs_server));
    }

    // Once both servers are made and before either serves, as for the RADIUS sockets: an
    // extension's server that cannot listen, such as the CoA relay, stops the start. A
    // configuration with TACACS+ alone starts the extensions too.
    let context = crate::extension::ServeContext { config: &config, radius: radius_running.clone(), tacacs: tacacs_running.clone(), metrics: metrics.clone(), db: db.clone() };
    let extensions_serving = match extensions.serve(&context) {
        Ok(serving) => serving,
        Err(e) => {
            let _ = tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await;
            logging::flush();
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if let Some((radius_server, sockets)) = radius_serving {
        tokio::spawn(radius_server.serve(sockets));
    }
    if let Some(tacacs_server) = tacacs_running.clone() {
        tokio::spawn(async move {
            if let Err(e) = tacacs_server.run().await {
                tracing::error!("TACACS+ server error: {}", e);
            }
        });
    }

    if let Some(addr) = config.metrics {
        let metrics = metrics.clone();
        tokio::spawn(async move {
            if let Err(e) = metrics::serve(addr, metrics).await {
                tracing::error!("metrics server error: {}", e);
            }
        });
    }

    tracing::info!("Servers started. Press Ctrl+C to stop.");
    let serving = Serving { config_path: config_path.into(), running: config, shared, radius: radius_running, tacacs: tacacs_running, extensions: extensions_serving };
    let mut checked = tokio::time::interval(Duration::from_secs(1));
    let mut failed = false;
    loop {
        tokio::select! {
            // SIGTERM is what `docker stop` sends, among others.
            _ = stop_asked(&mut terminate, &mut interrupt) => break,
            // Signals arriving during a reload become one more reload.
            _ = hangup.recv() => serving.reload(),
            // A stopped Raft core does not start again, and the node would go on answering from a
            // replica that no longer follows the cluster: a clean exit, for a restart to recover it.
            _ = checked.tick() => {
                if let Some(e) = db.cluster_failure() {
                    tracing::error!("cluster node {}: its Raft core stopped ({e}); exiting so that a restart recovers it", cluster_node_id);
                    failed = true;
                    break;
                }
            }
        }
    }
    // What was written stays on disk for the next start; a request being replayed may run again.
    if let Some(replay) = spool_replay {
        replay.abort();
    }
    for served in &serving.extensions {
        served.stop();
    }
    // A node whose core stopped is shut down within a limit: a shutdown that hangs would leave it
    // neither serving nor ended, and so never restarted.
    let shut = match failed {
        true => tokio::time::timeout(SHUTDOWN_WAIT, db.shutdown()).await.unwrap_or_else(|_| Err("no clean shutdown within the limit".to_string())),
        false => db.shutdown().await,
    };
    if let Err(e) = shut {
        tracing::error!("cluster node shutdown error: {}", e);
    }
    logging::flush();
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::{Extension, Extensions};
    use std::sync::Arc;

    /// An extension whose section has no place to go.
    struct Misplaced;

    impl Extension for Misplaced {
        fn name(&self) -> &'static str {
            "misplaced"
        }
        fn schemas(&self) -> Vec<(&'static str, schemars::Schema)> {
            vec![("nowhere.section", schemars::json_schema!({"type": "object"}))]
        }
    }

    /// A writer whose reader has gone, as after `| head`.
    struct ClosedPipe;

    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_a_schema_that_cannot_be_built() -> Result<(), String> {
        let extensions = Extensions::new(vec![Arc::new(Misplaced)])?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(write_schema("probe", &extensions, &[], &mut out, &mut err), 1);
        assert!(out.is_empty());
        let message = String::from_utf8_lossy(&err);
        assert!(message.starts_with("schema: extension misplaced: `nowhere.section`"), "{message}");
        Ok(())
    }

    /// `--lang en` and `--lang zh-TW` print the schema; another language or argument the usage.
    #[test]
    fn test_schema_languages() {
        let extensions = crate::extensions::builtin();
        for (args, status) in [(&["--lang", "en"][..], 0), (&["--lang", "zh-TW"][..], 0), (&["--lang", "fr"][..], 2), (&["--lang"][..], 2), (&["extra"][..], 2)] {
            let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert_eq!(write_schema("rradius-server", &extensions, &args, &mut out, &mut err), status, "{args:?}");
            if status == 2 {
                assert!(out.is_empty() && String::from_utf8_lossy(&err).contains(SCHEMA_USAGE), "{args:?}");
            }
        }
        // The usage names the program that runs.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(write_schema("rradius-enterprise", &extensions, &["extra".to_string()], &mut out, &mut err), 2);
        assert_eq!(String::from_utf8_lossy(&err), "usage: rradius-enterprise schema [--lang en|zh-TW]\n");
    }

    #[test]
    fn test_a_closed_output() {
        let mut err = Vec::new();
        assert_eq!(write_schema("probe", &crate::extensions::builtin(), &[], &mut ClosedPipe, &mut err), 1);
        assert!(String::from_utf8_lossy(&err).starts_with("schema: "));
    }
}
