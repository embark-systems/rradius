# 11 802.1X and EAP

> After this chapter you can:
> - Prepare the server certificate and CA for EAP and configure `[radius.eap]`
> - Let company-issued device certificates log in with EAP-TLS, and use a table or a script to decide who gets in
> - Block lost or revoked certificates with a CRL
> - Let employees log in with an account and password using PEAP or TTLS, and pick the right password storage format
> - Read the EAP metrics and logs to find out why a login failed

Prerequisites:

- You have read [Chapter 03](03-quick-start.md) and know how to start the server and test logins with `rradius-bench`.
- You know what an auth chain and the sql handler are ([Chapter 07](07-users-and-chains.md)).
- `openssl` is installed on the machine, to generate the demo certificates.

In this chapter:

- [Concepts](#concepts)
- [The three methods the core provides](#the-three-methods-the-core-provides)
- [How one EAP login proceeds](#how-one-eap-login-proceeds)
- [Prepare the certificates](#prepare-the-certificates)
- [Configure radius.eap](#configure-radiuseap)
- [Which methods each client can use](#which-methods-each-client-can-use)
- [EAP-TLS: log in with a certificate](#eap-tls-log-in-with-a-certificate)
- [Authorization after the certificate passes](#authorization-after-the-certificate-passes)
- [Block revoked certificates with a CRL](#block-revoked-certificates-with-a-crl)
- [Replace the server certificate](#replace-the-server-certificate)
- [PEAP and TTLS: log in with an account and password](#peap-and-ttls-log-in-with-an-account-and-password)
- [How to store passwords](#how-to-store-passwords)
- [Seeing EAP in Lua scripts](#seeing-eap-in-lua-scripts)
- [Conversation limits and timeouts](#conversation-limits-and-timeouts)
- [When the database is unavailable](#when-the-database-is-unavailable)
- [EAP metrics and logs](#eap-metrics-and-logs)
- [Connect real NASes and clients](#connect-real-nases-and-clients)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Concepts

802.1X is the login method for wired networks and Wi-Fi (WPA2/WPA3-Enterprise). A login has three parties:

| Role | Who | What it does |
| --- | --- | --- |
| Client (supplicant) | The 802.1X software on a laptop or phone | Presents a certificate or an account and password |
| NAS (network access server, for example an AP controller or a switch) | A client in the rradius configuration | Wraps the EAP packets from the supplicant in RADIUS and passes them to rradius |
| rradius | The authentication server | Runs the EAP exchange with the supplicant, decides accept or reject, and hands the encryption keys to the NAS |

The NAS does not understand the content of EAP. It only relays. The conversation between the supplicant and rradius travels in the `EAP-Message` attribute and takes several round trips: in each round rradius answers with an Access-Challenge that carries a `State`, and the NAS sends the same `State` back in the next round, so rradius knows which conversation it belongs to. The last round is an Access-Accept or an Access-Reject.

Besides your reply attributes (for example a VLAN), the Access-Accept carries `MS-MPPE-Recv-Key` and `MS-MPPE-Send-Key`. These keys are derived from the TLS handshake, and the NAS uses them to set up wireless encryption with the supplicant. rradius adds both attributes itself; you do not configure them.

A few things differ from a normal PAP login:

- **rradius handles the whole conversation itself.** Once `[radius.eap]` is configured, requests that carry `EAP-Message` do not run the auth chain or a script in every round. The auth chain is asked only once, at the end: for EAP-TLS after the handshake is complete, for PEAP and TTLS when the password inside the tunnel has arrived.
- **Message-Authenticator is required.** If a request with `EAP-Message` has no `Message-Authenticator`, rradius drops it without a reply, whatever the client's `require_message_authenticator` says (RFC 3579). A NAS includes it whenever it sends EAP, so you normally never see this.
- **The conversation lives in the memory of the node that received it.** When the cluster has several nodes, every round of one conversation must go to the same node. A different node has no such `State` and answers EAP-Failure. If there is a load balancer between the NAS and rradius, make it send the requests of one NAS to the same node.
- **Without `[radius.eap]`**, `EAP-Message` is just an ordinary attribute, and the request goes to the auth chain or the script as usual.

## The three methods the core provides

`rradiusd` provides three EAP methods. You write the method names in the configuration:

| Name in the configuration | Method | The supplicant presents | TLS version | Client certificate needed | What the auth chain receives |
| --- | --- | --- | --- | --- | --- |
| `tls` | EAP-TLS (RFC 5216, RFC 9190) | A certificate | TLS 1.2, 1.3 | Yes, and it must be issued by a CA in `client_ca_file` | The name on the certificate; the certificate itself is the proof |
| `peap` | PEAP version 0, inner EAP-MSCHAPv2 | Account and password | TLS 1.2 only | No | The inner account and the MS-CHAPv2 response |
| `ttls` | EAP-TTLS version 0 (RFC 5281), inner PAP or MS-CHAPv2 | Account and password | TLS 1.2 only | No | The inner account and password (PAP) or MS-CHAPv2 response |

Methods that are not provided:

- EAP-MD5 is not provided. When a supplicant asks only for EAP-MD5, the conversation ends with EAP-Failure.
- When `methods` has any other name, `check` rejects it, for example `md5` gives `'md5' is not a method this server offers; it has tls, peap and ttls`.

TLS session resumption is not provided either: every login is a full handshake.

> **Enterprise edition:** EAP-SIM, EAP-AKA', TLS session resumption and OCSP certificate checking are in the enterprise edition. `check` in the core rejects configuration that uses them; see [Common mistakes](#common-mistakes) in this chapter.

How to choose:

- **Company-issued devices that can hold a certificate** (laptops, managed phones): use EAP-TLS. There is no password to guess, and you revoke a lost device with a CRL.
- **Employees' own devices**: use PEAP. Almost every operating system has PEAP-MSCHAPv2 built in.
- **TTLS with inner PAP** suits account sources whose passwords can only be compared in clear text, for example a directory that checks the password with an LDAP bind ([Chapter 08](08-ldap-and-ad.md)). The inner password is inside the TLS tunnel, so it does not cross the air in clear text.

## How one EAP login proceeds

Take EAP-TLS as the example. The `rounds per finished conversation: 6.0` that `rradius-bench` reports is these six rounds:

1. The NAS sends the supplicant's EAP-Response/Identity (for example `laptop-001.corp.example.com`).
2. rradius picks the first method this client may use and answers with EAP-TLS Start.
3. The next rounds are the TLS handshake. When a TLS message is too long, rradius cuts it into several fragments according to `fragment_size` and waits for the supplicant to acknowledge each one before sending the next.
4. After a successful handshake, rradius takes the name from the certificate and hands it to the auth chain (or to the Lua script when there is no auth chain) to decide.
5. On accept it answers Access-Accept with EAP-Success, the MPPE keys and the reply attributes; on reject it answers Access-Reject with EAP-Failure.

When the supplicant does not want the method rradius proposed first, it answers with a Legacy Nak that lists the methods it wants. rradius restarts with one of the methods this client allows; if none is allowed, it ends the conversation. This can only happen before the supplicant has sent any TLS data. Once the handshake has started, the method cannot change.

When a conversation ends, the server log has one authentication record with `chain="eap"` and a `handler` of `eap-tls`, `eap-peap` or `eap-ttls`. The Access-Challenge rounds in between are not logged.

## Prepare the certificates

EAP needs at least a server certificate and its private key. For EAP-TLS you also need the CA that issues the client certificates.

| File | Setting | Purpose |
| --- | --- | --- |
| Server certificate chain (PEM) | `cert_file` | The supplicant uses it to confirm it is talking to the real RADIUS server |
| Server private key (PEM) | `key_file` | Kept only on the server, mode 0600 |
| CA of the client certificates (PEM) | `client_ca_file` | EAP-TLS accepts only client certificates issued by these CAs |
| Revocation list (PEM) | `crl_file` | Optional; EAP-TLS uses it to block revoked certificates |

Some points:

- **Client certificates are trusted only through `client_ca_file`.** The system root certificates are not trusted; otherwise a certificate issued by any public CA could log in.
- **The server certificate must verify on the supplicant.** The supplicant must trust the CA that issued the server certificate and check the server name (for example `domain_match` in a wpa_supplicant configuration). The example server certificate is `DNS:radius.corp.example.com` with the usage `serverAuth`.
- **Private key permissions.** If others can read `key_file`, every configuration load logs a warning:

  ```text
  … WARN rradius_core::common::seal: radius.eap.key_file: 'certs/server.key' has mode 644, which others can reach or its group can write; make it 0600 or 0640
  ```

  Whoever gets this private key can impersonate your RADIUS server and collect PEAP password hashes.

Both scenarios ship a script that generates demo certificates:

| Scenario | Script | Files produced (in `certs/`) |
| --- | --- | --- |
| `enterprise-wifi` | `make-certs.sh` | `ca.pem`, `ca.key`, `server.pem`/`.key`, one device certificate `device.pem`/`.key` (`DNS:laptop-001.corp.example.com`) |
| `wifi-certs` | `setup.sh` | `ca.pem`, `ca.key`, `server.pem`/`.key`, the two laptop certificates `laptop-001` and `laptop-002`, `ca.crl` (which lists `laptop-002`), and `crl-db/` for `openssl ca` |

Both scripts generate keys and CSRs with `openssl req` and sign with `openssl x509`; both OpenSSL and LibreSSL work. `setup.sh` does not overwrite an existing `certs/ca.pem`.

> **Warning:** The CA these scripts generate is for testing only. In production, use your company CA to issue the server certificate and the device certificates, and do not keep the CA's private key on the RADIUS server.

## Configure radius.eap

`[radius.eap]` sits under `[radius]`. The smallest EAP-TLS configuration needs only four file paths:

```toml
# conf.d/eap.toml
[radius.eap]
methods = ["tls"]
cert_file = "certs/server.pem"
key_file = "certs/server.key"
client_ca_file = "certs/ca.pem"
crl_file = "certs/ca.crl"
```

This fragment goes in the `conf.d/` of the `config init` starter and has been verified with `check` (`configuration ok`). Paths are relative to the directory of the main configuration file `rradius.toml`.

All keys (defaults are taken from `config show`; the full description is in [Appendix A](90-config-reference.md)):

| Key | Default | Meaning |
| --- | --- | --- |
| `methods` | `["tls"]` | The methods offered, in order; the first one is proposed at the start. At least one, no duplicates |
| `cert_file` | none | Server certificate chain, PEM. Required whenever any method is set |
| `key_file` | none | Server private key, PEM. Required |
| `client_ca_file` | none | CA that issues the client certificates. Required when `methods` has `tls`; not allowed when it does not |
| `crl_file` | none | Revocation list for the client certificates. Used only for `tls`; not allowed when `methods` has no `tls` |
| `match_identity` | `true` | EAP-TLS: the name on the certificate must match the EAP Identity the supplicant reports |
| `fragment_size` | `1000` | Upper limit of each TLS data fragment, 256 to 1400 bytes |
| `session_timeout_ms` | `30000` | How long an unfinished conversation may go without a next round before it is dropped (milliseconds); must be greater than 0 |
| `max_sessions` | `4096` | Total number of unfinished conversations kept |
| `max_sessions_per_client` | `1024` | Number of unfinished conversations kept per client (NAS) |
| `max_sessions_per_device` | `4` | Number of unfinished conversations kept per device (same client, same `Calling-Station-Id`) |

All of `[radius.eap]` can be reloaded: after SIGHUP, new conversations use the new settings and the reloaded certificates and CRL, while conversations already running finish with the settings they started with. When the new settings have an error (for example a certificate file is missing), the reload fails, the server keeps running with the old settings, and logins are not affected. Log from a real test:

```text
… ERROR rradius_core::reload: reload failed, keeping the running configuration: rradius.toml: radius.eap.cert_file 'certs/gone.pem': I/O error: No such file or directory (os error 2)
```

At that point `rradius_reload_total{result="error"}` goes up by 1. When you replace certificates, first confirm the new files with `check`, then send SIGHUP.

When `methods` has `peap` or `ttls`, the configuration must have `[auth]`: only the auth chain can check the password inside the tunnel. Without it, `check` answers `radius.eap.methods has a method whose password only a chain can check, and there is no [auth] section`.

## Which methods each client can use

A client's `eap_methods` decides which methods the devices behind that NAS may use, proposed in order:

```toml
# rradius.toml (excerpt)
[[radius.clients]]
name = "wlc"
address = "10.30.0.10"
secret_file = "wlc.secret"
eap_methods = ["tls", "peap"]
# … (other settings omitted)
```

Rules:

- When `radius.eap.methods` has only one method, a client can leave out `eap_methods` and gets that one method.
- When `radius.eap.methods` has two or more methods, **every client must write** `eap_methods`. A client that does not use EAP writes `eap_methods = []`. If you leave it out, `check` answers `radius client 'example': eap_methods is required when radius.eap.methods has more than one method; [] for a client without EAP`.
- `eap_methods` can list only methods that are in `radius.eap.methods`; the order may differ. For example, with `methods = ["tls", "peap"]`, a NAS with `eap_methods = ["peap"]` proposes PEAP first to its devices.
- When a client with `eap_methods = []` sends EAP, rradius answers EAP-Failure at once and opens no conversation.

Why write it for every client? A supplicant can ask to change the method with a Legacy Nak. If adding PEAP automatically turned it on for every NAS, a network that was meant for certificate login only could start accepting password logins. Writing it per client means adding a method does not open another network by accident.

## EAP-TLS: log in with a certificate

**Goal:** Let `laptop-001` log in with its certificate in the `wifi-certs` scenario, and see the revoked `laptop-002` get blocked.

The key parts of this scenario's configuration:

```toml
# rradius.toml (excerpt)
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT NULL FROM devices WHERE name = ?"
certificate_query = "SELECT 1 FROM devices WHERE name = ? AND enabled = 1"
reply_query = """WITH d AS (SELECT vlan FROM devices WHERE name = ?)
  SELECT 'Tunnel-Type:1', 'VLAN' FROM d
  UNION ALL SELECT 'Tunnel-Medium-Type:1', 'IEEE-802' FROM d
  UNION ALL SELECT 'Tunnel-Private-Group-Id:1', vlan FROM d"""

[radius.eap]
cert_file = "certs/server.pem"
key_file = "certs/server.key"
client_ca_file = "certs/ca.pem"
methods = ["tls"]
crl_file = "certs/ca.crl"

[accounts]
enable = "UPDATE devices SET enabled = 1 WHERE name = ?1"
disable = "UPDATE devices SET enabled = 0 WHERE name = ?1"
show = "SELECT name, vlan, enabled FROM devices WHERE name = ?1"
# … (other settings omitted)
```

- Devices are listed in the `devices` table (`migrations/1_schema.sql`). The name is the DNS name on the certificate, and `vlan` decides which VLAN the device goes into.
- `certificate_query` is the query used after the certificate has passed: one row found means accept. `user_query` returns a `NULL` password, so this table cannot be used for password logins.
- `reply_query` returns three VLAN attributes; `:1` is the tag.
- `[accounts]` has only `enable`, `disable` and `show`, so you can disable or enable a device with the `user` command.

**Set up:** Prepare the working directory as in [Chapter 03](03-quick-start.md), then generate the certificates and replace the example secrets. The scenario's client address is `10.30.0.10`; change it to `127.0.0.1` for a local test:

```console
$ cp -R rradius-server/examples/scenarios/wifi-certs ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/wifi-certs/rradius.toml
$ cd ~/rradius-lab/wifi-certs
$ sh setup.sh
wrote certs in …/rradius-lab/wifi-certs/certs: ca.pem, ca.crl, server, laptop-001, laptop-002 (revoked)
$ for f in raft api wlc; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
$ sed -i.bak2 's/^address = "10.30.0.10"/address = "127.0.0.1"/' rradius.toml
$ rradiusd check --config rradius.toml
configuration ok
```

If you have not run `setup.sh` first, `check` fails: `rradius.toml: radius.eap.cert_file 'certs/server.pem': I/O error: No such file or directory (os error 2)`.

Start the server (`rradiusd serve --config rradius.toml`), then open another terminal.

**Verify:** Use `rradius-bench` to play the supplicant and the NAS:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user laptop-001.corp.example.com --load eap-tls,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com \
    --cert-file certs/laptop-001.pem --key-file certs/laptop-001.key
measured 0.004 s after 0.000 s of warmup

eap-tls (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 29%
  eap-tls: 224/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 6.0
    latency us: p50 4415.5, p90 4415.5, p99 4415.5, p99.9 4415.5, max 4415.5
```

Parameters of the EAP load:

- `--user`: the EAP Identity. For EAP-TLS it must equal the name on the certificate (see `match_identity` in the next section).
- `--cert-file`, `--key-file`: the client certificate and private key. `eap-tls` requires them; without them you get `rradius-bench: eap-tls needs --cert-file`.
- `--ca-file`: the CA used to check the server certificate. If you leave it out, any server certificate is accepted; give it in tests too, so you can confirm the server sends the right certificate.
- `--tls-name`: the name the server certificate must match. It defaults to the value of `--server`; when you connect with `127.0.0.1`, write the name on the certificate, `radius.corp.example.com`, explicitly.
- `--tls-version 1.3`: use TLS 1.3 instead (`eap-tls` only). Tested: same `accept 1`, 6 rounds.

Server log:

```text
… INFO rradius_auth: authentication protocol="radius" client="wlc" source=127.0.0.1 user="laptop-001.corp.example.com" account="laptop-001.corp.example.com" chain="eap" handler="eap-tls" decision="accept"
```

Now try the revoked `laptop-002`:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user laptop-002.corp.example.com --load eap-tls,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com \
    --cert-file certs/laptop-002.pem --key-file certs/laptop-002.key
rradius-bench: eap-tls: TLS: received fatal alert: CertificateRevoked
…
  eap-tls: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 0, invalid 1, late 0, socket errors 0, retransmits 0
…
```

`laptop-002` is still enabled in the table. What blocks it is the CRL: during the handshake rradius sends the TLS alert `CertificateRevoked`, and the auth chain is never asked.

> **Note:** `rradius-bench` stops when it receives a TLS alert and counts it as `invalid 1`; it does not send the last acknowledgement round. A real supplicant acknowledges the alert and then receives an Access-Reject with EAP-Failure. So when you test this case with `rradius-bench`, the conversation on the server is cleared only after `session_timeout_ms` (30 seconds) and counted as `result="timeout"`, not `reject`.

Finally, disable `laptop-001` with the `user` command:

```console
$ rradiusd user disable --config rradius.toml laptop-001.corp.example.com
disabled laptop-001.corp.example.com
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user laptop-001.corp.example.com --load eap-tls,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com \
    --cert-file certs/laptop-001.pem --key-file certs/laptop-001.key
…
  eap-tls: 520/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd user enable --config rradius.toml laptop-001.corp.example.com
enabled laptop-001.corp.example.com
```

This time the handshake succeeds, and `certificate_query` finds no enabled device, so the login is rejected. Log:

```text
… WARN rradius_core::lua_engine: EAP conversation with client wlc refused: not authorized
… INFO rradius_auth: authentication protocol="radius" client="wlc" source=127.0.0.1 user="laptop-001.corp.example.com" account="laptop-001.corp.example.com" chain="eap" handler="eap-tls" decision="reject"
```

**What the result looks like:** A certificate must pass two gates to log in: the TLS handshake (CA, validity period, CRL), and then authorization (table, directory or script). When the first fails, the supplicant receives a TLS alert; when the second fails, the log says `refused: not authorized`.

## Authorization after the certificate passes

### Which name on the certificate

After a successful handshake, rradius takes the login name from the client certificate:

1. It first looks at the DNS names and email names in the subject alternative name.
2. Only when there are neither does it use the common name (CN). When DNS or email names exist, the CN does not count, because a CA that checks only the alternative names may let the applicant fill in the CN freely.
3. If the alternative names cannot be read (for example a DNS name that is not text), the whole certificate is rejected; rradius does not fall back to the CN.

With `match_identity = true` (the default), the EAP Identity the supplicant reports must be one of these names, ignoring case. When it does not match, the login is rejected and the auth chain does not run:

```text
… WARN rradius_core::lua_engine: EAP conversation with client wlc refused: the EAP Identity does not fit the certificate
… INFO rradius_auth: authentication protocol="radius" client="wlc" source=127.0.0.1 user="alice" account="" chain="eap" handler="eap-tls" decision="reject"
```

(This is the result of a real test with the `laptop-001` certificate and `--user alice`. The `user` in the record is the RADIUS `User-Name` the NAS sent; the line that gives the reason does not print the EAP Identity, because it comes from outside and may contain control characters.)

Some supplicants always report an EAP Identity such as `anonymous`. In that case set `match_identity = false`, and the name becomes the first name on the certificate. Tested: with the same `laptop-001` certificate and `--user anonymous`, `match_identity = true` rejects; after changing it to `false` and sending SIGHUP, the login is accepted, and the record is `user="anonymous" account="laptop-001.corp.example.com"`.

The name goes to the auth chain as written on the certificate. The `User-Name` that the auth chain and scripts see is this name, not the Identity the supplicant reported.

### Decide with the sql handler

For a certificate login, the sql handler works like this:

- If there is a `certificate_query`, it uses that and accepts when one row is found; if not, it uses `user_query`.
- When only `user_query` is used, the row found must have a password value to be accepted: an empty password is often used to mean a disabled account, and a certificate login is treated the same way. If you do not want to store passwords for devices, write a `certificate_query`.
- `default_user` does not apply: a certificate cannot log in under a name that does not exist.
- `reply_query` runs as usual, and the result goes into the Access-Accept.

The `enterprise-wifi` scenario keeps employee accounts and devices in the same `accounts` table, told apart by `kind`:

```toml
# rradius.toml (excerpt)
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM accounts WHERE name = ? AND kind = 'user' AND enabled = 1"
certificate_query = "SELECT 1 FROM accounts WHERE name = ? AND kind = 'device' AND enabled = 1"
reply_query = "SELECT r.attribute, r.value FROM accounts a JOIN group_reply r ON r.grp = a.grp WHERE a.name = ?"
```

This way a device certificate cannot pose as an employee account, and an employee password cannot log in as a device.

### Decide with the ldap handler

The ldap handler processes certificate logins only when you write a `certificate_filter`, for example `certificate_filter = "(uid={user})"`. It accepts when an entry is found and `required_groups` is satisfied, without a bind. An ldap handler without `certificate_filter` skips a certificate login and logs the line `ignoring a certificate login: this handler has no certificate_filter`. The other settings of the ldap handler are in [Chapter 08](08-ldap-and-ad.md).

### Decide with a Lua script when there is no auth chain

When the configuration has no `[auth]`, the top-level Lua script decides after the certificate passes, and it can see `eap_method` and `tls_subject`. For an example, see [Seeing EAP in Lua scripts](#seeing-eap-in-lua-scripts).

### Name normalization and certificates

`strip_realm` and `lowercase` in `[auth.username]` are also applied to the name from the certificate, but only changes of letter case are allowed. When normalization would remove a realm, the certificate login is rejected:

```text
… WARN rradius_core::lua_engine: auth chain: rejecting a request from client wlc: normalization changes a certificate's name in more than case
```

This is the result of a real test in `enterprise-wifi` (`strip_realm = "suffix"`) with a certificate whose email name is `alice@corp.example.com`. A certificate proves the whole name, and stripping the realm turns it into a different account. So store the device's full name from the certificate in the table; DNS names have no `@` and are not affected.

### What the Access-Accept carries

On accept, the Access-Accept carries:

- EAP-Success and `MS-MPPE-Recv-Key`, `MS-MPPE-Send-Key`.
- The attributes the auth chain or script returned, for example a VLAN.
- `User-Name`: when the auth chain or script does not return `User-Name`, rradius puts in the name from the certificate, so the NAS uses the same name in its logs and accounting.

Reply attributes are put only in the Access-Accept, never in the Access-Challenge rounds in between.

A certificate login does not count toward `auth.lockout`: there is nothing to guess, and a rejection does not count as a failure.

## Block revoked certificates with a CRL

`crl_file` is a PEM revocation list, and it can hold several. Once it is set, every certificate in the client certificate chain is checked during the handshake.

**Three things you must know:**

1. **Every CA level in the chain needs a CRL.** If you give only the root CA's CRL and the client certificate was issued by an intermediate CA, the handshake fails. Tested: with a certificate issued by an intermediate CA and a `crl_file` that held only the root CA's CRL, the supplicant received an `UnknownCA` alert; after the root CA's and the intermediate CA's CRLs were concatenated into one file, the handshake passed.
2. **Send SIGHUP after you update the CRL file.** rradius reads the CRL only when it loads the configuration.
3. **An expired CRL is still used.** A CRL past its Next Update does not make every login fail, but a newly revoked certificate is not blocked until you put in a new list. Regenerate the CRL regularly and reload.

The procedure to revoke a certificate, using `crl-db/` of `wifi-certs` as the example (run in the `certs/` directory):

```console
$ openssl ca -config crl-db/ca.cnf -revoke laptop-001.pem
Using configuration from crl-db/ca.cnf
Adding Entry with serial number 72F38A3DEE757B6E6A37D73476289C204A1B85A5 to DB for /CN=laptop-001
Revoking Certificate 72F38A3DEE757B6E6A37D73476289C204A1B85A5.
Database updated
$ openssl ca -config crl-db/ca.cnf -gencrl -out ca.crl
Using configuration from crl-db/ca.cnf
$ kill -HUP <PID of the server>
```

The order and results of the real test:

1. After the new `ca.crl` is generated and before SIGHUP is sent, `laptop-001` still gets `accept 1`: the new list has not been read yet.
2. After SIGHUP, the log shows `configuration reloaded from rradius.toml`, and `rradius_reload_total{result="ok"}` goes up by 1.
3. Log in again, and `rradius-bench` prints `TLS: received fatal alert: CertificateRevoked`.

In a cluster, every node needs the same new CRL and its own SIGHUP.

> **Note:** A conversation finishes with the settings it started with. A conversation can last at most four times `session_timeout_ms`, so an old CRL cannot stay in force just because a supplicant keeps talking.

### How many gates a certificate login passes

```
Handshake: issued by client_ca_file? expired? in crl_file?
  ▼
Name: does the EAP Identity match a name on the certificate? (match_identity)
  ▼
Authorization: does certificate_query (or the script) find this device?
  ▼
Access-Accept
```

Each gate can stop a device, and they take effect at different speeds:

| Method | How fast it takes effect | Suits |
| --- | --- | --- |
| Disable in the table (for example `enabled = 0`, judged by `certificate_query`) | At the next login, across the whole cluster | Everyday disabling; blocks only at rradius |
| Revocation list (`crl_file`) | After the new list is on every node and SIGHUP is sent | When the CA issues CRLs, or the same certificate is used in other systems |

(This comparison table comes from the old manual and has not been measured again.) When the CA offers only online lookup (OCSP), you need the enterprise edition (see the enterprise note at the start of this chapter).

### Update the CRL on a schedule

A CRL has a Next Update, and rradius keeps using it after it expires (see above), so you have to refresh it regularly yourself:

1. Use cron or a systemd timer to download the CRL from the CA regularly and convert it to PEM (see the format notes under [Common mistakes](#common-mistakes)).
2. Download to a temporary file first, confirm that it starts with `-----BEGIN X509 CRL-----` and is not empty, and then swap it into the place of `crl_file` with `mv`. When the download fails, keep the old file; do not put an empty file there. An empty file, or a file without a CRL, is rejected by `check` and by reload (`crl_file '…' holds no certificate revocation list`), and when a reload fails the server keeps running with the old settings.
3. Send SIGHUP. Every node must do this.

This schedule itself (the download source, the CA's CRL distribution point) has not been verified on real equipment.

### ECDSA or RSA for the server certificate

When login volume is high, switch the server certificate to **ECDSA P-256**: the CPU cost of each full handshake is about one third of RSA 2048 (source: a measurement in the old `docs/usage.md`, not measured again in this chapter; for the baseline rows see [Chapter 19](19-performance.md)).

- **Use the P-256 curve, not P-384.** Community reports say old Android (7.0) supports only P-256, and a P-384 certificate chain fails there (not official documentation, not verified on a real device). The same goes for the CA and intermediate certificates.
- **The server uses only one certificate at a time.** Before you switch to ECDSA, connect once with each kind of device.
- **Generate the EC private key with OpenSSL 3.** The old documentation records that rradius cannot read EC private keys generated with the LibreSSL that ships with macOS (`failed to parse private key`); this was not verified again. To generate ECDSA, replace `-newkey rsa:2048` with `-newkey ec -pkeyopt ec_paramgen_curve:prime256v1`.

### Monitoring

A sudden rise of `rradius_eap_sessions_total{method="tls",result="reject"}` is often a CA level missing from the CRL (see `UnknownRevocationStatus` under [Common mistakes](#common-mistakes)), but it can also be a batch of devices whose certificates have expired or been revoked.

## Replace the server certificate

After the server certificate expires, supplicants that check it refuse this server (supplicant behavior, not verified on real equipment in this chapter), so replace it before it expires. `openssl x509 -in certs/server.pem -noout -enddate` shows the expiry date.

The steps:

1. Issue a new server certificate with the same CA, with the same name (for example `DNS:radius.corp.example.com`) and usage (`serverAuth`) as the old one. Supplicants trust the CA and the name, not that particular certificate, so you do not need to change the supplicant settings.
2. Put the new certificate and private key where `cert_file` and `key_file` point, and set the private key mode to 0600.
3. Run `rradiusd check --config rradius.toml` and confirm `configuration ok`. When the private key and the certificate are not a pair, you see `does not fit cert_file` here.
4. Send SIGHUP. Conversations that start afterwards use the new certificate; conversations in progress finish with the old one.
5. Every node in the cluster must be changed and given its own SIGHUP.

Tested: with the server running, a new certificate and private key issued by the same CA were put in place, `check` passed, and after SIGHUP an EAP-TLS login still gave `accept 1`. To confirm that the new certificate is really what is sent, a server certificate issued by another CA was put in and SIGHUP sent: after that `rradius-bench --ca-file certs/ca.pem` got `TLS: invalid peer certificate: BadSignature`, and only with the new CA's `--ca-file` did it get `accept 1`. So new conversations after SIGHUP do use the new certificate.

Changing the CA is different: every supplicant must trust the new CA first. In the transition period, let supplicants trust both the old and the new CA, and change the server certificate after all of them are updated.

`client_ca_file` can hold several CA certificates. To let device certificates issued by two CAs log in (for example while changing the CA), concatenate them in one file. When a client certificate has expired, the handshake fails, and just like with revocation it never reaches authorization.

## PEAP and TTLS: log in with an account and password

**Goal:** Use the `enterprise-wifi` scenario to let employees log in with PEAP and devices log in with EAP-TLS, and then add TTLS.

The EAP and client settings of this scenario:

```toml
# rradius.toml (excerpt)
[auth.username]
strip_realm = "suffix"
lowercase = true

[radius.eap]
cert_file = "certs/server.pem"
key_file = "certs/server.key"
client_ca_file = "certs/ca.pem"
methods = ["tls", "peap"]

[[radius.clients]]
name = "wlc"
address = "10.30.0.10"
secret_file = "wlc.secret"
eap_methods = ["tls", "peap"]
```

- EAP-TLS is proposed first; a device without a certificate uses a Legacy Nak to ask for PEAP.
- An employee may type `Staff1@corp.example.com`, and `[auth.username]` turns it into `staff1` in the table.
- The demo accounts are in `migrations/2_demo_data.sql`: `staff1` (password `staff1-password`, stored as `{nthash}`, enabled), `staff2` (disabled), and the device `laptop-001.corp.example.com`.

**Set up:** Prepare the working directory as in the previous section, run `sh make-certs.sh`, replace the three `.secret` files, change the client address to `127.0.0.1`, run `check`, and start.

**Verify:**

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user Staff1@corp.example.com --password staff1-password --load peap,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com
…
  peap: 398/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 10.0
…
```

PEAP takes 10 rounds: rradius proposes EAP-TLS first, the supplicant answers with a Nak to ask for PEAP, and then come the handshake, the MS-CHAPv2 inside the tunnel and the result acknowledgement.

The same command in a few other situations, with the real results:

| Test | Result | Reason in the log |
| --- | --- | --- |
| `staff1`, wrong password | `reject 1` | `failed: the chain did not accept the inner name` |
| `staff2` (disabled), correct password | `reject 1` | Same |
| Device certificate `--load eap-tls`, `--cert-file certs/device.pem` | `accept 1` | none |
| `staff1`, `--load ttls` | `reject 1`, only 2 rounds | `failed: the peer asked only for methods this server does not offer` |

The last row: this client does not have `ttls`, so the supplicant's Nak request is refused.

The authentication record in the log:

```text
… INFO rradius_auth: authentication protocol="radius" client="wlc" source=127.0.0.1 user="Staff1@corp.example.com" account="staff1" chain="eap" handler="eap-peap" decision="accept"
```

`user` is the outer name the NAS sent, and `account` is the inner name the auth chain used after normalization. For PEAP and TTLS the inner name decides everything; the outer name can be an anonymous value such as `anonymous@corp.example.com` without affecting the result.

**Add TTLS:** Change two lines, then reload:

```console
$ rradiusd config diff rradius.toml.before rradius.toml
radius.clients[wlc].eap_methods: ["tls", "peap"] | ["tls", "peap", "ttls"]
radius.eap.methods: ["tls", "peap"] | ["tls", "peap", "ttls"]
$ kill -HUP <PID of the server>
$ rradius-bench run --server 127.0.0.1 --secret "$(cat wlc.secret)" \
    --user staff1@corp.example.com --password staff1-password --load ttls,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com
…
  ttls: 687/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 6.0
…
```

(`rradius.toml.before` is a copy saved before the change.) The `ttls` load of `rradius-bench` uses inner PAP.

**What the result looks like:** On the same NAS, devices use certificates and employees use passwords, and both go through the same auth chain; `handler` is `eap-tls`, `eap-peap` or `eap-ttls`, and `account` is the normalized account. When the password is wrong or the account is disabled, the log says `failed: the chain did not accept the inner name`.

### What happens inside the tunnel

**PEAP:**

- Only PEAP version 0 is implemented, with only EAP-MSCHAPv2 inside, only TLS 1.2, and no client certificate required. When the supplicant asks for another version or insists on TLS 1.3, the conversation ends with EAP-Failure.
- The inner MS-CHAPv2 response goes to the client's auth chain. It counts as accepted only when the auth chain really compared the password and returned `MS-CHAP2-Success`. A script returning `accept` does not count, and a Lua handler cannot put `MS-CHAP2-Success` in by itself.
- When the password is wrong, the supplicant receives an MS-CHAPv2 Failure (`E=691`) in the tunnel, and then an EAP-Failure.

**TTLS:**

- Only version 0 is implemented, with only TLS 1.2 and no client certificate required.
- The inner method can be PAP (`User-Name`, `User-Password`) or MS-CHAPv2 (`MS-CHAP-Challenge`, `MS-CHAP2-Response`). The MS-CHAPv2 challenge must be a value exported from the TLS tunnel; the supplicant cannot pick it.
- Inner PAP counts only when the handler that accepts it is one that compares passwords: the sql handler, the ldap handler, or a lua handler with `verifies_password = true`. A script that only checks whether the account is enabled does not count, and the log says `the chain accepted the inner name without checking a password`.
- The inner name is at most 253 bytes and the PAP password at most 128 bytes; anything longer is rejected before the auth chain. An unknown inner AVP that is marked mandatory (M flag) is also rejected.

**Common to both:**

- The client must have an auth chain. Without one, the login is rejected, and the log says `this client has no chain to check a password with`.
- `default_user` does not apply to the password inside the tunnel.
- The MPPE keys that the auth chain returns are not sent; the keys derived from the TLS tunnel are sent instead. Other reply attributes go into the Access-Accept as usual. When the auth chain does not return `User-Name`, rradius puts in the normalized account name.
- A wrong password is logged, with the authentication record, the moment the auth chain rejects it, without waiting for the supplicant to finish the conversation; the same conversation is not recorded twice.
- A password login inside the tunnel counts toward `auth.lockout` as usual ([Chapter 07](07-users-and-chains.md)).
- At most 8 rounds inside the tunnel.

> **Not verified on real hardware:** TTLS inner MS-CHAPv2 was only checked against the code and the tests. The `ttls` load of `rradius-bench` sends only inner PAP, so this chapter never actually ran inner MS-CHAPv2.

## How to store passwords

PEAP uses MS-CHAPv2, and the server must be able to compute the NT hash of the password to compare. So for PEAP accounts, store the password as `{nthash}` or in clear text; a one-way hash such as bcrypt cannot be used to compare MS-CHAPv2. The inner PAP of TTLS receives the clear-text password, so any format can be compared.

Tested: add `[accounts]` to `enterprise-wifi` and add one account with each of two `scheme` values:

```toml
# rradius.toml (add at the end of the file)
[accounts]
scheme = "nthash"
add = "INSERT INTO accounts (name, kind, password, grp) VALUES (?1, 'user', ?2, 'staff')"
passwd = "UPDATE accounts SET password = ?2 WHERE name = ?1 AND kind = 'user'"
show = "SELECT name, kind, grp, enabled FROM accounts WHERE name = ?1"
```

`staff3` was added with `scheme = "nthash"`, and `staff4` was added after changing `scheme` to `"bcrypt"` (`printf 'staff3-password\n' | rradiusd user add --config rradius.toml staff3`). Results:

| Account | Storage format | PEAP | TTLS (inner PAP) |
| --- | --- | --- | --- |
| `staff3` | `nthash` | `accept 1` | `accept 1` |
| `staff4` | `bcrypt` | `reject 1` | `accept 1` |

The rules for choosing:

- To use PEAP, store `nthash`. An NT hash has no salt, so if the table leaks it is about as dangerous as clear text; protect the cluster data directory well.
- When you use only TTLS inner PAP, you can store a safer hash such as bcrypt.
- When the account is in LDAP and the password can only be checked with a bind, the bind accepts only a clear-text password, so it suits TTLS inner PAP only, not PEAP.

`rradius_password_checks_total` is counted per storage format; for example, the tests above raised `ok` for `scheme="nthash"`. The formats are described in [Chapter 07](07-users-and-chains.md).

## Seeing EAP in Lua scripts

When an EAP login reaches the auth chain or a script, the script has three more global variables:

| Variable | When it has a value | Content |
| --- | --- | --- |
| `eap_method` | EAP login | `"tls"`, `"peap"` or `"ttls"`; nil when it is not EAP |
| `tls_subject` | When `eap_method` is `"tls"` | The subject of the client certificate, as text, in the order on the certificate, with values escaped per RFC 4514, for example `CN=laptop-002` |
| `tls_subject_attrs` | Same | A table: attribute name to an array of its values (not escaped), for example `tls_subject_attrs.CN[1]` |

`get_attr('User-Name')` gives the name on the certificate (EAP-TLS) or the name inside the tunnel (PEAP, TTLS).

In a configuration without `[auth]`, the script below lets certificates whose CN starts with `laptop-` log in, and puts in a `Filter-Id`:

```lua
-- auth.lua: with no [auth], this script decides after an EAP-TLS certificate passes
-- This demo records no accounting: an accounting script that returns 'accept' means it has been recorded, see Chapter 13.
if phase ~= 'authentication' then
  return 'reject'
end
if eap_method == 'tls' then
  log('certificate', get_attr('User-Name'), tls_subject)
  local cn = tls_subject_attrs and tls_subject_attrs.CN and tls_subject_attrs.CN[1]
  if cn and cn:match('^laptop%-') then
    set_attr('Filter-Id', 'corp-devices')
    return 'accept'
  end
end
return 'reject'
```

Tested (the `config init` starter plus the `conf.d/eap.toml` above, without `crl_file`): a login with the `laptop-002` certificate gives `accept 1`, and the log:

```text
… INFO rradius_core::lua_engine: script auth.lua (radius/authentication, client example): certificate laptop-002.corp.example.com CN=laptop-002
… INFO rradius_auth: authentication protocol="radius" client="example" source=127.0.0.1 user="laptop-002.corp.example.com" account="laptop-002.corp.example.com" chain="eap" handler="eap-tls" decision="accept"
```

Note that `crl_file` is not set here, so the revoked `laptop-002` got in too. A script can only decide what happens after the handshake passes; revocation depends on the CRL.

A few limits:

- This script is no use for PEAP and TTLS: their passwords must be checked by the auth chain, so the configuration must have `[auth]`.
- When a lua handler in the auth chain is to compare the password for TTLS inner PAP, set `verifies_password = true` and compare `User-Password` yourself.
- Other ways of writing scripts are in [Chapter 10](10-lua-policies.md), and the full API is in [Appendix D](93-lua-reference.md).

## Conversation limits and timeouts

Every unfinished conversation takes memory. Anyone who can send an EAP Identity through a NAS can open a conversation, so rradius puts limits on them:

| Situation | What rradius does | Count (`result` of `rradius_eap_sessions_total`) |
| --- | --- | --- |
| No next round within `session_timeout_ms` | Drops the conversation, cleared within about a second | `timeout` |
| Rounds keep coming, but more than four times `session_timeout_ms` has passed | Drops it | `timeout` |
| A conversation receives its 257th request | Answers EAP-Failure and drops it | `reject` |
| The total number of conversations reaches `max_sessions`, or this client's reaches `max_sessions_per_client` | Opens the new conversation and drops an old one: first those that went silent after the Identity, then the one idle for the longest time | `evicted` |
| The unfinished conversations of one device reach `max_sessions_per_device` | Does not answer the new request; the NAS retransmits | `overload` |
| The same device goes silent after the Identity and then starts again | The old one is replaced by the new one | `replaced` |

- "The same device" is the same client with the same `Calling-Station-Id`. When a request has no `Calling-Station-Id`, devices cannot be told apart, and only `max_sessions` and `max_sessions_per_client` take effect.
- A conversation in progress is heard in every round and ranks last for dropping, so a flood of fake Identities cannot push out real logins.
- Once a conversation has been recorded as a rejection, a later timeout or eviction is not counted again.

You normally do not need to tune these values. For when you do:

- When one controller has many devices and many people log in at the same time (for example at the start of the working day) and the `1024` of `max_sessions_per_client` is not enough, raise it.
- When `session_timeout_ms` is too short, slow supplicants, or phones that wait for the user to tap "trust certificate", get dropped midway.
- `fragment_size` is `1000` by default. Lower it when the NAS or the network in between has trouble with large RADIUS packets; the minimum is 256.

`rradius_eap_sessions_open` is the number of unfinished conversations right now.

`rradius-bench` can also measure throughput with an EAP load (for example `--load eap-tls,concurrency=16`); that is covered in [Chapter 19](19-performance.md). When measuring, remember to set `max_sessions` and `max_sessions_per_client` higher than the number of simultaneous conversations; `rradius-server/benchmarks/eap.toml` in the source does exactly that.

## When the database is unavailable

The EAP conversation itself is in memory, but the last step (the auth chain querying the table) needs the cluster database. When the database is temporarily unavailable (for example a call times out; read-only queries are still answered from the local replica when there is no leader, see Chapter 16):

- The request is not answered, `rradius_radius_discarded_total{reason="database"}` goes up by 1, and the log says `EAP request from client <name> not answered: …`.
- The conversation is kept. When the NAS retransmits the same EAP-Response, rradius redoes that step and does not read the TLS data again.
- Each retransmission counts as a round and is still subject to `session_timeout_ms` and the limit of 256 rounds.

So when the database is down briefly, supplicants only log in more slowly and are not rejected; when the outage is too long, the conversation times out and the supplicant must start over.

> **Not verified on real hardware:** This part was only checked against the code and the spec, and was never run under a real database failure. For the overall behavior when the cluster has no leader, see [Chapter 16](16-cluster.md).

## EAP metrics and logs

The EAP metrics on `/metrics` (from a real test; names and labels copied as they are):

```text
rradius_eap_sessions_open 0
rradius_eap_sessions_total{method="tls",result="accept"} 1
rradius_eap_sessions_total{method="tls",result="reject"} 1
rradius_eap_sessions_total{method="peap",result="accept"} 2
rradius_eap_sessions_total{method="peap",result="reject"} 3
rradius_eap_sessions_total{method="ttls",result="accept"} 3
rradius_eap_sessions_total{method="ttls",result="reject"} 1
…
```

`method` is `tls`, `peap` or `ttls`, and `result` is one of six values: `accept`, `reject`, `timeout`, `overload`, `replaced`, `evicted` (their meaning is in the previous section). Every combination appears, including those with the value 0. A conversation is counted only once.

How to read them when troubleshooting:

- `reject` keeps rising: usually wrong passwords, disabled accounts or authorization that does not pass. Look at the reason in the log.
- `timeout` keeps rising: supplicants open conversations but do not finish them. The usual causes are that the supplicant does not trust the server certificate (it just gives up), or that packets are lost between the NAS and rradius.
- `overload` and `evicted` rise: the number of conversations has hit a limit; see the previous section.

`status` has an EAP section that shows the number of unfinished conversations:

```console
$ rradiusd status --config rradius.toml
…
EAP
  Sessions under way  1
…
```

(This is what you see in `wifi-certs` after testing a revoked certificate with `rradius-bench`: that conversation is waiting for the 30-second timeout.)

The log lines related to EAP are all WARN:

| Start of the line | Meaning |
| --- | --- |
| `EAP conversation with client <name> refused: <reason>` | The handshake succeeded but authorization did not pass, for example `not authorized`, `the EAP Identity does not fit the certificate` |
| `EAP conversation with client <name> failed: <reason>` | The conversation failed midway, for example wrong password, method not allowed, TLS handshake failure |
| `EAP request from client <name> not answered: <reason>` | The database is unavailable and the request got no answer; the conversation waits for the NAS to retransmit |

Each conversation that ends also has an `rradius_auth` authentication record. The log never prints the MSK or TLS content, and never prints the password inside the tunnel. The full description of metrics and logs is in [Chapter 17](17-monitoring.md) and [Appendix C](92-metrics-reference.md).

## Connect real NASes and clients

The tests in this chapter use `rradius-bench` to play both the NAS and the supplicant. When you connect real equipment:

1. **NAS:** On the AP controller or switch, point the RADIUS server at the rradius authentication port (default 1812), with the shared secret equal to `secret_file`; change the client's `address` back to the NAS's real address. Configure accounting as in [Chapter 13](13-accounting.md).
2. **Supplicant:** Set it to trust the CA that issued the server certificate and give the server name (the example is `radius.corp.example.com`). For EAP-TLS, also install the device certificate and private key. When there are many devices, use MDM or group policy (GPO) to push the CA certificate, the server name and the Wi-Fi settings to the devices together, instead of relying on users to tap "trust" by hand; without preconfiguration, users are asked again and again whether to trust the certificate, or they simply choose not to verify. This step is an operation of each operating system and management system, and this manual has not tested it.
3. **VLAN:** The NAS must support assigning a VLAN by `Tunnel-Private-Group-Id`, so that the reply attributes in the examples have an effect.

The `eapol/` directory of the `enterprise-wifi` scenario has two `eapol_test` configurations (`tls.conf`, `peap.conf`), which let a Linux machine simulate a supplicant. `eapol_test` is a test tool that ships with wpa_supplicant and is not in the repo.

> **Warning:** Supplicants must verify the server certificate. If they do not, a fake access point can impersonate your network, collect the PEAP MS-CHAPv2 responses, and guess passwords offline.

> **Not verified on real hardware:** This chapter has not been tested with a real AP, a switch, `eapol_test` or the 802.1X supplicant built into any operating system. The steps above and the settings in `eapol/` were only checked against the scenario files and the code.

## Common mistakes

**Symptom:** `check` prints `rradius.toml: radius.eap.cert_file 'certs/server.pem': I/O error: No such file or directory (os error 2)`.
**Cause:** The certificates have not been generated yet, or the path is wrong. Paths are relative to the directory of the main configuration file.
**Fix:** Run the scenario's `make-certs.sh` or `setup.sh` first, or change to the correct path.

**Symptom:** `check` prints `radius.eap.client_ca_file is required`.
**Cause:** `methods` has `tls` (the default is `["tls"]`), but there is no `client_ca_file`.
**Fix:** Add the CA that issues the client certificates; when you use only PEAP or TTLS, write `methods` explicitly and do not rely on the default.

**Symptom:** `check` prints `radius.eap.crl_file only applies to tls, which methods does not have` (`client_ca_file` gets the same message).
**Cause:** The methods have no `tls`, so these settings have no effect.
**Fix:** Delete them, or add `tls` to `methods`.

**Symptom:** `check` prints `radius.eap.key_file 'certs/other.key' does not fit cert_file 'certs/server.pem': keys may not be consistent: KeyMismatch`.
**Cause:** The private key and the certificate are not a pair.
**Fix:** Use the private key that was used to issue this certificate.

**Symptom:** `check` prints `radius.eap.client_ca_file '…' holds no certificate` or `radius.eap.crl_file '…' holds no certificate revocation list`.
**Cause:** The wrong file was given, or it is not in PEM format.
**Fix:** Confirm that the file content is `-----BEGIN CERTIFICATE-----` or `-----BEGIN X509 CRL-----`. Convert DER to PEM first with `openssl x509` or `openssl crl`.

**Symptom:** `check` prints `radius.eap.methods has a method whose password only a chain can check, and there is no [auth] section`.
**Cause:** `peap` or `ttls` is used, but there is no auth chain.
**Fix:** Add `[auth]` and a handler that compares passwords ([Chapter 07](07-users-and-chains.md)).

**Symptom:** `check` prints `radius client 'example': eap_methods is required when radius.eap.methods has more than one method; [] for a client without EAP`.
**Cause:** When two or more methods are offered, every client must write `eap_methods`.
**Fix:** Write its methods for each client; write `[]` for those that do not use EAP.

**Symptom:** `check` prints `radius client 'example': eap_methods has a method that radius.eap.methods does not`, or `eap_methods needs [radius.eap]`.
**Cause:** The client lists a method the server does not offer, or there is no `[radius.eap]` at all.
**Fix:** Make `eap_methods` a subset of `radius.eap.methods`.

**Symptom:** `check` prints `radius.eap.fragment_size must be from 256 to 1400, not 1500`, `radius.eap.session_timeout_ms must be greater than 0`, or `radius.eap.max_sessions must be greater than 0`.
**Cause:** A value is out of range.
**Fix:** Change it as the message says.

**Symptom:** `check` prints `radius.eap.<key>: not supported by this program (… needs rradius-enterprise)`, or `radius.eap.methods: <method> is not supported by this program (… needs rradius-enterprise)`.
**Cause:** The configuration has a key or method that exists only in the enterprise edition. The keys that are rejected this way are listed under "Settings the core does not support or that have no effect" in [Appendix A](90-config-reference.md).
**Fix:** Delete them from the `rradiusd` configuration; use only `tls`, `peap` and `ttls` as methods.

**Symptom:** `check` prints `radius.eap.methods: 'md5' is not a method this server offers; it has tls, peap and ttls`.
**Cause:** The core has only the three methods `tls`, `peap` and `ttls`.
**Fix:** Use one of these three.

**Symptom:** `rradius-bench` prints `TLS: received fatal alert: CertificateRevoked`.
**Cause:** The client certificate is in `crl_file`.
**Fix:** This is the expected result. If the certificate should not be revoked, check whether the CRL is the latest one.

**Symptom:** Certificates issued by an intermediate CA always fail the handshake and the supplicant receives `UnknownCA`, but without `crl_file` they can log in.
**Cause:** `crl_file` has only the root CA's CRL. Every CA level in the chain needs a CRL.
**Fix:** Concatenate the CRL of every CA level into the same `crl_file`, then send SIGHUP.

**Symptom:** After `crl_file` is set, all devices are rejected, and the reason in the log is `invalid peer certificate: UnknownRevocationStatus`.
**Cause:** One CA level in the chain has no matching CRL (it was not put in `crl_file`, or another CA's was put in). The program does not turn on "let it through when the revocation status cannot be found", so a missing status means rejection (`rradius-core/src/common/tls.rs` only calls `with_crls`, not `allow_unknown_revocation_status`). The alert the supplicant receives varies with the chain; in the intermediate CA example in the previous test it was `UnknownCA`. The phrase `UnknownRevocationStatus` itself comes from the library's error type and was not reproduced on real equipment separately.
**Fix:** Concatenate the CRLs of the root CA and of every intermediate CA into the same `crl_file`, run `check`, then send SIGHUP.

**Symptom:** A new CRL was put in, but revoked certificates can still log in.
**Cause:** It has not been reloaded.
**Fix:** Send SIGHUP and confirm that the log has `configuration reloaded`.

**Symptom:** The log says `refused: the EAP Identity does not fit the certificate`.
**Cause:** The EAP Identity the supplicant reports is not a DNS or email name on the certificate (when these names exist, the CN does not count).
**Fix:** Make the supplicant's Identity use a name on the certificate; when the supplicant can only report an anonymous name, set `match_identity = false`.

**Symptom:** The log says `refused: not authorized`.
**Cause:** The handshake succeeded, but the auth chain or script did not accept the name. A common cause with the sql handler is that `certificate_query` finds nothing, the device is disabled, or there is no `certificate_query` and the password that `user_query` finds is empty.
**Fix:** Look up the table with the `account` name from the log.

**Symptom:** The log says `auth chain: rejecting a request from client wlc: normalization changes a certificate's name in more than case`.
**Cause:** `strip_realm` in `[auth.username]` would remove the realm of the certificate name.
**Fix:** Store the full name from the certificate in the table and let certificate logins use another auth chain without `strip_realm`, or use certificates with DNS names.

**Symptom:** PEAP is always `reject`, while the same account can log in with TTLS inner PAP.
**Cause:** The password is stored as a one-way hash such as bcrypt, which cannot be used to compare MS-CHAPv2.
**Fix:** Store `{nthash}` for PEAP accounts (`accounts.scheme = "nthash"`), or use TTLS.

**Symptom:** The log says `failed: the peer asked only for methods this server does not offer`.
**Cause:** The method the supplicant wants is not in this client's `eap_methods` (or the supplicant wants only EAP-MD5).
**Fix:** Add that method to `radius.eap.methods` and to the client's `eap_methods`, or change the supplicant's settings.

**Symptom:** The log says `failed: the chain accepted the inner name without checking a password`.
**Cause:** TTLS inner PAP was accepted only by a lua handler that does not compare the password.
**Fix:** Add a sql or ldap handler, or let the lua handler compare the password itself and set `verifies_password = true`.

**Symptom:** `rradius-bench` prints `rradius-bench: --no-message-authenticator: EAP loads always carry Message-Authenticator (RFC 3579)` and exits with code 3.
**Cause:** EAP loads always carry `Message-Authenticator`; the server also drops EAP requests that do not have it.
**Fix:** Do not add `--no-message-authenticator` to an EAP load.

**Symptom:** The supplicant keeps retrying, `rradius_eap_sessions_total{result="timeout"}` keeps rising, and the log has no `refused` or `failed`.
**Cause:** The supplicant gives up during the TLS handshake, most often because it does not trust the server certificate or the server name does not match.
**Fix:** Check the supplicant's CA setting and server name; use `rradius-bench --ca-file … --tls-name …` to confirm that the certificate the server sends verifies.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [08 LDAP and Active Directory](08-ldap-and-ad.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [13 Accounting and sessions](13-accounting.md)
- [15 Scenarios: network access for organizations](15-access-recipes.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [19 Performance tuning and load testing](19-performance.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix C: Metrics reference](92-metrics-reference.md)
- [Appendix D: Lua API reference](93-lua-reference.md)
