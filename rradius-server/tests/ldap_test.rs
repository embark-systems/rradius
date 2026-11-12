//! Runs the `ldap` handler against a real OpenLDAP server in docker, over
//! LDAPS and StartTLS with a self-signed CA.
//!
//! Needs docker and network access to pull `alpine` and its OpenLDAP packages,
//! so it is ignored by default: `cargo test --test ldap_test -- --ignored`.

use rradius_core::auth_chain::{Credentials, Secret};
use rradius_core::ldap_handler::{GroupSource, LdapGroups, LdapHandler, LdapSettings, PasswordCheck};
use rradius_core::common::{AttrValue, Outcome};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const IMAGE: &str = "alpine:3.20";

const OPENSSL_CNF: &str = "[req]\ndistinguished_name = dn\n[dn]\n\
    [v3_ca]\nbasicConstraints = critical,CA:TRUE\nkeyUsage = critical,keyCertSign,cRLSign\nsubjectKeyIdentifier = hash\n\
    [server]\nbasicConstraints = CA:FALSE\nkeyUsage = critical,digitalSignature,keyEncipherment\n\
    extendedKeyUsage = serverAuth\nsubjectAltName = DNS:localhost,IP:127.0.0.1\n";

const SLAPD_CONF: &str = "include /etc/openldap/schema/core.schema\n\
    include /etc/openldap/schema/cosine.schema\n\
    include /etc/openldap/schema/inetorgperson.schema\n\
    modulepath /usr/lib/openldap\nmoduleload back_mdb.so\n\
    pidfile /run/openldap/slapd.pid\n\
    TLSCACertificateFile /work/ca.pem\nTLSCertificateFile /work/server.pem\nTLSCertificateKeyFile /work/server.key\n\
    database mdb\nsuffix \"dc=example,dc=com\"\nrootdn \"cn=admin,dc=example,dc=com\"\nrootpw adminpw\n\
    directory /var/lib/openldap/openldap-data\n\
    access to attrs=userPassword by self read by anonymous auth by * none\n\
    access to * by * read\n";

const SEED_LDIF: &str = "dn: dc=example,dc=com\nobjectClass: dcObject\nobjectClass: organization\ndc: example\no: Example\n\n\
    dn: ou=people,dc=example,dc=com\nobjectClass: organizationalUnit\nou: people\n\n\
    dn: uid=alice,ou=people,dc=example,dc=com\nobjectClass: inetOrgPerson\nuid: alice\ncn: Alice\nsn: A\n\
    userPassword: secret\ndescription: std.ppp\n\n\
    dn: ou=groups,dc=example,dc=com\nobjectClass: organizationalUnit\nou: groups\n\n\
    dn: cn=vpn,ou=groups,dc=example,dc=com\nobjectClass: groupOfNames\ncn: vpn\nmember: uid=alice,ou=people,dc=example,dc=com\n\n\
    dn: cn=admins,ou=groups,dc=example,dc=com\nobjectClass: groupOfNames\ncn: admins\nmember: cn=admin,dc=example,dc=com\n";

const START_SH: &str = "set -e\n\
    apk add --no-cache -q openldap openldap-back-mdb openldap-clients\n\
    mkdir -p /run/openldap /var/lib/openldap/openldap-data\n\
    slapadd -f /work/slapd.conf -l /work/seed.ldif\n\
    exec slapd -d 0 -f /work/slapd.conf -h 'ldap:/// ldaps:///'\n";

fn sh(program: &str, args: &[&str], dir: &Path) -> String {
    let output = Command::new(program).args(args).current_dir(dir).output().unwrap();
    assert!(output.status.success(), "{} {:?} failed: {}", program, args, String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A running OpenLDAP container, removed on drop.
struct Slapd {
    id: String,
    ldap_port: u16,
    ldaps_port: u16,
}

impl Drop for Slapd {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).output();
    }
}

/// Writes a CA, a server certificate for localhost, and the slapd files to
/// `dir`, then starts the container.
fn start_slapd(dir: &Path) -> Slapd {
    std::fs::write(dir.join("openssl.cnf"), OPENSSL_CNF).unwrap();
    sh("openssl", &["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-days", "2",
        "-subj", "/CN=rradius-test-ca", "-config", "openssl.cnf", "-extensions", "v3_ca"], dir);
    sh("openssl", &["req", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", "server.key", "-out", "server.csr",
        "-subj", "/CN=localhost", "-config", "openssl.cnf"], dir);
    sh("openssl", &["x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
        "-out", "server.pem", "-days", "2", "-extfile", "openssl.cnf", "-extensions", "server"], dir);
    std::fs::write(dir.join("slapd.conf"), SLAPD_CONF).unwrap();
    std::fs::write(dir.join("seed.ldif"), SEED_LDIF).unwrap();
    std::fs::write(dir.join("start.sh"), START_SH).unwrap();

    let volume = format!("{}:/work", dir.display());
    let id = sh("docker", &["run", "-d", "-p", "127.0.0.1::389", "-p", "127.0.0.1::636", "-v", &volume, IMAGE,
        "sh", "/work/start.sh"], dir);
    let port = |p: &str| -> u16 {
        let mapped = sh("docker", &["port", &id, p], dir);
        mapped.lines().next().unwrap().rsplit(':').next().unwrap().parse().unwrap()
    };
    let (ldap_port, ldaps_port) = (port("389/tcp"), port("636/tcp"));
    Slapd { id, ldap_port, ldaps_port }
}

fn settings(url: String, ca_file: Option<PathBuf>) -> LdapSettings {
    LdapSettings {
        urls: vec![url],
        bind_dn: "cn=admin,dc=example,dc=com".to_string(),
        bind_password: "adminpw".to_string(),
        base_dn: "dc=example,dc=com".to_string(),
        filter: "(uid={user})".to_string(),
        certificate_filter: None,
        password_check: PasswordCheck::Bind,
        reply_attributes: vec![("Filter-Id".to_string(), "description".to_string())],
        timeout: Duration::from_secs(5),
        ca_file,
        allow_plaintext: false,
        pool_size: 4,
        groups: Default::default(),
    }
}

fn pap(user: &str, pw: &str) -> Credentials {
    Credentials { user: Some(user.to_string()), secret: Secret::pap(pw.as_bytes().to_vec()) }
}

/// Authenticates `alice` twice on a fresh handler and prints how long the
/// first (new connection) and second (reused connection) requests took.
async fn measure(label: &str, handler: &LdapHandler) {
    let started = Instant::now();
    assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
    let first = started.elapsed();
    let started = Instant::now();
    assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
    eprintln!("{} bind authentication: new connection {:?}, reused connection {:?}", label, first, started.elapsed());
}

#[tokio::test]
#[ignore = "needs docker and network access"]
async fn test_openldap_over_tls() {
    let dir = tempfile::tempdir().unwrap();
    let slapd = start_slapd(dir.path());
    let ca = Some(dir.path().join("ca.pem"));
    let ldaps = LdapHandler::new(settings(format!("ldaps://localhost:{}", slapd.ldaps_port), ca.clone())).unwrap();

    // Package install and seeding take a while; wait until LDAPS answers.
    let deadline = Instant::now() + Duration::from_secs(180);
    while ldaps.authenticate(&pap("alice", "secret")).await.0 != Outcome::Accept {
        let logs = Command::new("docker").args(["logs", &slapd.id]).output().unwrap();
        assert!(Instant::now() < deadline, "slapd did not come up: {}", String::from_utf8_lossy(&logs.stderr));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let ldaps_url = format!("ldaps://localhost:{}", slapd.ldaps_port);
    measure("LDAPS", &LdapHandler::new(settings(ldaps_url, ca.clone())).unwrap()).await;

    let (outcome, attrs) = ldaps.authenticate(&pap("alice", "secret")).await;
    assert_eq!(outcome, Outcome::Accept);
    assert_eq!(attrs, vec![("Filter-Id".to_string(), AttrValue::Bytes(b"std.ppp".to_vec()))]);
    assert_eq!(ldaps.authenticate(&pap("alice", "wrong")).await.0, Outcome::Reject);
    assert_eq!(ldaps.authenticate(&pap("alice", "")).await.0, Outcome::Reject);
    assert_eq!(ldaps.authenticate(&pap("nobody", "secret")).await.0, Outcome::Ignore);
    // Groups come from a search, which a directory without memberOf needs.
    let groups = |required: &str| LdapGroups {
        required: vec![required.to_string()],
        source: GroupSource::Search { base_dn: "ou=groups,dc=example,dc=com".to_string(), filter: "(member={dn})".to_string() },
        reply: vec![("CN=VPN, OU=groups, DC=example, DC=com".to_string(), vec![("Class".to_string(), "vpn".to_string())])],
    };
    let with_groups = |required: &str| LdapHandler::new(LdapSettings { groups: groups(required), ..settings(format!("ldaps://localhost:{}", slapd.ldaps_port), ca.clone()) }).unwrap();
    let member = with_groups("cn=vpn,ou=groups,dc=example,dc=com");
    let (outcome, attrs) = member.authenticate(&pap("alice", "secret")).await;
    assert_eq!(outcome, Outcome::Accept);
    assert!(attrs.contains(&("Class".to_string(), AttrValue::Bytes(b"vpn".to_vec()))), "{attrs:?}");
    assert_eq!(member.authenticate(&pap("alice", "wrong")).await.0, Outcome::Reject);
    let stranger = with_groups("cn=admins,ou=groups,dc=example,dc=com");
    assert_eq!(stranger.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);

    // A verified certificate, with a filter a real directory has to evaluate.
    let certificate = Credentials { user: Some("alice".to_string()), secret: Secret::Certificate };
    let with_filter = |filter: Option<&str>| LdapHandler::new(LdapSettings { certificate_filter: filter.map(str::to_string), ..settings(format!("ldaps://localhost:{}", slapd.ldaps_port), ca.clone()) }).unwrap();
    assert_eq!(with_filter(None).authenticate(&certificate).await.0, Outcome::Ignore);
    assert_eq!(with_filter(Some("(&(uid={user})(!(uid=nobody)))")).authenticate(&certificate).await.0, Outcome::Accept);
    assert_eq!(with_filter(Some("(&(uid={user})(!(uid=alice)))")).authenticate(&certificate).await.0, Outcome::Ignore, "as for a locked account");

    // What the extra search costs, on connections already open.
    let plain = LdapHandler::new(settings(format!("ldaps://localhost:{}", slapd.ldaps_port), ca.clone())).unwrap();
    let time = |label: &'static str, handler: LdapHandler| async move {
        handler.authenticate(&pap("alice", "secret")).await;
        let started = Instant::now();
        for _ in 0..200 {
            assert_eq!(handler.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);
        }
        eprintln!("{label}: {:?} per authentication over 200", started.elapsed() / 200);
    };
    time("without a group search", plain).await;
    time("with a group search", member).await;

    // The name a chain looks the user up by, not the one received.
    assert_eq!(ldaps.authenticate_account(&pap("Alice@Corp", "secret"), Some("alice")).await.0, Outcome::Accept);
    assert_eq!(ldaps.authenticate(&pap("Alice@Corp", "secret")).await.0, Outcome::Ignore);

    // A reused connection still checks each password.
    assert_eq!(ldaps.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);

    let starttls = LdapHandler::new(settings(format!("ldap://localhost:{}", slapd.ldap_port), ca)).unwrap();
    measure("StartTLS", &starttls).await;
    assert_eq!(starttls.authenticate(&pap("alice", "wrong")).await.0, Outcome::Reject);
    assert_eq!(starttls.authenticate(&pap("alice", "secret")).await.0, Outcome::Accept);

    // Without the CA the certificate cannot be verified, over either transport.
    let untrusted = LdapHandler::new(settings(format!("ldaps://localhost:{}", slapd.ldaps_port), None)).unwrap();
    assert_eq!(untrusted.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);
    let untrusted = LdapHandler::new(settings(format!("ldap://localhost:{}", slapd.ldap_port), None)).unwrap();
    assert_eq!(untrusted.authenticate(&pap("alice", "secret")).await.0, Outcome::Reject);
}
