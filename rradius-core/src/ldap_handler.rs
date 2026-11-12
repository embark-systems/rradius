//! The `ldap` auth chain handler: finds the user with a service account and
//! verifies the password by binding as the user or by reading a password
//! attribute.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deadpool::managed::{self, Object, Pool, PoolError, RecycleError};
use ldap3::{Ldap, LdapConnAsync, LdapConnSettings, Scope, SearchEntry};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

use crate::auth_chain::{Credentials, Secret};
use crate::common::{AttrValue, Outcome};

/// LDAP result code for a failed bind with a wrong password.
const INVALID_CREDENTIALS: u32 = 49;

/// How the user's password is verified once the entry is found.
#[derive(Debug, Clone, PartialEq)]
pub enum PasswordCheck {
    /// Bind as the found entry with the given password.
    Bind,
    /// Compare against the first value of the named attribute.
    Attribute(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LdapSettings {
    pub urls: Vec<String>,
    pub bind_dn: String,
    pub bind_password: String,
    pub base_dn: String,
    /// Search filter with exactly one `{user}` placeholder.
    pub filter: String,
    /// What a verified certificate is searched with instead, also with one `{user}`. Without
    /// it certificates get nowhere here: a search alone cannot tell that an account is locked.
    pub certificate_filter: Option<String>,
    pub password_check: PasswordCheck,
    /// `(reply attribute name, LDAP attribute name)` in configuration order.
    pub reply_attributes: Vec<(String, String)>,
    /// Limit for connecting to, and the whole exchange with, one URL.
    pub timeout: Duration,
    /// Extra trusted CA certificates (PEM) on top of the system roots.
    pub ca_file: Option<PathBuf>,
    /// Allows `ldap://` without StartTLS.
    pub allow_plaintext: bool,
    /// Most open connections kept per URL.
    pub pool_size: usize,
    pub groups: LdapGroups,
}

/// What a user's groups decide, and where to find them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LdapGroups {
    /// The user has to belong to one of these; empty asks for none.
    pub required: Vec<String>,
    pub source: GroupSource,
    /// `(group, reply attributes)` in configuration order, for the groups the user is in.
    pub reply: Vec<(String, Vec<(String, String)>)>,
}

impl LdapGroups {
    /// Whether anything depends on the user's groups, so that they have to be looked up.
    fn in_use(&self) -> bool {
        !self.required.is_empty() || !self.reply.is_empty()
    }
}

/// Where a user's groups come from.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupSource {
    /// An attribute of the user's entry.
    Attribute(String),
    /// The entries a search finds; `filter` has one `{dn}`.
    Search { base_dn: String, filter: String },
}

impl Default for GroupSource {
    fn default() -> Self {
        GroupSource::Attribute("memberOf".to_string())
    }
}

/// Whether two DNs name the same entry, for how directories tend to differ in writing one:
/// case and spaces around commas. Anything else counts as different, which rejects.
fn same_dn(a: &str, b: &str) -> bool {
    let parts = |dn: &str| dn.split(',').map(|part| part.trim().to_ascii_lowercase()).collect::<Vec<_>>();
    parts(a) == parts(b)
}

/// Replaces `{user}` in `filter` with `user` escaped per RFC 4515, so a user
/// name cannot change the structure of the filter.
pub fn build_filter(filter: &str, user: &str) -> String {
    filter.replace("{user}", &ldap3::ldap_escape(user))
}

type Decision = (Outcome, Vec<(String, AttrValue)>);

/// What a session on a connection found out.
enum Checked {
    Decided(Decision),
    /// The password to compare once the connection is released, and the attributes to send.
    Stored { stored: String, reply: Vec<(String, AttrValue)>, allowed: bool },
}

/// Why one URL did not produce a decision.
enum Failure {
    /// Connection or TLS failure, a connection that broke, or timeout: try the next URL.
    Unreachable(String),
    /// The server answered that the service account or search failed.
    Fatal(String),
}

impl Failure {
    /// An error from the library. The server's answers are `LdapResult`; the rest means the
    /// connection broke, which says nothing of the other servers.
    fn of(what: &str, error: ldap3::LdapError) -> Self {
        use ldap3::LdapError as E;
        let message = format!("{what}: {error}");
        match error {
            E::Io { .. } | E::EndOfStream | E::OpSend { .. } | E::ResultRecv { .. } | E::IdScrubSend { .. } | E::Timeout { .. } => Failure::Unreachable(message),
            _ => Failure::Fatal(message),
        }
    }
}

/// Opens connections to one URL, each already bound as the service account.
struct LdapManager {
    url: String,
    conn_settings: LdapConnSettings,
    bind_dn: String,
    bind_password: String,
    /// Limit for the reuse check, so a silently dropped connection leaves
    /// time to open a new one within the request's timeout.
    recycle_timeout: Duration,
}

impl LdapManager {
    async fn service_bind(&self, ldap: &mut Ldap) -> Result<(), Failure> {
        match ldap.simple_bind(&self.bind_dn, &self.bind_password).await {
            Ok(r) if r.rc == 0 => Ok(()),
            Ok(r) => Err(Failure::Fatal(format!("service account bind failed: {}", r))),
            Err(e) => Err(Failure::of("service account bind failed", e)),
        }
    }
}

impl managed::Manager for LdapManager {
    type Type = Ldap;
    type Error = Failure;

    async fn create(&self) -> Result<Ldap, Failure> {
        let (conn, mut ldap) = LdapConnAsync::with_settings(self.conn_settings.clone(), &self.url)
            .await
            .map_err(|e| Failure::Unreachable(e.to_string()))?;
        ldap3::drive!(conn);
        self.service_bind(&mut ldap).await?;
        Ok(ldap)
    }

    /// Binds as the service account again, so a previous user bind does not
    /// carry over and a dead connection is noticed before it is used.
    async fn recycle(&self, ldap: &mut Ldap, _: &managed::Metrics) -> managed::RecycleResult<Failure> {
        if ldap.is_closed() {
            return Err(RecycleError::message("connection closed"));
        }
        ldap.with_timeout(self.recycle_timeout);
        self.service_bind(ldap).await.map_err(|failure| match failure {
            Failure::Unreachable(message) | Failure::Fatal(message) => RecycleError::message(message),
        })
    }
}

/// An `ldap` handler with a connection pool per URL, sharing one TLS
/// configuration.
pub struct LdapHandler {
    settings: LdapSettings,
    pools: Vec<(String, Pool<LdapManager>)>,
}

impl LdapHandler {
    /// Builds the TLS configuration from the system roots plus `ca_file` and
    /// an empty pool for each URL; no connection is opened yet.
    pub fn new(settings: LdapSettings) -> Result<Self, String> {
        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        roots.add_parsable_certificates(native.certs);
        if let Some(path) = &settings.ca_file {
            let certs = CertificateDer::pem_file_iter(path)
                .map_err(|e| format!("cannot read ca_file {}: {}", path.display(), e))?;
            for cert in certs {
                let cert = cert.map_err(|e| format!("cannot parse ca_file {}: {}", path.display(), e))?;
                roots
                    .add(cert)
                    .map_err(|e| format!("invalid certificate in ca_file {}: {}", path.display(), e))?;
            }
        }
        // Both rustls crypto providers are compiled in, so name one explicitly.
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS configuration error: {}", e))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tls = Arc::new(tls);

        let mut pools = Vec::with_capacity(settings.urls.len());
        for url in &settings.urls {
            let manager = LdapManager {
                url: url.clone(),
                conn_settings: LdapConnSettings::new()
                    .set_conn_timeout(settings.timeout)
                    .set_config(tls.clone())
                    .set_starttls(url.starts_with("ldap://") && !settings.allow_plaintext),
                bind_dn: settings.bind_dn.clone(),
                bind_password: settings.bind_password.clone(),
                recycle_timeout: settings.timeout / 2,
            };
            // No deadpool timeouts: `attempt` bounds the whole exchange itself.
            let pool = Pool::builder(manager)
                .max_size(settings.pool_size)
                .build()
                .map_err(|e| format!("cannot create LDAP connection pool for {}: {}", url, e))?;
            pools.push((url.clone(), pool));
        }
        Ok(LdapHandler { settings, pools })
    }

    /// Authenticates under the name in `creds`.
    pub async fn authenticate(&self, creds: &Credentials) -> Decision {
        self.authenticate_account(creds, creds.user.as_deref()).await
    }

    /// Tries each URL in order until one gives a decision; server errors reject. The search
    /// uses `account`; a stored password is checked with the name in `creds`, which is the
    /// one an MS-CHAPv2 response was computed over.
    pub async fn authenticate_account(&self, creds: &Credentials, account: Option<&str>) -> Decision {
        let (Some(user), Some(account)) = (creds.user.as_deref(), account) else {
            return (Outcome::Ignore, Vec::new());
        };
        if creds.secret == Secret::Certificate && self.settings.certificate_filter.is_none() {
            tracing::warn!("auth ldap handler: ignoring a certificate login: this handler has no certificate_filter");
            return (Outcome::Ignore, Vec::new());
        }
        for (url, pool) in &self.pools {
            match self.attempt(pool, account, &creds.secret).await {
                Ok(Checked::Decided(decision)) => return decision,
                Ok(Checked::Stored { stored, reply, allowed }) => return self.verify_stored(&stored, user, &creds.secret, reply, allowed).await,
                Err(Failure::Fatal(e)) => {
                    crate::lua_engine::log_line(tracing::Level::ERROR, format!("auth ldap handler: {}: {}; rejecting", url, e));
                    return (Outcome::Reject, Vec::new());
                }
                Err(Failure::Unreachable(e)) => tracing::warn!("auth ldap handler: {}: {}", url, e),
            }
        }
        tracing::error!("auth ldap handler: no server reachable; rejecting");
        (Outcome::Reject, Vec::new())
    }

    /// Takes a connection and runs the search and password check, all within
    /// `timeout`. A connection that failed or timed out is discarded rather
    /// than returned to the pool.
    async fn attempt(&self, pool: &Pool<LdapManager>, user: &str, secret: &Secret) -> Result<Checked, Failure> {
        let deadline = tokio::time::Instant::now() + self.settings.timeout;
        let mut conn = match tokio::time::timeout_at(deadline, pool.get()).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(PoolError::Backend(failure))) => return Err(failure),
            Ok(Err(_)) => return Err(Failure::Unreachable("connection pool error".to_string())),
            Err(_) => return Err(Failure::Unreachable("timed out".to_string())),
        };
        match tokio::time::timeout_at(deadline, self.session(&mut conn, user, secret)).await {
            Ok(Ok(checked)) => Ok(checked),
            Ok(Err(failure)) => {
                drop(Object::take(conn));
                Err(failure)
            }
            Err(_) => {
                drop(Object::take(conn));
                Err(Failure::Unreachable("timed out".to_string()))
            }
        }
    }

    /// Compares a stored password once the connection is back in the pool, so a slow hash
    /// does not hold one, and a timeout here is not mistaken for an unreachable server.
    async fn verify_stored(&self, stored: &str, user: &str, secret: &Secret, reply: Vec<(String, AttrValue)>, allowed: bool) -> Decision {
        match tokio::time::timeout(self.settings.timeout, crate::auth_chain::verify_secret(stored, user, secret)).await {
            Ok(Some(_)) if !allowed => (Outcome::Reject, Vec::new()),
            Ok(Some(mut attributes)) => {
                attributes.extend(reply);
                (Outcome::Accept, attributes)
            }
            Ok(None) => (Outcome::Reject, Vec::new()),
            Err(_) => {
                tracing::error!(user, "auth ldap handler: verifying the password timed out; rejecting");
                (Outcome::Reject, Vec::new())
            }
        }
    }

    /// Searches for the user on a connection bound as the service account, and either
    /// decides or returns the stored password to compare.
    async fn session(&self, ldap: &mut Ldap, user: &str, secret: &Secret) -> Result<Checked, Failure> {
        let s = &self.settings;
        let mut attrs: Vec<&str> = s.reply_attributes.iter().map(|(_, a)| a.as_str()).collect();
        if let PasswordCheck::Attribute(a) = &s.password_check {
            attrs.push(a);
        }
        if let (true, GroupSource::Attribute(a)) = (s.groups.in_use(), &s.groups.source) {
            attrs.push(a);
        }
        if attrs.is_empty() {
            // "1.1" asks for no attributes; an empty list would return all.
            attrs.push("1.1");
        }
        let filter = match (secret, &s.certificate_filter) {
            (Secret::Certificate, Some(certificate_filter)) => certificate_filter,
            _ => &s.filter,
        };
        let filter = build_filter(filter, user);
        let (mut entries, _) = ldap
            .search(&s.base_dn, Scope::Subtree, &filter, attrs)
            .await
            .and_then(|r| r.success())
            .map_err(|e| Failure::of("search failed", e))?;
        if entries.is_empty() {
            return Ok(Checked::Decided((Outcome::Ignore, Vec::new())));
        }
        if entries.len() > 1 {
            tracing::error!(filter = filter.as_str(), "auth ldap handler: filter matched {} entries; rejecting", entries.len());
            return Ok(Checked::Decided((Outcome::Reject, Vec::new())));
        }
        let entry = SearchEntry::construct(entries.remove(0));

        let mut reply: Vec<(String, AttrValue)> = s
            .reply_attributes
            .iter()
            .flat_map(|(name, a)| values(&entry, a).into_iter().map(move |v| (name.clone(), AttrValue::Bytes(v))))
            .collect();
        // Now, as the service account: a user bind may not be allowed to search. What it
        // means waits for the password check, so that a refusal says nothing about either.
        let groups = if s.groups.in_use() { self.user_groups(ldap, &entry).await? } else { Vec::new() };
        let is_in = |group: &str| groups.iter().any(|name| same_dn(name, group));
        let allowed = s.groups.required.is_empty() || s.groups.required.iter().any(|group| is_in(group));
        for (_, attributes) in s.groups.reply.iter().filter(|(group, _)| is_in(group)) {
            reply.extend(attributes.iter().map(|(name, value)| (name.clone(), AttrValue::Bytes(value.clone().into_bytes()))));
        }
        // EAP-TLS proved the name. The directory still says whether it may log in.
        if *secret == Secret::Certificate {
            return Ok(Checked::Decided(if allowed { (Outcome::Accept, reply) } else { (Outcome::Reject, Vec::new()) }));
        }
        match &s.password_check {
            PasswordCheck::Bind => {
                let decision = if bind_check(ldap, &entry.dn, secret).await? && allowed { (Outcome::Accept, reply) } else { (Outcome::Reject, Vec::new()) };
                Ok(Checked::Decided(decision))
            }
            PasswordCheck::Attribute(a) => Ok(match stored_password(&entry, a) {
                Some(stored) => Checked::Stored { stored, reply, allowed },
                None => Checked::Decided((Outcome::Reject, Vec::new())),
            }),
        }
    }
}

impl LdapHandler {
    /// The DNs of the groups `entry` belongs to.
    async fn user_groups(&self, ldap: &mut Ldap, entry: &SearchEntry) -> Result<Vec<String>, Failure> {
        match &self.settings.groups.source {
            GroupSource::Attribute(attribute) => {
                Ok(values(entry, attribute).into_iter().map(|value| String::from_utf8_lossy(&value).into_owned()).collect())
            }
            GroupSource::Search { base_dn, filter } => {
                let filter = filter.replace("{dn}", &ldap3::ldap_escape(entry.dn.as_str()));
                let (entries, _) = ldap
                    .search(base_dn, Scope::Subtree, &filter, vec!["1.1"])
                    .await
                    .and_then(|r| r.success())
                    .map_err(|e| Failure::of("group search failed", e))?;
                Ok(entries.into_iter().map(|entry| SearchEntry::construct(entry).dn).collect())
            }
        }
    }
}

/// Binds as `dn`; only non-empty PAP passwords are tried, because an empty
/// password makes an unauthenticated bind that succeeds (RFC 4513 5.1.2).
/// A wrong password is `Ok(false)`; any other bind error is a server error,
/// which rejects and discards the connection.
async fn bind_check(ldap: &mut Ldap, dn: &str, secret: &Secret) -> Result<bool, Failure> {
    let Secret::Pap(pw) = secret else { return Ok(false) };
    let Ok(pw) = std::str::from_utf8(pw) else { return Ok(false) };
    if pw.is_empty() {
        return Ok(false);
    }
    match ldap.simple_bind(dn, pw).await {
        Ok(r) if r.rc == 0 => Ok(true),
        Ok(r) if r.rc == INVALID_CREDENTIALS => Ok(false),
        Ok(r) => Err(Failure::Fatal(format!("bind as {} failed: {}", dn, r))),
        Err(e) => Err(Failure::of(&format!("bind as {dn} failed"), e)),
    }
}

/// The entry's stored password, if it has a usable one.
fn stored_password(entry: &SearchEntry, attr: &str) -> Option<String> {
    let stored = values(entry, attr).into_iter().next()?;
    String::from_utf8(stored).ok().filter(|stored| !stored.is_empty())
}

/// The values of `attr` in the order the server sent them; attribute names
/// are case-insensitive.
fn values(entry: &SearchEntry, attr: &str) -> Vec<Vec<u8>> {
    if let Some((_, v)) = entry.attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(attr)) {
        return v.iter().map(|s| s.as_bytes().to_vec()).collect();
    }
    if let Some((_, v)) = entry.bin_attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(attr)) {
        return v.clone();
    }
    Vec::new()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use ldap3_proto::proto::{LdapFilter, LdapPartialAttribute, LdapSearchResultEntry};
    use ldap3_proto::{LdapCodec, ServerOps};
    use std::sync::Mutex;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::codec::{FramedRead, FramedWrite};

    const SERVICE_DN: &str = "cn=radius,ou=services,dc=example,dc=com";
    const SERVICE_PW: &str = "service-pw";

    struct MockEntry {
        dn: &'static str,
        /// Password accepted by a bind as `dn`, if any.
        bind_pw: Option<&'static str>,
        attrs: Vec<(&'static str, Vec<&'static str>)>,
    }

    fn directory() -> Vec<MockEntry> {
        vec![
            MockEntry {
                dn: "uid=alice,ou=people,dc=example,dc=com",
                bind_pw: Some("secret"),
                attrs: vec![("uid", vec!["alice"]), ("radiusFilterId", vec!["std.ppp", "extra"])],
            },
            MockEntry {
                dn: "uid=bob,ou=people,dc=example,dc=com",
                bind_pw: None,
                attrs: vec![("uid", vec!["bob"]), ("userPassword", vec!["{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g="])],
            },
            MockEntry { dn: "uid=carol,ou=people,dc=example,dc=com", bind_pw: None, attrs: vec![("uid", vec!["carol"])] },
            MockEntry { dn: "uid=erin,ou=people,dc=example,dc=com", bind_pw: Some("erin-pw"), attrs: vec![("uid", vec!["erin"]), ("memberOf", vec!["CN=VPN, OU=Groups, DC=example, DC=com"])] },
            MockEntry { dn: STAR_DN, bind_pw: Some("star-pw"), attrs: vec![("uid", vec!["star"])] },
            MockEntry { dn: "cn=vpn,ou=groups,dc=example,dc=com", bind_pw: None, attrs: vec![("member", vec![ALICE_DN, "uid=dave,ou=people,dc=example,dc=com", STAR_DN])] },
            MockEntry { dn: "cn=staff,ou=groups,dc=example,dc=com", bind_pw: None, attrs: vec![("member", vec![ALICE_DN])] },
            MockEntry {
                dn: "uid=dave,ou=people,dc=example,dc=com",
                bind_pw: None,
                attrs: vec![("uid", vec!["dave"]), ("userPassword", vec!["{clear}dave-pw"]), ("memberOf", vec!["cn=vpn,ou=groups,dc=example,dc=com"])],
            },
            MockEntry { dn: "uid=twin,ou=a,dc=example,dc=com", bind_pw: Some("x"), attrs: vec![("uid", vec!["twin"])] },
            MockEntry { dn: "uid=twin,ou=b,dc=example,dc=com", bind_pw: Some("x"), attrs: vec![("uid", vec!["twin"])] },
        ]
    }

    /// What the mock server received.
    #[derive(Default)]
    pub(crate) struct MockLog {
        pub(crate) binds: Vec<String>,
        pub(crate) filters: Vec<LdapFilter>,
        attributes_asked: Vec<Vec<String>>,
        /// Binds by DN and searches as `search`, in order.
        events: Vec<String>,
        /// Connections accepted.
        pub(crate) connections: usize,
        /// Searches still to be delayed by `MockOptions::search_delay`.
        slow_searches: usize,
        /// User binds still to be answered with operationsError.
        failing_user_binds: usize,
    }

    #[derive(Clone, Copy, Default)]
    struct MockOptions {
        /// Close the connection after answering a bind as a user.
        close_after_user_bind: bool,
        /// How many of the first searches are answered late.
        slow_searches: usize,
        search_delay: Duration,
        /// How many of the first user binds fail with operationsError.
        failing_user_binds: usize,
        /// Stop answering, without closing, after answering a bind as a user.
        stall_after_user_bind: bool,
        /// Answer searches on `member` with an LDAP error.
        failing_member_search: bool,
        /// Close the connection, without answering, on a bind as the service account.
        close_on_service_bind: bool,
        /// Close the connection, without answering, on a search.
        close_on_search: bool,
        /// Close the connection, without answering, on a bind as a user.
        close_on_user_bind: bool,
    }

    /// A plain-text LDAP server answering Bind, Search and Unbind from
    /// `directory()`. Returns its URL and the request log.
    pub(crate) async fn mock_server() -> (String, Arc<Mutex<MockLog>>) {
        mock_server_with(MockOptions::default()).await
    }

    async fn mock_server_with(options: MockOptions) -> (String, Arc<Mutex<MockLog>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ldap://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(MockLog {
            slow_searches: options.slow_searches,
            failing_user_binds: options.failing_user_binds,
            ..MockLog::default()
        }));
        let server_log = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else { return };
                server_log.lock().unwrap().connections += 1;
                tokio::spawn(serve(socket, server_log.clone(), options));
            }
        });
        (url, log)
    }

    async fn serve(socket: TcpStream, log: Arc<Mutex<MockLog>>, options: MockOptions) {
        let (r, w) = socket.into_split();
        let mut reader = FramedRead::new(r, LdapCodec::new(None, None));
        let mut writer = FramedWrite::new(w, LdapCodec::new(None, None));
        let dir = directory();
        while let Some(Ok(msg)) = reader.next().await {
            let Ok(op) = ServerOps::try_from(msg) else { return };
            match op {
                ServerOps::SimpleBind(req) => {
                    if (options.close_on_service_bind && req.dn == SERVICE_DN) || (options.close_on_user_bind && req.dn != SERVICE_DN) {
                        return;
                    }
                    let failing = {
                        let mut log = log.lock().unwrap();
                        log.binds.push(req.dn.clone());
                        log.events.push(req.dn.clone());
                        let failing = req.dn != SERVICE_DN && log.failing_user_binds > 0;
                        if failing {
                            log.failing_user_binds -= 1;
                        }
                        failing
                    };
                    let ok = (req.dn == SERVICE_DN && req.pw == SERVICE_PW)
                        || dir.iter().any(|e| e.dn == req.dn && e.bind_pw == Some(req.pw.as_str()));
                    let reply = if failing {
                        req.gen_operror("simulated failure")
                    } else if ok {
                        req.gen_success()
                    } else {
                        req.gen_invalid_cred()
                    };
                    if writer.send(reply).await.is_err() || (options.close_after_user_bind && req.dn != SERVICE_DN) {
                        return;
                    }
                    if options.stall_after_user_bind && req.dn != SERVICE_DN {
                        // Like a connection a firewall dropped silently: open, never answering.
                        std::future::pending::<()>().await;
                    }
                }
                ServerOps::Search(req) => {
                    if options.close_on_search {
                        return;
                    }
                    let slow = {
                        let mut log = log.lock().unwrap();
                        log.filters.push(req.filter.clone());
                        log.attributes_asked.push(req.attrs.clone());
                        log.events.push("search".to_string());
                        let slow = log.slow_searches > 0;
                        log.slow_searches = log.slow_searches.saturating_sub(1);
                        slow
                    };
                    if slow {
                        tokio::time::sleep(options.search_delay).await;
                    }
                    let LdapFilter::Equality(attr, value) = &req.filter else { return };
                    if options.failing_member_search && attr == "member" {
                        if writer.send(req.gen_error(ldap3_proto::LdapResultCode::OperationsError, "no".to_string())).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    let matches = dir.iter().filter(|e| {
                        e.attrs.iter().any(|(a, vs)| a.eq_ignore_ascii_case(attr) && vs.contains(&value.as_str()))
                    });
                    for e in matches {
                        let attributes = e
                            .attrs
                            .iter()
                            .map(|(a, vs)| LdapPartialAttribute {
                                atype: a.to_string(),
                                vals: vs.iter().map(|v| v.as_bytes().to_vec()).collect(),
                            })
                            .collect();
                        let entry = req.gen_result_entry(LdapSearchResultEntry { dn: e.dn.to_string(), attributes });
                        if writer.send(entry).await.is_err() {
                            return;
                        }
                    }
                    if writer.send(req.gen_success()).await.is_err() {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    /// A URL on a port with nothing listening.
    async fn refused_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("ldap://{}", addr)
    }

    /// A URL whose server accepts connections and never answers.
    async fn silent_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ldap://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        url
    }

    pub(crate) fn settings(urls: Vec<String>, password_check: PasswordCheck) -> LdapSettings {
        LdapSettings {
            urls,
            bind_dn: SERVICE_DN.to_string(),
            bind_password: SERVICE_PW.to_string(),
            base_dn: "dc=example,dc=com".to_string(),
            filter: "(uid={user})".to_string(),
            certificate_filter: None,
            password_check,
            reply_attributes: Vec::new(),
            timeout: Duration::from_secs(5),
            ca_file: None,
            allow_plaintext: true,
            pool_size: 4,
            groups: LdapGroups::default(),
        }
    }

    fn pap(user: &str, pw: &str) -> Credentials {
        Credentials { user: Some(user.to_string()), secret: Secret::pap(pw.as_bytes().to_vec()) }
    }

    async fn run(settings: LdapSettings, creds: &Credentials) -> Decision {
        LdapHandler::new(settings).unwrap().authenticate(creds).await
    }

    #[test]
    fn test_build_filter_escapes_user() {
        assert_eq!(build_filter("(uid={user})", "a*)(uid=*"), r"(uid=a\2a\29\28uid=\2a)");
        assert_eq!(build_filter("(uid={user})", "back\\slash"), r"(uid=back\5cslash)");
        assert_eq!(build_filter("(uid={user})", "nul\0byte"), r"(uid=nul\00byte)");
        assert_eq!(build_filter("(&(objectClass=person)(uid={user}))", "alice"), "(&(objectClass=person)(uid=alice))");
    }

    #[tokio::test]
    async fn test_bind_check_accepts_right_password() {
        let (url, log) = mock_server().await;
        let (outcome, attrs) = run(settings(vec![url], PasswordCheck::Bind), &pap("alice", "secret")).await;
        assert_eq!(outcome, Outcome::Accept);
        assert!(attrs.is_empty());
        assert_eq!(log.lock().unwrap().binds, vec![SERVICE_DN, "uid=alice,ou=people,dc=example,dc=com"]);
    }

    #[tokio::test]
    async fn test_bind_check_rejects_wrong_password() {
        let (url, _) = mock_server().await;
        let (outcome, _) = run(settings(vec![url], PasswordCheck::Bind), &pap("alice", "wrong")).await;
        assert_eq!(outcome, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_empty_password_rejected_without_user_bind() {
        let (url, log) = mock_server().await;
        let (outcome, _) = run(settings(vec![url], PasswordCheck::Bind), &pap("alice", "")).await;
        assert_eq!(outcome, Outcome::Reject);
        assert_eq!(log.lock().unwrap().binds, vec![SERVICE_DN]);
    }

    #[tokio::test]
    async fn test_chap_with_bind_check_rejected() {
        let (url, log) = mock_server().await;
        let creds = Credentials {
            user: Some("alice".to_string()),
            secret: Secret::Chap { id: 1, response: vec![0; 16], challenge: vec![0; 16] },
        };
        let (outcome, _) = run(settings(vec![url], PasswordCheck::Bind), &creds).await;
        assert_eq!(outcome, Outcome::Reject);
        assert_eq!(log.lock().unwrap().binds, vec![SERVICE_DN]);
    }

    #[tokio::test]
    async fn test_attribute_check_with_mschapv2() {
        let entry = SearchEntry {
            dn: "uid=User".to_string(),
            attrs: [("userPassword".to_string(), vec!["clientPass".to_string()])].into(),
            bin_attrs: Default::default(),
        };
        let secret = Secret::MsChapV2 {
            ident: 7,
            peer_challenge: [0x21, 0x40, 0x23, 0x24, 0x25, 0x5E, 0x26, 0x2A, 0x28, 0x29, 0x5F, 0x2B, 0x3A, 0x33, 0x7C, 0x7E],
            challenge: [0x5B, 0x5D, 0x7C, 0x7D, 0x7B, 0x3F, 0x2F, 0x3E, 0x3C, 0x2C, 0x60, 0x21, 0x32, 0x26, 0x26, 0x28],
            response: [0x82, 0x30, 0x9E, 0xCD, 0x8D, 0x70, 0x8B, 0x5E, 0xA0, 0x8F, 0xAA, 0x39, 0x81, 0xCD, 0x83, 0x54, 0x42, 0x33, 0x11, 0x4A, 0x3D, 0x85, 0xD6, 0xDF],
        };
        let stored = stored_password(&entry, "userPassword").unwrap();
        let attributes = crate::auth_chain::verify_secret(&stored, "User", &secret).await.expect("not verified");
        assert_eq!(attributes[0].0, "MS-CHAP2-Success");
        assert!(crate::auth_chain::verify_secret(&stored, "Other", &secret).await.is_none());
    }

    #[tokio::test]
    async fn test_attribute_check() {
        let (url, _) = mock_server().await;
        let s = settings(vec![url], PasswordCheck::Attribute("userPassword".to_string()));
        assert_eq!(run(s.clone(), &pap("bob", "password")).await.0, Outcome::Accept);
        assert_eq!(run(s, &pap("bob", "wrong")).await.0, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_attribute_check_missing_attribute_rejected() {
        let (url, _) = mock_server().await;
        let s = settings(vec![url], PasswordCheck::Attribute("userPassword".to_string()));
        assert_eq!(run(s, &pap("carol", "anything")).await.0, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_unknown_user_ignored() {
        let (url, _) = mock_server().await;
        let (outcome, attrs) = run(settings(vec![url], PasswordCheck::Bind), &pap("nobody", "pw")).await;
        assert_eq!(outcome, Outcome::Ignore);
        assert!(attrs.is_empty());
    }

    #[tokio::test]
    async fn test_no_user_ignored() {
        let creds = Credentials { user: None, secret: Secret::None };
        let (outcome, _) = run(settings(vec![refused_url().await], PasswordCheck::Bind), &creds).await;
        assert_eq!(outcome, Outcome::Ignore);
    }

    #[tokio::test]
    async fn test_several_entries_rejected() {
        let (url, log) = mock_server().await;
        let (outcome, _) = run(settings(vec![url], PasswordCheck::Bind), &pap("twin", "x")).await;
        assert_eq!(outcome, Outcome::Reject);
        assert_eq!(log.lock().unwrap().binds, vec![SERVICE_DN]);
    }

    #[tokio::test]
    async fn test_server_receives_escaped_filter() {
        let (url, log) = mock_server().await;
        let (outcome, _) = run(settings(vec![url], PasswordCheck::Bind), &pap("a*)(uid=*", "pw")).await;
        assert_eq!(outcome, Outcome::Ignore);
        // One equality match on the literal name, not a wildcard or a second clause.
        assert_eq!(
            log.lock().unwrap().filters,
            vec![LdapFilter::Equality("uid".to_string(), "a*)(uid=*".to_string())]
        );
    }

    /// A DN with what a search filter treats specially.
    const STAR_DN: &str = "uid=star*(x),ou=people,dc=example,dc=com";
    const VPN: &str = "cn=vpn,ou=groups,dc=example,dc=com";
    const STAFF: &str = "cn=staff,ou=groups,dc=example,dc=com";

    fn with_groups(url: String, check: PasswordCheck, groups: LdapGroups) -> LdapSettings {
        LdapSettings { groups, ..settings(vec![url], check) }
    }

    fn required(group: &str) -> LdapGroups {
        LdapGroups { required: vec![group.to_string()], ..LdapGroups::default() }
    }

    fn by_search(required_group: &str) -> LdapGroups {
        LdapGroups {
            required: vec![required_group.to_string()],
            source: GroupSource::Search { base_dn: "ou=groups,dc=example,dc=com".to_string(), filter: "(member={dn})".to_string() },
            reply: Vec::new(),
        }
    }

    /// `memberOf` as a directory might write it: other case, and spaces after the commas.
    #[test]
    fn test_group_names_compare_loosely() {
        assert!(same_dn("CN=VPN, OU=Groups , dc=example,DC=com", VPN));
        assert!(!same_dn("cn=vpn2,ou=groups,dc=example,dc=com", VPN));
        assert!(!same_dn("", VPN));
    }

    /// The server only returns what it is asked for, which the mock does not enforce.
    #[tokio::test]
    async fn test_group_attribute_is_asked_for() {
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(with_groups(url, PasswordCheck::Bind, required(VPN))).unwrap();
        handler.authenticate(&pap("erin", "erin-pw")).await;
        let log = log.lock().unwrap();
        assert!(log.attributes_asked[0].iter().any(|attribute| attribute == "memberOf"), "{:?}", log.attributes_asked);
    }

    #[tokio::test]
    async fn test_member_written_differently_by_the_directory() {
        let (url, _) = mock_server().await;
        let handler = LdapHandler::new(with_groups(url, PasswordCheck::Bind, required(VPN))).unwrap();
        assert_eq!(handler.authenticate(&pap("erin", "erin-pw")).await.0, Outcome::Accept);
    }

    #[tokio::test]
    async fn test_bind_not_a_member_looks_like_a_wrong_password() {
        let (url, _) = mock_server().await;
        let member = LdapHandler::new(with_groups(url.clone(), PasswordCheck::Bind, required(VPN))).unwrap();
        let stranger = LdapHandler::new(with_groups(url, PasswordCheck::Bind, required(STAFF))).unwrap();
        assert_eq!(member.authenticate(&pap("erin", "wrong")).await, stranger.authenticate(&pap("erin", "erin-pw")).await);
        assert_eq!(stranger.authenticate(&pap("erin", "erin-pw")).await.0, Outcome::Reject);
    }

    /// The filter reaches the server decoded, as one value: unescaped, it would not parse
    /// as an equality match and the mock would close the connection.
    #[tokio::test]
    async fn test_group_search_escapes_the_dn() {
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(with_groups(url, PasswordCheck::Bind, by_search(VPN))).unwrap();
        assert_eq!(handler.authenticate(&pap("star", "star-pw")).await.0, Outcome::Accept);
        assert_eq!(log.lock().unwrap().filters[1], LdapFilter::Equality("member".to_string(), STAR_DN.to_string()));
    }

    /// Only `group_reply` is set, so nothing but the failure itself can reject: treating it
    /// as having no groups would accept the user without their attributes.
    #[tokio::test]
    async fn test_group_search_error_rejects_without_required_groups() {
        let (url, log) = mock_server_with(MockOptions { failing_member_search: true, ..MockOptions::default() }).await;
        let groups = LdapGroups {
            required: Vec::new(),
            source: GroupSource::Search { base_dn: "ou=groups,dc=example,dc=com".to_string(), filter: "(member={dn})".to_string() },
            reply: vec![(STAFF.to_string(), vec![("Filter-Id".to_string(), "staff".to_string())])],
        };
        let handler = LdapHandler::new(with_groups(url.clone(), PasswordCheck::Bind, groups)).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);
        let logged = crate::test_support::logged_errors().lock().unwrap().clone();
        assert!(logged.iter().any(|line| line.contains(&url) && line.contains("group search failed")), "{logged:?}");
        // The failed connection is not used again.
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);
        assert_eq!(log.lock().unwrap().connections, 2);
    }

    #[tokio::test]
    async fn test_group_reply_comes_after_reply_attributes() {
        let (url, _) = mock_server().await;
        let groups = LdapGroups {
            required: Vec::new(),
            source: GroupSource::Search { base_dn: "ou=groups,dc=example,dc=com".to_string(), filter: "(member={dn})".to_string() },
            reply: vec![(STAFF.to_string(), vec![("Filter-Id".to_string(), "staff".to_string())])],
        };
        let settings = LdapSettings { reply_attributes: vec![("Reply-Message".to_string(), "uid".to_string())], ..with_groups(url, PasswordCheck::Bind, groups) };
        let (_, attributes) = LdapHandler::new(settings).unwrap().authenticate(&pap("alice", "secret")).await;
        assert_eq!(attributes, vec![
            ("Reply-Message".to_string(), AttrValue::Bytes(b"alice".to_vec())),
            ("Filter-Id".to_string(), AttrValue::Bytes(b"staff".to_vec())),
        ]);
    }

    #[tokio::test]
    async fn test_member_by_attribute() {
        let (url, _) = mock_server().await;
        let attribute = PasswordCheck::Attribute("userPassword".to_string());
        let handler = LdapHandler::new(with_groups(url.clone(), attribute.clone(), required(VPN))).unwrap();
        assert_eq!(handler.authenticate(&pap("dave", "dave-pw")).await.0, Outcome::Accept);
        assert_eq!(handler.authenticate(&pap("dave", "wrong")).await.0, Outcome::Reject);
        let other = LdapHandler::new(with_groups(url, attribute, required(STAFF))).unwrap();
        assert_eq!(other.authenticate(&pap("dave", "dave-pw")).await.0, Outcome::Reject, "the password is right but dave is not in the group");
    }

    #[tokio::test]
    async fn test_not_a_member_looks_like_a_wrong_password() {
        let (url, _) = mock_server().await;
        let attribute = PasswordCheck::Attribute("userPassword".to_string());
        let member = LdapHandler::new(with_groups(url.clone(), attribute.clone(), required(VPN))).unwrap();
        let stranger = LdapHandler::new(with_groups(url, attribute, required(STAFF))).unwrap();
        assert_eq!(member.authenticate(&pap("dave", "wrong")).await, stranger.authenticate(&pap("dave", "dave-pw")).await);
    }

    #[tokio::test]
    async fn test_member_by_search_before_the_user_bind() {
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(with_groups(url.clone(), PasswordCheck::Bind, by_search(VPN))).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        {
            let log = log.lock().unwrap();
            assert_eq!(log.filters[1], LdapFilter::Equality("member".to_string(), ALICE_DN.to_string()));
            assert_eq!(log.events.iter().map(String::as_str).collect::<Vec<_>>(), [SERVICE_DN, "search", "search", ALICE_DN], "the group search comes before the user bind");
        }
        let not_required = LdapHandler::new(with_groups(url, PasswordCheck::Bind, by_search("cn=other,dc=example,dc=com"))).unwrap();
        assert_eq!(not_required.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_group_search_failure_rejects() {
        let (url, _) = mock_server().await;
        // A filter the mock does not understand makes it close the connection.
        let groups = LdapGroups {
            required: vec![VPN.to_string()],
            source: GroupSource::Search { base_dn: "ou=groups,dc=example,dc=com".to_string(), filter: "(&(member={dn})(cn=*))".to_string() },
            reply: Vec::new(),
        };
        let handler = LdapHandler::new(with_groups(url, PasswordCheck::Bind, groups)).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);
    }

    /// A verified certificate needs no bind, and the directory still decides.
    #[tokio::test]
    async fn test_certificate_found_by_its_filter_needs_no_user_bind() {
        let certificate = Credentials { user: Some("alice".to_string()), secret: Secret::Certificate };
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(LdapSettings { certificate_filter: Some("(uid={user})".to_string()), ..settings(vec![url], PasswordCheck::Bind) }).unwrap();
        assert_eq!(handler.authenticate(&certificate).await.0, Outcome::Accept);
        let log = log.lock().unwrap();
        assert_eq!(log.filters, vec![LdapFilter::Equality("uid".to_string(), "alice".to_string())]);
        assert_eq!(log.binds, vec![SERVICE_DN.to_string()], "the certificate is the proof");
    }

    #[tokio::test]
    async fn test_failures_are_on_record() {
        let logged = |settings: LdapSettings, credentials: Credentials| async move {
            let mut outcome = None;
            let log = crate::radius::tests::logged_during(async { outcome = Some(run(settings, &credentials).await.0) }).await;
            assert_eq!(outcome, Some(Outcome::Reject), "{log}");
            log
        };
        let log = logged(settings(vec![refused_url().await], PasswordCheck::Bind), pap("alice", "secret")).await;
        assert!(log.contains("no server reachable; rejecting"), "{log}");
        let (url, _) = mock_server().await;
        let log = logged(settings(vec![url], PasswordCheck::Bind), pap("twin", "x")).await;
        assert!(log.contains("filter matched 2 entries; rejecting"), "{log}");
        let (url, _) = mock_server().await;
        let log = logged(LdapSettings { bind_password: "nope".to_string(), ..settings(vec![url], PasswordCheck::Bind) }, pap("alice", "secret")).await;
        assert!(log.contains("service account bind failed") && log.contains("rejecting"), "{log}");
        assert!(!log.contains("nope"), "{log}");
    }

    #[tokio::test]
    async fn test_certificate_needs_a_certificate_filter() {
        let certificate = Credentials { user: Some("alice".to_string()), secret: Secret::Certificate };
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap();
        assert_eq!(handler.authenticate(&certificate).await, (Outcome::Ignore, Vec::new()));
        assert!(log.lock().unwrap().filters.is_empty(), "no search");
    }

    #[tokio::test]
    async fn test_certificate_filter_replaces_the_filter() {
        let certificate = Credentials { user: Some("a*lice".to_string()), secret: Secret::Certificate };
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(LdapSettings { certificate_filter: Some("(cn={user})".to_string()), ..settings(vec![url], PasswordCheck::Bind) }).unwrap();
        assert_eq!(handler.authenticate(&certificate).await, (Outcome::Ignore, Vec::new()), "that filter finds nothing");
        assert_eq!(log.lock().unwrap().filters, vec![LdapFilter::Equality("cn".to_string(), "a*lice".to_string())], "escaped, and in place of filter");
    }

    #[tokio::test]
    async fn test_certificate_filter_leaves_passwords_alone() {
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(LdapSettings { certificate_filter: Some("(cn={user})".to_string()), ..settings(vec![url], PasswordCheck::Bind) }).unwrap();
        handler.authenticate(&Credentials { user: Some("alice".to_string()), secret: Secret::pap(b"x".to_vec()) }).await;
        assert_eq!(log.lock().unwrap().filters[0], LdapFilter::Equality("uid".to_string(), "alice".to_string()));
    }

    #[tokio::test]
    async fn test_certificate_needs_the_required_group() {
        let certificate = Credentials { user: Some("alice".to_string()), secret: Secret::Certificate };
        let groups = |required: &str| LdapGroups {
            required: vec![required.to_string()],
            source: GroupSource::Search { base_dn: "ou=groups,dc=example,dc=com".to_string(), filter: "(member={dn})".to_string() },
            reply: vec![(STAFF.to_string(), vec![("Filter-Id".to_string(), "staff".to_string())])],
        };
        let settings = |url: String, groups: LdapGroups| LdapSettings { certificate_filter: Some("(uid={user})".to_string()), ..with_groups(url, PasswordCheck::Bind, groups) };
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(settings(url, groups(STAFF))).unwrap();
        let binds_before = log.lock().unwrap().binds.len();
        let (outcome, attributes) = handler.authenticate(&certificate).await;
        assert_eq!(outcome, Outcome::Accept);
        assert_eq!(attributes, vec![("Filter-Id".to_string(), AttrValue::Bytes(b"staff".to_vec()))]);
        assert!(log.lock().unwrap().binds.iter().skip(binds_before).all(|dn| !dn.contains("alice")), "no bind as the user");

        let (url, _) = mock_server().await;
        let handler = LdapHandler::new(settings(url, groups("cn=other,dc=example,dc=com"))).unwrap();
        assert_eq!(handler.authenticate(&certificate).await, (Outcome::Reject, Vec::new()));

        let (url, _) = mock_server().await;
        let handler = LdapHandler::new(settings(url, groups(STAFF))).unwrap();
        let stranger = Credentials { user: Some("mallory".to_string()), secret: Secret::Certificate };
        assert_eq!(handler.authenticate(&stranger).await.0, Outcome::Ignore, "no such entry");
    }

    #[tokio::test]
    async fn test_reply_attributes_by_group() {
        let (url, _) = mock_server().await;
        let groups = LdapGroups {
            required: Vec::new(),
            source: GroupSource::Search { base_dn: "ou=groups,dc=example,dc=com".to_string(), filter: "(member={dn})".to_string() },
            reply: vec![
                (STAFF.to_string(), vec![("Filter-Id".to_string(), "staff".to_string())]),
                ("cn=other,dc=example,dc=com".to_string(), vec![("Filter-Id".to_string(), "other".to_string())]),
                (VPN.to_string(), vec![("Class".to_string(), "vpn".to_string())]),
            ],
        };
        let handler = LdapHandler::new(with_groups(url, PasswordCheck::Bind, groups)).unwrap();
        let (outcome, attributes) = handler.authenticate(&pap("alice", "secret")).await;
        assert_eq!(outcome, Outcome::Accept);
        assert_eq!(attributes, vec![
            ("Filter-Id".to_string(), AttrValue::Bytes(b"staff".to_vec())),
            ("Class".to_string(), AttrValue::Bytes(b"vpn".to_vec())),
        ]);
    }

    #[tokio::test]
    async fn test_without_group_settings_nothing_is_looked_up() {
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        let log = log.lock().unwrap();
        assert_eq!(log.filters.len(), 1, "{:?}", log.filters);
        assert!(!log.attributes_asked[0].iter().any(|attribute| attribute.eq_ignore_ascii_case("memberOf")), "{:?}", log.attributes_asked);
    }

    #[tokio::test]
    async fn test_search_uses_the_account_name() {
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap();
        let (outcome, _) = handler.authenticate_account(&pap("Alice@Corp", "secret"), Some("alice")).await;
        assert_eq!(outcome, Outcome::Accept);
        assert_eq!(log.lock().unwrap().filters, vec![LdapFilter::Equality("uid".to_string(), "alice".to_string())]);
    }

    #[tokio::test]
    async fn test_mschapv2_verifies_with_the_received_name() {
        let (url, _log) = mock_server().await;
        let handler = LdapHandler::new(settings(vec![url], PasswordCheck::Attribute("userPassword".to_string()))).unwrap();
        let (peer_challenge, challenge) = ([5u8; 16], [6u8; 16]);
        let secret = |over: &str| Secret::MsChapV2 { ident: 1, peer_challenge, challenge, response: crate::mschap::response_for("dave-pw", over, &peer_challenge, &challenge) };
        let credentials = |over: &str| Credentials { user: Some("Dave@Corp".to_string()), secret: secret(over) };
        assert_eq!(handler.authenticate_account(&credentials("Dave@Corp"), Some("dave")).await.0, Outcome::Accept);
        assert_eq!(handler.authenticate_account(&credentials("dave"), Some("dave")).await.0, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_reply_attributes_from_ldap() {
        let (url, _) = mock_server().await;
        let mut s = settings(vec![url], PasswordCheck::Bind);
        s.reply_attributes = vec![("Filter-Id".to_string(), "radiusFilterId".to_string())];
        let (outcome, attrs) = run(s, &pap("alice", "secret")).await;
        assert_eq!(outcome, Outcome::Accept);
        assert_eq!(
            attrs,
            vec![
                ("Filter-Id".to_string(), AttrValue::Bytes(b"std.ppp".to_vec())),
                ("Filter-Id".to_string(), AttrValue::Bytes(b"extra".to_vec())),
            ]
        );
    }

    #[tokio::test]
    async fn test_reply_attributes_not_sent_on_reject() {
        let (url, _) = mock_server().await;
        let mut s = settings(vec![url], PasswordCheck::Bind);
        s.reply_attributes = vec![("Filter-Id".to_string(), "radiusFilterId".to_string())];
        let (outcome, attrs) = run(s, &pap("alice", "wrong")).await;
        assert_eq!(outcome, Outcome::Reject);
        assert!(attrs.is_empty());
    }

    #[tokio::test]
    async fn test_server_failover() {
        let (url, _) = mock_server().await;
        let urls = vec![refused_url().await, url];
        assert_eq!(run(settings(urls, PasswordCheck::Bind), &pap("alice", "secret")).await.0, Outcome::Accept);
    }

    /// Each URL gets `timeout`, so a server that never answers ends in a reject rather than a
    /// request that waits for ever. The handler is built first: reading the system's
    /// certificates takes about a second on macOS, and several right after a rebuild, which is
    /// not the request's time. The limit is far above 2 x 300 ms, as a busy test run delays
    /// everything; what is checked is that the request is bounded at all.
    #[tokio::test]
    async fn test_all_servers_unreachable_rejected() -> Result<(), String> {
        let mut s = settings(vec![refused_url().await, silent_url().await], PasswordCheck::Bind);
        s.timeout = Duration::from_millis(300);
        let handler = LdapHandler::new(s)?;
        let Ok(outcome) = tokio::time::timeout(Duration::from_secs(10), handler.authenticate(&pap("alice", "secret"))).await else {
            return Err("the request waited past every URL's timeout".to_string());
        };
        assert_eq!(outcome.0, Outcome::Reject);
        Ok(())
    }

    #[tokio::test]
    async fn test_connection_that_breaks_tries_the_next_url() {
        for options in [
            MockOptions { close_on_service_bind: true, ..MockOptions::default() },
            MockOptions { close_on_search: true, ..MockOptions::default() },
            MockOptions { close_on_user_bind: true, ..MockOptions::default() },
        ] {
            let (broken, _) = mock_server_with(options).await;
            let (working, log) = mock_server().await;
            assert_eq!(run(settings(vec![broken, working], PasswordCheck::Bind), &pap("alice", "secret")).await.0, Outcome::Accept);
            assert_eq!(log.lock().unwrap().filters.len(), 1, "the second server decided");
        }
        // With nothing left to try it is a rejection.
        let (broken, _) = mock_server_with(MockOptions { close_on_search: true, ..MockOptions::default() }).await;
        assert_eq!(run(settings(vec![broken], PasswordCheck::Bind), &pap("alice", "secret")).await.0, Outcome::Reject);
    }

    #[tokio::test]
    async fn test_service_account_bind_failure_rejected() {
        let (url, log) = mock_server().await;
        let (url2, log2) = mock_server().await;
        let mut s = settings(vec![url, url2], PasswordCheck::Bind);
        s.bind_password = "nope".to_string();
        assert_eq!(run(s, &pap("alice", "secret")).await.0, Outcome::Reject);
        assert!(log.lock().unwrap().filters.is_empty());
        // A failed service bind is a configuration problem; the next URL is not tried.
        assert!(log2.lock().unwrap().binds.is_empty());
    }

    const ALICE_DN: &str = "uid=alice,ou=people,dc=example,dc=com";

    #[tokio::test]
    async fn test_connection_reused_between_requests() {
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        let log = log.lock().unwrap();
        assert_eq!(log.connections, 1);
        // The service account bind before reuse resets the identity of the user bind.
        assert_eq!(log.binds, vec![SERVICE_DN, ALICE_DN, SERVICE_DN, ALICE_DN]);
    }

    #[tokio::test]
    async fn test_wrong_password_keeps_connection() {
        let (url, log) = mock_server().await;
        let handler = LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "wrong")).await.0, Outcome::Reject);
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        assert_eq!(log.lock().unwrap().connections, 1);
    }

    #[tokio::test]
    async fn test_user_bind_error_discards_connection() {
        let (url, log) = mock_server_with(MockOptions { failing_user_binds: 1, ..MockOptions::default() }).await;
        let handler = LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        // An error other than invalidCredentials leaves the connection in an unknown state.
        assert_eq!(log.lock().unwrap().connections, 2);
    }

    #[tokio::test]
    async fn test_pool_size_bounds_open_connections() {
        let (url, log) = mock_server_with(MockOptions {
            slow_searches: usize::MAX,
            search_delay: Duration::from_millis(50),
            ..MockOptions::default()
        })
        .await;
        let mut s = settings(vec![url], PasswordCheck::Bind);
        s.pool_size = 2;
        let handler = LdapHandler::new(s).unwrap();
        let creds = pap("alice", "secret");
        let results = futures::future::join_all((0..10).map(|_| handler.authenticate(&creds))).await;
        assert!(results.iter().all(|(outcome, _)| *outcome == Outcome::Accept));
        assert_eq!(log.lock().unwrap().connections, 2);
    }

    #[tokio::test]
    async fn test_closed_connection_replaced() {
        let (url, log) =
            mock_server_with(MockOptions { close_after_user_bind: true, ..MockOptions::default() }).await;
        let handler = LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        assert_eq!(log.lock().unwrap().connections, 2);
    }

    #[tokio::test]
    async fn test_unresponsive_idle_connection_replaced() {
        let (url, log) =
            mock_server_with(MockOptions { stall_after_user_bind: true, ..MockOptions::default() }).await;
        let mut s = settings(vec![url], PasswordCheck::Bind);
        s.timeout = Duration::from_millis(1000);
        let handler = LdapHandler::new(s).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        let started = std::time::Instant::now();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        // The reuse check gives up after half of timeout_ms, leaving time for a new connection.
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(450) && elapsed < Duration::from_millis(800), "{elapsed:?}");
        assert_eq!(log.lock().unwrap().connections, 2);
    }

    #[tokio::test]
    async fn test_timed_out_connection_discarded() {
        let (url, log) = mock_server_with(MockOptions {
            slow_searches: 1,
            search_delay: Duration::from_millis(600),
            ..MockOptions::default()
        })
        .await;
        let mut s = settings(vec![url], PasswordCheck::Bind);
        s.timeout = Duration::from_millis(300);
        let handler = LdapHandler::new(s).unwrap();
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);
        assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        assert_eq!(log.lock().unwrap().connections, 2);
    }

    #[test]
    fn test_missing_ca_file_is_an_error() {
        let mut s = settings(vec!["ldaps://localhost".to_string()], PasswordCheck::Bind);
        s.ca_file = Some(PathBuf::from("/nonexistent/ca.pem"));
        assert!(LdapHandler::new(s).err().unwrap().contains("ca_file"));
    }
}
