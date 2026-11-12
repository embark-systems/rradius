//! `http_request` for scripts: only to hosts the configuration lists, within limits, so that
//! a slow API or an address taken from a request cannot take the server somewhere else.

use crate::config::{AllowedHost, ScriptHttpSettings};
use crate::lua_engine::log_line;
use crate::metrics::Metrics;
use reqwest::{redirect, Client, Method, Url};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// What a script asks for.
pub struct HttpOptions {
    pub url: String,
    pub method: Method,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Option<Vec<u8>>,
}

/// Any answer from the server, whatever its status.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

/// Which requests wait together, so that a burst of accounting cannot refuse logins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Authentication,
    Accounting,
}

/// Name lookups running at once, for logins and for accounting. Each holds a thread until it
/// really ends, whatever the request's time limit does, and those threads are the ones slow
/// password hashes run on. Apart, so that accounting stuck on slow DNS leaves logins alone.
const MAX_AUTHENTICATION_LOOKUPS: usize = 12;
const MAX_ACCOUNTING_LOOKUPS: usize = 4;

/// The system resolver, with a limit on lookups. A request waits for a place within its own
/// time limit, so that a burst of new connections is not refused.
struct LimitedResolver {
    lookups: Arc<Semaphore>,
}

impl reqwest::dns::Resolve for LimitedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let lookups = self.lookups.clone();
        Box::pin(async move {
            let lookup = lookups.acquire_owned().await?;
            let host = name.as_str().to_string();
            let addresses = tokio::task::spawn_blocking(move || {
                let _lookup = lookup;
                (host.as_str(), 0).to_socket_addrs().map(|addresses| addresses.collect::<Vec<_>>())
            })
            .await??;
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// One listed host's places, kept apart so that a slow API only holds up its own requests.
struct HostPlaces {
    host: AllowedHost,
    authentication: Arc<Semaphore>,
    accounting: Arc<Semaphore>,
}

pub struct ScriptHttp {
    authentication_client: Client,
    accounting_client: Client,
    #[cfg(test)]
    accounting_lookups: Arc<Semaphore>,
    settings: ScriptHttpSettings,
    hosts: Vec<HostPlaces>,
    metrics: Arc<Metrics>,
}

impl ScriptHttp {
    pub fn new(settings: &ScriptHttpSettings, metrics: Arc<Metrics>) -> Result<Self, String> {
        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        for error in &native.errors {
            log_line(tracing::Level::WARN, format!("lua.http: loading the system's certificates: {error}"));
        }
        roots.add_parsable_certificates(native.certs);
        if let Some(path) = &settings.ca_file {
            let certificates = CertificateDer::pem_file_iter(path)
                .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
                .map_err(|e| format!("lua.http.ca_file '{}': {}", path.display(), e))?;
            if certificates.is_empty() {
                return Err(format!("lua.http.ca_file '{}' holds no certificate", path.display()));
            }
            for certificate in certificates {
                roots.add(certificate).map_err(|e| format!("lua.http.ca_file '{}': {}", path.display(), e))?;
            }
        }
        let needs_tls = settings.allowed_hosts.iter().any(|host| !host.plaintext);
        if needs_tls && roots.is_empty() {
            return Err("lua.http: no trusted certificates; this system has none, so set lua.http.ca_file".to_string());
        }
        // Both rustls crypto providers are compiled in, so name one explicitly.
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("lua.http: cannot set up TLS: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        // No proxy from the environment, and no redirects: either would send a request
        // somewhere the list of allowed hosts never saw.
        let client = |lookups: Arc<Semaphore>| {
            Client::builder()
                .tls_backend_preconfigured(tls.clone())
                .redirect(redirect::Policy::none())
                .no_proxy()
                .dns_resolver(Arc::new(LimitedResolver { lookups }))
                .build()
                .map_err(|e| format!("lua.http: cannot set up the client: {e}"))
        };
        let accounting_lookups = Arc::new(Semaphore::new(MAX_ACCOUNTING_LOOKUPS));
        let authentication_client = client(Arc::new(Semaphore::new(MAX_AUTHENTICATION_LOOKUPS)))?;
        let accounting_client = client(accounting_lookups.clone())?;
        let hosts = settings
            .allowed_hosts
            .iter()
            .map(|host| HostPlaces { host: host.clone(), authentication: Arc::new(Semaphore::new(settings.max_concurrent)), accounting: Arc::new(Semaphore::new(settings.max_concurrent)) })
            .collect();
        Ok(Self { authentication_client, accounting_client, #[cfg(test)] accounting_lookups, settings: settings.clone(), hosts, metrics })
    }

    /// The URL and its host's places if the settings allow it, or why not. The reason holds
    /// nothing of the URL but its host, since the rest often carries keys or user names.
    fn allowed_url(&self, text: &str) -> Result<(Url, &HostPlaces), String> {
        // The parser drops these, which could turn what the checks below see into something else.
        if text.contains(['\t', '\n', '\r']) {
            return Err("a URL with a tab or a line break is not allowed".to_string());
        }
        // It drops these too, and the checks below go by the text.
        if text.trim_matches(|c: char| c <= ' ') != text {
            return Err("a URL with a space or a control character at either end is not allowed".to_string());
        }
        let url = Url::parse(text).map_err(|_| "not a valid URL".to_string())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("not an http or https URL".to_string());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("a URL with a user name is not allowed".to_string());
        }
        if has_dot_segment(text) {
            return Err("a URL with . or .. in its path is not allowed".to_string());
        }
        let host = url.host_str().ok_or_else(|| "a URL without a host".to_string())?;
        let plaintext = url.scheme() == "http";
        let places = self.hosts.iter().find(|places| {
            let listed = &places.host;
            listed.host == host && listed.plaintext == plaintext && if listed.port.is_some() { listed.port == url.port_or_known_default() } else { url.port().is_none() }
        });
        match places {
            Some(places) => Ok((url, places)),
            None => Err(format!("{}://{host} is not in lua.http.allowed_hosts", url.scheme())),
        }
    }

    /// Sends a request. `Err` is a message for the script: refused, timed out, failed, or
    /// a response too large.
    pub async fn request(&self, options: HttpOptions, purpose: Purpose) -> Result<HttpResponse, String> {
        let (url, places) = self.allowed_url(&options.url).inspect_err(|reason| {
            self.metrics.script_http_denied.inc();
            log_line(tracing::Level::WARN, format!("script http_request refused: {reason}"));
        })?;
        let host = url.host_str().unwrap_or("").to_string();
        let semaphore = match purpose {
            Purpose::Authentication => &places.authentication,
            Purpose::Accounting => &places.accounting,
        };
        let Ok(_place) = semaphore.clone().try_acquire_owned() else {
            self.metrics.script_http_busy.inc();
            log_line(tracing::Level::WARN, format!("script http_request to {host}: {} already waiting", self.settings.max_concurrent));
            return Err("too many requests at once".to_string());
        };
        let result = match tokio::time::timeout(self.settings.timeout, self.exchange(url, options, purpose)).await {
            Ok(result) => result,
            Err(_) => Err(Failure::Timeout),
        };
        match result {
            Ok(response) => {
                self.metrics.script_http_ok.inc();
                Ok(response)
            }
            Err(failure) => {
                let (counter, message) = match failure {
                    Failure::Timeout => (&self.metrics.script_http_timeout, format!("timed out after {:?}", self.settings.timeout)),
                    Failure::TooLarge => (&self.metrics.script_http_too_large, format!("the response is over {} octets", self.settings.max_response_bytes)),
                    Failure::Other(kind) => (&self.metrics.script_http_error, kind.to_string()),
                };
                counter.inc();
                log_line(tracing::Level::WARN, format!("script http_request to {host}: {message}"));
                Err(message)
            }
        }
    }

    async fn exchange(&self, url: Url, options: HttpOptions, purpose: Purpose) -> Result<HttpResponse, Failure> {
        let client = match purpose {
            Purpose::Authentication => &self.authentication_client,
            Purpose::Accounting => &self.accounting_client,
        };
        let mut request = client.request(options.method, url);
        for (name, value) in options.headers {
            request = request.header(name, value);
        }
        if let Some(body) = options.body {
            request = request.body(body);
        }
        let failed = |e: reqwest::Error| Failure::Other(failure_kind(&e));
        let mut response = request.send().await.map_err(failed)?;
        let status = response.status().as_u16();
        let headers = response.headers().iter().map(|(name, value)| (name.as_str().to_string(), value.as_bytes().to_vec())).collect();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            if body.len() + chunk.len() > self.settings.max_response_bytes {
                return Err(Failure::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse { status, headers, body })
    }

    /// How long a request can take.
    pub fn timeout(&self) -> Duration {
        self.settings.timeout
    }
}

/// A fixed description of what went wrong. Not the error's own words, which can hold the URL.
fn failure_kind(error: &reqwest::Error) -> &'static str {
    let mut causes = String::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        causes.push_str(&cause.to_string().to_lowercase());
        causes.push(' ');
        source = cause.source();
    }
    if causes.contains("certificate") {
        "the server's certificate was not accepted"
    } else if causes.contains("dns") || causes.contains("nodename") || causes.contains("name resolution") {
        "the name lookup failed"
    } else if causes.contains("refused") {
        "the connection was refused"
    } else if error.is_connect() {
        "the connection failed"
    } else {
        "the request failed"
    }
}

/// Whether a URL's path has a `.` or `..` segment, however it is written. The URL parser
/// removes them, so a name from a login could otherwise walk to another path on an allowed
/// host. Looks at the text, since the parsed URL no longer shows them.
fn has_dot_segment(text: &str) -> bool {
    let after_scheme = text.split_once("://").map_or(text, |(_, rest)| rest);
    let path = after_scheme.split(['?', '#']).next().unwrap_or("");
    path.split(['/', '\\']).skip(1).any(|segment| {
        let segment = segment.to_ascii_lowercase().replace("%2e", ".");
        segment == "." || segment == ".."
    })
}

enum Failure {
    Timeout,
    TooLarge,
    Other(&'static str),
}

/// Percent-encodes everything but RFC 3986's unreserved characters, so that a value from a
/// request stays one piece of a URL. `.`, `..` and nothing cannot be one piece, however they
/// are encoded, since a URL parser takes them as path steps, so those are errors.
pub fn url_encode(text: &[u8]) -> Result<String, String> {
    if matches!(text, b"" | b"." | b"..") {
        return Err(format!("url_encode: '{}' cannot be one piece of a URL", String::from_utf8_lossy(text)));
    }
    let mut encoded = String::with_capacity(text.len());
    for byte in text {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => encoded.push(char::from(*byte)),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    Ok(encoded)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::lua_engine::{LuaAuthEngine, LuaRequest, LuaVms};
    use crate::test_support::{logged_errors, test_db, test_vms};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn settings(allowed: Vec<AllowedHost>) -> ScriptHttpSettings {
        ScriptHttpSettings { allowed_hosts: allowed, timeout: Duration::from_millis(500), max_response_bytes: 1024, max_concurrent: 8, ca_file: None }
    }

    /// Allows `address` as plain text, which is what the test server speaks.
    fn allowing(address: &str) -> ScriptHttpSettings {
        settings(vec![AllowedHost::parse(&format!("http://{address}")).unwrap()])
    }

    fn get(url: String) -> HttpOptions {
        HttpOptions { url, method: Method::GET, headers: Vec::new(), body: None }
    }

    fn http(settings: &ScriptHttpSettings) -> Arc<ScriptHttp> {
        Arc::new(ScriptHttp::new(settings, Arc::new(Metrics::default())).unwrap())
    }

    /// A server answering by path, counting requests.
    async fn server() -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buffer[..n]),
                        }
                    }
                    seen.fetch_add(1, Ordering::SeqCst);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    let path = text.split(' ').nth(1).unwrap_or("").to_string();
                    let answer = |status: &str, headers: &str, body: &str| format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len());
                    let response = match path.as_str() {
                        "/ok" => answer("200 OK", "X-Thing: yes\r\n", "ok"),
                        "/echo" => answer("200 OK", "", &text),
                        "/fail" => answer("500 Oops", "", "no"),
                        "/redirect" => answer("302 Found", "Location: http://192.0.2.1/elsewhere\r\n", ""),
                        "/big" => answer("200 OK", "", &"x".repeat(5000)),
                        "/slow" => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            answer("200 OK", "", "late")
                        }
                        _ => answer("404 Not Found", "", ""),
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (address, count)
    }

    /// Runs `script` with `http`, whose result has to be `accept`.
    async fn passes(http: Arc<ScriptHttp>, script: String) {
        let engine = LuaAuthEngine::new(test_vms(), test_db(), &script).unwrap().with_script_http(Some(http));
        let decision = engine.authenticate(LuaRequest::new("radius", "authentication", vec![])).await;
        assert!(decision.accepted(), "{script}");
    }

    #[tokio::test]
    async fn test_without_the_table() {
        let engine = LuaAuthEngine::new(test_vms(), test_db(), "return (http_request == nil and url_encode == nil) and 'accept' or 'reject'").unwrap();
        assert!(engine.authenticate(LuaRequest::new("radius", "authentication", vec![])).await.accepted());
    }

    #[tokio::test]
    async fn test_allowed_request() {
        let (address, count) = server().await;
        let script = format!("local r = http_request{{url = 'http://{address}/ok'}} return (r.status == 200 and r.body == 'ok' and r.headers['x-thing'] == 'yes') and 'accept' or 'reject'");
        passes(http(&allowing(&address)), script).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_method_headers_and_body() {
        let (address, _) = server().await;
        let script = format!("local r = http_request{{url = 'http://{address}/echo', method = 'POST', headers = {{ ['X-Key'] = 'k1' }}, body = 'hello'}} return (r.body:find('POST /echo', 1, true) == 1 and r.body:lower():find('x-key: k1', 1, true) ~= nil) and 'accept' or 'reject'");
        passes(http(&allowing(&address)), script).await;
    }

    #[tokio::test]
    async fn test_error_statuses_are_responses() {
        let (address, _) = server().await;
        passes(http(&allowing(&address)), format!("local r = http_request{{url = 'http://{address}/fail'}} return r.status == 500 and 'accept' or 'reject'")).await;
    }

    #[test]
    fn test_url_with_something_at_either_end() {
        let http_client = http(&settings(vec![AllowedHost::parse("api.example.com").unwrap()]));
        for url in ["https://api.example.com/users/.. ", "https://api.example.com/users/..\u{1}", " https://api.example.com/", "\u{1f}https://api.example.com/"] {
            let Err(reason) = http_client.allowed_url(url) else { panic!("{url:?} allowed") };
            assert!(reason.contains("either end"), "{url:?}: {reason}");
        }
        assert!(http_client.allowed_url("https://api.example.com/users/a b").is_ok(), "only the ends");
    }

    #[tokio::test]
    async fn test_refused_before_sending() {
        let (address, count) = server().await;
        let strict = settings(vec![AllowedHost::parse("api.example.com").unwrap()]);
        let http_client = http(&strict);
        for url in [
            "https://evil.example/".to_string(),
            "https://api.example.com@evil.example/".to_string(),
            "https://user:pw@api.example.com/".to_string(),
            "https://api.example.com.evil.example/".to_string(),
            "https://api.example.com:8443/".to_string(),
            "http://api.example.com/".to_string(),
            "file:///etc/passwd".to_string(),
            "https://api.example.com/users/.. ".to_string(),
            "https://api.example.com/users/..\u{1}".to_string(),
            "https://api.example.com/users/../admin".to_string(),
            "https://api.example.com/users/./x".to_string(),
            "https://api.example.com/users/%2E%2e/admin".to_string(),
            "https://api.example.com/users/%2e".to_string(),
            "https://api.example.com/users\\..\\admin".to_string(),
            "https://api.example.com/users/.\t./admin".to_string(),
            "https://api.example.com/users/.\n./admin".to_string(),
            "https://api.example.com/users/\r../admin".to_string(),
            "not a url".to_string(),
            format!("http://{address}/ok"),
        ] {
            assert!(http_client.request(get(url.clone()), Purpose::Authentication).await.is_err(), "{url}");
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(http_client.metrics.script_http_denied.get(), 19);
    }

    #[test]
    fn test_url_host_forms() {
        let check = |allowed: &str, url: &str| {
            http(&settings(vec![AllowedHost::parse(allowed).unwrap()])).allowed_url(url).is_ok()
        };
        assert!(check("api.example.com", "https://API.example.com/x?y=1"), "hosts compare without case");
        assert!(check("api.example.com", "https://api.example.com:443/"), "the default port is no port");
        assert!(check("api.example.com:443", "https://api.example.com/"));
        assert!(check("api.example.com:8443", "https://api.example.com:8443/"));
        assert!(!check("api.example.com:8443", "https://api.example.com/"));
        assert!(check("[::1]:8443", "https://[::1]:8443/"));
        assert!(check("api.example.com", "https://api.example.com/a..b/c.d?x=../y"), "dots that are not path steps");
    }

    /// Plain text is for the host written that way, and only that one.
    #[test]
    fn test_plain_text_is_per_host() {
        let client = http(&settings(vec![AllowedHost::parse("http://legacy.internal:8080").unwrap(), AllowedHost::parse("api.example.com").unwrap()]));
        assert!(client.allowed_url("http://legacy.internal:8080/").is_ok());
        assert!(client.allowed_url("https://legacy.internal:8080/").is_err());
        assert!(client.allowed_url("https://api.example.com/").is_ok());
        assert!(client.allowed_url("http://api.example.com/").is_err());
    }

    /// A burst of accounting to one host leaves logins to it, and everything to other hosts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_places_are_per_purpose_and_per_host() {
        let (slow_host, _) = server().await;
        let (other_host, _) = server().await;
        let mut limited = settings(vec![AllowedHost::parse(&format!("http://{slow_host}")).unwrap(), AllowedHost::parse(&format!("http://{other_host}")).unwrap()]);
        limited.max_concurrent = 1;
        limited.timeout = Duration::from_secs(2);
        let client = http(&limited);
        let waiting = {
            let client = client.clone();
            let url = format!("http://{slow_host}/slow");
            tokio::spawn(async move { client.request(get(url), Purpose::Accounting).await.map(|_| ()) })
        };
        while client.hosts[0].accounting.available_permits() != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(client.request(get(format!("http://{slow_host}/ok")), Purpose::Accounting).await.is_err(), "its own kind has to wait");
        assert!(client.request(get(format!("http://{slow_host}/ok")), Purpose::Authentication).await.is_ok(), "a login still gets through");
        assert!(client.request(get(format!("http://{other_host}/ok")), Purpose::Accounting).await.is_ok(), "another host is not held up");
        waiting.abort();
    }

    #[tokio::test]
    async fn test_lookups_are_limited() {
        use reqwest::dns::Resolve;
        let resolver = LimitedResolver { lookups: Arc::new(Semaphore::new(0)) };
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        // It waits for a place: the request's own time limit decides how long.
        assert!(tokio::time::timeout(Duration::from_millis(50), resolver.resolve(name)).await.is_err());
        let resolver = LimitedResolver { lookups: Arc::new(Semaphore::new(1)) };
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        assert!(resolver.resolve(name).await.is_ok());
        assert_eq!(resolver.lookups.available_permits(), 1, "the place comes back");
    }

    /// Accounting stuck on name lookups does not hold up logins, and ends at its own time limit.
    #[tokio::test]
    async fn test_accounting_lookups_do_not_hold_up_logins() {
        let (address, _) = server().await;
        let port = address.rsplit(':').next().unwrap();
        let host = format!("localhost:{port}");
        let client = http(&allowing(&host));
        let _taken = client.accounting_lookups.clone().acquire_many_owned(MAX_ACCOUNTING_LOOKUPS as u32).await.unwrap();
        let url = format!("http://{host}/ok");
        assert_eq!(client.request(get(url.clone()), Purpose::Authentication).await.unwrap().status, 200);
        assert!(client.request(get(url), Purpose::Accounting).await.map(|_| ()).unwrap_err().contains("time"));
    }

    /// A TLS server for `localhost`, answering every request with 200.
    async fn tls_server(dir: &std::path::Path) -> u16 {
        use crate::test_support::certs;
        use tokio_rustls::TlsAcceptor;
        let issuer = certs::ca(dir, "ca");
        certs::leaf(dir, "server", &issuer);
        certs::ca(dir, "other-ca");
        let chain = vec![CertificateDer::from_pem_file(dir.join("server.pem")).unwrap()];
        let key = rustls::pki_types::PrivateKeyDer::from_pem_file(dir.join("server.key")).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(socket).await else { return };
                    let mut buffer = [0u8; 4096];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn test_https_checks_the_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let port = tls_server(dir.path()).await;
        let url = format!("https://localhost:{port}/");
        let with_ca = |ca: &str| ScriptHttpSettings { ca_file: Some(dir.path().join(ca)), ..settings(vec![AllowedHost::parse(&format!("localhost:{port}")).unwrap()]) };

        let response = http(&with_ca("ca.pem")).request(get(url.clone()), Purpose::Authentication).await.unwrap();
        assert_eq!(response.status, 200, "a certificate from ca_file");

        let refused = http(&with_ca("other-ca.pem")).request(get(url), Purpose::Authentication).await.map(|_| ()).unwrap_err();
        assert!(refused.contains("certificate"), "{refused}");
    }

    #[tokio::test]
    async fn test_failure_is_named_without_the_url() {
        let client = http(&allowing("127.0.0.1:1"));
        let message = client.request(get("http://127.0.0.1:1/x".to_string()), Purpose::Authentication).await.map(|_| ()).unwrap_err();
        assert_eq!(message, "the connection was refused");
    }

    #[tokio::test]
    async fn test_redirect_not_followed() {
        let (address, count) = server().await;
        let script = format!("local r = http_request{{url = 'http://{address}/redirect'}} return (r.status == 302 and r.headers.location == 'http://192.0.2.1/elsewhere') and 'accept' or 'reject'");
        passes(http(&allowing(&address)), script).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_timeout() {
        let (address, _) = server().await;
        let client = http(&allowing(&address));
        let script = format!("local r, problem = http_request{{url = 'http://{address}/slow'}} return (r == nil and problem:find('timed out', 1, true) ~= nil) and 'accept' or 'reject'");
        passes(client.clone(), script).await;
        // Not through a script: the VMs tests share can be busy with other tests.
        let started = std::time::Instant::now();
        assert!(client.request(get(format!("http://{address}/slow")), Purpose::Authentication).await.is_err());
        assert!(started.elapsed() < client.timeout() + Duration::from_secs(1));
        assert_eq!(client.metrics.script_http_timeout.get(), 2);
    }

    #[tokio::test]
    async fn test_response_too_large() {
        let (address, _) = server().await;
        let client = http(&allowing(&address));
        passes(client.clone(), format!("local r, problem = http_request{{url = 'http://{address}/big'}} return (r == nil and problem ~= nil) and 'accept' or 'reject'")).await;
        assert_eq!(client.metrics.script_http_too_large.get(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_too_many_at_once() {
        let (address, _) = server().await;
        let mut limited = allowing(&address);
        limited.max_concurrent = 1;
        limited.timeout = Duration::from_secs(2);
        let client = http(&limited);
        let slow = get(format!("http://{address}/slow"));
        let waiting = { let client = client.clone(); tokio::spawn(async move { client.request(slow, Purpose::Authentication).await.map(|_| ()) }) };
        while client.hosts[0].authentication.available_permits() != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let started = std::time::Instant::now();
        assert_eq!(client.request(get(format!("http://{address}/ok")), Purpose::Authentication).await.map(|_| ()), Err("too many requests at once".to_string()));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(client.metrics.script_http_busy.get(), 1);
        waiting.abort();
    }

    #[tokio::test]
    async fn test_wrong_options() {
        let (address, count) = server().await;
        let client = http(&allowing(&address));
        for call in ["http_request()", "http_request('x')", "http_request{}", "http_request{url = 1}", "http_request{url = 'http://x', method = 'TRACE'}", "http_request{url = 'http://x', headers = 'no'}", "http_request{url = 'http://x', body = 1}", "http_request{url = 'http://x', headers = { Host = 'internal' }}", "http_request{url = 'http://x', headers = { ['content-length'] = '1' }}", "http_request{url = 'http://x', headers = { ['Transfer-Encoding'] = 'chunked' }}", "url_encode('..')", "url_encode('.')", "url_encode('')"] {
            let engine = LuaAuthEngine::new(test_vms(), test_db(), format!("local ok = pcall(function() {call} end) return ok and 'reject' or 'accept'")).unwrap().with_script_http(Some(client.clone()));
            assert!(engine.authenticate(LuaRequest::new("radius", "authentication", vec![])).await.accepted(), "{call}");
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_url_encode_from_a_script() {
        let (address, _) = server().await;
        let engine = LuaAuthEngine::new(test_vms(), test_db(), "return url_encode('a b/c') == 'a%20b%2Fc' and 'accept' or 'reject'").unwrap().with_script_http(Some(http(&allowing(&address))));
        assert!(engine.authenticate(LuaRequest::new("radius", "authentication", vec![])).await.accepted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_place_given_up() {
        let (address, _) = server().await;
        let mut patient = allowing(&address);
        patient.timeout = Duration::from_secs(3);
        let vms = Arc::new(LuaVms::with_count(1).unwrap());
        let waiting = LuaAuthEngine::new(vms.clone(), test_db(), format!("http_request{{url = 'http://{address}/slow'}} return 'accept'")).unwrap().with_script_http(Some(http(&patient)));
        let first = tokio::spawn(async move { waiting.authenticate(LuaRequest::new("radius", "accounting", vec![])).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let second = LuaAuthEngine::new(vms, test_db(), "return 'accept'").unwrap();
        let decision = tokio::time::timeout(Duration::from_secs(1), second.authenticate(LuaRequest::new("radius", "accounting", vec![]))).await;
        assert!(decision.expect("the script waiting for HTTP kept its place").accepted());
        assert!(!first.is_finished());
        first.abort();
    }

    #[tokio::test]
    async fn test_log_holds_no_request_details() {
        let client = http(&allowing("127.0.0.1:1"));
        let message = client.request(get("http://127.0.0.1:1/users/log-alice?key=secret-value".to_string()), Purpose::Authentication).await.map(|_| ()).unwrap_err();
        assert!(!message.contains("secret-value") && !message.contains("log-alice"), "{message}");
        let logged = logged_errors().lock().unwrap().clone();
        assert!(logged.iter().any(|line| line.contains("http_request to 127.0.0.1")), "{logged:?}");
        assert!(!logged.iter().any(|line| line.contains("secret-value") || line.contains("log-alice")), "{logged:?}");
    }

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode(b"Al-ice_1.~").unwrap(), "Al-ice_1.~");
        assert_eq!(url_encode("a b/c?d=é".as_bytes()).unwrap(), "a%20b%2Fc%3Fd%3D%C3%A9");
        assert_eq!(url_encode(b"../x").unwrap(), "..%2Fx");
        for text in [&b""[..], b".", b".."] {
            assert!(url_encode(text).is_err(), "{text:?}");
        }
    }
}
