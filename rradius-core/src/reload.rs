//! What a reload replaces, and building it. Startup builds the same thing, so a reload
//! is checked exactly as startup is.

use crate::auth_chain::{AuthChains, LuaLimits};
use crate::clients::RadsecClients;
use crate::config::{self, ServerConfig};
use crate::dictionary::Dictionary;
use crate::script_http::ScriptHttp;
use crate::lua_engine::{Db, LuaAuthEngine, LuaVms};
use crate::metrics::Metrics;
use crate::radius::{RadiusServer, RadiusState};
use crate::tacacs::{TacacsServer, TacacsState};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// A value that requests read once and then keep: replacing it leaves those already
/// holding the old value to finish with it.
pub struct Live<T>(RwLock<Arc<T>>);

impl<T> Live<T> {
    pub fn new(value: T) -> Self {
        Self(RwLock::new(Arc::new(value)))
    }

    pub fn get(&self) -> Arc<T> {
        self.0.read().clone()
    }

    pub fn set(&self, value: T) {
        *self.0.write() = Arc::new(value);
    }
}

/// What stays the same for the life of the process and is needed to build a generation.
pub struct Shared {
    pub db: Db,
    pub vms: Arc<LuaVms>,
    pub dictionary: Arc<Dictionary>,
    pub metrics: Arc<Metrics>,
    /// The program's extensions, which give what a configuration's servers use of theirs.
    pub extensions: crate::extension::Extensions,
}

/// Everything built from one reading of the configuration.
pub struct Generation {
    pub radius: Option<RadiusState>,
    pub tacacs: Option<TacacsState>,
}

/// Shared secrets shorter than this get a warning: RFC 8907 section 10.5.1 asks TACACS+
/// administrators for at least 16 characters, and RFC 2865 prefers 16 octets for RADIUS.
/// 24 random octets are what the warning recommends.
pub const MIN_SECRET_LEN: usize = 16;

/// Warns about shared secrets that are easy to guess, without saying how long they are.
/// All that protects a TACACS+ body is its secret.
pub fn warn_about_short_secrets(config: &ServerConfig) {
    let radius = config.radius.as_ref().map(|radius| ("RADIUS", &radius.clients));
    let tacacs = config.tacacs.as_ref().map(|tacacs| ("TACACS+", &tacacs.clients));
    for (protocol, clients) in radius.into_iter().chain(tacacs) {
        for client in clients.clients().filter(|client| client.secret.len() < MIN_SECRET_LEN) {
            tracing::warn!("{protocol} client '{}' has a shared secret shorter than {MIN_SECRET_LEN} octets; use at least 24 random octets", client.name);
        }
    }
    crate::extension::installed().warn_about(config);
}

/// RadSec clients that are networks and take any certificate of `client_ca_file` from anywhere
/// in them, unless they are told otherwise.
pub fn warn_about_radsec_networks(config: &ServerConfig) {
    let Some(radius) = config.radius.as_ref() else { return };
    if config.extension::<RadsecClients>().is_none_or(|radsec| radsec.network_clients_need_tls_name) {
        return;
    }
    for (network, client) in radius.clients.network_clients().filter(|(_, client)| client.tls_name.is_none()) {
        tracing::warn!("RadSec client '{}' is the network {network} with no tls_name: any certificate of radius.tls.client_ca_file is taken from anywhere in it; set tls_name, or network_clients_need_tls_name = true in [radius.tls]", client.name);
    }
}

/// Compiles the scripts and builds the chains and client lists of `config`, and changes
/// nothing. Errors say what is wrong.
pub fn build(config: &ServerConfig, shared: &Shared) -> Result<Generation, String> {
    warn_about_short_secrets(config);
    warn_about_radsec_networks(config);
    let script = &config.lua.script;
    let source = std::fs::read_to_string(script).map_err(|e| format!("cannot read script '{}': {}", script.display(), e))?;
    let engine = LuaAuthEngine::new(shared.vms.clone(), shared.db.clone(), source).map_err(|e| format!("cannot compile script '{}': {}", script.display(), e))?;
    let context = crate::dynamic_authorization::DynamicAuthorizationContext { config, dictionary: &shared.dictionary };
    let dynamic_auth = shared.extensions.dynamic_authorization(&context);
    let http = config.lua.http.as_ref().map(|settings| ScriptHttp::new(settings, shared.metrics.clone())).transpose()?.map(Arc::new);
    let engine = Arc::new(
        engine
            .with_script_http(http.clone())
            .with_script_name(script.display().to_string())
            .with_query_timeout(config.lua.query_timeout)
            .with_max_execution(config.lua.max_execution)
            .with_dynamic_auth(dynamic_auth.clone()),
    );

    // With [auth], each client is authenticated by the chain it selects (or the default
    // chain); clients without one fall back to the top-level script. Both protocols share
    // the chains, so each handler, LDAP pools included, exists once.
    let limits = LuaLimits { dynamic_auth, http, ..LuaLimits::from_settings(&config.lua) };
    let chains = match &config.auth {
        Some(auth) => Some(Arc::new(AuthChains::from_settings(auth, &shared.db, &shared.vms, &limits)?)),
        None => None,
    };

    let forwarder = match config.radius {
        Some(_) => shared.extensions.forwarder(&crate::forwarding::ForwardingContext { config, dictionary: &shared.dictionary, metrics: &shared.metrics })?,
        None => None,
    };
    let second_factor = shared.extensions.second_factor(&crate::second_factor::SecondFactorContext { config, db: &shared.db });
    Ok(Generation {
        radius: config.radius.as_ref().map(|radius| RadiusState::new(radius.clients.clone(), engine.clone(), chains.clone())
                .with_radsec_network_names(config.extension::<RadsecClients>().is_some_and(|radsec| radsec.network_clients_need_tls_name))
                .with_forwarder(forwarder.clone())
                .with_pre_chain(shared.extensions.pre_chain(&crate::pre_chain::PreChainContext { config, db: &shared.db }))
                .with_second_factor(second_factor.clone())
                .with_client_source(shared.extensions.client_source(&crate::client_source::ClientSourceContext { config, db: &shared.db, metrics: &shared.metrics }))
                .with_trace(config.trace.as_ref())),
        tacacs: config.tacacs.as_ref().map(|tacacs| TacacsState { enable: tacacs.enable, clients: tacacs.clients.clone(), engine: engine.clone(), chains: chains.clone(), tls_only: config.extension::<crate::tacacs_transport::TacacsTlsServed>().is_some_and(|served| served.tls_only), second_factor: second_factor.clone() }),
    })
}

/// The running servers and what it takes to replace what they serve with.
pub struct Serving {
    pub config_path: PathBuf,
    /// The configuration the listeners started with. It stays, so that a setting
    /// needing a restart is reported until there has been one.
    pub running: ServerConfig,
    pub shared: Shared,
    pub radius: Option<Arc<RadiusServer>>,
    pub tacacs: Option<Arc<TacacsServer>>,
    /// The servers the program's extensions started.
    pub extensions: Vec<Arc<dyn crate::extension::Served>>,
}

impl Serving {
    /// Reads the configuration again and applies it, or changes nothing. The result is
    /// one line in the log. Requests being handled finish with what they started with.
    pub fn reload(&self) {
        let metrics = &self.shared.metrics;
        match self.try_reload() {
            Ok(restart) => {
                metrics.reload_ok.inc();
                metrics.reload_last_success.set(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs()));
                if restart.is_empty() {
                    tracing::info!("configuration reloaded from {}", self.config_path.display());
                } else {
                    tracing::warn!("configuration reloaded from {}; these need a restart to apply: {}", self.config_path.display(), restart.join(", "));
                }
            }
            Err(e) => {
                metrics.reload_error.inc();
                tracing::error!("reload failed, keeping the running configuration: {}", e.replace('\n', " "));
            }
        }
    }

    /// Everything is built before anything is replaced. Returns the settings that
    /// differ and were not applied.
    #[doc(hidden)]
    pub fn try_reload(&self) -> Result<Vec<&'static str>, String> {
        let config = config::load_with(&self.config_path, &self.shared.extensions).map_err(|e| e.to_string())?;
        let generation = build(&config, &self.shared)?;
        if let (Some(server), Some(state)) = (&self.radius, generation.radius) {
            server.replace_state(state);
        }
        if let (Some(server), Some(radius)) = (&self.radius, &config.radius) {
            server.replace_eap(radius.eap.clone());
        }
        if let (Some(server), Some(state)) = (&self.tacacs, generation.tacacs) {
            server.replace_state(state);
        }
        for served in &self.extensions {
            served.reload(&config);
        }
        Ok(needs_restart(&self.running, &config, &self.shared.extensions))
    }
}

/// The settings a reload does not apply and that differ between the two configurations.
/// Those of the program's `extensions` come after `[radius]`'s, also when neither configuration
/// has `[radius]`, and not when only one has it: that one already needs a restart.
#[doc(hidden)]
pub fn needs_restart(running: &ServerConfig, new: &ServerConfig, extensions: &crate::extension::Extensions) -> Vec<&'static str> {
    let mut names = Vec::new();
    if running.dictionary != new.dictionary {
        names.push("dictionary");
    }
    if running.lua.max_memory != new.lua.max_memory {
        names.push("lua.max_memory_mb");
    }
    if running.cluster != new.cluster {
        names.push("[cluster]");
    }
    if running.metrics != new.metrics {
        names.push("[metrics]");
    }
    if running.log != new.log {
        names.push("[log]");
    }
    if running.runtime != new.runtime {
        names.push("[runtime]");
    }
    let (running_config, new_config) = (running, new);
    match (&running.radius, &new.radius) {
        (Some(running), Some(new)) => {
            let mut differs = |name, differs| {
                if differs {
                    names.push(name);
                }
            };
            differs("radius.bind", running.more_binds != new.more_binds || running.auth_addr.ip() != new.auth_addr.ip() || running.acct_addr.ip() != new.acct_addr.ip());
            differs("radius.auth_port", running.auth_addr.port() != new.auth_addr.port());
            differs("radius.acct_port", running.acct_addr.port() != new.acct_addr.port());
            differs("radius.max_auth_inflight", running.max_auth_inflight != new.max_auth_inflight);
            differs("radius.max_acct_inflight", running.max_acct_inflight != new.max_acct_inflight);
            differs("radius.status_server", running.status_server != new.status_server);
            differs("radius.sockets", running.sockets != new.sockets);
            differs("[radius.accounting_spool]", running.accounting_spool != new.accounting_spool);
            names.extend(extensions.needs_restart(running_config, new_config));
        }
        (None, None) => names.extend(extensions.needs_restart(running_config, new_config)),
        _ => names.push("[radius]"),
    }
    match (&running.tacacs, &new.tacacs) {
        (Some(running), Some(new)) => {
            let mut differs = |name, differs| {
                if differs {
                    names.push(name);
                }
            };
            differs("tacacs.bind", running.addr.ip() != new.addr.ip());
            differs("tacacs.port", running.addr.port() != new.addr.port());
            differs("tacacs.max_connections", running.max_connections != new.max_connections);
            differs("tacacs.max_body_len", running.max_body_len != new.max_body_len);
            differs("tacacs.idle_timeout_ms", running.idle_timeout != new.idle_timeout);
        }
        (None, None) => {}
        _ => names.push("[tacacs]"),
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_db, test_vms};
    use crate::radius::{RadiusPacket, ACCESS_ACCEPT, ACCESS_REJECT, ACCESS_REQUEST};
    use crate::radius::PacketKind;
    use std::path::Path;
    use std::time::Duration;

    const CLUSTER: &str = "[cluster]\ndata_dir = \"data\"\nsecret_raft = \"raft-secret-0123456\"\nsecret_api = \"api-secret-01234567\"\n";
    const CLIENT: &str = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";

    fn write_config(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("rradius.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A configuration with a query limit that slow test queries stay within.
    fn config_text(radius: &str) -> String {
        format!("dictionary = \"d\"\n[lua]\nscript = \"auth.lua\"\nquery_timeout_ms = 120000\n{CLUSTER}[radius]\n{radius}\n")
    }

    /// A server built from `text`, not listening, and what reloads it.
    fn serving(dir: &Path, script: &str, text: &str) -> (Serving, Arc<RadiusServer>) {
        serving_with(dir, script, text, crate::extension::installed().clone())
    }

    /// The same, with `extensions` giving the servers what they use of theirs.
    fn serving_with(dir: &Path, script: &str, text: &str, extensions: crate::extension::Extensions) -> (Serving, Arc<RadiusServer>) {
        std::fs::write(dir.join("auth.lua"), script).unwrap();
        let config_path = write_config(dir, text);
        let running = config::load(&config_path).unwrap();
        let shared = Shared { db: test_db(), vms: test_vms(), dictionary: crate::radius::share_test_dictionary(), metrics: Arc::default(), extensions };
        let generation = build(&running, &shared).unwrap();
        let settings = running.radius.as_ref().unwrap();
        let server = Arc::new(RadiusServer::from_state(settings.auth_addr, settings.acct_addr, generation.radius.unwrap(), crate::radius::share_test_dictionary()));
        (Serving { config_path, running, shared, radius: Some(server.clone()), tacacs: None, extensions: Vec::new() }, server)
    }

    fn request(identifier: u8) -> Vec<u8> {
        RadiusPacket::create_access_request(identifier, [identifier; 16], vec![(1, b"alice".to_vec())], b"s")
    }

    fn code(response: Option<Vec<u8>>) -> u8 {
        RadiusPacket::parse(&response.expect("no response")).unwrap().code
    }

    #[tokio::test]
    async fn test_failures_keep_the_current_state() {
        let good = config_text(CLIENT);
        let cases: [(&str, String, &str, &str); 5] = [
            ("invalid TOML", "not = = toml".to_string(), "return 'accept'", "rradius.toml"),
            ("script syntax error", good.clone(), "return 'accept", "auth.lua"),
            ("bad EAP settings", config_text(&format!("{CLIENT}[radius.eap]\nmethods = []\n")), "return 'accept'", "radius.eap.methods"),
            ("missing secret_file", config_text("[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret_file = \"missing.secret\"\n"), "return 'accept'", "127.0.0.1"),
            ("unknown chain", config_text("[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\nauth = \"vpn\"\n"), "return 'accept'", "vpn"),
        ];
        for (what, text, script, names) in cases {
            let dir = tempfile::tempdir().unwrap();
            let (serving, server) = serving(dir.path(), "return 'accept'", &good);
            let before = server.state();
            write_config(dir.path(), &text);
            std::fs::write(dir.path().join("auth.lua"), script).unwrap();
            let error = serving.try_reload().expect_err(what);
            assert!(error.contains(names), "{what}: {error}");
            assert!(Arc::ptr_eq(&before, &server.state()), "{what} replaced something");
        }
    }

    #[tokio::test]
    async fn test_mismatched_key_keeps_the_current_state() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = crate::test_support::certs::ca(dir.path(), "ca");
        crate::test_support::certs::leaf(dir.path(), "server", &issuer);
        crate::test_support::certs::leaf(dir.path(), "other", &issuer);
        let good = config_text(CLIENT);
        let (serving, server) = serving(dir.path(), "return 'accept'", &good);
        let before = server.state();
        write_config(dir.path(), &config_text(&format!("{CLIENT}[radius.eap]\nmethods = [\"tls\"]\ncert_file = \"server.pem\"\nkey_file = \"other.key\"\nclient_ca_file = \"ca.pem\"\n")));
        assert!(serving.try_reload().unwrap_err().contains("key_file"));
        assert!(Arc::ptr_eq(&before, &server.state()));
    }

    #[tokio::test]
    async fn test_request_running_during_reload() {
        let dir = tempfile::tempdir().unwrap();
        let slow = "sql_query('WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 5000000) SELECT count(*) AS n FROM c') return 'accept'";
        let (serving, server) = serving(dir.path(), slow, &config_text(CLIENT));
        let running = server.clone();
        let source = "127.0.0.1:50000".parse().unwrap();
        let first = tokio::spawn(async move { running.receive(PacketKind::Auth, &request(1), source).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        std::fs::write(dir.path().join("auth.lua"), "return 'reject'").unwrap();
        serving.try_reload().unwrap();
        assert!(!first.is_finished(), "the query was too quick to prove anything");

        assert_eq!(code(server.receive(PacketKind::Auth, &request(2), source).await), ACCESS_REJECT, "the next request runs the new script");
        assert_eq!(code(first.await.unwrap()), ACCESS_ACCEPT, "the running request finishes with the old script");
    }

    #[tokio::test]
    async fn test_auto_setting_kept_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let client = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\nrequire_message_authenticator = \"auto\"\n";
        let (serving, server) = serving(dir.path(), "return 'accept'", &config_text(client));
        let source = "127.0.0.1".parse().unwrap();
        assert_eq!(code(server.handle_auth(&request(1), source).await), ACCESS_ACCEPT, "settles on requiring it");

        serving.try_reload().unwrap();
        let bare = RadiusPacket { code: ACCESS_REQUEST, identifier: 2, authenticator: [2; 16], attributes: vec![(1, b"alice".to_vec())] }.encode();
        assert!(server.handle_auth(&bare, source).await.is_none());
    }

    /// Loads both configurations and returns what differs.
    fn restart_names(before: &str, after: &str) -> Vec<&'static str> {
        let dir = tempfile::tempdir().unwrap();
        let issuer = crate::test_support::certs::ca(dir.path(), "ca");
        crate::test_support::certs::leaf(dir.path(), "server", &issuer);
        let path = write_config(dir.path(), before);
        let before = config::load(&path).unwrap();
        write_config(dir.path(), after);
        needs_restart(&before, &config::load(&path).unwrap(), crate::extension::installed())
    }

    #[test]
    fn test_settings_that_need_a_restart() {
        const TACACS: &str = "[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        let radius = |settings: &str| config_text(&format!("{settings}\n{CLIENT}"));
        let tacacs = |settings: &str| format!("{}[tacacs]\n{settings}\n{TACACS}", config_text(CLIENT));
        let plain = radius("");
        // What enable requests get is applied too.
        let tacacs_enable = |mode: &str| format!("{}[tacacs]\nenable = \"{mode}\"\n{TACACS}", config_text(CLIENT));
        assert!(restart_names(&tacacs_enable("deny"), &tacacs_enable("login")).is_empty());
        // TACACS+ authorization groups come with the client list.
        let groups = |rule: &str| format!("{}[tacacs]\n[tacacs.authorize_groups]\ng = ['{rule}']\n{TACACS}authorize_group = \"g\"\n", config_text(CLIENT));
        assert!(restart_names(&groups("permit service=shell"), &groups("deny service=shell")).is_empty());
        // The script and the secret are applied, so they are not reported.
        assert!(restart_names(&plain, &plain.replace("auth.lua", "other.lua")).is_empty());
        assert!(restart_names(&plain, &plain.replace("secret = \"s\"", "secret = \"t\"")).is_empty());

        let dictionary = plain.replace("dictionary = \"d\"", "dictionary = \"e\"");
        let cluster = plain.replace("data_dir = \"data\"", "data_dir = \"other\"");
        let snapshots = plain.replace("data_dir = \"data\"", "data_dir = \"data\"\nsnapshot_every = 1000");
        let readers = plain.replace("data_dir = \"data\"", "data_dir = \"data\"\nread_connections = 1");
        let cases: Vec<(String, String, &str)> = vec![
            (plain.clone(), dictionary, "dictionary"),
            (plain.clone(), cluster, "[cluster]"),
            (plain.clone(), snapshots, "[cluster]"),
            (plain.clone(), readers, "[cluster]"),
            (plain.clone(), format!("{plain}[log]\nlevel = \"debug\"\n"), "[log]"),
            (plain.clone(), radius("[radius.accounting_spool]\ndir = \"spool\"\n"), "[radius.accounting_spool]"),
            (plain.clone(), format!("{plain}[metrics]\n"), "[metrics]"),
            (plain.clone(), radius("bind = \"127.0.0.1\""), "radius.bind"),
            (plain.clone(), radius("bind = [\"0.0.0.0\", \"::\"]"), "radius.bind"),
            (radius("bind = [\"0.0.0.0\", \"::\"]"), radius("bind = [\"0.0.0.0\", \"::1\"]"), "radius.bind"),
            (plain.clone(), radius("auth_port = 1"), "radius.auth_port"),
            (plain.clone(), radius("acct_port = 1"), "radius.acct_port"),
            (plain.clone(), radius("max_auth_inflight = 1"), "radius.max_auth_inflight"),
            (plain.clone(), radius("max_acct_inflight = 1"), "radius.max_acct_inflight"),
            (plain.clone(), radius("status_server = false"), "radius.status_server"),
            (plain.clone(), tacacs(""), "[tacacs]"),
            (tacacs(""), tacacs("bind = \"127.0.0.1\""), "tacacs.bind"),
            (tacacs(""), tacacs("port = 1"), "tacacs.port"),
            (tacacs(""), tacacs("max_connections = 1"), "tacacs.max_connections"),
            (tacacs(""), tacacs("max_body_len = 1"), "tacacs.max_body_len"),
            (tacacs(""), tacacs("idle_timeout_ms = 1"), "tacacs.idle_timeout_ms"),
            (plain.clone(), plain.replace("[lua]\n", "[lua]\nmax_memory_mb = 1\n"), "lua.max_memory_mb"),
        ];
        let mut cases = cases;
        // Only Linux takes more than one socket, or pinning.
        if cfg!(target_os = "linux") {
            cases.push((radius("sockets = 1"), radius("sockets = 2"), "radius.sockets"));
            cases.push((plain.clone(), format!("{plain}[runtime]\npin_threads = true\n"), "[runtime]"));
        }
        for (before, after, name) in cases {
            assert_eq!(restart_names(&before, &after), vec![name]);
        }
        let without_radius = format!("dictionary = \"d\"\n[lua]\nscript = \"auth.lua\"\nquery_timeout_ms = 120000\n{CLUSTER}[tacacs]\n{TACACS}");
        assert!(restart_names(&tacacs(""), &without_radius).contains(&"[radius]"));
    }

    /// An extension that starts a server noting the `max_acct_inflight` of each reload, and
    /// names a setting of its own when `max_auth_inflight` changes.
    struct Watch(Arc<std::sync::Mutex<Vec<usize>>>);

    impl crate::extension::Served for Watch {
        fn reload(&self, config: &ServerConfig) {
            if let (Ok(mut seen), Some(radius)) = (self.0.lock(), config.radius.as_ref()) {
                seen.push(radius.max_acct_inflight);
            }
        }
    }

    impl crate::extension::Extension for Watch {
        fn name(&self) -> &'static str {
            "watch"
        }
        fn serve(&self, _context: &crate::extension::ServeContext<'_>) -> Result<Option<Arc<dyn crate::extension::Served>>, String> {
            Ok(Some(Arc::new(Watch(self.0.clone()))))
        }
        fn needs_restart(&self, running: &ServerConfig, new: &ServerConfig) -> Vec<&'static str> {
            let inflight = |config: &ServerConfig| config.radius.as_ref().map(|radius| radius.max_auth_inflight);
            if inflight(running) == inflight(new) { Vec::new() } else { vec!["watch.setting"] }
        }
    }

    #[tokio::test]
    async fn test_extensions_hear_of_reloads_and_name_restarts() -> Result<(), String> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let extensions = crate::extension::Extensions::new(vec![Arc::new(Watch(seen.clone()))])?;
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let text = |auth: usize, acct: usize, port: u16| format!("{}[tacacs]\nport = {port}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n", config_text(&format!("max_auth_inflight = {auth}\nmax_acct_inflight = {acct}\n{CLIENT}")));
        let (mut serving, server) = serving(dir.path(), "return 'accept'", &text(10, 10, 49));
        let context = crate::extension::ServeContext { config: &serving.running, radius: Some(server), tacacs: None, metrics: serving.shared.metrics.clone(), db: serving.shared.db.clone() };
        serving.extensions = extensions.serve(&context)?;
        write_config(dir.path(), &text(10, 11, 49));
        serving.try_reload()?;
        assert_eq!(seen.lock().map_err(|e| e.to_string())?.clone(), vec![11]);
        // A reload that fails tells it nothing.
        write_config(dir.path(), &text(10, 12, 49).replace("[tacacs]", "[tacacs"));
        assert!(serving.try_reload().is_err());
        assert_eq!(seen.lock().map_err(|e| e.to_string())?.clone(), vec![11]);

        // Its names come after those of [radius], before those of [tacacs].
        let load = |text: &str| -> Result<ServerConfig, String> {
            write_config(dir.path(), text);
            config::load(&dir.path().join("rradius.toml")).map_err(|e| e.to_string())
        };
        let (before, after) = (load(&text(10, 10, 49))?, load(&text(20, 20, 50))?);
        assert_eq!(needs_restart(&before, &after, &extensions), ["radius.max_auth_inflight", "radius.max_acct_inflight", "watch.setting", "tacacs.port"]);
        Ok(())
    }

    /// Runs one request through `server` and returns the code of the response.
    async fn ask(server: &RadiusServer, identifier: u8) -> u8 {
        code(server.handle_auth(&request(identifier), "127.0.0.1".parse().unwrap()).await)
    }

    #[tokio::test]
    async fn test_reload_applies_lua_limits() {
        let dir = tempfile::tempdir().unwrap();
        let text = |limits: &str| format!("dictionary = \"d\"\n[lua]\nscript = \"auth.lua\"\n{limits}\n{CLUSTER}[radius]\n{CLIENT}");

        let looping = "for i = 1, 20000000 do end return 'accept'";
        let (serving, server) = serving(dir.path(), looping, &text("max_execution_ms = 1"));
        assert_eq!(ask(&server, 1).await, ACCESS_REJECT, "the limit stops the loop");
        write_config(dir.path(), &text("max_execution_ms = 60000"));
        serving.try_reload().unwrap();
        assert_eq!(ask(&server, 2).await, ACCESS_ACCEPT);

        let slow = "sql_query('WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 3000000) SELECT count(*) AS n FROM c') return 'accept'";
        std::fs::write(dir.path().join("auth.lua"), slow).unwrap();
        write_config(dir.path(), &text("query_timeout_ms = 1"));
        serving.try_reload().unwrap();
        // A query past its limit leaves the request unanswered, for the NAS to send again.
        assert!(server.handle_auth(&request(3), std::net::IpAddr::from([127, 0, 0, 1])).await.is_none(), "the query limit applies");
        write_config(dir.path(), &text("query_timeout_ms = 120000"));
        serving.try_reload().unwrap();
        assert_eq!(ask(&server, 4).await, ACCESS_ACCEPT);
    }

    /// An EAP-TLS conversation with `server` from the first response on, until it ends.
    async fn finish_eap(server: &RadiusServer, peer: &mut crate::eap::tests::Peer, mut identifier: u8, mut response: RadiusPacket) -> u8 {
        for _ in 0..64 {
            if response.code != crate::radius::ACCESS_CHALLENGE {
                return response.code;
            }
            let eap = crate::eap::join_message(response.attributes.iter().filter(|(kind, _)| *kind == crate::eap::EAP_MESSAGE).map(|(_, value)| value.as_slice()));
            identifier += 1;
            let answer = peer.answer(&eap);
            response = eap_step(server, identifier, &answer, response.attribute(crate::radius::STATE)).await;
        }
        panic!("the conversation did not end");
    }

    async fn eap_step(server: &RadiusServer, identifier: u8, eap: &[u8], state: Option<&[u8]>) -> RadiusPacket {
        let mut attributes = vec![(1, b"alice.example.com".to_vec())];
        attributes.extend(crate::eap::split_message(eap));
        attributes.extend(state.map(|state| (crate::radius::STATE, state.to_vec())));
        let request = RadiusPacket::create_access_request(identifier, [identifier; 16], attributes, b"s");
        RadiusPacket::parse(&server.handle_auth(&request, "127.0.0.1".parse().unwrap()).await.expect("no response")).unwrap()
    }

    #[tokio::test]
    async fn test_reload_applies_trace() {
        let dir = tempfile::tempdir().unwrap();
        let (serving, server) = serving(dir.path(), "return 'accept'", &config_text(CLIENT));
        assert!(server.state().trace.is_none());
        write_config(dir.path(), &format!("{}\n[log.trace]\nusers = [\"alice\"]\nminutes = 5\n", config_text(CLIENT)));
        assert_eq!(serving.try_reload().unwrap(), Vec::<&str>::new(), "no restart");
        assert!(server.state().trace.is_some());
        write_config(dir.path(), &config_text(CLIENT));
        serving.try_reload().unwrap();
        assert!(server.state().trace.is_none());
    }

    #[tokio::test]
    async fn test_reload_refuses_ttls_without_a_chain() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = crate::test_support::certs::ca(dir.path(), "ca");
        crate::test_support::certs::leaf(dir.path(), "server", &issuer);
        let good = config_text(CLIENT);
        let (serving, _server) = serving(dir.path(), "return 'accept'", &good);
        write_config(dir.path(), &config_text(&format!("{CLIENT}[radius.eap]\nmethods = [\"ttls\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\n")));
        let error = serving.try_reload().expect_err("no [auth]");
        assert!(error.contains("radius.eap.methods"), "{error}");
    }

    #[tokio::test]
    async fn test_reload_during_an_eap_conversation() {
        use rustls::pki_types::{pem::PemObject, CertificateDer};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let issuer = crate::test_support::certs::ca(path, "ca");
        crate::test_support::certs::leaf(path, "server", &issuer);
        crate::test_support::certs::named_leaf(path, "alice", "alice.example.com", 20, &issuer);
        let text = config_text(&format!(
            "{CLIENT}[radius.eap]\ncert_file = \"{0}/server.pem\"\nkey_file = \"{0}/server.key\"\nclient_ca_file = \"{0}/ca.pem\"\n",
            path.display()
        ));
        let (serving, server) = serving(path, "return 'accept'", &text);
        server.replace_eap(serving.running.radius.as_ref().unwrap().eap.clone());
        let certificate = || CertificateDer::from_pem_file(path.join("server.pem")).unwrap().to_vec();
        let peer = || crate::eap::tests::Peer::new(crate::eap::tests::client_connection(path, Some("alice"), &rustls::version::TLS13));
        let identity = crate::eap::tests::identity_response(1, b"alice.example.com");
        let old = certificate();

        let mut under_way = peer();
        let first = eap_step(&server, 1, &identity, None).await;

        crate::test_support::certs::leaf(path, "server", &issuer);
        let new = certificate();
        assert_ne!(old, new);
        serving.try_reload().unwrap();

        assert_eq!(finish_eap(&server, &mut under_way, 1, first).await, ACCESS_ACCEPT);
        assert_eq!(under_way.client.peer_certificates().unwrap()[0].to_vec(), old, "the conversation under way kept the old certificate");

        let mut next = peer();
        let first = eap_step(&server, 100, &identity, None).await;
        assert_eq!(finish_eap(&server, &mut next, 100, first).await, ACCESS_ACCEPT);
        assert_eq!(next.client.peer_certificates().unwrap()[0].to_vec(), new, "the next conversation used the new certificate");
    }


    #[tokio::test]
    async fn test_reload_reaches_tacacs() {
        let dir = tempfile::tempdir().unwrap();
        let tacacs = |secret: &str| format!("{}[tacacs]\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"{secret}\"\n", config_text(CLIENT));
        let (mut serving, _server) = serving(dir.path(), "return 'accept'", &tacacs("old"));
        let shared = &serving.shared;
        let settings = serving.running.tacacs.as_ref().unwrap();
        let state = build(&serving.running, shared).unwrap().tacacs.unwrap();
        let server = Arc::new(TacacsServer::from_state(settings.addr, state, settings.max_body_len));
        serving.tacacs = Some(server.clone());
        let local = "127.0.0.1".parse().unwrap();
        let secret = |server: &TacacsServer| server.state().clients.lookup(local).unwrap().secret.clone();

        // A failed reload leaves it alone.
        write_config(dir.path(), &tacacs("new"));
        std::fs::write(dir.path().join("auth.lua"), "return 'accept").unwrap();
        serving.try_reload().unwrap_err();
        assert_eq!(secret(&server), b"old");

        std::fs::write(dir.path().join("auth.lua"), "return 'accept'").unwrap();
        serving.try_reload().unwrap();
        assert_eq!(secret(&server), b"new");
    }

    #[tokio::test]
    async fn test_reload_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (serving, _server) = serving(dir.path(), "return 'accept'", &config_text(CLIENT));
        let metrics = serving.shared.metrics.clone();
        serving.reload();
        assert_eq!((metrics.reload_ok.get(), metrics.reload_error.get()), (1, 0));
        let at = metrics.reload_last_success.get();
        assert!(at > 1_000_000_000, "not a timestamp: {at}");

        std::fs::write(dir.path().join("auth.lua"), "return 'accept").unwrap();
        metrics.reload_last_success.set(1);
        serving.reload();
        assert_eq!((metrics.reload_ok.get(), metrics.reload_error.get()), (1, 1));
        assert_eq!(metrics.reload_last_success.get(), 1, "only a success moves the timestamp");
    }

    #[tokio::test]
    async fn test_reload_gives_scripts_the_new_client_list() {
        let dir = tempfile::tempdir().unwrap();
        let script = "local ok, result = pcall(send_coa, '192.0.2.1', { ['User-Name'] = 'bob' }) return (ok and result == 'timeout') and 'accept' or 'reject'";
        let timing = "coa_timeout_ms = 1\ncoa_attempts = 1\n";
        let (serving, server) = serving_with(dir.path(), script, &config_text(&format!("{timing}{CLIENT}")), fake_coa());
        let source = "127.0.0.1".parse().unwrap();
        assert_eq!(code(server.handle_auth(&request(1), source).await), ACCESS_REJECT, "not a client yet");

        write_config(dir.path(), &config_text(&format!("{timing}{CLIENT}[[radius.clients]]\naddress = \"192.0.2.1\"\nsecret = \"t\"\n")));
        serving.try_reload().unwrap();
        assert_eq!(code(server.handle_auth(&request(2), source).await), ACCESS_ACCEPT);
    }

    /// A sender for tests: what a configuration's RADIUS clients are, by name or address, is
    /// sent to, and times out; anything else is not a configured client.
    struct ClientsSender(Vec<String>);

    struct TimesOut;

    impl crate::dynamic_authorization::PreparedRequest for TimesOut {
        fn send(self: Box<Self>) -> futures::future::BoxFuture<'static, crate::dynamic_authorization::Outcome> {
            Box::pin(async { crate::dynamic_authorization::Outcome::Timeout })
        }
    }

    impl crate::dynamic_authorization::DynamicAuthorization for ClientsSender {
        fn prepare(&self, _kind: crate::dynamic_authorization::RequestType, target: &str, _attributes: &[(String, crate::common::AttrValue)]) -> Result<Box<dyn crate::dynamic_authorization::PreparedRequest>, String> {
            match self.0.iter().any(|known| known == target) {
                true => Ok(Box::new(TimesOut)),
                false => Err(format!("'{target}' is not a configured client")),
            }
        }
    }

    /// An extension that gives each configuration a `ClientsSender` for its clients.
    struct FakeCoa;

    impl crate::extension::Extension for FakeCoa {
        fn name(&self) -> &'static str {
            "fake-coa"
        }
        fn dynamic_authorization(&self, context: &crate::dynamic_authorization::DynamicAuthorizationContext<'_>) -> Option<Arc<dyn crate::dynamic_authorization::DynamicAuthorization>> {
            let clients = context.config.radius.iter().flat_map(|radius| radius.clients.network_clients().map(|(network, _)| network.addr().to_string()).chain(radius.clients.names().map(str::to_string))).collect();
            Some(Arc::new(ClientsSender(clients)))
        }
    }

    fn fake_coa() -> crate::extension::Extensions {
        crate::extension::Extensions::unchecked(vec![Arc::new(FakeCoa)])
    }

    /// The handlers of a chain send through what the program gives, as the script does.
    #[tokio::test]
    async fn test_lua_handlers_have_dynamic_authorization() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("handler.lua"), "local ok, err = pcall(send_coa, 'nobody', { ['User-Name'] = 'bob' }) return string.find(tostring(err), 'not a configured client', 1, true) and 'accept' or 'reject'").unwrap();
        let text = format!("dictionary = \"d\"\n[lua]\nscript = \"auth.lua\"\n{CLUSTER}[auth]\n[[auth.handlers]]\ntype = \"lua\"\nscript = \"handler.lua\"\n[radius]\n{CLIENT}");
        let (_serving, server) = serving_with(dir.path(), "return 'reject'", &text, fake_coa());
        assert_eq!(code(server.handle_auth(&request(1), "127.0.0.1".parse().unwrap()).await), ACCESS_ACCEPT);
    }

    #[tokio::test]
    async fn test_port_change_applies_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let (serving, server) = serving(dir.path(), "return 'accept'", &config_text(CLIENT));
        std::fs::write(dir.path().join("auth.lua"), "return 'reject'").unwrap();
        write_config(dir.path(), &config_text(&format!("auth_port = 1\n{CLIENT}")));
        assert_eq!(serving.try_reload().unwrap(), vec!["radius.auth_port"]);
        assert_eq!(code(server.handle_auth(&request(1), "127.0.0.1".parse().unwrap()).await), ACCESS_REJECT);
    }
}
