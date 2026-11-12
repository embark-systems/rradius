use ipnet::IpNet;
use serde::Deserialize;
use std::sync::Arc;
use std::fmt;
use std::net::IpAddr;

/// How strictly a client's Access-Requests are held to carrying a Message-Authenticator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageAuthenticatorPolicy {
    #[default]
    Yes,
    No,
    /// Decided by the first Access-Request after startup.
    Auto,
}

/// Where a NAS listens for dynamic authorization requests unless configured otherwise (RFC 5176).
pub const DEFAULT_COA_PORT: u16 = 3799;

/// A RADIUS or TACACS+ client (NAS) and its shared secret.
#[derive(Debug, Clone, PartialEq)]
pub struct Client {
    /// The configured `name`, or the configured address string when unnamed.
    pub name: String,
    pub secret: Vec<u8>,
    /// The named authentication chain for this client; `None` uses the default.
    pub auth: Option<String>,
    pub require_message_authenticator: MessageAuthenticatorPolicy,
    pub limit_proxy_state: MessageAuthenticatorPolicy,
    /// The port of this NAS for Disconnect-Request and CoA-Request.
    pub coa_port: u16,
    /// The name its RadSec certificate has to be valid for.
    pub tls_name: Option<String>,
    /// The EAP types its conversations may use, in order; `None` for all the server has.
    pub eap_methods: Option<Vec<u8>>,
    /// The most RadSec connections it may have open at once.
    pub max_radsec_connections: Option<usize>,
    /// On the table of one RadSec connection: the subject of its certificate.
    pub radsec_subject: Option<Arc<crate::eap::CertificateSubject>>,
    /// TACACS+ only: the rules that decide its authorization instead of the script.
    pub authorize_group: Option<Arc<crate::tacacs_rules::AuthorizationRules>>,
    /// RADIUS only: may send Disconnect-Request and CoA-Request on its RadSec connection, for
    /// the CoA relay to pass on.
    pub coa_sender: bool,
}

/// The settings of a client other than its address, name and secret.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub auth: Option<String>,
    pub require_message_authenticator: MessageAuthenticatorPolicy,
    pub limit_proxy_state: MessageAuthenticatorPolicy,
    pub coa_port: u16,
    pub tls_name: Option<String>,
    pub eap_methods: Option<Vec<u8>>,
    pub max_radsec_connections: Option<usize>,
    pub authorize_group: Option<Arc<crate::tacacs_rules::AuthorizationRules>>,
    /// RADIUS only: may send Disconnect-Request and CoA-Request on its RadSec connection, for
    /// the CoA relay to pass on.
    pub coa_sender: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            auth: None,
            require_message_authenticator: MessageAuthenticatorPolicy::default(),
            limit_proxy_state: MessageAuthenticatorPolicy::default(),
            coa_port: DEFAULT_COA_PORT,
            tls_name: None,
            max_radsec_connections: None,
            eap_methods: None,
            authorize_group: None,
            coa_sender: false,
        }
    }
}

#[derive(Debug)]
pub struct ClientTableError(String);

impl fmt::Display for ClientTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClientTableError {}

/// What the core needs of `[radius.tls]`, which an extension reads: whether a client that is a
/// network takes a certificate only for its `tls_name`. Without it there is no `[radius.tls]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadsecClients {
    pub network_clients_need_tls_name: bool,
}

/// The secret RFC 6614 section 2.3 gives every RadSec client.
pub const RADSEC_SECRET: &[u8] = b"radsec";

/// Clients keyed by network; lookups pick the longest matching prefix, so
/// the result does not depend on the order clients were listed in.
#[derive(Debug, Clone, Default)]
pub struct ClientTable {
    entries: Vec<(IpNet, Client)>,
    /// Clients known by their RadSec certificate alone. Not in `entries`, so that nothing
    /// looking for a source address, as UDP does, can find them.
    without_address: Vec<Client>,
    /// RadSec: a client that is a network takes a certificate only for its `tls_name`.
    networks_need_tls_name: bool,
}

impl ClientTable {
    /// Builds a table from (address, name, secret, auth chain) entries. An
    /// address is an IP (a host network) or a CIDR network; the same network
    /// twice is an error.
    pub fn new<I, A>(entries: I) -> Result<Self, ClientTableError>
    where
        I: IntoIterator<Item = (A, Option<String>, Vec<u8>, Option<String>)>,
        A: AsRef<str>,
    {
        let mut table = ClientTable::default();
        for (address, name, secret, auth) in entries {
            table.add(address.as_ref(), name, secret, ClientOptions { auth, ..ClientOptions::default() })?;
        }
        Ok(table)
    }

    /// Adds a client. `auto` remembers how one device behaves, so it takes a single
    /// host: a network can hold devices that differ.
    pub fn add(&mut self, address: &str, name: Option<String>, secret: Vec<u8>, options: ClientOptions) -> Result<(), ClientTableError> {
        let ClientOptions { auth, require_message_authenticator, limit_proxy_state, coa_port, tls_name, eap_methods, max_radsec_connections, authorize_group, coa_sender } = options;
        let net = parse_network(address)?;
        if self.entries.iter().any(|(existing, _)| *existing == net) {
            return Err(ClientTableError(format!("duplicate client network '{}'", net)));
        }
        for (setting, policy) in [("require_message_authenticator", require_message_authenticator), ("limit_proxy_state", limit_proxy_state)] {
            if policy == MessageAuthenticatorPolicy::Auto && net.prefix_len() != net.max_prefix_len() {
                return Err(ClientTableError(format!("client '{address}': {setting} = \"auto\" needs a single host, not a network")));
            }
        }
        let name = name.unwrap_or_else(|| address.to_string());
        self.entries.push((net, Client { name, secret, auth, require_message_authenticator, limit_proxy_state, coa_port, eap_methods, tls_name, max_radsec_connections, radsec_subject: None, authorize_group, coa_sender }));
        Ok(())
    }

    /// Adds a client known by the name in its RadSec certificate, from any address.
    pub fn add_without_address(&mut self, name: Option<String>, secret: Vec<u8>, options: ClientOptions) -> Result<(), ClientTableError> {
        let ClientOptions { auth, require_message_authenticator, limit_proxy_state, coa_port, tls_name, eap_methods, max_radsec_connections, authorize_group, coa_sender } = options;
        let tls_name = tls_name.ok_or_else(|| ClientTableError("a client without an address needs a tls_name".to_string()))?;
        if self.without_address.iter().any(|client| client.tls_name.as_deref().is_some_and(|other| other.eq_ignore_ascii_case(&tls_name))) {
            return Err(ClientTableError(format!("tls_name '{tls_name}' is used by two clients without an address")));
        }
        let name = name.unwrap_or_else(|| tls_name.clone());
        self.without_address.push(Client { name, secret, auth, require_message_authenticator, limit_proxy_state, coa_port, eap_methods, tls_name: Some(tls_name), max_radsec_connections, radsec_subject: None, authorize_group, coa_sender });
        Ok(())
    }

    /// The name of every client.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(_, client)| client).chain(&self.without_address).map(|client| client.name.as_str())
    }

    /// The clients known by their certificate alone.
    pub fn clients_without_address(&self) -> impl Iterator<Item = &Client> {
        self.without_address.iter()
    }

    /// A table in which every source address finds `client`: what RadSec packets are
    /// handled with once a certificate has identified it.
    pub fn just(client: &Client) -> ClientTable {
        let any = [IpNet::V4(Default::default()), IpNet::V6(Default::default())];
        ClientTable { entries: any.map(|network| (network, client.clone())).to_vec(), without_address: Vec::new(), networks_need_tls_name: false }
    }

    /// Every client, in the order they were added.
    pub fn clients(&self) -> impl Iterator<Item = &Client> {
        self.entries.iter().map(|(_, client)| client).chain(&self.without_address)
    }

    pub fn lookup(&self, source: IpAddr) -> Option<&Client> {
        self.lookup_entry(source).map(|(_, client)| client)
    }

    /// The client `source` belongs to, and the network it was configured with.
    pub fn lookup_entry(&self, source: IpAddr) -> Option<(&IpNet, &Client)> {
        let source = match source {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(source),
            v4 => v4,
        };
        self.entries.iter().filter(|(net, _)| net.contains(&source)).max_by_key(|(net, _)| net.prefix_len()).map(|(net, client)| (net, client))
    }

    /// Resolves what a script names as the target of a dynamic authorization request: a
    /// client's name, which must have a single address, or an address within a client.
    pub fn dynamic_auth_target(&self, target: &str) -> Result<(IpAddr, &Client), String> {
        if let Ok(address) = target.parse::<IpAddr>() {
            let client = self.lookup(address).ok_or_else(|| format!("'{target}' is not a configured client"))?;
            return Ok((address, client));
        }
        if self.without_address.iter().any(|client| client.name == target) {
            return Err(format!("client '{target}' has no address to send to"));
        }
        let mut named = self.entries.iter().filter(|(_, client)| client.name == target);
        let (net, client) = named.next().ok_or_else(|| format!("'{target}' is not a configured client"))?;
        if named.next().is_some() {
            return Err(format!("more than one client is named '{target}'"));
        }
        if net.prefix_len() != net.max_prefix_len() {
            return Err(format!("client '{target}' is a network, which has no single address; pass an address"));
        }
        Ok((net.addr(), client))
    }

    /// The same clients as RadSec sees them: RFC 6614 fixes the shared secret, and with
    /// integrity from TLS the Message-Authenticator settings have nothing to protect.
    pub fn for_radsec(&self) -> ClientTable {
        let mut table = self.clone();
        for client in table.entries.iter_mut().map(|(_, client)| client).chain(&mut table.without_address) {
            client.secret = RADSEC_SECRET.to_vec();
            client.require_message_authenticator = MessageAuthenticatorPolicy::No;
            client.limit_proxy_state = MessageAuthenticatorPolicy::No;
        }
        table
    }

    /// `[radius.tls] network_clients_need_tls_name`, for the table RadSec identifies clients in.
    pub fn with_networks_needing_tls_name(mut self, on: bool) -> Self {
        self.networks_need_tls_name = on;
        self
    }

    pub fn networks_need_tls_name(&self) -> bool {
        self.networks_need_tls_name
    }

    /// The clients with an address that is a network rather than one address, and that network.
    pub fn network_clients(&self) -> impl Iterator<Item = (&IpNet, &Client)> {
        self.entries.iter().filter(|(net, _)| net.prefix_len() < net.max_prefix_len()).map(|(net, client)| (net, client))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.without_address.is_empty()
    }
}

pub fn parse_network(address: &str) -> Result<IpNet, ClientTableError> {
    let net = match address.parse::<IpNet>() {
        Ok(net) => net,
        Err(_) => address
            .parse::<IpAddr>()
            .map(IpNet::from)
            .map_err(|_| ClientTableError(format!("invalid client address '{}'", address)))?,
    };
    // Normalize so "10.0.0.5/24" and "10.0.0.0/24" count as the same network.
    Ok(net.trunc())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(address: &str, name: Option<&str>, secret: &str) -> (String, Option<String>, Vec<u8>, Option<String>) {
        (address.to_string(), name.map(str::to_string), secret.as_bytes().to_vec(), None)
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn test_longest_prefix_wins() {
        let table = ClientTable::new(vec![
            entry("192.168.1.0/24", Some("core"), "s1"),
            entry("192.168.1.7", Some("nas7"), "s2"),
        ])
        .unwrap();
        assert_eq!(table.lookup(ip("192.168.1.7")).unwrap().name, "nas7");
        assert_eq!(table.lookup(ip("192.168.1.7")).unwrap().secret, b"s2");
        assert_eq!(table.lookup(ip("192.168.1.20")).unwrap().name, "core");
    }

    #[test]
    fn test_ipv4_mapped_source() {
        let table = ClientTable::new(vec![entry("192.168.1.0/24", Some("core"), "s1")]).unwrap();
        assert_eq!(table.lookup(ip("::ffff:192.168.1.20")).unwrap().name, "core");
    }

    #[test]
    fn test_no_match() {
        let table = ClientTable::new(vec![entry("192.168.1.0/24", Some("core"), "s1")]).unwrap();
        assert!(table.lookup(ip("10.0.0.1")).is_none());
    }

    #[test]
    fn test_duplicate_network_rejected() {
        let err = ClientTable::new(vec![entry("10.0.0.5", None, "a"), entry("10.0.0.5/32", None, "b")]).unwrap_err();
        assert!(err.to_string().contains("10.0.0.5/32"), "unexpected error: {err}");
    }

    #[test]
    fn test_unnamed_client_uses_address() {
        let table = ClientTable::new(vec![entry("10.0.0.0/8", None, "k")]).unwrap();
        assert_eq!(table.lookup(ip("10.1.2.3")).unwrap().name, "10.0.0.0/8");
    }

    #[test]
    fn test_auth_chain_kept() {
        let table = ClientTable::new(vec![
            ("10.1.0.0/16".to_string(), Some("vpn-gw".to_string()), b"k".to_vec(), Some("vpn".to_string())),
            entry("10.0.0.0/8", None, "k"),
        ])
        .unwrap();
        assert_eq!(table.lookup(ip("10.1.2.3")).unwrap().auth.as_deref(), Some("vpn"));
        assert_eq!(table.lookup(ip("10.2.0.1")).unwrap().auth, None);
    }

    #[test]
    fn test_invalid_address_rejected() {
        assert!(ClientTable::new(vec![entry("not-an-ip", None, "k")]).is_err());
    }
}
