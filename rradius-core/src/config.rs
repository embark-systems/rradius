use crate::clients::{ClientOptions, ClientTable, MessageAuthenticatorPolicy, DEFAULT_COA_PORT};
use crate::ldap_handler::{GroupSource, LdapGroups, LdapSettings, PasswordCheck};
use crate::tacacs_rules::AuthorizationRules;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Fully resolved server settings; every path is absolute or relative to the
/// configuration file's directory.
#[derive(Debug)]
pub struct ServerConfig {
    pub dictionary: PathBuf,
    pub lua: LuaSettings,
    pub cluster: ClusterSettings,
    pub auth: Option<AuthSettings>,
    pub radius: Option<RadiusSettings>,
    pub tacacs: Option<TacacsSettings>,
    /// Where `/metrics` is served; `None` serves nothing.
    pub metrics: Option<SocketAddr>,
    pub log: LogSettings,
    /// Which requests to write out in full for a while. A reload applies it.
    pub trace: Option<TraceSettings>,
    /// Where requests for other realms go. A reload applies it.
    /// What the `user` commands run; `None` without `[accounts]`.
    pub accounts: Option<AccountSettings>,
    /// `[runtime]`. A start applies it; a reload does not.
    pub runtime: RuntimeSettings,
    /// What the program's extensions read from their sections.
    pub extensions: crate::extension::ExtensionSettings,
}

impl ServerConfig {
    /// What the extension that reads into a `T` made of its sections.
    pub fn extension<T: std::any::Any>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }
}

/// `[runtime]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeSettings {
    /// Each runtime worker thread bound to one CPU; Linux only.
    pub pin_threads: bool,
}

/// Whether several SO_REUSEPORT sockets on one port share its UDP packets out. Linux hashes
/// each source to one of them; other systems give every packet to the same socket.
const SPREADS_UDP_OVER_SOCKETS: bool = cfg!(target_os = "linux");

/// `[radius] sockets`: by default one per runtime worker thread where that spreads the load.
fn radius_sockets(value: Option<usize>) -> Result<usize, String> {
    match value {
        None if SPREADS_UDP_OVER_SOCKETS => Ok(crate::runtime::worker_threads()),
        None => Ok(1),
        Some(0) => Err("radius.sockets must be greater than 0".to_string()),
        Some(n) if n > 1 && !SPREADS_UDP_OVER_SOCKETS => Err(format!("radius.sockets = {n}: only Linux shares UDP packets out over several sockets on one port, so it must be 1 here")),
        Some(n) => Ok(n),
    }
}

/// `[accounts]`: the statement each `user` command runs, with `?1` for the account
/// name and `?2` for the stored password value, and how passwords are stored.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AccountSettings {
    pub add: Option<String>,
    pub passwd: Option<String>,
    pub enable: Option<String>,
    pub disable: Option<String>,
    pub delete: Option<String>,
    pub show: Option<String>,
    pub scheme: PasswordScheme,
}

/// How the `user` commands store a password; each is a form PAP can check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PasswordScheme {
    /// `{crypt}` with bcrypt.
    #[default]
    Bcrypt,
    /// `{crypt}` with SHA-512 crypt (`$6$`): far quicker to check than bcrypt.
    Sha512Crypt,
    /// `{nthash}`, which MS-CHAP and PEAP need.
    Nthash,
    /// `{clear}`, which CHAP needs.
    Clear,
}

/// `[log.trace]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSettings {
    pub users: Vec<String>,
    pub clients: Vec<String>,
    pub duration: Duration,
}

/// How much the server logs. Each level includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

/// `[log]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogSettings {
    pub level: LogLevel,
    pub format: LogFormat,
    /// Whether to write a record of each authentication decision.
    pub auth: bool,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self { level: LogLevel::default(), format: LogFormat::default(), auth: true }
    }
}

/// How an authentication chain combines its handlers' results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPolicy {
    /// The first handler that does not ignore decides.
    #[default]
    FirstMatch,
    /// Every handler runs; any reject rejects, and at least one must accept.
    AllAccept,
}

// Only read at startup and on reload, so size does not matter here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AuthHandlerSettings {
    Sql { user_query: String, certificate_query: Option<String>, reply_query: Option<String>, default_user: Option<String>, timeout: Duration },
    /// `verifies_password`: the script compares passwords, so its accept proves one inside EAP.
    Lua { script: PathBuf, verifies_password: bool },
    Ldap(LdapSettings),
}

/// One authentication chain: handlers run in order and are combined by `policy`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainSettings {
    pub policy: AuthPolicy,
    pub handlers: Vec<AuthHandlerSettings>,
    /// How to derive the account name; `None` uses the received name unchanged.
    pub username: Option<UsernameSettings>,
}

/// Which part of a user name is a realm, and so left out of the account name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StripRealm {
    #[default]
    None,
    /// From the last `@` on.
    Suffix,
    /// Up to and including the last backslash.
    Prefix,
    /// `Prefix`, then `Suffix`.
    Both,
}

/// A chain's `username` table.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsernameSettings {
    pub strip_realm: StripRealm,
    pub lowercase: bool,
    pub allowed: Option<AllowedNames>,
}

/// A compiled `allowed` pattern, which must match the whole name.
#[derive(Debug, Clone)]
pub struct AllowedNames(regex::Regex);

impl AllowedNames {
    /// Checks `pattern` alone first: wrapped, `a)|(?:b` would compile into something
    /// that anchors neither side of the pattern as a whole.
    pub fn new(pattern: &str) -> Result<Self, regex::Error> {
        regex::Regex::new(pattern)?;
        regex::Regex::new(&format!("^(?:{pattern})$")).map(Self)
    }

    pub fn is_match(&self, name: &str) -> bool {
        self.0.is_match(name)
    }
}

impl PartialEq for AllowedNames {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

/// `[auth]`: the default chain from `[[auth.handlers]]`, if any, and the
/// named chains from `[auth.chains.<name>]` that clients select with `auth`.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthSettings {
    pub default: Option<ChainSettings>,
    pub chains: BTreeMap<String, ChainSettings>,
}

/// One member of the hiqlite cluster.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterNode {
    pub id: u64,
    pub raft: String,
    pub api: String,
}

/// The top-level Lua script and the limits for its database calls and its own
/// execution, which also apply to `lua` handlers in an authentication chain.
#[derive(Debug, Clone, PartialEq)]
pub struct LuaSettings {
    pub script: PathBuf,
    pub query_timeout: Duration,
    pub max_execution: Duration,
    /// What one Lua VM may hold, in bytes.
    pub max_memory: usize,
    /// `http_request` exists only with this.
    pub http: Option<ScriptHttpSettings>,
}

/// `[lua.http]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptHttpSettings {
    pub allowed_hosts: Vec<AllowedHost>,
    pub timeout: Duration,
    pub max_response_bytes: usize,
    pub max_concurrent: usize,
    pub ca_file: Option<PathBuf>,
}

/// A host scripts may call, on the port given or else the scheme's default.
#[derive(Debug, Clone, PartialEq)]
pub struct AllowedHost {
    pub host: String,
    pub port: Option<u16>,
    /// Written `http://...`: this host, and no other, takes plain text instead of TLS.
    pub plaintext: bool,
}

impl AllowedHost {
    /// From `host`, `host:port`, or either after `http://` to allow plain text; nothing
    /// else. Only the characters of a host and port: a URL parser would quietly take
    /// `a.example\\b.example` to be `a.example`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (plaintext, address) = match text.strip_prefix("http://") {
            Some(address) => (true, address),
            None => (false, text),
        };
        let plain = !address.is_empty() && address.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
        let scheme = if plaintext { "http" } else { "https" };
        let url = plain.then(|| reqwest::Url::parse(&format!("{scheme}://{address}/")).ok()).flatten();
        let host = url.as_ref().and_then(|url| url.host_str().map(str::to_string));
        match (url, host) {
            (Some(url), Some(host)) => {
                let default = if plaintext { ":80" } else { ":443" };
                // Written out, the default port is still that port: the URL parser drops it.
                let port = if address.ends_with(default) { url.port_or_known_default() } else { url.port() };
                Ok(Self { host, port, plaintext })
            }
            _ => Err(format!("'{text}' is not host, host:port or http://host:port")),
        }
    }
}

/// Settings for the embedded hiqlite node.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSettings {
    pub node_id: u64,
    pub data_dir: PathBuf,
    pub secret_raft: String,
    pub secret_api: String,
    pub listen: IpAddr,
    pub nodes: Vec<ClusterNode>,
    /// SQL files applied at startup, before serving.
    pub migrations_dir: Option<PathBuf>,
    /// How long to wait for the cluster before applying them.
    pub migrations_wait: Duration,
    /// Raft log entries applied between two snapshots of the database.
    pub snapshot_every: u64,
    /// Connections for reading the local database.
    pub read_connections: usize,
    /// `stop_without_leader_ms`: authentication goes unanswered after this long without a leader.
    pub stop_without_leader: Option<Duration>,
}

/// Read connections by default: twice the worker threads, at most `DEFAULT_READ_CONNECTIONS_CAP`.
/// hiqlite's own default, 4, made logins that read the database wait for a connection; past 14
/// on 7 CPUs more did not help (see docs/usage.md section 2.2). Each holds its own page cache
/// and files, so a large host should not open hundreds by default.
pub fn default_read_connections() -> usize {
    read_connections_for(crate::runtime::worker_threads())
}

/// The default for `workers` worker threads.
fn read_connections_for(workers: usize) -> usize {
    (2 * workers).min(DEFAULT_READ_CONNECTIONS_CAP)
}

/// The most read connections the default gives.
pub const DEFAULT_READ_CONNECTIONS_CAP: usize = 32;
/// The most read connections a configuration may ask for, as hiqlite allows elsewhere.
pub const MAX_READ_CONNECTIONS: usize = 128;

/// hiqlite's own default, 10,000, made the node copy its whole database every few seconds under
/// a few thousand writes a second (see docs/usage.md section 2.2).
pub const DEFAULT_SNAPSHOT_EVERY: u64 = 100_000;

#[derive(Debug)]
pub struct RadiusSettings {
    pub auth_addr: SocketAddr,
    pub acct_addr: SocketAddr,
    /// The other addresses of `bind`, on the same ports.
    pub more_binds: Vec<IpAddr>,
    /// Sockets on each address and port, each with its own receive loop.
    pub sockets: usize,
    pub clients: Arc<ClientTable>,
    /// Whether to answer Status-Server packets.
    pub status_server: bool,
    pub max_auth_inflight: usize,
    pub max_acct_inflight: usize,
    /// How long one attempt of `send_disconnect` / `send_coa` waits for its response.
    pub coa_timeout: Duration,
    /// How many times those functions send a request, the first time included.
    pub coa_attempts: u32,
    pub accounting_spool: Option<SpoolSettings>,
    pub eap: Option<Arc<EapSettings>>,
    /// `require_message_authenticator` and `limit_proxy_state`, for clients a client source finds.
    pub client_policies: (MessageAuthenticatorPolicy, MessageAuthenticatorPolicy),
}

/// The CoA port of RFC 5176, where a NAS and the CoA relay listen.
pub fn default_coa_relay_port() -> u16 {
    3799
}

/// `[radius.eap]`, with its certificates already read and checked.
#[derive(Debug)]
pub struct EapSettings {
    /// EAP types, the first being the one to start with.
    pub methods: Vec<u8>,
    /// For EAP-TLS; `None` without `tls` in `methods`.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// What checked EAP-TLS certificates, to check a resumed session's again.
    pub tls_verifier: Option<Arc<dyn rustls::server::danger::ClientCertVerifier>>,
    /// For PEAP and TTLS; `None` without either in `methods`. It resumes nothing.
    pub tunnel_tls: Option<Arc<rustls::ServerConfig>>,
    /// `ocsp`: asked about each EAP-TLS client certificate before the authorization.
    pub revocation: Option<crate::revocation::Revocation>,
    /// Session resumption, made by the extension that resumes sessions from `resumption_ms` and
    /// the settings with it; prepared before each EAP request. `None` resumes nothing.
    pub resumption: Option<crate::session_resumption::Resumption>,
    /// For PEAP when sessions are resumed. TTLS never resumes: a peer that resumes a TTLS session
    /// skips its inner method (wpa_supplicant does), and nothing here would check a password.
    pub peap_tls: Option<Arc<rustls::ServerConfig>>,
    /// The methods of the program's extensions that `methods` names, made with these settings.
    pub extra: Vec<Arc<dyn crate::eap_method::EapMethod>>,
    pub fragment_size: usize,
    pub session_timeout: Duration,
    pub max_sessions: usize,
    pub max_sessions_per_client: usize,
    pub max_sessions_per_device: usize,
    pub match_identity: bool,
}

/// `[radius.accounting_spool]`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpoolSettings {
    pub dir: PathBuf,
    pub max_bytes: u64,
    pub replay_interval: Duration,
    /// How many requests replay sends at once: by default 32 for `[proxy.accounting_spool]`
    /// and 1, strictly in order, for `[radius.accounting_spool]`.
    pub replay_window: usize,
}

/// `replay_window` of `[proxy.accounting_spool]` when it is not set, and the most it may be.
const DEFAULT_REPLAY_WINDOW: usize = 32;
const MAX_REPLAY_WINDOW: usize = 1024;

#[derive(Debug)]
pub struct TacacsSettings {
    /// What enable requests get.
    pub enable: crate::tacacs::EnableMode,
    pub addr: SocketAddr,
    pub max_body_len: usize,
    pub clients: Arc<ClientTable>,
    pub max_connections: usize,
    pub idle_timeout: Duration,
}

#[derive(Debug)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    dictionary: PathBuf,
    /// Moved to `[lua].script`; only read to report the new place.
    script: Option<toml::Value>,
    lua: Option<RawLua>,
    cluster: Option<RawCluster>,
    auth: Option<RawAuth>,
    radius: Option<RawRadius>,
    tacacs: Option<RawTacacs>,
    metrics: Option<RawMetrics>,
    runtime: Option<RawRuntime>,
    log: Option<RawLog>,
    /// `[proxy]`, which an extension reads.
    proxy: Option<toml::Value>,
    accounts: Option<RawAccounts>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    #[serde(default)]
    pin_threads: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccounts {
    add: Option<String>,
    passwd: Option<String>,
    enable: Option<String>,
    disable: Option<String>,
    delete: Option<String>,
    show: Option<String>,
    scheme: Option<String>,
    /// `[accounts.clients]`, which an extension reads.
    clients: Option<toml::Value>,
    /// `[accounts.dpsk]`, which an extension reads.
    dpsk: Option<toml::Value>,
    /// Read by the extension for EAP-AKA'.
    aka: Option<toml::Value>,
}



#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLog {
    level: Option<String>,
    format: Option<String>,
    #[serde(default = "default_log_auth")]
    auth: bool,
    trace: Option<RawTrace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrace {
    #[serde(default)]
    users: Vec<String>,
    #[serde(default)]
    clients: Vec<String>,
    minutes: Option<u64>,
}

fn default_log_auth() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetrics {
    #[serde(default = "loopback_address")]
    bind: IpAddr,
    #[serde(default = "default_metrics_port")]
    port: u16,
}

fn loopback_address() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn default_metrics_port() -> u16 {
    9812
}

pub fn default_radsec_port() -> u16 {
    2083
}

fn default_coa_timeout_ms() -> u64 {
    1000
}

fn default_coa_attempts() -> u32 {
    3
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLua {
    script: PathBuf,
    #[serde(default = "default_query_timeout_ms")]
    query_timeout_ms: u64,
    #[serde(default = "default_max_execution_ms")]
    max_execution_ms: u64,
    #[serde(default = "default_max_memory_mb")]
    max_memory_mb: u32,
    http: Option<RawScriptHttp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScriptHttp {
    allowed_hosts: Vec<String>,
    #[serde(default = "default_http_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_http_max_response_bytes")]
    max_response_bytes: usize,
    #[serde(default = "default_http_max_concurrent")]
    max_concurrent: usize,
    ca_file: Option<PathBuf>,
}

fn default_http_timeout_ms() -> u64 {
    2000
}

fn default_http_max_response_bytes() -> usize {
    1048576
}

fn default_http_max_concurrent() -> usize {
    64
}

fn default_max_memory_mb() -> u32 {
    256
}

fn default_max_execution_ms() -> u64 {
    1000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    policy: Option<AuthPolicy>,
    username: Option<RawUsername>,
    #[serde(default)]
    handlers: Vec<RawAuthHandler>,
    #[serde(default)]
    chains: BTreeMap<String, RawChain>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChain {
    #[serde(default)]
    policy: AuthPolicy,
    username: Option<RawUsername>,
    #[serde(default)]
    handlers: Vec<RawAuthHandler>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUsername {
    strip_realm: Option<String>,
    #[serde(default)]
    lowercase: bool,
    allowed: Option<String>,
}

// Only read at startup and on reload, so size does not matter here.
#[allow(clippy::large_enum_variant)]
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawAuthHandler {
    Sql {
        user_query: String,
        certificate_query: Option<String>,
        reply_query: Option<String>,
        default_user: Option<String>,
        #[serde(default = "default_sql_timeout_ms")]
        timeout_ms: u64,
    },
    Lua {
        script: PathBuf,
        #[serde(default)]
        verifies_password: bool,
    },
    Ldap(RawLdap),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLdap {
    urls: Vec<String>,
    bind_dn: String,
    bind_password: Option<String>,
    bind_password_file: Option<PathBuf>,
    base_dn: String,
    filter: String,
    certificate_filter: Option<String>,
    #[serde(default)]
    password_check: RawPasswordCheck,
    password_attribute: Option<String>,
    #[serde(default)]
    reply_attributes: OrderedPairs,
    #[serde(default = "default_ldap_timeout_ms")]
    timeout_ms: u64,
    ca_file: Option<PathBuf>,
    #[serde(default)]
    allow_plaintext: bool,
    #[serde(default = "default_ldap_pool_size")]
    pool_size: usize,
    required_groups: Option<Vec<String>>,
    group_attribute: Option<String>,
    group_base_dn: Option<String>,
    group_filter: Option<String>,
    #[serde(default)]
    group_reply: Vec<RawGroupReply>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroupReply {
    group: String,
    #[serde(default)]
    attributes: OrderedPairs,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum RawPasswordCheck {
    #[default]
    Bind,
    Attribute,
}

fn default_sql_timeout_ms() -> u64 {
    5000
}

fn default_ldap_timeout_ms() -> u64 {
    5000
}

fn default_ldap_pool_size() -> usize {
    4
}

/// A string-to-string table that keeps the order its keys were written in.
#[derive(Default)]
struct OrderedPairs(Vec<(String, String)>);

impl<'de> Deserialize<'de> for OrderedPairs {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OrderedPairs;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a table of strings")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<OrderedPairs, M::Error> {
                let mut pairs = Vec::new();
                while let Some(pair) = map.next_entry()? {
                    pairs.push(pair);
                }
                Ok(OrderedPairs(pairs))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCluster {
    #[serde(default = "default_node_id")]
    node_id: u64,
    data_dir: PathBuf,
    secret_raft: Option<String>,
    secret_raft_file: Option<PathBuf>,
    secret_api: Option<String>,
    secret_api_file: Option<PathBuf>,
    listen: Option<IpAddr>,
    /// Moved to `[lua].query_timeout_ms`; only read to report the new place.
    query_timeout_ms: Option<toml::Value>,
    nodes: Option<Vec<RawClusterNode>>,
    migrations_dir: Option<PathBuf>,
    #[serde(default = "default_migrations_wait_ms")]
    migrations_wait_ms: u64,
    #[serde(default = "default_snapshot_every")]
    snapshot_every: u64,
    #[serde(default = "default_read_connections")]
    read_connections: usize,
    stop_without_leader_ms: Option<u64>,
}

fn default_snapshot_every() -> u64 {
    DEFAULT_SNAPSHOT_EVERY
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClusterNode {
    id: u64,
    raft: String,
    api: String,
}

fn default_node_id() -> u64 {
    1
}

fn default_query_timeout_ms() -> u64 {
    5000
}

/// Minimum length hiqlite accepts for `secret_raft` and `secret_api`.
const MIN_CLUSTER_SECRET_LEN: usize = 16;

pub fn any_address() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRadius {
    /// One address or a list of them; `radius_binds` reads it, to say what is wrong in its own words.
    bind: Option<toml::Value>,
    #[serde(default = "default_auth_port")]
    auth_port: u16,
    #[serde(default = "default_acct_port")]
    acct_port: u16,
    #[serde(default = "default_max_inflight")]
    max_auth_inflight: usize,
    #[serde(default = "default_max_inflight")]
    max_acct_inflight: usize,
    #[serde(default)]
    require_message_authenticator: MessageAuthenticatorPolicy,
    #[serde(default)]
    limit_proxy_state: MessageAuthenticatorPolicy,
    #[serde(default = "default_status_server")]
    status_server: bool,
    sockets: Option<usize>,
    #[serde(default = "default_coa_timeout_ms")]
    coa_timeout_ms: u64,
    #[serde(default = "default_coa_attempts")]
    coa_attempts: u32,
    /// `[radius.tls]`, which an extension reads.
    tls: Option<toml::Value>,
    accounting_spool: Option<RawSpool>,
    eap: Option<RawEap>,
    /// `[radius.coa]`, which an extension reads.
    coa: Option<toml::Value>,
    /// `[radius.dynamic_clients]`, which an extension reads.
    dynamic_clients: Option<toml::Value>,
    /// `[radius.dpsk]`, which an extension reads.
    dpsk: Option<toml::Value>,
    #[serde(default)]
    clients: Vec<RawClient>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEap {
    /// `[radius.eap.aka]`, which an extension reads.
    aka: Option<toml::Value>,
    cert_file: Option<PathBuf>,
    key_file: Option<PathBuf>,
    client_ca_file: Option<PathBuf>,
    crl_file: Option<PathBuf>,
    #[serde(default = "default_eap_methods")]
    methods: Vec<String>,
    #[serde(default = "default_eap_fragment_size")]
    fragment_size: usize,
    #[serde(default = "default_eap_session_timeout_ms")]
    session_timeout_ms: u64,
    #[serde(default = "default_eap_max_sessions")]
    max_sessions: usize,
    #[serde(default = "default_eap_max_sessions_per_client")]
    max_sessions_per_client: usize,
    #[serde(default = "default_eap_max_sessions_per_device")]
    max_sessions_per_device: usize,
    #[serde(default = "default_true")]
    match_identity: bool,
    /// How long an EAP-TLS or PEAP session can be resumed; read, with the three settings after
    /// it, by the extension that resumes sessions.
    resumption_ms: Option<u64>,
    /// Session tickets under keys the cluster rotates, which any node resumes, TLS 1.3 included.
    shared_tickets: Option<bool>,
    /// Seals the rotating keys in the database: every node needs the same one.
    ticket_seal_file: Option<PathBuf>,
    /// The file before `ticket_seal_file`, while changing it: opens keys, seals none.
    previous_ticket_seal_file: Option<PathBuf>,
    /// Read by the extension that checks revocation, as is `ocsp_url`.
    ocsp: Option<toml::Value>,
    ocsp_url: Option<toml::Value>,
}

pub fn default_true() -> bool {
    true
}

fn default_eap_methods() -> Vec<String> {
    vec!["tls".to_string()]
}

fn default_eap_fragment_size() -> usize {
    1000
}

fn default_eap_session_timeout_ms() -> u64 {
    30_000
}

fn default_eap_max_sessions_per_client() -> usize {
    1024
}

fn default_eap_max_sessions_per_device() -> usize {
    4
}

fn default_eap_max_sessions() -> usize {
    4096
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSpool {
    dir: PathBuf,
    #[serde(default = "default_spool_max_bytes")]
    max_bytes: u64,
    #[serde(default = "default_spool_replay_interval_ms")]
    replay_interval_ms: u64,
    replay_window: Option<usize>,
}

fn default_spool_max_bytes() -> u64 {
    1 << 30
}

fn default_spool_replay_interval_ms() -> u64 {
    5000
}

pub fn default_auth_port() -> u16 {
    1812
}

pub fn default_acct_port() -> u16 {
    1813
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTacacs {
    #[serde(default = "any_address")]
    bind: IpAddr,
    #[serde(default = "default_tacacs_port")]
    port: u16,
    #[serde(default = "default_max_body_len")]
    max_body_len: usize,
    #[serde(default = "default_max_connections")]
    max_connections: usize,
    #[serde(default = "default_tacacs_idle_timeout_ms")]
    idle_timeout_ms: u64,
    #[serde(default)]
    clients: Vec<RawClient>,
    /// `"deny"` or `"login"`: what enable requests get.
    enable: Option<String>,
    /// Group name to its rules, each a string such as `permit service=shell cmd\* {priv-lvl=15}`.
    #[serde(default)]
    authorize_groups: BTreeMap<String, Vec<String>>,
    /// `[tacacs.tls]`, which the extension that serves TACACS+ over TLS reads.
    tls: Option<toml::Value>,
}

/// `ocsp` and `ocsp_url` of `section`: `None` when off. The CAs of `client_ca_file` are where it
/// finds the issuer of a certificate whose chain does not hold it.
/// The revocation check of `section` from its `ocsp` and `ocsp_url`, which the extension that
/// reads them makes, given the CAs of `client_ca_file`. Without one, they cannot be used.
pub fn revocation_check(section: &str, ocsp: Option<toml::Value>, ocsp_url: Option<toml::Value>, client_ca_file: &Path, extensions: &crate::extension::Extensions) -> Result<Option<crate::revocation::Revocation>, String> {
    let found: Vec<(&'static str, toml::Value)> = [("ocsp", ocsp), ("ocsp_url", ocsp_url)].into_iter().filter_map(|(key, value)| value.map(|value| (key, value))).collect();
    let Some((first, _)) = found.first() else {
        return Ok(None);
    };
    let Some(owner) = extensions.revocation_owner(first) else {
        return Err(format!("{section}.{first}: not supported by this program (OCSP needs rradius-enterprise)"));
    };
    let cas = crate::common::tls::read_certificates("client_ca_file", client_ca_file).map_err(|e| format!("{section}.{e}"))?;
    owner.revocation(section, found, cas)
}

fn default_tacacs_idle_timeout_ms() -> u64 {
    crate::tacacs::DEFAULT_IDLE_TIMEOUT_MS
}

fn default_tacacs_port() -> u16 {
    49
}

fn default_max_inflight() -> usize {
    crate::radius::DEFAULT_MAX_INFLIGHT
}

pub fn default_max_connections() -> usize {
    crate::tacacs::DEFAULT_MAX_CONNECTIONS
}

fn default_max_body_len() -> usize {
    crate::tacacs::MAX_BODY_LEN
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawClient {
    name: Option<String>,
    address: Option<String>,
    /// RADIUS only: the name the client's RadSec certificate has to be valid for.
    tls_name: Option<String>,
    secret: Option<String>,
    secret_file: Option<PathBuf>,
    /// Name of the `[auth.chains]` entry that authenticates this client.
    auth: Option<String>,
    /// RADIUS only: override the values in `[radius]`.
    require_message_authenticator: Option<MessageAuthenticatorPolicy>,
    limit_proxy_state: Option<MessageAuthenticatorPolicy>,
    coa_port: Option<u16>,
    eap_methods: Option<Vec<String>>,
    /// RADIUS only: the most RadSec connections this client may have open at once.
    max_radsec_connections: Option<usize>,
    /// TACACS+ only: the `[tacacs.authorize_groups]` entry that decides its authorization.
    authorize_group: Option<String>,
    /// RADIUS only: may send CoA on its RadSec connection, for the CoA relay.
    coa_sender: Option<bool>,
}

/// Loads and validates the TOML configuration at `path`. Errors name the file
/// and the offending setting.
pub fn load(path: &Path) -> Result<ServerConfig, ConfigError> {
    load_with(path, crate::extension::installed())
}

/// Like `load`, with `extensions` reading their sections.
pub fn load_with(path: &Path, extensions: &crate::extension::Extensions) -> Result<ServerConfig, ConfigError> {
    let fail = |msg: String| ConfigError(format!("{}: {}", path.display(), msg));
    let text = fs::read_to_string(path).map_err(|e| fail(format!("cannot read configuration: {}", e)))?;
    let mut raw: RawConfig = toml::from_str(&text).map_err(|e| fail(syntax_error(&text, &e)))?;
    let base = path.parent().unwrap_or(Path::new(""));

    // Settings that moved: say where they went before any other check.
    if raw.script.is_some() {
        return Err(fail("`script` moved to `[lua].script`".to_string()));
    }
    if raw.cluster.as_ref().is_some_and(|c| c.query_timeout_ms.is_some()) {
        return Err(fail("`cluster.query_timeout_ms` moved to `[lua].query_timeout_ms`".to_string()));
    }

    // The sections the program's extensions read, which the core only holds a place for. Read
    // first, as a mistake in `[radius.tls]` was found when it was the core's. Of `[radius.tls]`
    // the core only asks whether it is there.
    let radsec_section = raw.radius.as_mut().and_then(|radius| radius.tls.take());
    let radsec = radsec_section.is_some();
    let dynamic_clients = raw.radius.as_mut().and_then(|radius| radius.dynamic_clients.take());
    let client_statements = raw.accounts.as_mut().and_then(|accounts| accounts.clients.take());
    let coa_relay = raw.radius.as_mut().and_then(|radius| radius.coa.take());
    let dpsk = raw.radius.as_mut().and_then(|radius| radius.dpsk.take());
    let dpsk_statements = raw.accounts.as_mut().and_then(|accounts| accounts.dpsk.take());
    let aka = raw.radius.as_mut().and_then(|radius| radius.eap.as_mut()).and_then(|eap| eap.aka.take());
    let tacacs_tls = raw.tacacs.as_mut().and_then(|tacacs| tacacs.tls.take());
    let found = [("radius.eap.aka", aka), ("accounts.aka", raw.accounts.as_mut().and_then(|accounts| accounts.aka.take())), ("radius.dpsk", dpsk), ("accounts.dpsk", dpsk_statements), ("radius.tls", radsec_section), ("radius.dynamic_clients", dynamic_clients), ("accounts.clients", client_statements), ("radius.coa", coa_relay), ("proxy", raw.proxy.take()), ("tacacs.tls", tacacs_tls)];
    let shared_text: Arc<str> = Arc::from(text.as_str());
    let found = found.into_iter().filter_map(|(section, value)| value.map(|value| crate::extension::Section::new(section, value, shared_text.clone()))).collect();
    let extension_settings = crate::extension::settings(extensions, found, base).map_err(fail)?;

    if raw.radius.is_none() && raw.tacacs.is_none() {
        return Err(fail("at least one of [radius] or [tacacs] must be configured".to_string()));
    }

    let cluster = match raw.cluster {
        Some(c) => cluster_settings(c, base).map_err(fail)?,
        None => return Err(fail("the [cluster] section is required".to_string())),
    };
    let lua = match raw.lua {
        Some(l) if l.query_timeout_ms == 0 => {
            return Err(fail("lua.query_timeout_ms must be greater than 0".to_string()));
        }
        Some(l) if l.max_execution_ms == 0 => {
            return Err(fail("lua.max_execution_ms must be greater than 0".to_string()));
        }
        Some(l) if l.max_memory_mb == 0 => {
            return Err(fail("lua.max_memory_mb must be greater than 0".to_string()));
        }
        Some(l) => LuaSettings {
            max_memory: l.max_memory_mb as usize * (1 << 20),
            script: base.join(l.script),
            query_timeout: Duration::from_millis(l.query_timeout_ms),
            max_execution: Duration::from_millis(l.max_execution_ms),
            http: l.http.map(|http| script_http_settings(http, base)).transpose().map_err(fail)?,
        },
        None => return Err(fail("the [lua] section is required".to_string())),
    };
    let (raw_log, raw_trace) = match raw.log {
        Some(mut log) => {
            let trace = log.trace.take();
            (Some(log), trace)
        }
        None => (None, None),
    };
    let log = raw_log.map(log_settings).transpose().map_err(fail)?.unwrap_or_default();
    let trace = raw_trace.map(trace_settings).transpose().map_err(fail)?;
    // Chains first, so client `auth` references can be checked against them.
    let auth = match raw.auth {
        Some(a) => Some(auth_settings(a, base).map_err(fail)?),
        None => None,
    };
    let chains = auth.as_ref().map(|a| &a.chains);
    // Whether this program has RadSec, which client settings only RadSec uses need.
    let radsec_program = extensions.owner("radius.tls").is_some();
    if let Some(r) = raw.radius.as_ref().filter(|_| !radsec) {
        if let Some(client) = r.clients.iter().find(|client| client.address.is_none() && client.tls_name.is_some()) {
            let name = client.name.as_ref().or(client.tls_name.as_ref()).map_or("", String::as_str);
            let needs = if radsec_program { "[radius.tls]" } else { "rradius-enterprise" };
            return Err(fail(format!("radius client '{name}' has no address, so it can only use RadSec, which needs {needs}")));
        }
    }
    let extension_methods = extensions.eap_method_names();
    let binds = raw.radius.as_ref().map(|radius| radius_binds(radius.bind.as_ref())).transpose().map_err(fail)?.unwrap_or_default();
    let radius = match raw.radius {
        Some(r) => Some(RadiusSettings {
            auth_addr: SocketAddr::new(binds[0], r.auth_port),
            acct_addr: SocketAddr::new(binds[0], r.acct_port),
            more_binds: binds[1..].to_vec(),
            sockets: radius_sockets(r.sockets).map_err(fail)?,
            clients: client_table("radius", r.clients, base, chains, Some((r.require_message_authenticator, r.limit_proxy_state)), None, &extension_methods).map_err(fail)?,
            status_server: r.status_server,
            max_auth_inflight: above_zero("radius.max_auth_inflight", r.max_auth_inflight).map_err(fail)?,
            max_acct_inflight: above_zero("radius.max_acct_inflight", r.max_acct_inflight).map_err(fail)?,
            coa_timeout: Duration::from_millis(above_zero("radius.coa_timeout_ms", r.coa_timeout_ms as usize).map_err(fail)? as u64),
            coa_attempts: above_zero("radius.coa_attempts", r.coa_attempts as usize).map_err(fail)? as u32,
            accounting_spool: r.accounting_spool.map(|spool| spool_settings(spool, base, "radius.accounting_spool")).transpose().map_err(fail)?,
            eap: r.eap.map(|eap| eap_settings(eap, base, extensions).map(Arc::new)).transpose().map_err(fail)?,
            client_policies: (r.require_message_authenticator, r.limit_proxy_state),
        }),
        None => None,
    };
    if let Some(radius) = &radius {
        if !radsec_program {
            if let Some(client) = radius.clients.clients().find(|client| client.max_radsec_connections.is_some()) {
                return Err(fail(format!("radius client '{}': max_radsec_connections applies only to RadSec, which needs rradius-enterprise", client.name)));
            }
            if let Some(client) = radius.clients.clients().find(|client| client.coa_sender) {
                return Err(fail(format!("radius client '{}': coa_sender needs RadSec, which needs rradius-enterprise", client.name)));
            }
        }
        client_eap_methods(radius).map_err(fail)?;
    }
    if auth.is_none() && radius.as_ref().and_then(|radius: &RadiusSettings| radius.eap.as_ref()).is_some_and(|eap| eap.methods.iter().any(|method| matches!(*method, crate::eap::TYPE_PEAP | crate::eap::TYPE_TTLS))) {
        return Err(fail("radius.eap.methods has a method whose password only a chain can check, and there is no [auth] section".to_string()));
    }
    let tacacs = match raw.tacacs {
        Some(t) => Some(TacacsSettings {
            enable: match &t.enable {
                None => crate::tacacs::EnableMode::default(),
                Some(value) => crate::tacacs::EnableMode::parse(value).ok_or_else(|| fail(format!("tacacs.enable must be \"deny\" or \"login\", not \"{value}\"")))?,
            },
            addr: SocketAddr::new(t.bind, t.port),
            max_body_len: t.max_body_len,
            clients: client_table("tacacs", t.clients, base, chains, None, Some(&authorize_groups(t.authorize_groups).map_err(fail)?), &[]).map_err(fail)?,
            max_connections: above_zero("tacacs.max_connections", t.max_connections).map_err(fail)?,
            idle_timeout: Duration::from_millis(above_zero("tacacs.idle_timeout_ms", t.idle_timeout_ms as usize).map_err(fail)? as u64),
        }),
        None => None,
    };
    if let Some(tacacs) = &tacacs {
        // The extension that serves TACACS+ over TLS reads only `[tacacs.tls]`, so the port it
        // shares with `[tacacs]` is checked here.
        let tls_served = extension_settings.get::<crate::tacacs_transport::TacacsTlsServed>();
        if let Some(served) = tls_served.filter(|served| served.port == tacacs.addr.port()) {
            return Err(fail(format!("tacacs.tls.port {} is the port of TACACS+ without TLS; RFC 9887 needs another", served.port)));
        }
        if tls_served.is_none() {
            if let Some(client) = tacacs.clients.clients().find(|client| client.tls_name.is_some()) {
                return Err(fail(format!("tacacs client '{}': tls_name needs [tacacs.tls]", client.name)));
            }
        }
    }

    if let Some(trace) = &trace {
        let radius = radius.as_ref().ok_or_else(|| fail("log.trace needs [radius]: it only covers RADIUS".to_string()))?;
        if let Some(name) = trace.clients.iter().find(|name| !radius.clients.names().any(|client| client == name.as_str())) {
            return Err(fail(format!("log.trace.clients: '{name}' is not the name of a RADIUS client")));
        }
    }
    let mut config = ServerConfig {
        dictionary: base.join(raw.dictionary),
        lua,
        cluster,
        auth,
        radius,
        tacacs,
        metrics: raw.metrics.map(|m| SocketAddr::new(m.bind, m.port)),
        log,
        trace,
        accounts: raw.accounts.map(account_settings).transpose().map_err(fail)?,
        runtime: match raw.runtime {
            Some(runtime) if runtime.pin_threads && !cfg!(target_os = "linux") => {
                return Err(fail("runtime.pin_threads is only supported on Linux".to_string()));
            }
            Some(runtime) => RuntimeSettings { pin_threads: runtime.pin_threads },
            None => RuntimeSettings::default(),
        },
        extensions: extension_settings,
    };
    // What the program's extensions need of the rest of the configuration.
    extensions.check(&config).map_err(fail)?;
    // The EAP methods of the extensions, made from it, go with the EAP settings.
    let methods = extensions.eap_methods(&config);
    if !methods.is_empty() {
        let eap = config.radius.as_mut().and_then(|radius| radius.eap.as_mut()).and_then(Arc::get_mut).ok_or_else(|| fail("EAP methods without EAP settings to hold them".to_string()))?;
        eap.extra = methods;
    }
    Ok(config)
}

/// Checks `[accounts]`: numbered parameters only, `?2` exactly where a password goes.
fn account_settings(raw: RawAccounts) -> Result<AccountSettings, String> {
    let scheme = match raw.scheme.as_deref() {
        None | Some("bcrypt") => PasswordScheme::Bcrypt,
        Some("sha512-crypt") => PasswordScheme::Sha512Crypt,
        Some("nthash") => PasswordScheme::Nthash,
        Some("clear") => PasswordScheme::Clear,
        Some(other) => return Err(format!("accounts.scheme: '{other}' is not one of bcrypt, sha512-crypt, nthash, clear")),
    };
    let check = |name: &str, sql: Option<String>, takes_password: bool| -> Result<Option<String>, String> {
        let Some(sql) = sql else { return Ok(None) };
        let placeholders = crate::lua_engine::sql_placeholders(&sql).map_err(|e| format!("accounts.{name}: {e}"))?;
        if placeholders.iter().any(|placeholder| placeholder.number.is_none()) {
            return Err(format!("accounts.{name}: use ?1 for the account name{}, not ?", if takes_password { " and ?2 for the password" } else { "" }));
        }
        let highest = placeholders.iter().filter_map(|placeholder| placeholder.number).max().unwrap_or(0);
        let allowed = if takes_password { 2 } else { 1 };
        if highest > allowed || placeholders.iter().any(|placeholder| placeholder.number == Some(0)) {
            return Err(format!("accounts.{name}: only ?1 (the account name){} can be used", if takes_password { " and ?2 (the password)" } else { "" }));
        }
        // Without it the statement is not about one account: an UPDATE would change them all.
        if !placeholders.iter().any(|placeholder| placeholder.number == Some(1)) {
            return Err(format!("accounts.{name}: must pick the account with ?1"));
        }
        if takes_password && highest < 2 {
            return Err(format!("accounts.{name}: must store the password with ?2"));
        }
        Ok(Some(sql))
    };
    Ok(AccountSettings {
        add: check("add", raw.add, true)?,
        passwd: check("passwd", raw.passwd, true)?,
        enable: check("enable", raw.enable, false)?,
        disable: check("disable", raw.disable, false)?,
        delete: check("delete", raw.delete, false)?,
        show: check("show", raw.show, false)?,
        scheme,
    })
}

/// How a statement of `[accounts.<sub-section>]` may use one numbered parameter.
#[derive(Clone, Copy)]
pub enum Use {
    Required(&'static str),
    Allowed(&'static str),
}

impl Use {
    fn what(self) -> &'static str {
        match self {
            Self::Required(what) | Self::Allowed(what) => what,
        }
    }
}

/// Checks a statement of a sub-section of `[accounts]`: numbered parameters only, `?N` for
/// the N-th of `uses`, each `Required` one used, none beyond them. `?1` is always required, so
/// that a statement is about one row.
pub fn statement(name: &str, sql: Option<String>, uses: &[Use]) -> Result<Option<String>, String> {
    let Some(sql) = sql else { return Ok(None) };
    let listed = uses.iter().enumerate().map(|(i, used)| format!("?{} ({})", i + 1, used.what())).collect::<Vec<_>>().join(", ");
    let placeholders = crate::lua_engine::sql_placeholders(&sql).map_err(|e| format!("accounts.{name}: {e}"))?;
    let mut numbers = Vec::with_capacity(placeholders.len());
    for placeholder in &placeholders {
        match placeholder.number {
            Some(number) if number >= 1 && (number as usize) <= uses.len() => numbers.push(number as usize),
            Some(number) => return Err(format!("accounts.{name}: ?{number} cannot be used; only {listed}")),
            None => return Err(format!("accounts.{name}: use numbered parameters, {listed}, not ?")),
        }
    }
    for (i, used) in uses.iter().enumerate() {
        if let Use::Required(what) = used {
            if !numbers.contains(&(i + 1)) {
                return Err(format!("accounts.{name}: must use ?{} for {what}", i + 1));
            }
        }
    }
    Ok(Some(sql))
}

/// What serde says of a value it cannot use, without the value: a secret written without
/// quotes, or on the wrong line, is one. The last `, expected` is serde's, since the value
/// comes before it and can hold those words too.
fn without_the_value(message: &str) -> String {
    let expected = message.rfind(", expected").map_or("", |at| &message[at..]);
    if message.starts_with("unknown variant ") {
        return format!("unknown variant{expected}");
    }
    for prefix in ["invalid type: ", "invalid value: "] {
        if let Some(rest) = message.strip_prefix(prefix) {
            let kind = rest.split([' ', ',']).next().unwrap_or_default();
            return format!("{prefix}{kind}{expected}");
        }
    }
    message.to_string()
}

/// Where the file is wrong and why, in one line. The error's own display is not used
/// because it quotes the line, which can hold a secret.
pub(crate) fn syntax_error(text: &str, error: &toml::de::Error) -> String {
    let message = without_the_value(&error.message().replace('\n', " "));
    let Some(span) = error.span() else {
        return message;
    };
    let before = &text.as_bytes()[..span.start.min(text.len())];
    let line = before.iter().filter(|byte| **byte == b'\n').count();
    let is_key = |key: &&str| !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    let key = text.lines().nth(line).and_then(|text| text.split_once('=')).map(|(key, _)| key.trim()).filter(is_key);
    match key {
        Some(key) => format!("line {}: `{key}`: {message}", line + 1),
        None => format!("line {}: {message}", line + 1),
    }
}

pub fn above_zero(name: &str, value: usize) -> Result<usize, String> {
    if value == 0 {
        return Err(format!("{} must be greater than 0", name));
    }
    Ok(value)
}

fn auth_settings(raw: RawAuth, base: &Path) -> Result<AuthSettings, String> {
    if raw.handlers.is_empty() && raw.username.is_some() {
        return Err("auth.username is set but the default chain has no [[auth.handlers]] entries".to_string());
    }
    if raw.handlers.is_empty() && raw.chains.is_empty() {
        return Err(
            "[auth] needs [[auth.handlers]] entries or at least one [auth.chains.<name>] (auth.handlers is empty)"
                .to_string(),
        );
    }
    if raw.handlers.is_empty() && raw.policy.is_some() {
        return Err("auth.policy is set but the default chain has no [[auth.handlers]] entries".to_string());
    }
    let default = if raw.handlers.is_empty() {
        None
    } else {
        let handlers = chain_handlers("auth.handlers", raw.handlers, base)?;
        let username = raw.username.map(|username| username_settings("auth.username", username)).transpose()?;
        Some(ChainSettings { policy: raw.policy.unwrap_or_default(), handlers, username })
    };
    let mut chains = BTreeMap::new();
    for (name, chain) in raw.chains {
        let prefix = format!("auth.chains.{}.handlers", name);
        if chain.handlers.is_empty() {
            return Err(format!("{} is empty; a chain needs at least one handler", prefix));
        }
        let handlers = chain_handlers(&prefix, chain.handlers, base)?;
        let username = chain.username.map(|username| username_settings(&format!("auth.chains.{}.username", name), username)).transpose()?;
        chains.insert(name, ChainSettings { policy: chain.policy, handlers, username });
    }
    Ok(AuthSettings { default, chains })
}

fn script_http_settings(raw: RawScriptHttp, base: &Path) -> Result<ScriptHttpSettings, String> {
    if raw.allowed_hosts.is_empty() {
        return Err("lua.http.allowed_hosts must list at least one host".to_string());
    }
    let allowed_hosts = raw.allowed_hosts.iter().map(|host| AllowedHost::parse(host)).collect::<Result<Vec<_>, _>>().map_err(|e| format!("lua.http.allowed_hosts: {e}"))?;
    if raw.timeout_ms == 0 {
        return Err("lua.http.timeout_ms must be greater than 0".to_string());
    }
    if raw.max_response_bytes == 0 {
        return Err("lua.http.max_response_bytes must be greater than 0".to_string());
    }
    if raw.max_concurrent == 0 {
        return Err("lua.http.max_concurrent must be greater than 0".to_string());
    }
    let ca_file = raw.ca_file.map(|file| base.join(file));
    if let Some(path) = &ca_file {
        fs::File::open(path).map_err(|e| format!("lua.http.ca_file: cannot read '{}': {}", path.display(), e))?;
    }
    Ok(ScriptHttpSettings { allowed_hosts, timeout: Duration::from_millis(raw.timeout_ms), max_response_bytes: raw.max_response_bytes, max_concurrent: raw.max_concurrent, ca_file })
}

/// Checks each client's `eap_methods` against what the server offers. With several methods each
/// client has to say which it wants: adding a method for one must not offer it to all, since a peer
/// can then switch to it with a Legacy Nak, from a certificate to a password for instance.
fn client_eap_methods(radius: &RadiusSettings) -> Result<(), String> {
    for client in radius.clients.clients() {
        match (&radius.eap, &client.eap_methods) {
            (None, Some(_)) => return Err(format!("radius client '{}': eap_methods needs [radius.eap]", client.name)),
            (Some(eap), Some(methods)) if methods.iter().any(|method| !eap.methods.contains(method)) => {
                return Err(format!("radius client '{}': eap_methods has a method that radius.eap.methods does not", client.name));
            }
            (Some(eap), None) if eap.methods.len() > 1 => {
                return Err(format!("radius client '{}': eap_methods is required when radius.eap.methods has more than one method; [] for a client without EAP", client.name));
            }
            _ => {}
        }
    }
    Ok(())
}

/// The EAP types of method names, which may not repeat.
fn eap_method_types(names: &[String], extension_methods: &[crate::eap_method::EapMethodName]) -> Result<Vec<u8>, String> {
    let mut methods = Vec::new();
    for method in names {
        let kind = match method.as_str() {
            "tls" => crate::eap::TYPE_TLS,
            "peap" => crate::eap::TYPE_PEAP,
            "ttls" => crate::eap::TYPE_TTLS,
            other => match (extension_methods.iter().find(|names| names.name == other), crate::extension::eap_method_needs_enterprise(other)) {
                (Some(names), _) => names.kind,
                (None, Some(hint)) => return Err(format!("{other} is not supported by this program ({hint})")),
                (None, None) => {
                    let offered: Vec<&str> = ["tls", "peap", "ttls"].into_iter().chain(extension_methods.iter().map(|names| names.name)).collect();
                    let (last, rest) = offered.split_last().map_or(("", &[][..]), |(last, rest)| (*last, rest));
                    return Err(format!("'{other}' is not a method this server offers; it has {} and {last}", rest.join(", ")));
                }
            },
        };
        if methods.contains(&kind) {
            return Err(format!("names '{method}' twice"));
        }
        methods.push(kind);
    }
    Ok(methods)
}

fn eap_settings(raw: RawEap, base: &Path, extensions: &crate::extension::Extensions) -> Result<EapSettings, String> {
    if raw.methods.is_empty() {
        return Err("radius.eap.methods must name at least one method".to_string());
    }
    let methods = eap_method_types(&raw.methods, &extensions.eap_method_names()).map_err(|e| format!("radius.eap.methods: {e}"))?;
    if !(256..=1400).contains(&raw.fragment_size) {
        return Err(format!("radius.eap.fragment_size must be from 256 to 1400, not {}", raw.fragment_size));
    }
    if raw.session_timeout_ms == 0 {
        return Err("radius.eap.session_timeout_ms must be greater than 0".to_string());
    }
    above_zero("radius.eap.max_sessions", raw.max_sessions)?;
    above_zero("radius.eap.max_sessions_per_client", raw.max_sessions_per_client)?;
    above_zero("radius.eap.max_sessions_per_device", raw.max_sessions_per_device)?;
    let needed = |file: Option<PathBuf>, name: &str| file.map(|file| base.join(file)).ok_or_else(|| format!("radius.eap.{name} is required"));
    // EAP-TLS, PEAP and TTLS; the methods of the extensions have no TLS.
    let with_tls = methods.iter().any(|method| matches!(*method, crate::eap::TYPE_TLS | crate::eap::TYPE_PEAP | crate::eap::TYPE_TTLS));
    let (cert_file, key_file) = if with_tls {
        (needed(raw.cert_file, "cert_file")?, needed(raw.key_file, "key_file")?)
    } else {
        // Left over from a method with TLS, these would look like they did something.
        for (name, file) in [("cert_file", &raw.cert_file), ("key_file", &raw.key_file)] {
            if file.is_some() {
                return Err(format!("radius.eap.{name} only applies to a method with TLS, which methods does not have"));
            }
        }
        (PathBuf::new(), PathBuf::new())
    };
    let requested = crate::session_resumption::RequestedResumption {
        resumption_ms: raw.resumption_ms,
        shared_tickets: raw.shared_tickets,
        ticket_seal_file: raw.ticket_seal_file,
        previous_ticket_seal_file: raw.previous_ticket_seal_file,
    };
    let resumption = match requested.first_set() {
        None => None,
        Some(first) => {
            let context = crate::session_resumption::ResumptionContext { base, methods: &methods, max_sessions: raw.max_sessions };
            match extensions.session_resumption(&requested, &context) {
                Some(made) => made?,
                None => return Err(format!("radius.eap.{first}: not supported by this program (session resumption needs rradius-enterprise)")),
            }
        }
    };
    let mut revocation = None;
    let (tls, tls_verifier) = if methods.contains(&crate::eap::TYPE_TLS) {
        let client_ca_file = needed(raw.client_ca_file, "client_ca_file")?;
        revocation = revocation_check("radius.eap", raw.ocsp.clone(), raw.ocsp_url.clone(), &client_ca_file, extensions)?;
        let crl_file = raw.crl_file.map(|file| base.join(file));
        let (config, verifier) = crate::eap::tls_config_and_verifier(&cert_file, &key_file, &client_ca_file, crl_file.as_deref()).map_err(|e| format!("radius.eap.{e}"))?;
        let config = match &resumption {
            Some(resumption) => resumption.eap_tls(&config),
            None => config,
        };
        (Some(config), Some(verifier))
    } else {
        // Left over from EAP-TLS, these would look like certificates are still being checked.
        for (name, file) in [("client_ca_file", &raw.client_ca_file), ("crl_file", &raw.crl_file)] {
            if file.is_some() {
                return Err(format!("radius.eap.{name} only applies to tls, which methods does not have"));
            }
        }
        if raw.ocsp.is_some() || raw.ocsp_url.is_some() {
            return Err("radius.eap.ocsp only applies to tls, which methods does not have".to_string());
        }
        (None, None)
    };
    let tunnel_tls = if methods.iter().any(|method| matches!(*method, crate::eap::TYPE_PEAP | crate::eap::TYPE_TTLS)) {
        Some(crate::eap::tunnel_tls_config(&cert_file, &key_file).map_err(|e| format!("radius.eap.{e}"))?)
    } else {
        None
    };
    let peap_tls = match (methods.contains(&crate::eap::TYPE_PEAP), &resumption, &tunnel_tls) {
        (true, Some(resumption), Some(config)) => Some(resumption.peap(config)),
        _ => None,
    };
    Ok(EapSettings {
        extra: Vec::new(),
        tunnel_tls,
        peap_tls,
        resumption,
        revocation,
        methods,
        tls,
        tls_verifier,
        fragment_size: raw.fragment_size,
        session_timeout: Duration::from_millis(raw.session_timeout_ms),
        max_sessions: raw.max_sessions,
        max_sessions_per_client: raw.max_sessions_per_client,
        max_sessions_per_device: raw.max_sessions_per_device,
        match_identity: raw.match_identity,
    })
}

/// `section` names it in errors: `radius.accounting_spool` or `proxy.accounting_spool`.
pub fn spool_settings(raw: RawSpool, base: &Path, section: &str) -> Result<SpoolSettings, String> {
    if raw.max_bytes == 0 {
        return Err(format!("{section}.max_bytes must be greater than 0"));
    }
    if raw.replay_interval_ms == 0 {
        return Err(format!("{section}.replay_interval_ms must be greater than 0"));
    }
    // The local spool goes through scripts, which may expect a session's records in order.
    let replay_window = match (section, raw.replay_window) {
        ("proxy.accounting_spool", None) => DEFAULT_REPLAY_WINDOW,
        (_, None) => 1,
        (_, Some(window)) if (1..=MAX_REPLAY_WINDOW).contains(&window) => window,
        (_, Some(_)) => return Err(format!("{section}.replay_window must be from 1 to {MAX_REPLAY_WINDOW}")),
    };
    Ok(SpoolSettings { dir: base.join(raw.dir), max_bytes: raw.max_bytes, replay_interval: Duration::from_millis(raw.replay_interval_ms), replay_window })
}

fn log_settings(raw: RawLog) -> Result<LogSettings, String> {
    let level = match raw.level.as_deref() {
        None | Some("info") => LogLevel::Info,
        Some("error") => LogLevel::Error,
        Some("warn") => LogLevel::Warn,
        Some("debug") => LogLevel::Debug,
        Some(other) => return Err(format!("log.level: '{other}' is not error, warn, info or debug")),
    };
    let format = match raw.format.as_deref() {
        None | Some("text") => LogFormat::Text,
        Some("json") => LogFormat::Json,
        Some(other) => return Err(format!("log.format: '{other}' is not text or json")),
    };
    Ok(LogSettings { level, format, auth: raw.auth })
}

/// The addresses of `[radius] bind`, at least one.
fn radius_binds(value: Option<&toml::Value>) -> Result<Vec<IpAddr>, String> {
    let address = |value: &toml::Value| -> Result<IpAddr, String> {
        let text = value.as_str().ok_or_else(|| format!("radius.bind: {value} is not an address"))?;
        text.parse().map_err(|_| format!("radius.bind: '{text}' is not an address"))
    };
    let addresses = match value {
        None => vec![any_address()],
        Some(toml::Value::Array(values)) => values.iter().map(address).collect::<Result<Vec<_>, _>>()?,
        Some(value) => vec![address(value)?],
    };
    if addresses.is_empty() {
        return Err("radius.bind must list at least one address".to_string());
    }
    for (index, one) in addresses.iter().enumerate() {
        // Several means IPv6 only, which an IPv4-mapped address cannot be bound with.
        if addresses.len() > 1 && matches!(one, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some()) {
            return Err(format!("radius.bind lists {one}, an IPv4 address written as IPv6: list it as IPv4"));
        }
        if addresses[..index].contains(one) {
            return Err(format!("radius.bind lists {one} twice"));
        }
        // The wildcard already takes the port on every address of its family.
        if one.is_unspecified() && addresses.iter().any(|other| other != one && other.is_ipv4() == one.is_ipv4()) {
            return Err(format!("radius.bind lists {one}, which takes all of its family, beside another of them"));
        }
    }
    Ok(addresses)
}

fn trace_settings(raw: RawTrace) -> Result<TraceSettings, String> {
    let minutes = raw.minutes.ok_or("log.trace.minutes is required: tracing has to end by itself")?;
    if !(1..=1440).contains(&minutes) {
        return Err(format!("log.trace.minutes: {minutes} is not from 1 to 1440"));
    }
    if raw.users.is_empty() && raw.clients.is_empty() {
        return Err("log.trace lists no users and no clients".to_string());
    }
    Ok(TraceSettings { users: raw.users, clients: raw.clients, duration: Duration::from_secs(minutes * 60) })
}

/// Validates one chain's `username` table; errors start with `<prefix>.<field>`.
fn username_settings(prefix: &str, raw: RawUsername) -> Result<UsernameSettings, String> {
    let strip_realm = match raw.strip_realm.as_deref() {
        None | Some("none") => StripRealm::None,
        Some("suffix") => StripRealm::Suffix,
        Some("prefix") => StripRealm::Prefix,
        Some("both") => StripRealm::Both,
        Some(other) => return Err(format!("{prefix}.strip_realm: '{other}' is not none, suffix, prefix or both")),
    };
    let allowed = raw.allowed.map(|pattern| AllowedNames::new(&pattern).map_err(|e| format!("{prefix}.allowed: {e}"))).transpose()?;
    Ok(UsernameSettings { strip_realm, lowercase: raw.lowercase, allowed })
}

/// Validates one chain's handlers; errors start with `<prefix>[N].<field>`.
fn chain_handlers(prefix: &str, raw: Vec<RawAuthHandler>, base: &Path) -> Result<Vec<AuthHandlerSettings>, String> {
    let one_placeholder = |index: usize, name: &str, query: &str| -> Result<(), String> {
        let count = query.matches('?').count();
        if count == 1 {
            Ok(())
        } else {
            Err(format!("{}[{}].{} must contain exactly one ? placeholder, found {}", prefix, index, name, count))
        }
    };
    let mut handlers = Vec::with_capacity(raw.len());
    for (index, handler) in raw.into_iter().enumerate() {
        handlers.push(match handler {
            RawAuthHandler::Sql { user_query, certificate_query, reply_query, default_user, timeout_ms } => {
                one_placeholder(index, "user_query", &user_query)?;
                if let Some(certificate_query) = &certificate_query {
                    one_placeholder(index, "certificate_query", certificate_query)?;
                }
                if let Some(reply_query) = &reply_query {
                    one_placeholder(index, "reply_query", reply_query)?;
                }
                if timeout_ms == 0 {
                    return Err(format!("{}[{}].timeout_ms must be greater than 0", prefix, index));
                }
                let timeout = Duration::from_millis(timeout_ms);
                AuthHandlerSettings::Sql { user_query, certificate_query, reply_query, default_user, timeout }
            }
            RawAuthHandler::Lua { script, verifies_password } => AuthHandlerSettings::Lua { script: base.join(script), verifies_password },
            RawAuthHandler::Ldap(ldap) => AuthHandlerSettings::Ldap(
                ldap_settings(ldap, base).map_err(|e| format!("{}[{}].{}", prefix, index, e))?,
            ),
        });
    }
    Ok(handlers)
}

/// Validates an `ldap` handler; errors start with the offending field name.
fn ldap_settings(raw: RawLdap, base: &Path) -> Result<LdapSettings, String> {
    if raw.urls.is_empty() {
        return Err("urls must list at least one server".to_string());
    }
    if let Some(url) = raw.urls.iter().find(|u| !(u.starts_with("ldap://") || u.starts_with("ldaps://"))) {
        return Err(format!("urls: '{}' must start with ldap:// or ldaps://", url));
    }
    if let Some(certificate_filter) = &raw.certificate_filter {
        let placeholders = certificate_filter.matches("{user}").count();
        if placeholders != 1 {
            return Err(format!("certificate_filter must contain exactly one {{user}} placeholder, found {}", placeholders));
        }
    }
    let placeholders = raw.filter.matches("{user}").count();
    if placeholders != 1 {
        return Err(format!("filter must contain exactly one {{user}} placeholder, found {}", placeholders));
    }
    let password_check = match raw.password_check {
        RawPasswordCheck::Bind => PasswordCheck::Bind,
        RawPasswordCheck::Attribute => match raw.password_attribute {
            Some(attr) if !attr.is_empty() => PasswordCheck::Attribute(attr),
            _ => return Err("password_attribute is required when password_check = \"attribute\"".to_string()),
        },
    };
    let bind_password = match (raw.bind_password, raw.bind_password_file) {
        (Some(pw), None) => pw,
        (None, Some(file)) => {
            let path = base.join(file);
            let text = fs::read_to_string(&path).map_err(|e| format!("bind_password_file: cannot read '{}': {}", path.display(), e))?;
            text.trim_end_matches(['\r', '\n']).to_string()
        }
        _ => return Err("bind_password: exactly one of bind_password or bind_password_file must be set".to_string()),
    };
    if bind_password.is_empty() {
        return Err("bind_password must not be empty: an empty password makes an anonymous bind".to_string());
    }
    if raw.timeout_ms == 0 {
        return Err("timeout_ms must be greater than 0".to_string());
    }
    if raw.pool_size == 0 {
        return Err("pool_size must be greater than 0".to_string());
    }
    let ca_file = match raw.ca_file {
        Some(file) => {
            let path = base.join(file);
            fs::File::open(&path).map_err(|e| format!("ca_file: cannot read '{}': {}", path.display(), e))?;
            Some(path)
        }
        None => None,
    };
    Ok(LdapSettings {
        urls: raw.urls,
        bind_dn: raw.bind_dn,
        bind_password,
        base_dn: raw.base_dn,
        filter: raw.filter,
        certificate_filter: raw.certificate_filter,
        password_check,
        reply_attributes: raw.reply_attributes.0,
        timeout: Duration::from_millis(raw.timeout_ms),
        ca_file,
        allow_plaintext: raw.allow_plaintext,
        pool_size: raw.pool_size,
        groups: ldap_groups(raw.required_groups, raw.group_attribute, raw.group_base_dn, raw.group_filter, raw.group_reply)?,
    })
}

/// Validates an `ldap` handler's group settings; errors start with the offending field name.
fn ldap_groups(
    required: Option<Vec<String>>,
    attribute: Option<String>,
    base_dn: Option<String>,
    filter: Option<String>,
    reply: Vec<RawGroupReply>,
) -> Result<LdapGroups, String> {
    if required.as_ref().is_some_and(Vec::is_empty) {
        return Err("required_groups must list at least one group; leave it out to require none".to_string());
    }
    if required.is_none() && reply.is_empty() {
        let unused = [("group_attribute", attribute.is_some()), ("group_base_dn", base_dn.is_some()), ("group_filter", filter.is_some())];
        if let Some((field, _)) = unused.iter().find(|(_, set)| *set) {
            // Nothing would look the groups up, while a filter looks like a restriction.
            return Err(format!("{field} has no effect without required_groups or group_reply"));
        }
    }
    let source = match (base_dn, filter, attribute) {
        (Some(_), Some(_), Some(_)) => return Err("group_attribute cannot be set together with group_filter".to_string()),
        (Some(base_dn), Some(filter), None) => {
            if filter.matches("{dn}").count() != 1 {
                return Err("group_filter must contain exactly one {dn} placeholder".to_string());
            }
            GroupSource::Search { base_dn, filter }
        }
        (None, Some(_), _) => return Err("group_filter needs group_base_dn".to_string()),
        (Some(_), None, _) => return Err("group_base_dn needs group_filter".to_string()),
        (None, None, Some(attribute)) if attribute.is_empty() => return Err("group_attribute cannot be empty".to_string()),
        (None, None, Some(attribute)) => GroupSource::Attribute(attribute),
        (None, None, None) => GroupSource::default(),
    };
    let reply = reply
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            if entry.attributes.0.is_empty() {
                return Err(format!("group_reply[{index}].attributes must list at least one attribute"));
            }
            Ok((entry.group, entry.attributes.0))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(LdapGroups { required: required.unwrap_or_default(), source, reply })
}

fn cluster_settings(raw: RawCluster, base: &Path) -> Result<ClusterSettings, String> {
    if raw.node_id == 0 {
        return Err("cluster.node_id must be at least 1".to_string());
    }
    // A single node has nobody to reach it, and the database API has only a shared secret.
    let listen = raw.listen.unwrap_or(if raw.nodes.is_some() { any_address() } else { IpAddr::V4(Ipv4Addr::LOCALHOST) });
    let nodes: Vec<ClusterNode> = match raw.nodes {
        Some(nodes) => nodes.into_iter().map(|n| ClusterNode { id: n.id, raft: n.raft, api: n.api }).collect(),
        None => vec![ClusterNode { id: 1, raft: "127.0.0.1:8100".to_string(), api: "127.0.0.1:8200".to_string() }],
    };
    for (i, node) in nodes.iter().enumerate() {
        if node.id == 0 {
            return Err("cluster.nodes: node ids must be at least 1".to_string());
        }
        if nodes[..i].iter().any(|other| other.id == node.id) {
            return Err(format!("cluster.nodes: duplicate node id {}", node.id));
        }
        for (field, addr) in [("raft", &node.raft), ("api", &node.api)] {
            if !is_host_port(addr) {
                return Err(format!("cluster.nodes: node {} {} '{}' must be host:port", node.id, field, addr));
            }
        }
    }
    if !nodes.iter().any(|n| n.id == 1) {
        return Err("cluster.nodes: node 1 is required".to_string());
    }
    if !nodes.iter().any(|n| n.id == raw.node_id) {
        return Err(format!("cluster.node_id {} is not in cluster.nodes", raw.node_id));
    }
    let secret_raft = cluster_secret("secret_raft", raw.secret_raft, raw.secret_raft_file, base)?;
    let secret_api = cluster_secret("secret_api", raw.secret_api, raw.secret_api_file, base)?;
    if raw.migrations_wait_ms == 0 {
        return Err("cluster.migrations_wait_ms must be greater than 0".to_string());
    }
    if raw.snapshot_every == 0 {
        return Err("cluster.snapshot_every must be greater than 0".to_string());
    }
    if raw.stop_without_leader_ms == Some(0) {
        return Err("cluster.stop_without_leader_ms must be greater than 0".to_string());
    }
    if raw.read_connections == 0 || raw.read_connections > MAX_READ_CONNECTIONS {
        return Err(format!("cluster.read_connections must be from 1 to {MAX_READ_CONNECTIONS}"));
    }
    Ok(ClusterSettings {
        node_id: raw.node_id,
        data_dir: base.join(raw.data_dir),
        secret_raft,
        secret_api,
        listen,
        nodes,
        migrations_dir: raw.migrations_dir.map(|dir| base.join(dir)),
        migrations_wait: Duration::from_millis(raw.migrations_wait_ms),
        snapshot_every: raw.snapshot_every,
        read_connections: raw.read_connections,
        stop_without_leader: raw.stop_without_leader_ms.map(Duration::from_millis),
    })
}

fn default_status_server() -> bool {
    true
}

fn default_migrations_wait_ms() -> u64 {
    60000
}

fn is_host_port(addr: &str) -> bool {
    match addr.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

/// Reads a cluster secret from exactly one of `<name>` or `<name>_file` and
/// checks hiqlite's minimum length.
fn cluster_secret(name: &str, value: Option<String>, file: Option<PathBuf>, base: &Path) -> Result<String, String> {
    let secret = match (value, file) {
        (Some(value), None) => value,
        (None, Some(file)) => {
            let path = base.join(file);
            let text = fs::read_to_string(&path).map_err(|e| format!("cannot read cluster.{}_file '{}': {}", name, path.display(), e))?;
            text.trim_end_matches(['\r', '\n']).to_string()
        }
        _ => return Err(format!("exactly one of cluster.{} or cluster.{}_file must be set", name, name)),
    };
    if secret.chars().count() < MIN_CLUSTER_SECRET_LEN {
        return Err(format!("cluster.{} must be at least {} characters", name, MIN_CLUSTER_SECRET_LEN));
    }
    Ok(secret)
}

pub fn client_table(
    protocol: &str,
    clients: Vec<RawClient>,
    base: &Path,
    chains: Option<&BTreeMap<String, ChainSettings>>,
    // The `[radius]` security settings; `None` for a protocol that has none.
    security: Option<(MessageAuthenticatorPolicy, MessageAuthenticatorPolicy)>,
    // The TACACS+ authorization groups; `None` for a protocol that has none.
    authorize_groups: Option<&BTreeMap<String, Arc<AuthorizationRules>>>,
    // The EAP methods of the program's extensions, which `eap_methods` may name too.
    extension_methods: &[crate::eap_method::EapMethodName],
) -> Result<Arc<ClientTable>, String> {
    if clients.is_empty() {
        return Err(format!("[{}] is enabled but has no [[{}.clients]] entries", protocol, protocol));
    }
    let mut table = ClientTable::default();
    for client in clients {
        let label = client.name.clone().or_else(|| client.tls_name.clone()).or_else(|| client.address.clone()).unwrap_or_default();
        if client.address.is_none() && client.tls_name.is_none() {
            return Err(format!("{protocol} client '{label}': address is required, or tls_name for a client without a fixed address"));
        }
        // A TACACS+ client can have one with its address: its certificate has to be for that name.
        if security.is_none() && client.tls_name.is_some() && protocol != "tacacs" {
            return Err(format!("{protocol} client '{label}': tls_name applies only to RADIUS and TACACS+ clients"));
        }
        if protocol == "tacacs" && client.tls_name.is_some() && client.address.is_none() {
            return Err(format!("{protocol} client '{label}': tls_name needs an address on a TACACS+ client"));
        }
        if let Some(name) = &client.tls_name {
            if !matches!(rustls::pki_types::ServerName::try_from(name.as_str()), Ok(rustls::pki_types::ServerName::DnsName(_))) {
                return Err(format!("{protocol} client '{label}': tls_name '{name}' is not a DNS name"));
            }
        }
        let secret = client_secret(&client, base).map_err(|e| format!("{protocol} client '{label}': {e}"))?;
        if let Some(chain) = &client.auth {
            if !chains.is_some_and(|c| c.contains_key(chain)) {
                return Err(format!("{} client '{}': unknown auth chain '{}' (no [auth.chains.{}])", protocol, label, chain, chain));
            }
        }
        if client.coa_port == Some(0) {
            return Err(format!("{protocol} client '{label}': coa_port must be greater than 0"));
        }
        if security.is_none() && client.coa_port.is_some() {
            return Err(format!("{protocol} client '{label}': coa_port applies only to RADIUS clients"));
        }
        if security.is_none() {
            for (setting, value) in [("require_message_authenticator", client.require_message_authenticator), ("limit_proxy_state", client.limit_proxy_state)] {
                if value.is_some() {
                    return Err(format!("{protocol} client '{label}': {setting} applies only to RADIUS clients"));
                }
            }
        }
        if security.is_none() && client.max_radsec_connections.is_some() {
            return Err(format!("{protocol} client '{label}': max_radsec_connections applies only to RADIUS clients"));
        }
        if client.max_radsec_connections == Some(0) {
            return Err(format!("{protocol} client '{label}': max_radsec_connections must be greater than 0"));
        }
        if security.is_none() && client.coa_sender.is_some() {
            return Err(format!("{protocol} client '{label}': coa_sender applies only to RADIUS clients"));
        }
        if security.is_none() && client.eap_methods.is_some() {
            return Err(format!("{protocol} client '{label}': eap_methods applies only to RADIUS clients"));
        }
        let eap_methods = client.eap_methods.as_deref().map(|names| eap_method_types(names, extension_methods)).transpose().map_err(|e| format!("{protocol} client '{label}': eap_methods: {e}"))?;
        let authorize_group = match (&client.authorize_group, authorize_groups) {
            (None, _) => None,
            (Some(_), None) => return Err(format!("{protocol} client '{label}': authorize_group applies only to TACACS+ clients")),
            (Some(group), Some(groups)) => Some(
                groups
                    .get(group)
                    .cloned()
                    .ok_or_else(|| format!("{protocol} client '{label}': unknown authorize_group '{group}' (no tacacs.authorize_groups.{group})"))?,
            ),
        };
        let (require, limit) = security.unwrap_or_default();
        let options = ClientOptions {
            auth: client.auth,
            require_message_authenticator: client.require_message_authenticator.unwrap_or(require),
            limit_proxy_state: client.limit_proxy_state.unwrap_or(limit),
            coa_port: client.coa_port.unwrap_or(DEFAULT_COA_PORT),
            tls_name: client.tls_name,
            eap_methods,
            max_radsec_connections: client.max_radsec_connections,
            authorize_group,
            coa_sender: client.coa_sender.unwrap_or(false),
        };
        match &client.address {
            Some(address) => table.add(address, client.name, secret, options),
            None => table.add_without_address(client.name, secret, options),
        }
        .map_err(|e| format!("{protocol} clients: {e}"))?;
    }
    Ok(Arc::new(table))
}

/// Compiles `[tacacs.authorize_groups]`; an error names the group and the rule.
fn authorize_groups(raw: BTreeMap<String, Vec<String>>) -> Result<BTreeMap<String, Arc<AuthorizationRules>>, String> {
    raw.into_iter()
        .map(|(name, rules)| {
            if name.is_empty() {
                return Err("tacacs.authorize_groups: a group name cannot be empty".to_string());
            }
            let group = AuthorizationRules::parse(&rules).map_err(|e| format!("tacacs.authorize_groups.{name}: {e}"))?;
            Ok((name, Arc::new(group)))
        })
        .collect()
}

/// Reads the client's secret from exactly one of `secret` or `secret_file`,
/// stripping trailing CR/LF from file contents.
fn client_secret(client: &RawClient, base: &Path) -> Result<Vec<u8>, String> {
    let secret = match (&client.secret, &client.secret_file) {
        (Some(secret), None) => secret.as_bytes().to_vec(),
        (None, Some(file)) => {
            let path = base.join(file);
            let mut bytes = fs::read(&path).map_err(|e| format!("cannot read secret_file '{}': {}", path.display(), e))?;
            while matches!(bytes.last(), Some(b'\n') | Some(b'\r')) {
                bytes.pop();
            }
            if bytes.is_empty() {
                return Err(format!("secret_file '{}' is empty", path.display()));
            }
            bytes
        }
        _ => return Err("exactly one of secret or secret_file must be set".to_string()),
    };
    if secret.is_empty() {
        return Err("secret must not be empty".to_string());
    }
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const RADIUS_CLIENT: &str = "\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";

    /// Writes `body` as rradius.toml in a fresh directory and loads it.
    fn load_str(body: &str) -> (tempfile::TempDir, Result<ServerConfig, ConfigError>) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rradius.toml");
        fs::write(&path, body).unwrap();
        let result = load(&path);
        (dir, result)
    }

    const CLUSTER_SECRETS: &str = "secret_raft = \"raft-secret-0123456\"\nsecret_api = \"api-secret-01234567\"\n";

    /// dictionary + script, a minimal [cluster] unless `extra` has its own, then `extra`.
    fn base(extra: &str) -> String {
        let cluster = if extra.contains("[cluster]") { String::new() } else { format!("[cluster]\ndata_dir = \"data\"\n{}", CLUSTER_SECRETS) };
        format!("dictionary = \"dict/dictionary\"\n[lua]\nscript = \"auth.lua\"\n{}{}", cluster, extra)
    }

    fn error(body: &str) -> String {
        load_str(body).1.unwrap_err().to_string()
    }

    #[test]
    fn test_relative_paths_and_defaults() {
        let (dir, result) = load_str(&base(RADIUS_CLIENT));
        let config = result.unwrap();
        assert_eq!(config.dictionary, dir.path().join("dict/dictionary"));
        assert_eq!(config.lua.script, dir.path().join("auth.lua"));
        assert_eq!(config.lua.query_timeout, Duration::from_millis(5000));
        assert_eq!(config.lua.max_execution, Duration::from_millis(1000));
        let radius = config.radius.unwrap();
        assert_eq!(radius.auth_addr, "0.0.0.0:1812".parse().unwrap());
        assert_eq!(radius.acct_addr, "0.0.0.0:1813".parse().unwrap());
        assert!(radius.more_binds.is_empty());
        assert_eq!(config.cluster.data_dir, dir.path().join("data"));
        assert!(config.tacacs.is_none());
    }

    #[test]
    fn test_no_protocol_section() {
        assert!(error(&base("")).contains("at least one of [radius] or [tacacs]"));
    }

    #[test]
    fn test_unknown_field() {
        let err = error(&base("\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecert = \"x\"\n"));
        assert!(err.contains("secert"), "unexpected error: {err}");
        assert!(err.contains("rradius.toml"), "unexpected error: {err}");
    }

    #[test]
    fn test_secret_source_not_exactly_one() {
        let both = error(&base("\n[radius]\n[[radius.clients]]\naddress = \"10.0.0.9\"\nsecret = \"x\"\nsecret_file = \"s.secret\"\n"));
        assert!(both.contains("10.0.0.9") && both.contains("exactly one"), "unexpected error: {both}");
        let neither = error(&base("\n[radius]\n[[radius.clients]]\naddress = \"10.0.0.9\"\n"));
        assert!(neither.contains("10.0.0.9") && neither.contains("exactly one"), "unexpected error: {neither}");
    }

    #[test]
    fn test_secret_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("nas.secret"), "s3cret\n").unwrap();
        fs::write(dir.path().join("empty.secret"), "\n").unwrap();
        let path = dir.path().join("rradius.toml");

        fs::write(&path, base("\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret_file = \"nas.secret\"\n")).unwrap();
        let clients = load(&path).unwrap().radius.unwrap().clients;
        assert_eq!(clients.lookup("127.0.0.1".parse().unwrap()).unwrap().secret, b"s3cret");

        fs::write(&path, base("\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret_file = \"empty.secret\"\n")).unwrap();
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("empty.secret"), "unexpected error: {err}");

        fs::write(&path, base("\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret_file = \"missing.secret\"\n")).unwrap();
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("missing.secret"), "unexpected error: {err}");
    }

    #[test]
    fn test_duplicate_client_network() {
        let err = error(&base("\n[radius]\n[[radius.clients]]\naddress = \"10.0.0.5\"\nsecret = \"a\"\n[[radius.clients]]\naddress = \"10.0.0.5/32\"\nsecret = \"b\"\n"));
        assert!(err.contains("duplicate client network"), "unexpected error: {err}");
        assert!(err.contains("10.0.0.5/32"), "error does not name the network: {err}");
    }

    #[test]
    fn test_enabled_protocol_without_clients() {
        let err = error(&base("\n[tacacs]\nport = 4949\n"));
        assert!(err.contains("[tacacs]"), "unexpected error: {err}");
    }

    #[test]
    fn test_overload_limits() {
        const TACACS_CLIENT: &str = "[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        let limits = |radius: &str, tacacs: &str| {
            base(&format!("\n[radius]\n{radius}[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n[tacacs]\n{tacacs}{TACACS_CLIENT}"))
        };

        let config = load_str(&limits("", "")).1.unwrap();
        let (radius, tacacs) = (config.radius.unwrap(), config.tacacs.unwrap());
        assert_eq!((radius.max_auth_inflight, radius.max_acct_inflight, tacacs.max_connections), (4096, 4096, 1024));

        let config = load_str(&limits("max_auth_inflight = 100\nmax_acct_inflight = 50\n", "max_connections = 10\n")).1.unwrap();
        let (radius, tacacs) = (config.radius.unwrap(), config.tacacs.unwrap());
        assert_eq!((radius.max_auth_inflight, radius.max_acct_inflight, tacacs.max_connections), (100, 50, 10));

        let err = error(&limits("max_auth_inflight = 0\n", ""));
        assert!(err.contains("radius.max_auth_inflight"), "unexpected error: {err}");
        let err = error(&limits("max_acct_inflight = 0\n", ""));
        assert!(err.contains("radius.max_acct_inflight"), "unexpected error: {err}");
        let err = error(&limits("", "max_connections = 0\n"));
        assert!(err.contains("tacacs.max_connections"), "unexpected error: {err}");
    }

    #[test]
    fn test_tacacs_settings() {
        let (_dir, result) = load_str(&base(
            "\n[tacacs]\nbind = \"127.0.0.1\"\nport = 4949\nmax_body_len = 1024\n\
             [[tacacs.clients]]\nname = \"router-1\"\naddress = \"10.0.0.5\"\nsecret = \"k\"\n",
        ));
        let config = result.unwrap();
        let tacacs = config.tacacs.unwrap();
        assert_eq!(tacacs.addr, "127.0.0.1:4949".parse().unwrap());
        assert_eq!(tacacs.max_body_len, 1024);
        assert_eq!(tacacs.clients.lookup("10.0.0.5".parse().unwrap()).unwrap().name, "router-1");
        assert!(config.radius.is_none());
    }

    #[test]
    fn test_missing_file() {
        let err = load(Path::new("/nonexistent/rradius.toml")).unwrap_err().to_string();
        assert!(err.contains("/nonexistent/rradius.toml"), "unexpected error: {err}");
    }

    #[test]
    fn test_example_configuration() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rradius-server/examples/rradius.toml");
        let config = load(&path).unwrap();
        assert!(config.dictionary.exists(), "{} does not exist", config.dictionary.display());
        assert!(config.lua.script.exists(), "{} does not exist", config.lua.script.display());
        let radius = config.radius.expect("RADIUS is enabled in the example");
        let client = radius.clients.lookup("127.0.0.1".parse().unwrap()).expect("127.0.0.1 is a client");
        assert_eq!(client.secret, b"testing123");
        assert!(config.tacacs.is_none());
        assert_eq!(config.cluster.nodes.len(), 1, "the example is a single-node cluster");
        assert_eq!(config.cluster.data_dir, path.parent().unwrap().join("data"));
    }


    #[test]
    fn test_radius_bind() {
        let loaded = |bind: &str| {
            let (_dir, result) = load_str(&base(&format!("\n[radius]\nbind = {bind}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n")));
            result.map(|config| config.radius.unwrap()).map_err(|e| e.0)
        };
        let one = loaded("\"127.0.0.1\"").unwrap();
        assert_eq!((one.auth_addr, one.acct_addr), ("127.0.0.1:1812".parse().unwrap(), "127.0.0.1:1813".parse().unwrap()));
        assert!(one.more_binds.is_empty());
        let several = loaded("[\"127.0.0.1\", \"::1\", \"10.0.0.1\"]").unwrap();
        assert_eq!(several.auth_addr, "127.0.0.1:1812".parse().unwrap());
        assert_eq!(several.more_binds, vec!["::1".parse::<IpAddr>().unwrap(), "10.0.0.1".parse().unwrap()]);
        // The wildcards of both families, and a wildcard beside an address of the other.
        assert_eq!(loaded("[\"0.0.0.0\", \"::\"]").unwrap().more_binds, vec!["::".parse::<IpAddr>().unwrap()]);
        assert_eq!(loaded("[\"::\", \"10.0.0.1\"]").unwrap().more_binds, vec!["10.0.0.1".parse::<IpAddr>().unwrap()]);
        assert!(loaded("\"::ffff:10.0.0.1\"").is_ok(), "alone it is what it was");
        // A list of one is one address.
        let alone = loaded("[\"::\"]").unwrap();
        assert!(alone.more_binds.is_empty());
        assert_eq!(alone.auth_addr, "[::]:1812".parse().unwrap());
        for bind in ["[]", "[\"127.0.0.1\", \"127.0.0.1\"]", "[\"nowhere\"]", "\"nowhere\"", "5", "[5]", "[\"0.0.0.0\", \"192.0.2.1\"]", "[\"192.0.2.1\", \"0.0.0.0\"]", "[\"::\", \"::1\"]", "[\"0.0.0.0\", \"::ffff:10.0.0.1\"]"] {
            let error = loaded(bind).map(|_| ()).unwrap_err();
            assert!(error.contains("radius.bind"), "{bind}: {error}");
        }
    }

    #[test]
    fn test_trace_settings() {
        const RADIUS: &str = "[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        let loaded = |table: &str| {
            let (_dir, result) = load_str(&base(&format!("\n[log.trace]\n{table}\n{RADIUS}")));
            result.map(|config| config.trace.unwrap()).map_err(|e| e.0)
        };
        assert_eq!(loaded("users = [\"alice\"]\nminutes = 30").unwrap(), TraceSettings { users: vec!["alice".to_string()], clients: Vec::new(), duration: Duration::from_secs(1800) });
        assert_eq!(loaded("clients = [\"127.0.0.1\"]\nminutes = 1440").unwrap().clients, ["127.0.0.1"]);
        for (table, expected) in [
            ("users = [\"alice\"]", "log.trace.minutes"),
            ("users = [\"alice\"]\nminutes = 0", "log.trace.minutes"),
            ("users = [\"alice\"]\nminutes = 1441", "log.trace.minutes"),
            ("minutes = 30", "log.trace"),
            ("users = []\nclients = []\nminutes = 30", "log.trace"),
            ("clients = [\"nobody\"]\nminutes = 30", "log.trace.clients: 'nobody'"),
        ] {
            let error = loaded(table).map(|_| ()).unwrap_err();
            assert!(error.contains(expected), "{table}: {error}");
        }
        let (_dir, result) = load_str(&base(RADIUS));
        assert!(result.unwrap().trace.is_none());
        let tacacs = "[tacacs]\n[[tacacs.clients]]\naddress = \"10.0.0.5\"\nsecret = \"s\"\n";
        let (_dir, result) = load_str(&base(&format!("\n[log.trace]\nusers = [\"alice\"]\nminutes = 5\n{tacacs}")));
        let error = result.map(|_| ()).unwrap_err().0;
        assert!(error.contains("log.trace needs [radius]"), "{error}");
    }


    #[test]
    fn test_missing_cluster_section() {
        let err = error(&format!("dictionary = \"d\"\n[lua]\nscript = \"s\"\n{}", RADIUS_CLIENT));
        assert!(err.contains("[cluster]"), "unexpected error: {err}");
    }

    const MINIMAL_CLUSTER: &str = "[cluster]\ndata_dir = \"data\"\nsecret_raft = \"raft-secret-0123456\"\nsecret_api = \"api-secret-01234567\"\n";

    #[test]
    fn test_missing_lua_section() {
        let err = error(&format!("dictionary = \"d\"\n{}{}", MINIMAL_CLUSTER, RADIUS_CLIENT));
        assert!(err.contains("[lua]"), "unexpected error: {err}");
    }

    #[test]
    fn test_moved_settings() {
        let err = error(&format!("dictionary = \"d\"\nscript = \"auth.lua\"\n{}{}", MINIMAL_CLUSTER, RADIUS_CLIENT));
        assert!(err.contains("`script` moved to `[lua].script`"), "unexpected error: {err}");
        let err = error(&base(&format!("\n{}query_timeout_ms = 500\n{}", MINIMAL_CLUSTER, RADIUS_CLIENT)));
        assert!(
            err.contains("`cluster.query_timeout_ms` moved to `[lua].query_timeout_ms`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_lua_query_timeout() {
        let (_dir, result) = load_str(&format!(
            "dictionary = \"d\"\n[lua]\nscript = \"s\"\nquery_timeout_ms = 250\n{}{}",
            MINIMAL_CLUSTER, RADIUS_CLIENT
        ));
        assert_eq!(result.unwrap().lua.query_timeout, Duration::from_millis(250));
        let err = error(&format!(
            "dictionary = \"d\"\n[lua]\nscript = \"s\"\nquery_timeout_ms = 0\n{}{}",
            MINIMAL_CLUSTER, RADIUS_CLIENT
        ));
        assert!(err.contains("lua.query_timeout_ms"), "unexpected error: {err}");
    }

    #[test]
    fn test_empty_bind_password() {
        let (dir, _) = load_str("");
        std::fs::write(dir.path().join("empty.secret"), "\n").unwrap();
        for fields in ["bind_password = \"\"\nfilter = \"(uid={user})\"\n".to_string(), format!("bind_password_file = \"{}\"\nfilter = \"(uid={{user}})\"\n", dir.path().join("empty.secret").display())] {
            let error = load_str(&ldap_config(&fields)).1.unwrap_err().to_string();
            assert!(error.contains("bind_password") && error.contains("empty"), "{fields}: {error}");
        }
    }

    #[test]
    fn test_tacacs_idle_timeout() {
        let text = |setting: &str| format!("dictionary = \"d\"\n[lua]\nscript = \"s\"\n{MINIMAL_CLUSTER}\n[tacacs]\n{setting}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n");
        let idle = |setting: &str| load_str(&text(setting)).1.unwrap().tacacs.unwrap().idle_timeout;
        assert_eq!(idle(""), Duration::from_millis(300_000));
        assert_eq!(idle("idle_timeout_ms = 2500"), Duration::from_millis(2500));
        let error = load_str(&text("idle_timeout_ms = 0")).1.unwrap_err().to_string();
        assert!(error.contains("tacacs.idle_timeout_ms"), "{error}");
    }

    #[test]
    fn test_lua_max_memory() {
        let text = |setting: &str| format!("dictionary = \"d\"\n[lua]\nscript = \"s\"\n{setting}{}{}", MINIMAL_CLUSTER, RADIUS_CLIENT);
        assert_eq!(load_str(&text("")).1.unwrap().lua.max_memory, 256 << 20);
        assert_eq!(load_str(&text("max_memory_mb = 32\n")).1.unwrap().lua.max_memory, 32 << 20);
        for bad in ["max_memory_mb = 0\n", "max_memory_mb = 18446744073709551615\n"] {
            let error = load_str(&text(bad)).1.unwrap_err().to_string();
            assert!(error.contains("max_memory_mb"), "{bad}: {error}");
        }
    }

    #[test]
    fn test_lua_max_execution() {
        let (_dir, result) = load_str(&format!(
            "dictionary = \"d\"\n[lua]\nscript = \"s\"\nmax_execution_ms = 250\n{}{}",
            MINIMAL_CLUSTER, RADIUS_CLIENT
        ));
        assert_eq!(result.unwrap().lua.max_execution, Duration::from_millis(250));
        let err = error(&format!(
            "dictionary = \"d\"\n[lua]\nscript = \"s\"\nmax_execution_ms = 0\n{}{}",
            MINIMAL_CLUSTER, RADIUS_CLIENT
        ));
        assert!(err.contains("lua.max_execution_ms"), "unexpected error: {err}");
    }

    #[test]
    fn test_legacy_database_section() {
        let err = error(&base(&format!("\n[database]\nurl = \"sqlite::memory:\"\n{}", RADIUS_CLIENT)));
        assert!(err.contains("database"), "unexpected error: {err}");
    }

    fn cluster_config(cluster: &str) -> String {
        base(&format!("\n[cluster]\n{}{}", cluster, RADIUS_CLIENT))
    }

    #[test]
    fn test_cluster_defaults() {
        let (dir, result) = load_str(&cluster_config(&format!("data_dir = \"data\"\n{}", CLUSTER_SECRETS)));
        let cluster = result.unwrap().cluster;
        assert_eq!(cluster.node_id, 1);
        assert_eq!(cluster.data_dir, dir.path().join("data"));
        assert_eq!(cluster.listen, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(cluster.nodes, vec![ClusterNode { id: 1, raft: "127.0.0.1:8100".to_string(), api: "127.0.0.1:8200".to_string() }]);
    }

    #[test]
    fn test_read_connections() {
        let read_connections = |line: &str| load_str(&cluster_config(&format!("data_dir = \"data\"\n{line}\n{CLUSTER_SECRETS}"))).1.map(|config| config.cluster.read_connections).map_err(|e| e.to_string());
        assert_eq!(read_connections(""), Ok((2 * crate::runtime::worker_threads()).min(32)));
        assert_eq!((read_connections_for(7), read_connections_for(16), read_connections_for(96)), (14, 32, 32), "a large host stops at 32");
        assert_eq!(read_connections("read_connections = 6"), Ok(6));
        assert_eq!(read_connections("read_connections = 128"), Ok(128));
        for refused in ["read_connections = 0", "read_connections = 129"] {
            assert!(read_connections(refused).unwrap_err().contains("cluster.read_connections"), "{refused}");
        }
    }

    #[test]
    fn test_snapshot_every() {
        let snapshot_every = |line: &str| load_str(&cluster_config(&format!("data_dir = \"data\"\n{line}\n{CLUSTER_SECRETS}"))).1.map(|config| config.cluster.snapshot_every).map_err(|e| e.to_string());
        assert_eq!(snapshot_every(""), Ok(100_000));
        assert_eq!(snapshot_every("snapshot_every = 20000"), Ok(20_000));
        assert!(snapshot_every("snapshot_every = 0").unwrap_err().contains("cluster.snapshot_every"));
    }

    #[test]
    fn test_stop_without_leader_setting() -> Result<(), String> {
        let load = |extra: &str| load_str(&cluster_config(&format!("data_dir = \"data\"\n{extra}{}", CLUSTER_SECRETS))).1.map(|config| config.cluster.stop_without_leader).map_err(|e| e.to_string());
        assert_eq!(load("")?, None, "off unless set");
        assert_eq!(load("stop_without_leader_ms = 10000\n")?, Some(Duration::from_millis(10000)));
        let Err(error) = load("stop_without_leader_ms = 0\n") else { return Err("0 taken".to_string()) };
        assert!(error.contains("stop_without_leader_ms"), "{error}");
        Ok(())
    }

    #[test]
    fn test_migration_settings() {
        let (dir, result) = load_str(&cluster_config(&format!("data_dir = \"data\"\nmigrations_dir = \"migrations\"\n{}", CLUSTER_SECRETS)));
        let cluster = result.unwrap().cluster;
        assert_eq!(cluster.migrations_dir, Some(dir.path().join("migrations")));
        assert_eq!(cluster.migrations_wait, Duration::from_millis(60000));

        let (_dir, result) = load_str(&cluster_config(&format!("data_dir = \"data\"\n{}", CLUSTER_SECRETS)));
        assert_eq!(result.unwrap().cluster.migrations_dir, None);

        let (_dir, result) = load_str(&cluster_config(&format!("data_dir = \"data\"\nmigrations_wait_ms = 2000\n{}", CLUSTER_SECRETS)));
        assert_eq!(result.unwrap().cluster.migrations_wait, Duration::from_millis(2000));

        let (_dir, result) = load_str(&cluster_config(&format!("data_dir = \"data\"\nmigrations_wait_ms = 0\n{}", CLUSTER_SECRETS)));
        assert!(result.unwrap_err().to_string().contains("cluster.migrations_wait_ms"));
    }

    #[test]
    fn test_syntax_error_is_one_line_without_the_value() {
        let message = error("dictionary = \"d\"\nsecret = \"hunter2\" x\n");
        assert!(message.contains("rradius.toml") && message.contains("line 2") && message.contains("`secret`"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
        assert_eq!(message.lines().count(), 1, "{message}");
    }

    #[test]
    fn test_type_error_does_not_show_the_value() {
        let client = |secret: &str| base(&format!("\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = {secret}\n"));
        for (secret, value) in [("918273645", "918273645"), ("1.5", "1.5"), ("true", "true")] {
            let message = error(&client(secret));
            assert!(message.contains("line ") && message.contains("`secret`"), "{message}");
            assert!(!message.contains(value), "{message}");
            assert!(message.contains("expected"), "still says what is wrong: {message}");
            assert_eq!(message.lines().count(), 1, "{message}");
        }
        // A secret on the wrong line.
        let message = error(&base("\n[radius]\nauth_port = \"hunter2\"\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"));
        assert!(!message.contains("hunter2"), "{message}");
        let message = error(&base("\n[radius]\nauth_port = 918273645\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"));
        assert!(!message.contains("918273645") && message.contains("invalid value"), "{message}");
        // A value that looks like the rest of the message.
        let message = error(&base("\n[radius]\nauth_port = \"abc, expected hunter2\"\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"));
        assert!(!message.contains("hunter2") && message.contains("expected"), "{message}");
        // Values with no text of their own read properly too.
        for secret in ["[]", "{ a = 1 }"] {
            let message = error(&client(secret));
            assert!(!message.contains(",,") && message.contains("expected"), "{message}");
        }
    }

    #[test]
    fn test_word_a_setting_does_not_take_is_not_shown() {
        for text in [format!("\n[auth]\npolicy = \"hunter2\"\n{SQL_HANDLER}{RADIUS_CLIENT}"), format!("\n[auth]\n[[auth.handlers]]\ntype = \"hunter2\"\n{RADIUS_CLIENT}"), format!("\n[auth]\npolicy = \"hunter2, expected one\"\n{SQL_HANDLER}{RADIUS_CLIENT}")] {
            let message = error(&base(&text));
            assert!(!message.contains("hunter2"), "{message}");
            assert!(message.contains("line ") && message.contains("unknown variant") && message.contains("expected"), "{message}");
            assert_eq!(message.lines().count(), 1, "{message}");
        }
        let message = error(&base(&format!("\n[auth]\npolicy = \"hunter2\"\n{SQL_HANDLER}{RADIUS_CLIENT}")));
        assert!(message.contains("first_match") && message.contains("all_accept"), "still lists what it takes: {message}");
    }

    #[test]
    fn test_without_the_value_with_no_list_of_what_is_expected() {
        assert_eq!(without_the_value("unknown variant `hunter2`, there are no variants"), "unknown variant");
        assert_eq!(without_the_value("invalid type: string \"hunter2\""), "invalid type: string");
    }

    #[test]
    fn test_cluster_listen_default_goes_by_the_nodes() {
        let (_dir, result) = load_str(&cluster_config(&format!("data_dir = \"data\"\n{}listen = \"10.0.0.1\"\n", CLUSTER_SECRETS)));
        assert_eq!(result.unwrap().cluster.listen, "10.0.0.1".parse::<IpAddr>().unwrap(), "what is set");
        let nodes = "[[cluster.nodes]]\nid = 1\nraft = \"10.0.0.1:8100\"\napi = \"10.0.0.1:8200\"\n";
        let (_dir, result) = load_str(&cluster_config(&format!("data_dir = \"data\"\n{}{}", CLUSTER_SECRETS, nodes)));
        assert_eq!(result.unwrap().cluster.listen, "0.0.0.0".parse::<IpAddr>().unwrap(), "other nodes must reach it");
    }

    #[test]
    fn test_metrics_settings() {
        let (_dir, result) = load_str(&base(&format!("\n[metrics]\n{RADIUS_CLIENT}")));
        assert_eq!(result.unwrap().metrics, Some("127.0.0.1:9812".parse().unwrap()));
        let (_dir, result) = load_str(&base(&format!("\n[metrics]\nbind = \"10.0.0.1\"\nport = 9000\n{RADIUS_CLIENT}")));
        assert_eq!(result.unwrap().metrics, Some("10.0.0.1:9000".parse().unwrap()));
        let (_dir, result) = load_str(&base(RADIUS_CLIENT));
        assert_eq!(result.unwrap().metrics, None);
        assert!(error(&base(&format!("\n[metrics]\npath = \"/stats\"\n{RADIUS_CLIENT}"))).contains("path"));
    }

    /// `[radius]` holding `settings`, with one client holding `client`.
    fn coa_config(settings: &str, client: &str) -> String {
        base(&format!("\n[radius]\n{settings}\n[[radius.clients]]\naddress = \"10.0.0.5\"\nsecret = \"s\"\n{client}\n"))
    }

    #[test]
    fn test_dynamic_authorization_settings() {
        let source = "10.0.0.5".parse().unwrap();
        let radius = load_str(&coa_config("", "")).1.unwrap().radius.unwrap();
        assert_eq!((radius.coa_timeout, radius.coa_attempts), (Duration::from_millis(1000), 3));
        assert_eq!(radius.clients.lookup(source).unwrap().coa_port, 3799);

        let radius = load_str(&coa_config("coa_timeout_ms = 500\ncoa_attempts = 2", "coa_port = 1700")).1.unwrap().radius.unwrap();
        assert_eq!((radius.coa_timeout, radius.coa_attempts), (Duration::from_millis(500), 2));
        assert_eq!(radius.clients.lookup(source).unwrap().coa_port, 1700);

        assert!(error(&coa_config("coa_attempts = 0", "")).contains("coa_attempts"));
        assert!(error(&coa_config("coa_timeout_ms = 0", "")).contains("coa_timeout_ms"));
        assert!(error(&coa_config("", "coa_port = 0")).contains("coa_port"));
        let tacacs = base("\n[tacacs]\n[[tacacs.clients]]\naddress = \"10.0.0.5\"\nsecret = \"s\"\ncoa_port = 1700\n");
        assert!(error(&tacacs).contains("coa_port"));
    }


    /// `[tacacs]` with `tacacs` and one client with `client`, in a directory with certificates.
    fn load_tacacs(tacacs: &str, client: &str) -> Result<ServerConfig, String> {
        let dir = tempdir().map_err(|e| e.to_string())?;
        let issuer = crate::test_support::certs::ca(dir.path(), "ca");
        crate::test_support::certs::leaf(dir.path(), "server", &issuer);
        let body = base(&format!("\n[tacacs]\nport = 49\n{tacacs}\n[[tacacs.clients]]\n{client}secret = \"s\"\n"));
        let path = dir.path().join("rradius.toml");
        fs::write(&path, body).map_err(|e| e.to_string())?;
        load(&path).map_err(|e| e.to_string())
    }

    /// The table of the tacacs-aaa spec ("TACACS+ over TLS Only in the Enterprise Program"), this
    /// program's column: any `[tacacs.tls]` is refused before its files are read.
    #[test]
    fn test_tacacs_tls_needs_the_enterprise_program() -> Result<(), String> {
        const REFUSED: &str = "tacacs.tls: not supported by this program (TACACS+ over TLS needs rradius-enterprise)";
        let client = "address = \"127.0.0.1\"\n";
        assert!(load_tacacs("", client)?.tacacs.is_some());
        for tls in [
            format!("[tacacs.tls]\n{RADSEC_FILES}"),
            format!("[tacacs.tls]\n{RADSEC_FILES}tls_only = true\n"),
            format!("[tacacs.tls]\nport = 49\n{RADSEC_FILES}"),
            "[tacacs.tls]\ncert_file = \"missing.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n".to_string(),
            format!("[tacacs.tls]\n{RADSEC_FILES}ocsp = \"hard\"\n"),
            format!("[tacacs.tls]\nport = \"x\"\n{RADSEC_FILES}"),
        ] {
            let error = load_tacacs(&tls, client).err().ok_or_else(|| format!("{tls}: loaded"))?;
            assert!(error.ends_with(REFUSED), "{tls}: {error}");
        }
        Ok(())
    }

    /// Without `[tacacs.tls]`, a client cannot be known by its certificate.
    #[test]
    fn test_tacacs_tls_name() -> Result<(), String> {
        let error = load_tacacs("", "address = \"127.0.0.1\"\ntls_name = \"device.example.com\"\n").err().ok_or("tls_name without TLS accepted")?;
        assert!(error.ends_with("tacacs client '127.0.0.1': tls_name needs [tacacs.tls]"), "{error}");
        Ok(())
    }

    const RADSEC_FILES: &str = "cert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n";

    #[test]
    fn test_radius_sockets() {
        let sockets = |line: &str| load_str(&base(&RADIUS_CLIENT.replace("[radius]\n", &format!("[radius]\n{line}\n")))).1.map(|config| config.radius.unwrap().sockets).map_err(|e| e.to_string());
        assert!(RADIUS_CLIENT.contains("[radius]\n"));
        if cfg!(target_os = "linux") {
            assert_eq!(sockets(""), Ok(crate::runtime::worker_threads()), "one per worker thread");
            assert_eq!(sockets("sockets = 4"), Ok(4));
        } else {
            assert_eq!(sockets(""), Ok(1));
            assert!(sockets("sockets = 4").unwrap_err().contains("radius.sockets"));
        }
        assert_eq!(sockets("sockets = 1"), Ok(1));
        assert!(sockets("sockets = 0").unwrap_err().contains("radius.sockets"));
    }

    #[test]
    fn test_runtime_settings() {
        let (_dir, result) = load_str(&base(RADIUS_CLIENT));
        assert_eq!(result.unwrap().runtime, RuntimeSettings { pin_threads: false });
        let (_dir, result) = load_str(&base(&format!("{RADIUS_CLIENT}\n[runtime]\npin_threads = true\n")));
        if cfg!(target_os = "linux") {
            assert!(result.unwrap().runtime.pin_threads);
        } else {
            assert!(result.unwrap_err().to_string().contains("runtime.pin_threads"));
        }
        assert!(error(&base(&format!("{RADIUS_CLIENT}\n[runtime]\nthreads = 8\n"))).contains("threads"));
    }

    #[test]
    fn test_accounts_settings() {
        let (_dir, result) = load_str(&base(&format!("{RADIUS_CLIENT}\n[accounts]\nadd = \"INSERT INTO users (username, password) VALUES (?1, ?2)\"\npasswd = \"UPDATE users SET password = ?2 WHERE username = ?1\"\ndisable = \"UPDATE users SET enabled = 0 WHERE username = ?1\"\nscheme = \"sha512-crypt\"\n")));
        let accounts = result.unwrap().accounts.unwrap();
        assert_eq!(accounts.add.as_deref(), Some("INSERT INTO users (username, password) VALUES (?1, ?2)"));
        assert_eq!(accounts.disable.as_deref(), Some("UPDATE users SET enabled = 0 WHERE username = ?1"));
        assert_eq!((accounts.enable, accounts.delete, accounts.show), (None, None, None));
        assert_eq!(accounts.scheme, PasswordScheme::Sha512Crypt);

        let (_dir, result) = load_str(&base(&format!("{RADIUS_CLIENT}\n[accounts]\n")));
        assert_eq!(result.unwrap().accounts.unwrap().scheme, PasswordScheme::Bcrypt);
        let (_dir, result) = load_str(&base(RADIUS_CLIENT));
        assert!(result.unwrap().accounts.is_none());
    }

    #[test]
    fn test_accounts_settings_refused() {
        for (accounts, expected) in [
            ("disable = \"UPDATE users SET enabled = 0 WHERE username = ?\"", "accounts.disable"),
            ("add = \"INSERT INTO users (username) VALUES (?1)\"", "accounts.add"),
            ("passwd = \"UPDATE users SET enabled = 1 WHERE username = ?1\"", "accounts.passwd"),
            ("delete = \"DELETE FROM users WHERE username = ?1 AND password = ?2\"", "accounts.delete"),
            ("show = \"SELECT * FROM users WHERE username = ?3\"", "accounts.show"),
            ("enable = \"UPDATE users SET enabled = 1 WHERE username = ?0\"", "accounts.enable"),
            ("enable = \"UPDATE users SET enabled = 1 WHERE username = :name\"", "accounts.enable"),
            ("passwd = \"UPDATE users SET password = ?2\"", "accounts.passwd"),
            ("delete = \"DELETE FROM users\"", "accounts.delete"),
            ("show = \"SELECT username FROM users WHERE id = ?2\"", "accounts.show"),
            ("scheme = \"md5\"", "scheme"),
            ("shell = \"x\"", "shell"),
        ] {
            let error = error(&base(&format!("{RADIUS_CLIENT}\n[accounts]\n{accounts}\n")));
            assert!(error.contains(expected), "{accounts}: {error}");
        }
        // Text and comments are not parameters.
        let (_dir, result) = load_str(&base(&format!("{RADIUS_CLIENT}\n[accounts]\nenable = \"UPDATE users SET note = '?' WHERE username = ?1 -- ?2\"\n")));
        assert!(result.unwrap().accounts.unwrap().enable.is_some());
    }

    #[test]
    fn test_record_statements_refused() {
        let error = error(&base(&format!("{RADIUS_CLIENT}\n[accounts.nas]\nadd = \"x\"\n")));
        assert!(error.contains("nas"), "{error}");
    }



    #[test]
    fn test_tacacs_enable_setting() -> Result<(), String> {
        use crate::tacacs::EnableMode;
        let tacacs = |setting: &str| base(&format!("\n[tacacs]\n{setting}\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"));
        for (setting, expected) in [("", EnableMode::Deny), ("enable = \"deny\"", EnableMode::Deny), ("enable = \"login\"", EnableMode::Login)] {
            let (_dir, result) = load_str(&tacacs(setting));
            assert_eq!(result.map_err(|e| e.to_string())?.tacacs.ok_or("no [tacacs]")?.enable, expected, "{setting}");
        }
        let (_dir, result) = load_str(&tacacs("enable = \"allow\""));
        let Err(error) = result else {
            return Err("enable = \"allow\" was accepted".to_string());
        };
        assert!(error.to_string().contains("tacacs.enable must be \"deny\" or \"login\""), "{error}");
        Ok(())
    }

    #[test]
    fn test_tacacs_authorize_groups() -> Result<(), String> {
        let tacacs = |groups: &str, client: &str| {
            base(&format!("\n[tacacs]\n[tacacs.authorize_groups]\n{groups}\n[[tacacs.clients]]\nname = \"core\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{client}\n"))
        };
        let (_dir, result) = load_str(&tacacs("group15 = ['permit service=shell cmd\\* {priv-lvl=15}']\nnone = []", "authorize_group = \"group15\""));
        let config = result.map_err(|e| e.to_string())?;
        let tacacs_settings = config.tacacs.ok_or("no [tacacs]")?;
        let client = tacacs_settings.clients.lookup("127.0.0.1".parse().map_err(|e| format!("{e}"))?).ok_or("no client")?;
        let group = client.authorize_group.as_ref().ok_or("no authorize_group")?;
        assert_eq!(group.decide(&[b"service=shell".as_slice(), b"cmd*".as_slice()]), Some([b"priv-lvl=15".to_vec()].as_slice()));
        // Without the setting the client has no group, and an unused or empty group is fine.
        let (_dir, result) = load_str(&tacacs("none = []", ""));
        let config = result.map_err(|e| e.to_string())?;
        let client_without = config.tacacs.ok_or("no [tacacs]")?.clients.lookup("127.0.0.1".parse().map_err(|e| format!("{e}"))?).ok_or("no client")?.authorize_group.clone();
        assert!(client_without.is_none());

        for (body, expected) in [
            (tacacs("g = ['allow service=shell']", ""), "tacacs.authorize_groups.g: rule 1: "),
            (tacacs("g = ['permit', 'permit cmd=(show']", ""), "tacacs.authorize_groups.g: rule 2: "),
            (tacacs("g = ['permit {a=\"x y}']", ""), "tacacs.authorize_groups.g: rule 1: the reply's double quotes are not paired"),
            (tacacs("\"\" = []", ""), "group name cannot be empty"),
            (tacacs("g = []", "authorize_group = \"missing\""), "tacacs client 'core': unknown authorize_group 'missing'"),
            (base("\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\nauthorize_group = \"g\"\n"), "authorize_group applies only to TACACS+ clients"),
        ] {
            let (_dir, result) = load_str(&body);
            let Err(error) = result else {
                return Err(format!("loaded: {body}"));
            };
            assert!(error.to_string().contains(expected), "{error}");
        }
        Ok(())
    }





    #[test]
    fn test_tacacs_client_has_no_radsec_limit() {
        let (_dir, result) = load_str(&base("\n[tacacs]\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\nmax_radsec_connections = 1\n"));
        assert!(result.unwrap_err().to_string().contains("applies only to RADIUS clients"));
    }




    #[test]
    fn test_radius_security_settings() {
        use crate::clients::MessageAuthenticatorPolicy::{No, Yes};
        let source = "10.0.0.1".parse().unwrap();
        let other = "10.0.0.2".parse().unwrap();
        let two = "[[radius.clients]]\naddress = \"10.0.0.1\"\nsecret = \"x\"\nCLIENT\n[[radius.clients]]\naddress = \"10.0.0.2\"\nsecret = \"x\"\n";
        let config = |radius: &str, client: &str| base(&format!("\n[radius]\n{radius}\n{}", two.replace("CLIENT", client)));

        let (_dir, result) = load_str(&config("", ""));
        let radius = result.unwrap().radius.unwrap();
        assert!(radius.status_server);
        let client = radius.clients.lookup(source).unwrap();
        assert_eq!((client.require_message_authenticator, client.limit_proxy_state), (Yes, Yes));

        let (_dir, result) = load_str(&config("require_message_authenticator = \"yes\"\nstatus_server = false", "require_message_authenticator = \"no\""));
        let radius = result.unwrap().radius.unwrap();
        assert!(!radius.status_server);
        assert_eq!(radius.clients.lookup(source).unwrap().require_message_authenticator, No);
        assert_eq!(radius.clients.lookup(other).unwrap().require_message_authenticator, Yes);

        let (_dir, result) = load_str(&config("limit_proxy_state = \"no\"", ""));
        assert_eq!(result.unwrap().radius.unwrap().clients.lookup(other).unwrap().limit_proxy_state, No);

        let (_dir, result) = load_str(&config("require_message_authenticator = \"sometimes\"", ""));
        assert!(result.unwrap_err().to_string().contains("require_message_authenticator"));
        let (_dir, result) = load_str(&config("", "limit_proxy_state = \"sometimes\""));
        assert!(result.unwrap_err().to_string().contains("limit_proxy_state"));
    }

    #[test]
    fn test_auto_needs_a_single_host() {
        let net = "[[radius.clients]]\naddress = \"10.0.0.0/24\"\nsecret = \"x\"\n";
        let host = "[[radius.clients]]\naddress = \"10.0.0.1\"\nsecret = \"x\"\n";
        let (_dir, result) = load_str(&base(&format!("\n[radius]\n{net}require_message_authenticator = \"auto\"\n")));
        let error = result.unwrap_err().to_string();
        assert!(error.contains("require_message_authenticator") && error.contains("10.0.0.0/24"), "{error}");

        let (_dir, result) = load_str(&base(&format!("\n[radius]\nlimit_proxy_state = \"auto\"\n{net}")));
        let error = result.unwrap_err().to_string();
        assert!(error.contains("limit_proxy_state") && error.contains("10.0.0.0/24"), "{error}");

        let (_dir, result) = load_str(&base(&format!("\n[radius]\nlimit_proxy_state = \"auto\"\nrequire_message_authenticator = \"auto\"\n{host}")));
        assert!(result.is_ok());
        let (_dir, result) = load_str(&base(&format!("\n[radius]\nrequire_message_authenticator = \"auto\"\n{host}[[radius.clients]]\naddress = \"10.1.0.0/24\"\nsecret = \"x\"\nrequire_message_authenticator = \"no\"\n")));
        assert!(result.is_ok(), "an explicit setting on the network overrides auto");
    }

    #[test]
    fn test_security_settings_are_for_radius_clients() {
        let (_dir, result) = load_str(&base("\n[tacacs]\n[[tacacs.clients]]\naddress = \"10.0.0.1\"\nsecret = \"x\"\nrequire_message_authenticator = \"no\"\n"));
        assert!(result.unwrap_err().to_string().contains("require_message_authenticator"));
    }

    fn three_nodes(node_id: u64, ids: [u64; 3]) -> String {
        let mut text = format!("node_id = {}\ndata_dir = \"data\"\n{}", node_id, CLUSTER_SECRETS);
        for id in ids {
            text.push_str(&format!("[[cluster.nodes]]\nid = {id}\nraft = \"10.0.0.{id}:8100\"\napi = \"10.0.0.{id}:8200\"\n"));
        }
        text
    }

    #[test]
    fn test_three_node_cluster() {
        let (_dir, result) = load_str(&cluster_config(&three_nodes(2, [1, 2, 3])));
        let cluster = result.unwrap().cluster;
        assert_eq!(cluster.nodes.len(), 3);
        assert_eq!(cluster.nodes.iter().find(|n| n.id == 2), Some(&ClusterNode { id: 2, raft: "10.0.0.2:8100".to_string(), api: "10.0.0.2:8200".to_string() }));
    }

    #[test]
    fn test_cluster_node_list_errors() {
        let err = error(&cluster_config(&three_nodes(4, [1, 2, 3])));
        assert!(err.contains("node_id"), "unexpected error: {err}");
        let err = error(&cluster_config(&three_nodes(2, [2, 3, 4])));
        assert!(err.contains("node 1 is required"), "unexpected error: {err}");
        let err = error(&cluster_config(&three_nodes(2, [1, 2, 2])));
        assert!(err.contains("duplicate node id 2"), "unexpected error: {err}");
    }

    #[test]
    fn test_cluster_secrets() {
        let short = error(&cluster_config("data_dir = \"data\"\nsecret_raft = \"short\"\nsecret_api = \"api-secret-01234567\"\n"));
        assert!(short.contains("secret_raft") && short.contains("16"), "unexpected error: {short}");

        let dir = tempdir().unwrap();
        fs::write(dir.path().join("api.secret"), "too-short\n").unwrap();
        fs::write(dir.path().join("good.secret"), "api-secret-from-file\n").unwrap();
        let path = dir.path().join("rradius.toml");
        fs::write(&path, cluster_config("data_dir = \"data\"\nsecret_raft = \"raft-secret-0123456\"\nsecret_api_file = \"api.secret\"\n")).unwrap();
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("secret_api"), "unexpected error: {err}");
        fs::write(&path, cluster_config("data_dir = \"data\"\nsecret_raft = \"raft-secret-0123456\"\nsecret_api_file = \"good.secret\"\n")).unwrap();
        assert_eq!(load(&path).unwrap().cluster.secret_api, "api-secret-from-file");

        let both = error(&cluster_config("data_dir = \"data\"\nsecret_raft = \"raft-secret-0123456\"\nsecret_api = \"api-secret-01234567\"\nsecret_api_file = \"x\"\n"));
        assert!(both.contains("secret_api") && both.contains("exactly one"), "unexpected error: {both}");
        let neither = error(&cluster_config("data_dir = \"data\"\nsecret_raft = \"raft-secret-0123456\"\n"));
        assert!(neither.contains("secret_api") && neither.contains("exactly one"), "unexpected error: {neither}");
    }

    #[test]
    fn test_cluster_address_validation() {
        let err = error(&cluster_config(&format!(
            "data_dir = \"data\"\n{}[[cluster.nodes]]\nid = 1\nraft = \"10.0.0.1\"\napi = \"10.0.0.1:8200\"\n",
            CLUSTER_SECRETS
        )));
        assert!(err.contains("host:port"), "unexpected error: {err}");
    }

    #[test]
    fn test_cluster_node_id_zero() {
        let err = error(&cluster_config(&three_nodes(0, [0, 1, 2])));
        assert!(err.contains("node_id must be at least 1") && err.contains("rradius.toml"), "unexpected error: {err}");
        let err = error(&cluster_config(&three_nodes(1, [0, 1, 2])));
        assert!(err.contains("node ids must be at least 1"), "unexpected error: {err}");
    }

    const SQL_HANDLER: &str = "[[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users WHERE username = ?\"\n";

    #[test]
    fn test_auth_chain_loaded_in_order() {
        let (dir, result) = load_str(&base(&format!(
            "\n[auth]\npolicy = \"all_accept\"\n{}reply_query = \"SELECT attribute, value FROM user_reply WHERE username = ?\"\n\
             [[auth.handlers]]\ntype = \"lua\"\nscript = \"site.lua\"\n{}",
            SQL_HANDLER, RADIUS_CLIENT
        )));
        let auth = result.unwrap().auth.unwrap().default.unwrap();
        assert_eq!(auth.policy, AuthPolicy::AllAccept);
        assert_eq!(auth.handlers.len(), 2);
        assert!(matches!(&auth.handlers[0], AuthHandlerSettings::Sql { reply_query: Some(_), default_user: None, .. }));
        assert_eq!(auth.handlers[1], AuthHandlerSettings::Lua { script: dir.path().join("site.lua"), verifies_password: false });
    }

    #[test]
    fn test_auth_defaults_and_absence() {
        let (_dir, result) = load_str(&base(&format!("\n[auth]\n{}{}", SQL_HANDLER, RADIUS_CLIENT)));
        assert_eq!(result.unwrap().auth.unwrap().default.unwrap().policy, AuthPolicy::FirstMatch);
        let (_dir, result) = load_str(&base(RADIUS_CLIENT));
        assert!(result.unwrap().auth.is_none());
    }

    #[test]
    fn test_sql_handler_timeout() {
        let timeout = |body: &str| match &load_str(&base(body)).1.unwrap().auth.unwrap().default.unwrap().handlers[0] {
            AuthHandlerSettings::Sql { timeout, .. } => *timeout,
            other => panic!("not an sql handler: {other:?}"),
        };
        assert_eq!(timeout(&format!("\n[auth]\n{}{}", SQL_HANDLER, RADIUS_CLIENT)), Duration::from_millis(5000));
        assert_eq!(timeout(&format!("\n[auth]\n{}timeout_ms = 250\n{}", SQL_HANDLER, RADIUS_CLIENT)), Duration::from_millis(250));
        let err = error(&base(&format!("\n[auth]\n{}timeout_ms = 0\n{}", SQL_HANDLER, RADIUS_CLIENT)));
        assert!(err.contains("auth.handlers[0].timeout_ms"), "unexpected error: {err}");
    }

    const VPN_CHAIN: &str = "[auth.chains.vpn]\npolicy = \"all_accept\"\n[[auth.chains.vpn.handlers]]\ntype = \"lua\"\nscript = \"vpn.lua\"\n";

    #[test]
    fn test_named_chains() {
        let (dir, result) = load_str(&base(&format!("\n[auth]\n{}{}{}", SQL_HANDLER, VPN_CHAIN, RADIUS_CLIENT)));
        let auth = result.unwrap().auth.unwrap();
        assert_eq!(auth.default.unwrap().handlers.len(), 1);
        assert_eq!(
            auth.chains.get("vpn"),
            Some(&ChainSettings {
                policy: AuthPolicy::AllAccept,
                handlers: vec![AuthHandlerSettings::Lua { script: dir.path().join("vpn.lua"), verifies_password: false }],
                username: None,
            })
        );
        // Only named chains: no default chain, and `vpn` holds one ldap handler.
        let ldap_chain = "[auth.chains.vpn]\n[[auth.chains.vpn.handlers]]\ntype = \"ldap\"\nurls = [\"ldaps://dc1.example.com\"]\n\
            bind_dn = \"cn=radius,dc=example,dc=com\"\nbind_password = \"pw\"\nbase_dn = \"dc=example,dc=com\"\nfilter = \"(uid={user})\"\n";
        let (_dir, result) = load_str(&base(&format!("\n{}{}", ldap_chain, RADIUS_CLIENT)));
        let auth = result.unwrap().auth.unwrap();
        assert!(auth.default.is_none());
        assert_eq!(auth.chains.keys().collect::<Vec<_>>(), vec!["vpn"]);
        assert!(matches!(auth.chains["vpn"].handlers.as_slice(), [AuthHandlerSettings::Ldap(_)]));
    }

    #[test]
    fn test_named_chain_errors() {
        let err = error(&base(&format!("\n[auth]\npolicy = \"all_accept\"\n{}{}", VPN_CHAIN, RADIUS_CLIENT)));
        assert!(err.contains("auth.handlers") && err.contains("auth.policy"), "unexpected error: {err}");
        let err = error(&base(&format!("\n[auth.chains.vpn]\npolicy = \"first_match\"\n{}", RADIUS_CLIENT)));
        assert!(err.contains("auth.chains.vpn.handlers"), "unexpected error: {err}");
        let err = error(&base(&format!(
            "\n[[auth.chains.vpn.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users\"\n{}",
            RADIUS_CLIENT
        )));
        assert!(err.contains("auth.chains.vpn.handlers[0].user_query"), "unexpected error: {err}");
    }

    #[test]
    fn test_username_settings() {
        let (_dir, result) = load_str(&base(&format!(
            "\n[auth]\n[auth.username]\nstrip_realm = \"both\"\nlowercase = true\nallowed = \"[a-z]+\"\n{}{}",
            SQL_HANDLER, RADIUS_CLIENT
        )));
        let username = result.unwrap().auth.unwrap().default.unwrap().username.unwrap();
        assert_eq!(username.strip_realm, StripRealm::Both);
        assert!(username.lowercase);
        let allowed = username.allowed.unwrap();
        assert!(allowed.is_match("alice"));
        assert!(!allowed.is_match("a'; --"), "the pattern must match the whole name");
    }

    #[test]
    fn test_username_defaults() {
        let (_dir, result) = load_str(&base(&format!("\n[auth]\n{}{}", SQL_HANDLER, RADIUS_CLIENT)));
        assert_eq!(result.unwrap().auth.unwrap().default.unwrap().username, None);
        // An empty table asks for nothing but is still a table.
        let (_dir, result) = load_str(&base(&format!("\n[auth]\n[auth.username]\n{}{}", SQL_HANDLER, RADIUS_CLIENT)));
        assert_eq!(result.unwrap().auth.unwrap().default.unwrap().username, Some(UsernameSettings::default()));
    }

    #[test]
    fn test_username_named_chains() {
        // A named chain has its own table, and does not take the default chain's.
        let (_dir, result) = load_str(&base(&format!(
            "\n[auth]\n[auth.username]\nlowercase = true\n{}{}{}",
            SQL_HANDLER, VPN_CHAIN, RADIUS_CLIENT
        )));
        let auth = result.unwrap().auth.unwrap();
        assert!(auth.default.unwrap().username.unwrap().lowercase);
        assert_eq!(auth.chains["vpn"].username, None);

        let (_dir, result) = load_str(&base(&format!(
            "\n[auth]\n{}[auth.chains.vpn.username]\nstrip_realm = \"prefix\"\n{}{}",
            SQL_HANDLER, VPN_CHAIN, RADIUS_CLIENT
        )));
        let auth = result.unwrap().auth.unwrap();
        assert_eq!(auth.default.unwrap().username, None);
        assert_eq!(auth.chains["vpn"].username.as_ref().unwrap().strip_realm, StripRealm::Prefix);
    }

    #[test]
    fn test_username_errors() {
        for (settings, expected) in [
            ("[auth.username]\nstrip_realm = \"left\"\n", "auth.username.strip_realm"),
            ("[auth.username]\nallowed = \"[\"\n", "auth.username.allowed"),
            // Compiles once wrapped in a group, but is no pattern by itself.
            ("[auth.username]\nallowed = \"a)|(?:b\"\n", "auth.username.allowed"),
            ("[auth.username]\ntrim = true\n", "trim"),
        ] {
            let err = error(&base(&format!("\n[auth]\n{}{}{}", settings, SQL_HANDLER, RADIUS_CLIENT)));
            assert!(err.contains(expected), "{settings}: {err}");
        }
        let err = error(&base(&format!("\n[auth]\n[auth.chains.vpn.username]\nallowed = \"[\"\n{}{}", VPN_CHAIN, RADIUS_CLIENT)));
        assert!(err.contains("auth.chains.vpn.username.allowed"), "{err}");
        let err = error(&base(&format!("\n[auth]\n[auth.username]\nlowercase = true\n{}{}", VPN_CHAIN, RADIUS_CLIENT)));
        assert!(err.contains("auth.username") && err.contains("auth.handlers"), "{err}");
        let err = error(&base(&format!("\n[auth]\n[auth.username]\nlowercase = true\n{}", RADIUS_CLIENT)));
        assert!(err.contains("auth.username"), "{err}");
    }

    #[test]
    fn test_log_defaults() {
        let (_dir, result) = load_str(&base(RADIUS_CLIENT));
        assert_eq!(result.unwrap().log, LogSettings { level: LogLevel::Info, format: LogFormat::Text, auth: true });
    }

    #[test]
    fn test_log_settings() {
        let (_dir, result) = load_str(&base(&format!("\n[log]\nlevel = \"debug\"\nformat = \"json\"\nauth = false\n{RADIUS_CLIENT}")));
        assert_eq!(result.unwrap().log, LogSettings { level: LogLevel::Debug, format: LogFormat::Json, auth: false });
        for (name, level) in [("error", LogLevel::Error), ("warn", LogLevel::Warn), ("info", LogLevel::Info), ("debug", LogLevel::Debug)] {
            let (_dir, result) = load_str(&base(&format!("\n[log]\nlevel = \"{name}\"\n{RADIUS_CLIENT}")));
            assert_eq!(result.unwrap().log.level, level);
        }
    }

    #[test]
    fn test_log_errors() {
        for (setting, expected) in [("level = \"loud\"", "log.level"), ("format = \"xml\"", "log.format"), ("color = true", "color")] {
            let err = error(&base(&format!("\n[log]\n{setting}\n{RADIUS_CLIENT}")));
            assert!(err.contains(expected), "{setting}: {err}");
        }
    }

    fn write_certificates(dir: &Path) {
        let issuer = crate::test_support::certs::ca(dir, "ca");
        crate::test_support::certs::leaf(dir, "server", &issuer);
    }



    #[test]
    fn test_tacacs_client_needs_an_address() {
        let error = error(&base("\n[tacacs]\n[[tacacs.clients]]\ntls_name = \"a.example\"\nsecret = \"s\"\n"));
        assert!(error.contains("tls_name") || error.contains("address"), "{error}");
        let error_with = super::tests::error(&base("\n[tacacs]\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\ntls_name = \"a.example\"\nsecret = \"s\"\n"));
        assert!(error_with.contains("tls_name"), "{error_with}");
    }

    fn with_lua_http(settings: &str) -> String {
        format!("dictionary = \"d\"\n[lua]\nscript = \"auth.lua\"\n[lua.http]\n{settings}\n[cluster]\ndata_dir = \"data\"\nsecret_raft = \"secret-for-raft-tests\"\nsecret_api = \"secret-for-api-tests\"\n{RADIUS_CLIENT}")
    }

    #[test]
    fn test_script_http_settings() {
        let (_dir, result) = load_str(&with_lua_http("allowed_hosts = [\"api.example.com\", \"billing.internal:8443\", \"http://legacy.internal:8080\"]"));
        let http = result.unwrap().lua.http.unwrap();
        assert_eq!(http.allowed_hosts, vec![
            AllowedHost { host: "api.example.com".to_string(), port: None, plaintext: false },
            AllowedHost { host: "billing.internal".to_string(), port: Some(8443), plaintext: false },
            AllowedHost { host: "legacy.internal".to_string(), port: Some(8080), plaintext: true },
        ]);
        assert_eq!((http.timeout, http.max_response_bytes, http.max_concurrent), (Duration::from_secs(2), 1048576, 64));
        let (_dir, result) = load_str(&base(RADIUS_CLIENT));
        assert_eq!(result.unwrap().lua.http, None);
    }

    #[test]
    fn test_script_http_errors() {
        let hosts = "allowed_hosts = [\"a.example\"]\n";
        for (settings, expected) in [
            ("allowed_hosts = []".to_string(), "lua.http.allowed_hosts"),
            ("allowed_hosts = [\"*.example.com\"]".to_string(), "lua.http.allowed_hosts"),
            ("allowed_hosts = [\"https://a.example\"]".to_string(), "lua.http.allowed_hosts"),
            ("allowed_hosts = [\"a.example/path\"]".to_string(), "lua.http.allowed_hosts"),
            ("allowed_hosts = [\"a.example\\\\b.example\"]".to_string(), "lua.http.allowed_hosts"),
            ("allowed_hosts = [\"user@a.example\"]".to_string(), "lua.http.allowed_hosts"),
            ("allowed_hosts = [\"\"]".to_string(), "lua.http.allowed_hosts"),
            (format!("{hosts}timeout_ms = 0"), "lua.http.timeout_ms"),
            (format!("{hosts}max_concurrent = 0"), "lua.http.max_concurrent"),
            (format!("{hosts}max_response_bytes = 0"), "lua.http.max_response_bytes"),
            (format!("{hosts}ca_file = \"missing.pem\""), "lua.http.ca_file"),
        ] {
            let err = error(&with_lua_http(&settings));
            assert!(err.contains(expected), "{settings}: {err}");
        }
    }

    const EAP_FILES: &str = "cert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n";

    /// A configuration with `[radius.eap]` holding `settings`, in a directory with certificates.
    fn load_eap(settings: &str) -> Result<ServerConfig, String> {
        let dir = tempfile::tempdir().unwrap();
        write_certificates(dir.path());
        std::fs::write(dir.path().join("empty.crl"), "").unwrap();
        let path = dir.path().join("rradius.toml");
        std::fs::write(&path, base(&format!("\n[radius]\n[radius.eap]\n{settings}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"))).unwrap();
        load(&path).map_err(|e| e.0)
    }

    /// Loads `body`, after the common settings, in a directory with `ca.pem` and `server.pem`,
    /// with only `extensions` reading their settings.
    fn load_with_extensions(body: &str, extensions: &crate::extension::Extensions) -> Result<ServerConfig, String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        write_certificates(dir.path());
        let path = dir.path().join("rradius.toml");
        std::fs::write(&path, base(body)).map_err(|e| e.to_string())?;
        load_with(&path, extensions).map_err(|e| e.0)
    }

    #[test]
    fn test_coa_sender() -> Result<(), String> {
        let body = base("\n[tacacs]\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\ncoa_sender = true\n");
        let (_dir, result) = load_str(&body);
        let error = result.err().ok_or_else(|| format!("loaded: {body}"))?;
        assert!(error.to_string().contains("coa_sender applies only to RADIUS clients"), "{error}");
        Ok(())
    }

    /// An extension that refuses every configuration in its check.
    struct Refusing;

    impl crate::extension::Extension for Refusing {
        fn name(&self) -> &'static str {
            "refusing"
        }
        fn check(&self, config: &ServerConfig) -> Result<(), String> {
            Err(format!("refusing: {} RADIUS clients", config.radius.as_ref().map_or(0, |radius| radius.clients.names().count())))
        }
    }

    /// An extension's check of a loaded configuration refuses it like any other mistake, after
    /// the path of the file.
    #[test]
    fn test_extension_check_refuses_a_configuration() -> Result<(), String> {
        let refusing = crate::extension::Extensions::new(vec![std::sync::Arc::new(Refusing)])?;
        let error = load_with_extensions("\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n", &refusing).err().ok_or("loaded")?;
        assert!(error.ends_with("rradius.toml: refusing: 1 RADIUS clients"), "{error}");
        Ok(())
    }

    /// A program without CoA refuses `[radius.coa]`, saying it needs the enterprise program;
    /// `coa_timeout_ms`, `coa_attempts` and a client's `coa_port` load, doing nothing.
    #[test]
    fn test_coa_needs_the_enterprise_program() -> Result<(), String> {
        let core = crate::extensions::builtin();
        let relay = "\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n[radius.coa]\n[[radius.coa.clients]]\naddress = \"192.0.2.1\"\nsecret = \"s\"\n";
        let error = load_with_extensions(relay, &core).err().ok_or("loaded [radius.coa]")?;
        assert!(error.ends_with("radius.coa: not supported by this program (CoA needs rradius-enterprise)"), "{error}");
        load_with_extensions("\n[radius]\ncoa_timeout_ms = 2000\ncoa_attempts = 2\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\ncoa_port = 1700\n", &core)?;
        Ok(())
    }

    /// A program without DPSK refuses its sections, saying it needs the enterprise program (the
    /// table of the dpsk spec, this program's column).
    #[test]
    fn test_dpsk_needs_the_enterprise_program() -> Result<(), String> {
        let core = crate::extensions::builtin();
        let radius = "\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        load_with_extensions(radius, &core)?;
        for (section, refused) in [
            ("[radius.dpsk]\ncandidates_query = \"SELECT identity, pmk FROM dpsk_keys WHERE ssid = ?\"\nkey_file = \"dpsk.key\"\n", "radius.dpsk"),
            ("[accounts.dpsk]\ndelete = \"DELETE FROM dpsk_keys WHERE identity = ?1 AND ssid = ?2\"\n", "accounts.dpsk"),
        ] {
            let error = load_with_extensions(&format!("{radius}{section}"), &core).err().ok_or_else(|| format!("loaded {section}"))?;
            assert!(error.ends_with(&format!("{refused}: not supported by this program (DPSK needs rradius-enterprise)")), "{section}: {error}");
        }
        Ok(())
    }

    /// A program without EAP-AKA' refuses its sections and its method, saying it needs the
    /// enterprise program (the table of the eap spec, this program's column).
    #[test]
    fn test_aka_needs_the_enterprise_program() -> Result<(), String> {
        let core = crate::extensions::builtin();
        let radius = |eap: &str, client: &str| format!("\n[auth]\n{SQL_HANDLER}[radius]\n{eap}\n[[radius.clients]]\nname = \"nas1\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{client}\n");
        const PEAP: &str = "[radius.eap]\nmethods = [\"peap\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\n";
        const AKA: &str = "[radius.eap]\nmethods = [\"aka-prime\"]\n";
        const SECTION: &str = "[radius.eap.aka]\nnetwork_name = \"WLAN\"\nkey_file = \"aka.key\"\nsubscriber_query = \"SELECT ki, opc FROM s WHERE imsi = ?\"\nnext_sqn_query = \"UPDATE s SET sqn = sqn + 32 WHERE imsi = ? RETURNING sqn\"\nresync_query = \"UPDATE s SET sqn = max(sqn, ?) + 32 WHERE imsi = ? RETURNING sqn\"\n";
        load_with_extensions(&radius(PEAP, ""), &core)?;
        for (body, refused) in [
            (radius(&format!("{AKA}{SECTION}"), ""), "radius.eap.aka: not supported by this program (EAP-AKA' needs rradius-enterprise)"),
            (radius(AKA, ""), "radius.eap.methods: aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)"),
            (radius(PEAP, "eap_methods = [\"peap\", \"aka-prime\"]"), "radius client 'nas1': eap_methods: aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)"),
            (radius("[radius.eap]\nmethods = [\"md5\"]\n", ""), "radius.eap.methods: 'md5' is not a method this server offers; it has tls, peap and ttls"),
            (format!("{}[accounts.aka]\nshow = \"SELECT imsi FROM aka_subscribers WHERE imsi = ?1\"\n", radius(PEAP, "")), "accounts.aka: not supported by this program (EAP-AKA' needs rradius-enterprise)"),
        ] {
            let error = load_with_extensions(&body, &core).err().ok_or_else(|| format!("loaded {body}"))?;
            assert!(error.ends_with(refused), "{body}: {error}");
        }
        Ok(())
    }

    /// A program without proxying refuses `[proxy]` and each of its parts alone, saying it needs
    /// the enterprise program (the table of the radius-proxy spec, this program's column).
    #[test]
    fn test_proxy_needs_the_enterprise_program() -> Result<(), String> {
        let core = crate::extensions::builtin();
        let radius = "\n[radius]\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        let server = "[[proxy.servers]]\nname = \"hq\"\naddress = \"192.0.2.1\"\nsecret = \"upstream-secret\"\n";
        load_with_extensions(radius, &core)?;
        for proxy in [format!("[proxy]\n{server}[[proxy.realms]]\nname = \"hq.test\"\nservers = [\"hq\"]\n"), server.to_string(), "[proxy.accounting_spool]\ndir = \"spool\"\n".to_string()] {
            let error = load_with_extensions(&format!("{radius}{proxy}"), &core).err().ok_or_else(|| format!("loaded {proxy}"))?;
            assert!(error.ends_with("proxy: not supported by this program (proxying needs rradius-enterprise)"), "{proxy}: {error}");
        }
        Ok(())
    }

    /// A program without dynamic clients refuses their sections, saying they need the enterprise
    /// program; without them, the configuration loads.
    #[test]
    fn test_dynamic_clients_need_the_enterprise_program() -> Result<(), String> {
        let core = crate::extensions::builtin();
        let client = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        let dynamic = "[radius.dynamic_clients]\nnetworks = [\"10.20.0.0/16\"]\nquery = \"SELECT name, secret FROM nas WHERE address = ?\"\n";
        let statements = "[accounts.clients]\nadd = \"INSERT INTO nas (address, name, secret) VALUES (?1, ?2, ?3)\"\n";
        for (body, expected) in [
            (format!("\n[radius]\n{client}"), None),
            (format!("\n[radius]\n{client}{dynamic}"), Some("radius.dynamic_clients: not supported by this program (dynamic clients need rradius-enterprise)")),
            (format!("\n[radius]\n{client}{statements}"), Some("accounts.clients: not supported by this program (dynamic clients need rradius-enterprise)")),
            (format!("\n[radius]\n{client}{dynamic}{statements}"), Some("radius.dynamic_clients: not supported by this program (dynamic clients need rradius-enterprise)")),
        ] {
            match (load_with_extensions(&body, &core), expected) {
                (Ok(_), None) => {}
                (Err(error), Some(expected)) => assert!(error.ends_with(expected), "{body}: {error}"),
                (result, expected) => return Err(format!("{body}: {:?} instead of {expected:?}", result.err())),
            }
        }
        Ok(())
    }

    /// A program without RadSec refuses `[radius.tls]` and the client settings only RadSec uses,
    /// saying RadSec needs the enterprise program; a client's `tls_name` alone loads.
    #[test]
    fn test_radsec_settings_need_the_enterprise_program() -> Result<(), String> {
        let core = crate::extensions::builtin();
        let radius = |clients: &str, sections: &str| format!("\n[radius]\n{sections}{clients}");
        let plain = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        let rows = [
            (radius(plain, ""), None),
            (radius(plain, &format!("[radius.tls]\n{RADSEC_FILES}")), Some("radius.tls: not supported by this program (RadSec needs rradius-enterprise)")),
            (radius("[[radius.clients]]\nname = \"branch\"\ntls_name = \"branch.example\"\n", ""), Some("radius client 'branch' has no address, so it can only use RadSec, which needs rradius-enterprise")),
            (radius("[[radius.clients]]\nname = \"nas1\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\nmax_radsec_connections = 3\n", ""), Some("radius client 'nas1': max_radsec_connections applies only to RadSec, which needs rradius-enterprise")),
            (radius("[[radius.clients]]\nname = \"hq-proxy\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\ncoa_sender = true\n", "[radius.coa]\n"), Some("radius.coa: not supported by this program (CoA needs rradius-enterprise)")),
            (radius("[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\ntls_name = \"nas1.example\"\n", ""), None),
        ];
        for (body, expected) in rows {
            match (load_with_extensions(&body, &core), expected) {
                (Ok(_), None) => {}
                (Err(error), Some(expected)) => assert!(error.ends_with(expected), "{body}: {error}"),
                (result, expected) => return Err(format!("{body}: {:?} instead of {expected:?}", result.err())),
            }
        }
        Ok(())
    }

    /// Without an extension that reads them, `ocsp` and `ocsp_url` cannot be used, not even `"off"`.
    #[test]
    fn test_ocsp_needs_its_extension() -> Result<(), String> {
        // The built-in extensions read no revocation setting.
        let none = crate::extensions::builtin();
        let client = "[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        let tacacs_client = "[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n";
        for (body, expected) in [
            (format!("\n[radius]\n[radius.eap]\n{EAP_FILES}ocsp = \"hard\"\n{client}"), "radius.eap.ocsp: not supported by this program (OCSP needs rradius-enterprise)"),
            // The whole section is refused, as only rradius-enterprise has TACACS+ over TLS.
            (format!("\n[tacacs]\n[tacacs.tls]\n{RADSEC_FILES}ocsp_url = \"http://ocsp.example.com/\"\n{tacacs_client}"), "tacacs.tls: not supported by this program (TACACS+ over TLS needs rradius-enterprise)"),
        ] {
            let error = load_with_extensions(&body, &none).err().unwrap_or_default();
            assert!(error.ends_with(expected), "{error}");
        }
        // Without them, a configuration loads all the same.
        load_with_extensions(&format!("\n[radius]\n[radius.eap]\n{EAP_FILES}{client}"), &none)?;
        // Where they could never apply, the core says so itself.
        let error = load_with_extensions(&format!("\n[radius]\n[radius.eap]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\nmethods = [\"peap\"]\nocsp = \"soft\"\n{client}"), &none).err().unwrap_or_default();
        assert!(error.ends_with("radius.eap.ocsp only applies to tls, which methods does not have"), "{error}");
        Ok(())
    }

    /// The settings of session resumption, which only the enterprise program has: each is
    /// refused, 0 too, and a value of the wrong type still fails on its line.
    #[test]
    fn test_session_resumption_needs_the_enterprise_program() -> Result<(), String> {
        for (settings, first) in [
            ("resumption_ms = 3600000", "resumption_ms"),
            ("resumption_ms = 0", "resumption_ms"),
            ("shared_tickets = true", "shared_tickets"),
            ("shared_tickets = false", "shared_tickets"),
            ("ticket_seal_file = \"ticket.seal\"", "ticket_seal_file"),
            ("previous_ticket_seal_file = \"ticket.seal\"", "previous_ticket_seal_file"),
            ("previous_ticket_seal_file = \"old.seal\"\nshared_tickets = true\nresumption_ms = 1000", "resumption_ms"),
        ] {
            let error = load_eap(&format!("{EAP_FILES}{settings}")).err().ok_or_else(|| format!("{settings} accepted"))?;
            assert!(error.ends_with(&format!("radius.eap.{first}: not supported by this program (session resumption needs rradius-enterprise)")), "{settings}: {error}");
        }
        let error = load_eap(&format!("{EAP_FILES}resumption_ms = \"an hour\"")).err().ok_or("a string accepted")?;
        assert!(error.contains("resumption_ms") && error.contains("line"), "{error}");
        let eap = load_eap(EAP_FILES)?.radius.ok_or("no radius")?.eap.ok_or("no eap")?;
        assert!(eap.resumption.is_none() && !eap.tls.as_ref().ok_or("no tls")?.session_storage.can_cache(), "every handshake a full one");
        Ok(())
    }

    #[test]
    fn test_eap_settings() {
        let eap = load_eap(EAP_FILES).unwrap().radius.unwrap().eap.unwrap();
        assert_eq!(eap.methods, [crate::eap::TYPE_TLS]);
        assert!(eap.tls.is_some());
        assert_eq!((eap.fragment_size, eap.session_timeout, eap.max_sessions, eap.match_identity), (1000, Duration::from_secs(30), 4096, true));
        let eap = load_eap(&format!("{EAP_FILES}fragment_size = 256\nsession_timeout_ms = 5\nmax_sessions = 7\nmatch_identity = false")).unwrap().radius.unwrap().eap.unwrap();
        assert_eq!((eap.fragment_size, eap.session_timeout, eap.max_sessions, eap.match_identity), (256, Duration::from_millis(5), 7, false));
        let (_dir, without) = load_str(&base(RADIUS_CLIENT));
        assert!(without.unwrap().radius.unwrap().eap.is_none());
    }

    #[test]
    fn test_peap_settings() {
        const FILES: &str = "methods = [\"peap\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\n";
        let load_with = |auth: &str, settings: &str| {
            let dir = tempfile::tempdir().unwrap();
            write_certificates(dir.path());
            let path = dir.path().join("rradius.toml");
            std::fs::write(&path, base(&format!("\n{auth}[radius]\n[radius.eap]\n{settings}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"))).unwrap();
            load(&path).map_err(|e| e.0)
        };
        let error = load_with("", FILES).map(|_| ()).unwrap_err();
        assert!(error.contains("methods") && error.contains("[auth]"), "{error}");
        let chain = format!("[auth]\n{SQL_HANDLER}");
        let eap = load_with(&chain, FILES).unwrap().radius.unwrap().eap.unwrap();
        assert_eq!(eap.methods, vec![crate::eap::TYPE_PEAP]);
        assert!(eap.tunnel_tls.is_some());
        assert!(eap.tls.is_none(), "no client_ca_file needed");
        let error = load_with(&chain, "methods = [\"tls\", \"peap\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\n").map(|_| ()).unwrap_err();
        assert!(error.contains("client_ca_file"), "{error}");
        for setting in ["client_ca_file = \"ca.pem\"", "crl_file = \"ca.crl\""] {
            let error = load_with(&chain, &format!("{FILES}{setting}\n")).map(|_| ()).unwrap_err();
            assert!(error.contains(setting.split(' ').next().unwrap()), "{setting}: {error}");
        }
    }

    #[test]
    fn test_verifies_password_setting() {
        let load_with = |handler: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("check.lua"), "return 'accept'").unwrap();
            let path = dir.path().join("rradius.toml");
            std::fs::write(&path, base(&format!("\n[auth]\n[[auth.handlers]]\n{handler}\n{RADIUS_CLIENT}"))).unwrap();
            load(&path).map_err(|e| e.0)
        };
        let handler = |text: &str| load_with(text).unwrap().auth.unwrap().default.unwrap().handlers.remove(0);
        assert!(matches!(handler("type = \"lua\"\nscript = \"check.lua\"\n"), AuthHandlerSettings::Lua { verifies_password: false, .. }), "off unless asked for");
        assert!(matches!(handler("type = \"lua\"\nscript = \"check.lua\"\nverifies_password = true\n"), AuthHandlerSettings::Lua { verifies_password: true, .. }));
        let error = load_with("type = \"sql\"\nuser_query = \"SELECT password FROM users WHERE username = ?\"\nverifies_password = true\n").map(|_| ()).unwrap_err();
        assert!(error.contains("verifies_password"), "{error}");
    }

    #[test]
    fn test_aka_key_file_others_can_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aka.key");
        std::fs::write(&path, "11".repeat(32)).unwrap();
        for (mode, fine) in [(0o600, true), (0o640, true), (0o400, true), (0o644, false), (0o604, false), (0o660, false), (0o601, false)] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let result = crate::common::seal::read_key_file(&path);
            assert_eq!(result.is_ok(), fine, "{mode:o}");
            if let Err(error) = result {
                assert!(error.contains("aka.key"), "{error}");
                assert!(!error.contains(&"11".repeat(4)), "{error}");
            }
        }
    }

    #[test]
    fn test_ttls_settings() {
        const FILES: &str = "methods = [\"ttls\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\n";
        let load_with = |auth: &str| {
            let dir = tempfile::tempdir().unwrap();
            write_certificates(dir.path());
            let path = dir.path().join("rradius.toml");
            std::fs::write(&path, base(&format!("\n{auth}[radius]\n[radius.eap]\n{FILES}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"))).unwrap();
            load(&path).map_err(|e| e.0)
        };
        let error = load_with("").map(|_| ()).unwrap_err();
        assert!(error.contains("methods") && error.contains("[auth]"), "{error}");
        let eap = load_with(&format!("[auth]\n{SQL_HANDLER}")).unwrap().radius.unwrap().eap.unwrap();
        assert_eq!(eap.methods, vec![crate::eap::TYPE_TTLS]);
        assert!(eap.tunnel_tls.is_some());
        assert!(eap.tls.is_none(), "no client_ca_file needed");
    }

    #[test]
    fn test_client_eap_methods() {
        const BOTH: &str = "[radius.eap]\nmethods = [\"tls\", \"peap\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n";
        const ONE: &str = "[radius.eap]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n";
        let load_with = |eap: &str, client: &str| {
            let dir = tempfile::tempdir().unwrap();
            write_certificates(dir.path());
            std::fs::write(dir.path().join("aka.key"), "11".repeat(32)).unwrap();
            std::fs::set_permissions(dir.path().join("aka.key"), std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
            let path = dir.path().join("rradius.toml");
            std::fs::write(&path, base(&format!("\n[auth]\n{SQL_HANDLER}[radius]\n{eap}\n[[radius.clients]]\nname = \"nas1\"\naddress = \"127.0.0.1\"\nsecret = \"s\"\n{client}\n"))).unwrap();
            load(&path).map_err(|e| e.0)
        };
        let methods = |eap: &str, client: &str| load_with(eap, client).unwrap().radius.unwrap().clients.lookup("127.0.0.1".parse().unwrap()).unwrap().eap_methods.clone();
        assert_eq!(methods(ONE, ""), None, "one method: nothing to choose");
        assert_eq!(methods(BOTH, "eap_methods = [\"peap\", \"tls\"]"), Some(vec![crate::eap::TYPE_PEAP, crate::eap::TYPE_TLS]), "its own order");
        assert_eq!(methods(BOTH, "eap_methods = []"), Some(vec![]), "no EAP at all");
        let three = "[radius.eap]\nmethods = [\"tls\", \"peap\", \"ttls\"]\ncert_file = \"server.pem\"\nkey_file = \"server.key\"\nclient_ca_file = \"ca.pem\"\n";
        assert_eq!(methods(three, "eap_methods = [\"ttls\"]"), Some(vec![crate::eap::TYPE_TTLS]));
        let both = load_with(BOTH, "eap_methods = [\"tls\"]").unwrap().radius.unwrap().eap.unwrap();
        assert_eq!(both.methods, vec![crate::eap::TYPE_TLS, crate::eap::TYPE_PEAP]);
        assert!(both.tls.is_some() && both.tunnel_tls.is_some());
        for (eap, client, expected) in [
            (BOTH, "", "nas1"),
            (ONE, "eap_methods = [\"peap\"]", "nas1"),
            ("", "eap_methods = [\"tls\"]", "nas1"),
            (BOTH, "eap_methods = [\"md5\"]", "nas1"),
            (BOTH, "eap_methods = [\"tls\", \"tls\"]", "nas1"),
        ] {
            let error = load_with(eap, client).map(|_| ()).unwrap_err();
            assert!(error.contains("eap_methods") && error.contains(expected), "{eap} {client}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rradius.toml");
        std::fs::write(&path, base("\n[tacacs]\n[[tacacs.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\neap_methods = [\"tls\"]\n")).unwrap();
        assert!(load(&path).map(|_| ()).unwrap_err().0.contains("eap_methods"));
    }

    #[test]
    fn test_eap_errors() {
        for (settings, expected) in [
            ("key_file = \"server.key\"\nclient_ca_file = \"ca.pem\"".to_string(), "cert_file"),
            ("cert_file = \"server.pem\"\nkey_file = \"server.key\"\nmethods = [\"tls\"]".to_string(), "client_ca_file"),
            (format!("{EAP_FILES}methods = []"), "methods"),
            (format!("{EAP_FILES}methods = [\"md5\"]"), "methods"),
            (format!("{EAP_FILES}methods = [\"tls\", \"tls\"]"), "methods"),
            (format!("{EAP_FILES}fragment_size = 100"), "fragment_size"),
            (format!("{EAP_FILES}fragment_size = 1401"), "fragment_size"),
            (format!("{EAP_FILES}session_timeout_ms = 0"), "session_timeout_ms"),
            (format!("{EAP_FILES}max_sessions = 0"), "max_sessions"),
            (format!("{EAP_FILES}max_sessions_per_client = 0"), "max_sessions_per_client"),
            (format!("{EAP_FILES}max_sessions_per_device = 0"), "max_sessions_per_device"),
            (format!("{EAP_FILES}crl_file = \"empty.crl\""), "crl_file"),
            (format!("{EAP_FILES}crl_file = \"missing.crl\""), "crl_file"),
            (format!("{EAP_FILES}unknown = 1"), "unknown"),
        ] {
            let error = load_eap(&settings).map(|_| ()).unwrap_err();
            assert!(error.contains(expected), "{settings}: {error}");
        }
    }

    #[test]
    fn test_accounting_spool_settings() {
        let (dir, result) = load_str(&base("\n[radius]\n[radius.accounting_spool]\ndir = \"spool\"\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n"));
        let spool = result.unwrap().radius.unwrap().accounting_spool.unwrap();
        assert_eq!(spool, SpoolSettings { dir: dir.path().join("spool"), max_bytes: 1 << 30, replay_interval: Duration::from_secs(5), replay_window: 1 });
        assert!(!dir.path().join("spool").exists(), "loading creates nothing");
        let (_dir, result) = load_str(&base(RADIUS_CLIENT));
        assert_eq!(result.unwrap().radius.unwrap().accounting_spool, None);
        for (setting, expected) in [("max_bytes = 0", "accounting_spool.max_bytes"), ("replay_interval_ms = 0", "accounting_spool.replay_interval_ms")] {
            let err = error(&base(&format!("\n[radius]\n[radius.accounting_spool]\ndir = \"spool\"\n{setting}\n[[radius.clients]]\naddress = \"127.0.0.1\"\nsecret = \"s\"\n")));
            assert!(err.contains(expected), "{setting}: {err}");
        }
    }

    #[test]
    fn test_client_chain_reference() {
        let radius = "\n[radius]\n[[radius.clients]]\nname = \"vpn-gw\"\naddress = \"10.1.0.0/16\"\nsecret = \"s\"\nauth = \"vpn\"\n";
        let (_dir, result) = load_str(&base(&format!("\n{}{}", VPN_CHAIN, radius)));
        let clients = result.unwrap().radius.unwrap().clients;
        assert_eq!(clients.lookup("10.1.2.3".parse().unwrap()).unwrap().auth.as_deref(), Some("vpn"));

        let tacacs = "\n[tacacs]\n[[tacacs.clients]]\nname = \"router-1\"\naddress = \"10.0.0.5\"\nsecret = \"s\"\nauth = \"corp\"\n";
        let err = error(&base(&format!("\n{}{}", VPN_CHAIN, tacacs)));
        assert!(err.contains("router-1") && err.contains("'corp'"), "unexpected error: {err}");
        // Without any [auth], every reference is unknown.
        let err = error(&base(tacacs));
        assert!(err.contains("router-1") && err.contains("'corp'"), "unexpected error: {err}");
    }

    #[test]
    fn test_auth_validation_errors() {
        let err = error(&base(&format!("\n[auth]\n[[auth.handlers]]\ntype = \"kerberos\"\n{}", RADIUS_CLIENT)));
        assert!(err.contains("unknown variant") && err.contains("expected") && !err.contains("kerberos"), "unexpected error: {err}");
        let err = error(&base(&format!("\n[auth]\npolicy = \"first_match\"\n{}", RADIUS_CLIENT)));
        assert!(err.contains("auth.handlers"), "unexpected error: {err}");
        let err = error(&base(&format!("\n[auth]\npolicy = \"majority\"\n{}{}", SQL_HANDLER, RADIUS_CLIENT)));
        assert!(err.contains("unknown variant") && err.contains("first_match") && !err.contains("majority"), "unexpected error: {err}");
        let err = error(&base(&format!(
            "\n[auth]\n[[auth.handlers]]\ntype = \"sql\"\nuser_query = \"SELECT password FROM users\"\n{}",
            RADIUS_CLIENT
        )));
        assert!(err.contains("user_query") && err.contains("found 0"), "unexpected error: {err}");
        let err = error(&base(&format!(
            "\n[auth]\n{}reply_query = \"SELECT a, v FROM r WHERE u = ? OR g = ?\"\n{}",
            SQL_HANDLER, RADIUS_CLIENT
        )));
        assert!(err.contains("reply_query") && err.contains("found 2"), "unexpected error: {err}");
    }

    const LDAP_HANDLER: &str = "[[auth.handlers]]\ntype = \"ldap\"\nurls = [\"ldaps://dc1.example.com\"]\n\
        bind_dn = \"cn=radius,ou=services,dc=example,dc=com\"\nbase_dn = \"ou=people,dc=example,dc=com\"\n";

    /// An `ldap` handler with `fields` added to the required ones (no password source).
    fn ldap_config(fields: &str) -> String {
        base(&format!("\n[auth]\n{}{}\n{}", LDAP_HANDLER, fields, RADIUS_CLIENT))
    }

    #[test]
    fn test_ldap_group_settings() {
        let (_dir, result) = load_str(&ldap_config(&format!(
            "{LDAP_INLINE}required_groups = [\"cn=vpn,dc=example\"]\ngroup_attribute = \"isMemberOf\"\n\
             [[auth.handlers.group_reply]]\ngroup = \"cn=staff,dc=example\"\nattributes = {{ \"Filter-Id\" = \"staff\" }}\n"
        )));
        let auth = result.unwrap().auth.unwrap().default.unwrap();
        let AuthHandlerSettings::Ldap(ldap) = &auth.handlers[0] else { panic!("not ldap") };
        assert_eq!(ldap.groups, LdapGroups {
            required: vec!["cn=vpn,dc=example".to_string()],
            source: GroupSource::Attribute("isMemberOf".to_string()),
            reply: vec![("cn=staff,dc=example".to_string(), vec![("Filter-Id".to_string(), "staff".to_string())])],
        });

        let (_dir, result) = load_str(&ldap_config(&format!(
            "{LDAP_INLINE}required_groups = [\"cn=vpn,dc=example\"]\ngroup_base_dn = \"dc=example\"\ngroup_filter = \"(member={{dn}})\"\n"
        )));
        let auth = result.unwrap().auth.unwrap().default.unwrap();
        let AuthHandlerSettings::Ldap(ldap) = &auth.handlers[0] else { panic!("not ldap") };
        assert_eq!(ldap.groups.source, GroupSource::Search { base_dn: "dc=example".to_string(), filter: "(member={dn})".to_string() });

        let (_dir, result) = load_str(&ldap_config(LDAP_INLINE));
        let auth = result.unwrap().auth.unwrap().default.unwrap();
        let AuthHandlerSettings::Ldap(ldap) = &auth.handlers[0] else { panic!("not ldap") };
        assert_eq!(ldap.groups, LdapGroups::default());
    }

    #[test]
    fn test_certificate_settings() {
        let sql = |query: &str| base(&format!("\n[auth]\n{SQL_HANDLER}certificate_query = \"{query}\"\n{RADIUS_CLIENT}"));
        let (_dir, result) = load_str(&sql("SELECT 1 FROM users WHERE username = ? AND enabled = 1"));
        let config = result.unwrap_or_else(|e| panic!("{e}"));
        let AuthHandlerSettings::Sql { certificate_query, .. } = &config.auth.unwrap().default.unwrap().handlers[0] else { panic!() };
        assert_eq!(certificate_query.as_deref(), Some("SELECT 1 FROM users WHERE username = ? AND enabled = 1"));
        let (_dir, result) = load_str(&sql("SELECT 1 FROM users"));
        assert!(result.unwrap_err().to_string().contains("certificate_query"));

        let (_dir, result) = load_str(&ldap_config(&format!("{LDAP_INLINE}certificate_filter = \"(&(uid={{user}})(!(locked=TRUE)))\"\n")));
        let config = result.unwrap_or_else(|e| panic!("{e}"));
        let AuthHandlerSettings::Ldap(ldap) = &config.auth.unwrap().default.unwrap().handlers[0] else { panic!() };
        assert_eq!(ldap.certificate_filter.as_deref(), Some("(&(uid={user})(!(locked=TRUE)))"));
        let (_dir, result) = load_str(&ldap_config(&format!("{LDAP_INLINE}certificate_filter = \"(uid=x)\"\n")));
        assert!(result.unwrap_err().to_string().contains("certificate_filter"));
    }

    #[test]
    fn test_ldap_group_errors() {
        for (setting, expected) in [
            ("required_groups = [\"cn=a\"]\ngroup_filter = \"(member={dn})\"\n", "group_base_dn"),
            ("required_groups = [\"cn=a\"]\ngroup_base_dn = \"dc=example\"\n", "group_filter"),
            ("required_groups = [\"cn=a\"]\ngroup_base_dn = \"dc=example\"\ngroup_filter = \"(member=x)\"\n", "{dn}"),
            ("required_groups = [\"cn=a\"]\ngroup_attribute = \"memberOf\"\ngroup_base_dn = \"dc=example\"\ngroup_filter = \"(member={dn})\"\n", "group_attribute"),
            ("required_groups = [\"cn=a\"]\ngroup_attribute = \"\"\n", "group_attribute"),
            // Nothing would use these, and a filter looks like a restriction.
            ("group_base_dn = \"dc=example\"\ngroup_filter = \"(&(member={dn})(cn=vpn))\"\n", "required_groups"),
            ("group_attribute = \"memberOf\"\n", "required_groups"),
            ("required_groups = []\n", "required_groups"),
            ("[[auth.handlers.group_reply]]\ngroup = \"cn=x\"\n", "attributes"),
        ] {
            let err = error(&ldap_config(&format!("{LDAP_INLINE}{setting}")));
            assert!(err.contains(expected), "{setting}: {err}");
        }
    }

    const LDAP_INLINE: &str = "bind_password = \"pw\"\nfilter = \"(uid={user})\"\n";
    const LDAP_REQUIRED: &str = "bind_password_file = \"ldap.secret\"\nfilter = \"(uid={user})\"\n";

    #[test]
    fn test_ldap_handler_defaults() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("ldap.secret"), "service-pw\r\n").unwrap();
        let path = dir.path().join("rradius.toml");
        fs::write(&path, ldap_config(LDAP_REQUIRED)).unwrap();
        let auth = load(&path).unwrap().auth.unwrap().default.unwrap();
        assert_eq!(
            auth.handlers,
            vec![AuthHandlerSettings::Ldap(LdapSettings {
                urls: vec!["ldaps://dc1.example.com".to_string()],
                bind_dn: "cn=radius,ou=services,dc=example,dc=com".to_string(),
                bind_password: "service-pw".to_string(),
                base_dn: "ou=people,dc=example,dc=com".to_string(),
                filter: "(uid={user})".to_string(),
                certificate_filter: None,
                password_check: PasswordCheck::Bind,
                reply_attributes: Vec::new(),
                timeout: Duration::from_millis(5000),
                ca_file: None,
                allow_plaintext: false,
                pool_size: 4,
                groups: LdapGroups::default(),
            })]
        );
    }

    #[test]
    fn test_ldap_handler_options() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("ca.pem"), "").unwrap();
        let path = dir.path().join("rradius.toml");
        fs::write(
            &path,
            ldap_config(
                "bind_password = \"pw\"\nfilter = \"(uid={user})\"\npassword_check = \"attribute\"\n\
                 password_attribute = \"userPassword\"\nreply_attributes = { \"Filter-Id\" = \"radiusFilterId\", \"Class\" = \"radiusClass\" }\n\
                 timeout_ms = 250\nca_file = \"ca.pem\"\nallow_plaintext = true\npool_size = 16\n",
            ),
        )
        .unwrap();
        let auth = load(&path).unwrap().auth.unwrap().default.unwrap();
        let AuthHandlerSettings::Ldap(ldap) = &auth.handlers[0] else { panic!("not an ldap handler") };
        assert_eq!(ldap.password_check, PasswordCheck::Attribute("userPassword".to_string()));
        // Configuration order, not alphabetical.
        assert_eq!(
            ldap.reply_attributes,
            vec![
                ("Filter-Id".to_string(), "radiusFilterId".to_string()),
                ("Class".to_string(), "radiusClass".to_string()),
            ]
        );
        assert_eq!(ldap.timeout, Duration::from_millis(250));
        assert_eq!(ldap.ca_file, Some(dir.path().join("ca.pem")));
        assert!(ldap.allow_plaintext);
        assert_eq!(ldap.pool_size, 16);
    }

    #[test]
    fn test_ldap_handler_validation_errors() {
        let pw = "bind_password = \"pw\"\n";
        let filter = "filter = \"(uid={user})\"\n";
        let cases = [
            (format!("urls = []\n{pw}{filter}"), "urls"),
            (format!("{pw}{filter}"), ""),
            (format!("{pw}filter = \"(cn=admin)\"\n"), "filter"),
            (format!("{pw}filter = \"(|(uid={{user}})(mail={{user}}))\"\n"), "filter"),
            (format!("{pw}{filter}password_check = \"attribute\"\n"), "password_attribute"),
            (format!("{pw}bind_password_file = \"ldap.secret\"\n{filter}"), "bind_password"),
            (filter.to_string(), "bind_password"),
            (format!("{pw}{filter}timeout_ms = 0\n"), "timeout_ms"),
            (format!("{pw}{filter}pool_size = 0\n"), "pool_size"),
            (format!("{pw}{filter}ca_file = \"missing.pem\"\n"), "ca_file"),
            (format!("{pw}{filter}password_check = \"compare\"\n"), "`bind` or `attribute`"),
        ];
        for (fields, field) in cases {
            // `urls = []` duplicates the key from LDAP_HANDLER; swap it in instead.
            let body = if let Some(rest) = fields.strip_prefix("urls = []\n") {
                ldap_config(rest).replace("urls = [\"ldaps://dc1.example.com\"]", "urls = []")
            } else {
                ldap_config(&fields)
            };
            let result = load_str(&body).1;
            if field.is_empty() {
                assert!(result.is_ok(), "valid config rejected: {:?}", result.err().map(|e| e.to_string()));
                continue;
            }
            let err = result.unwrap_err().to_string();
            assert!(err.contains(field), "expected {field} in: {err}");
            if field != "`bind` or `attribute`" {
                assert!(err.contains("auth.handlers[0]"), "expected handler index in: {err}");
            }
        }
        let err = error(&ldap_config(&format!("{pw}{filter}")).replace("ldaps://dc1", "http://dc1"));
        assert!(err.contains("auth.handlers[0].urls") && err.contains("http://dc1"), "unexpected error: {err}");
    }
}
