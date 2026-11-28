# 08 LDAP and Active Directory

> After this chapter you can:
> - Let users log in with their account and password from the company directory (LDAP or Active Directory)
> - Decide by directory group who may log in and which reply attributes they get
> - Connect to the directory securely with TLS, and know what the server does when the directory fails
> - Verify MS-CHAPv2 and PEAP with Netlogon in an Active Directory environment

Prerequisites: you have read [chapter 07](07-users-and-chains.md) and know auth chains, handlers and `first_match`/`all_accept`. You need an LDAP server or an Active Directory domain controller, and a service account that can search for users.

In this chapter:

- [Which handler to choose](#which-handler-to-choose)
- [The test directory used in this chapter](#the-test-directory-used-in-this-chapter)
- [Logging in with directory accounts](#logging-in-with-directory-accounts)
- [Connection encryption: ldaps, StartTLS and the CA](#connection-encryption-ldaps-starttls-and-the-ca)
- [Deciding by group who can log in](#deciding-by-group-who-can-log-in)
- [Giving reply attributes by group](#giving-reply-attributes-by-group)
- [Comparing a password attribute in the directory](#comparing-a-password-attribute-in-the-directory)
- [Search filter for certificate logins](#search-filter-for-certificate-logins)
- [When the directory cannot be reached](#when-the-directory-cannot-be-reached)
- [The remote-access scenario: VPN with AD accounts](#the-remote-access-scenario-vpn-with-ad-accounts)
- [Active Directory MS-CHAPv2: the netlogon handler](#active-directory-ms-chapv2-the-netlogon-handler)
- [Putting LDAP and netlogon in the same chain](#putting-ldap-and-netlogon-in-the-same-chain)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Which handler to choose

| | ldap handler | netlogon handler |
| --- | --- | --- |
| Connects to | Any LDAP server, including the LDAP of AD | The Netlogon service of an AD domain controller |
| How it verifies | Binds as the user (default), or reads the password attribute and compares | Hands the MS-CHAPv2 response to the domain controller to verify |
| Supported logins | bind: PAP only. Read attribute: PAP, CHAP, MS-CHAPv1, MS-CHAPv2, depending on the stored format (MS-CHAPv1 not verified on real equipment) | MS-CHAPv2 only (including the inner method of PEAP) |
| Groups | DN, for example `CN=vpn,OU=Groups,DC=corp,…` | SID, for example `S-1-5-21-…-1106` |
| Fits | VPN, web login, and devices that send PAP such as TACACS+ | Wi-Fi 802.1X (PEAP-MSCHAPv2) with AD accounts |

Why AD needs a separate handler: PEAP on Wi-Fi sends an MS-CHAPv2 response, not a clear-text password, so there is nothing to bind with. AD also never hands out the user's NT hash over LDAP, so the server cannot compare it itself. The netlogon handler gives the response to the domain controller, which answers whether it is correct.

You can put both handlers in the same chain. Each one handles its own kind of login. See [Putting LDAP and netlogon in the same chain](#putting-ldap-and-netlogon-in-the-same-chain).

## The test directory used in this chapter

The output in this chapter comes from an OpenLDAP server running in Docker (an alpine image with the OpenLDAP packages, the `memberof` overlay enabled, ldaps and StartTLS with a self-signed CA), and from the Samba AD domain controller image in `docker/interop-netlogon/` in the rradius source. They are test environments only, and your directory may be structured differently.

Data in the directory:

| DN | Password | Groups | Other |
| --- | --- | --- | --- |
| `cn=radius,ou=services,dc=example,dc=com` | `svc-password` | — | Service account, can read `userPassword` |
| `uid=alice,ou=people,dc=example,dc=com` | `alice-password` (clear text) | `vpn-users`, `it-staff` | `description: std.ppp` |
| `uid=bob,ou=people,dc=example,dc=com` | `bob-password` (clear text) | none | |
| `uid=dave,ou=people,dc=example,dc=com` | `dave-password` (stored as `{SSHA}`) | `vpn-users` | |

The groups are `groupOfNames` entries under `ou=groups,dc=example,dc=com`.

The rest of the server configuration follows the quick-start in [chapter 03](03-quick-start.md) (single-node cluster, one client at `127.0.0.1`). Login tests use `rradius-bench`; the shared arguments are shortened to `…`.

> **Not verified on real hardware:** This has not been tested on a real Windows Server Active Directory. The LDAP part was tested on OpenLDAP. The netlogon part was tested on a Samba AD domain controller.

## Logging in with directory accounts

Goal: users log in with PAP using their directory account and password.

How the ldap handler works:

1. It binds with the service account (`bind_dn`).
2. It searches for the user under `base_dn` with `filter`, replacing `{user}` with the account name.
3. It binds again with the DN it found and the password the user sent. If that succeeds, it accepts.

```toml
# rradius.toml (excerpt)
[auth]
[[auth.handlers]]
type = "ldap"
urls = ["ldaps://ldap1.example.com", "ldaps://ldap2.example.com"]
bind_dn = "cn=radius,ou=services,dc=example,dc=com"
bind_password_file = "ldap.secret"
base_dn = "ou=people,dc=example,dc=com"
filter = "(uid={user})"
ca_file = "corp-ca.pem"
reply_attributes = { "Filter-Id" = "description" }

[auth.username]
strip_realm = "both"
lowercase = true
```

| Key | Default | Description |
| --- | --- | --- |
| `urls` | — (required) | Start with `ldap://` or `ldaps://`, at least one, tried in order |
| `bind_dn` | — (required) | DN of the service account |
| `bind_password_file` | — | File with the service account password; the trailing newline is removed |
| `bind_password` | — | The password written directly. Set exactly one of this and `bind_password_file` |
| `base_dn` | — (required) | Where the user search starts, the whole subtree below it |
| `filter` | — (required) | Search filter, with exactly one `{user}` |
| `password_check` | `bind` | `bind` or `attribute` (see [Comparing a password attribute in the directory](#comparing-a-password-attribute-in-the-directory)) |
| `reply_attributes` | — | Maps reply attribute names to LDAP attributes of the user |
| `ca_file` | — | CA certificates to trust (see the next section) |
| `allow_plaintext` | `false` | Allow unencrypted `ldap://` |
| `pool_size` | `4` | Connections kept open per URL |
| `timeout_ms` | `5000` | Time limit for connecting to one URL plus the whole query, in milliseconds |

Every key can be reloaded. The group keys are in the next two sections. For the full list of fields see [Appendix A](90-config-reference.md).

Create the password file:

```console
$ printf '%s\n' '<service account password>' > ldap.secret
$ chmod 600 ldap.secret
```

- Before `{user}` is substituted, it is escaped as in RFC 4515 (`*`, `(`, `)`, `\` and NUL become forms such as `\2a`), so a user name cannot change the structure of the filter. (According to the code; this chapter did not test it separately.)
- The value substituted is the account name after the auth chain has normalized it. The `[auth.username]` above makes both `CORP\Alice` and `alice@example.com` search for `alice` ([chapter 07](07-users-and-chains.md)).
- When a `reply_attributes` value has several values, each value adds one attribute, in the order the directory returned them.
- The service account password cannot be empty: a bind with an empty password is an anonymous bind, and `check` rejects it.
- **Exclude disabled accounts with `filter`.** A user the search cannot find is treated as not existing, so you can exclude disabled accounts directly in the filter. For AD the form is `(&(sAMAccountName={user})(!(userAccountControl:1.2.840.113556.1.4.803:=2)))` (bit 2 of `userAccountControl` means disabled). If OpenLDAP has the password policy (ppolicy) enabled, you can use `(&(uid={user})(!(pwdAccountLockedTime=*)))` to exclude locked accounts. `check` only counts the `{user}` placeholders. Whether the filter itself works, you have to try on your own directory first, for example with `ldapsearch`: on an OpenLDAP without ppolicy, the ppolicy form found nobody, because the schema does not define the attribute.

> **Not verified on real hardware:** The directory-side behavior of the two exclusion forms (the `userAccountControl` bit, ppolicy's `pwdAccountLockedTime`) has not been tested on a real AD or on OpenLDAP with ppolicy enabled. The code only sends the filter as written, and `check` confirmed that the OpenLDAP one passes.

### Verification

```console
$ rradius-bench run … --user alice --password alice-password --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
$ rradius-bench run … --user alice --password wrong --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user nobody --password x --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user 'EXAMPLE\Alice' --password alice-password --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
```

Authentication log:

```text
… user="alice" account="alice" chain="default" handler="ldap" decision="accept"
… user="alice" account="alice" chain="default" handler="ldap" decision="reject"
… user="nobody" account="nobody" chain="default" handler="none" decision="reject"
… user="EXAMPLE\\Alice" account="alice" chain="default" handler="ldap" decision="accept"
```

- A wrong password is a reject from `handler="ldap"`.
- For a name the directory cannot find, the ldap handler ignores it and passes it to the next handler. There is none here, so it shows `handler="none"`.
- If the search finds two or more entries, the handler rejects and the log says `filter matched 2 entries; rejecting` (according to the code and its tests; the test directory in this chapter has no duplicate accounts).

Use `[log.trace]` to see the Access-Accept for `alice` ([chapter 17](17-monitoring.md)). The value of `description` became `Filter-Id`:

```text
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Filter-Id = std.ppp, Class = it-staff, Session-Timeout = 28800"
```

(`Class` and `Session-Timeout` come from [Giving reply attributes by group](#giving-reply-attributes-by-group).)

Bind mode cannot verify CHAP or MS-CHAPv2: these logins have no clear-text password to bind with, and the handler rejects them outright for a user its search finds (it does not ignore them). In the test, `alice` with CHAP and with MS-CHAPv2 both gave `reject 1`, and the MS-CHAPv2 Access-Reject carried `MS-CHAP-Error`.

**Verified**: the four logins `alice`, wrong password, `nobody` and `EXAMPLE\Alice`, the response in the trace, and the rejection of CHAP and MS-CHAPv2.

Also according to the code: an empty PAP password is rejected without sending a bind, because a bind with an empty password is an anonymous login in LDAP and would succeed.

### How connections are reused

- Each URL has its own connection pool of at most `pool_size` connections. A new connection finishes TLS and the service account bind before it is used.
- When a connection is returned to the pool and taken out again, it first binds again as the service account (time limit is half of `timeout_ms`), so the bind identity of the previous user never carries over to the next request.
- A connection is dropped, not returned to the pool, after a failed search, a bind error other than a wrong password, or when it goes over `timeout_ms`. A connection with a wrong password (invalidCredentials) is returned as usual.
- RADIUS and TACACS+ clients that point to the same chain share these pools.

## Connection encryption: ldaps, StartTLS and the CA

The user's password is sent to the directory to bind, so the connection must be encrypted.

| `urls` form | `allow_plaintext` | Result |
| --- | --- | --- |
| `ldaps://host[:port]` | either | TLS from the start (default port 636) |
| `ldap://host[:port]` | `false` (default) | After connecting, it does StartTLS first, then binds |
| `ldap://host[:port]` | `true` | Not encrypted. The service account and user passwords are sent in clear text |

Both kinds of TLS verify the server certificate and host name. The trusted CAs are the operating system root certificates plus the certificates in `ca_file` (PEM, can hold several). If the company directory uses a certificate signed by its own CA, put that CA in `ca_file`.

**Verified**:

- `ldaps://` with `ca_file`: login succeeds.
- `ldap://` with StartTLS and `ca_file`: login succeeds.
- Without `ca_file`: both `ldaps://` and StartTLS fail to connect, and the log says

  ```text
  … WARN rradius_core::ldap_handler: auth ldap handler: ldaps://localhost:636: I/O error: invalid peer certificate: UnknownIssuer
  … ERROR rradius_core::ldap_handler: auth ldap handler: no server reachable; rejecting
  … WARN rradius_core::lua_engine: RADIUS Access-Request from client localhost not answered: the authentication backend could not be used
  ```

  This login gets no response (see [When the directory cannot be reached](#when-the-directory-cannot-be-reached)).
- `ldap://` with `allow_plaintext = true`: login succeeds, but without encryption.

> **Warning:** `allow_plaintext = true` sends passwords over the network in clear text. Use it only in a test environment, or when the connection is already inside an encrypted tunnel (for example WireGuard).

The host name in `urls` must match the name on the directory certificate. If you write an IP address, the certificate needs a SAN for that IP.

## Deciding by group who can log in

Goal: the password must be correct and the user must also belong to a certain group to be let in.

```toml
# rradius.toml (add inside the ldap handler)
required_groups = ["cn=vpn-users,ou=groups,dc=example,dc=com"]
```

The user must belong to **one** of the groups in `required_groups` to be accepted. Group DNs are compared without regard to case or to spaces around commas, so `CN=VPN-Users, OU=Groups, DC=example, DC=com` counts as the same as the line above.

The handler can find out the user's groups in two ways:

| Way | Setting | Fits |
| --- | --- | --- |
| Read an attribute of the user | `group_attribute`, default `memberOf` | AD; OpenLDAP with the `memberof` overlay |
| Search for groups | `group_base_dn` plus `group_filter`, for example `(member={dn})` | Membership is recorded on the group side (`groupOfNames`) and the user has no `memberOf` |

```toml
# The second way: search for groups (add inside the ldap handler)
group_base_dn = "ou=groups,dc=example,dc=com"
group_filter = "(member={dn})"
required_groups = ["cn=vpn-users,ou=groups,dc=example,dc=com"]
```

- `group_filter` has exactly one `{dn}`, replaced with the DN of the user's entry (escaped). The search runs as the service account, and the DNs in the result are compared.
- Choose only one of `group_attribute` and `group_filter`; `group_filter` must be paired with `group_base_dn`.
- These keys only mean something when `required_groups` or `group_reply` is present. If only they are written, `check` fails, because it looks like a restriction but restricts nothing.
- **Nested groups:** both `memberOf` and `(member={dn})` count direct membership only. Membership a user has through a child group does not count. To count nested groups in AD, use the group search way with `group_filter = "(member:1.2.840.113556.1.4.1941:={dn})"` (AD's `LDAP_MATCHING_RULE_IN_CHAIN`), together with `group_base_dn`. This is an AD-specific matching rule, and the domain controller expands the nested groups.

> **Not verified on real hardware:** `check` accepts this `group_filter` (exactly one `{dn}`), but it has not been run against AD.

### Verification

`bob` has the right password but is not in `vpn-users`; `alice` and `dave` are:

```console
$ rradius-bench run … --user bob --password bob-password --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user dave --password dave-password --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
```

**Verified**: reading `memberOf` (default) and searching for groups give the same result.

A rejection for not being in the group looks exactly the same to the NAS as a rejection for a wrong password. An outsider cannot tell which one it was. The handler looks up the groups with the service account before it compares the password, and decides after the password result is in.

## Giving reply attributes by group

Goal: IT staff get different rules after connecting to the VPN and may stay connected longer.

```toml
# rradius.toml (after the ldap handler)
[[auth.handlers.group_reply]]
group = "cn=it-staff,ou=groups,dc=example,dc=com"
attributes = { "Class" = "it-staff", "Session-Timeout" = "28800" }
```

- When the user belongs to `group`, the reply attributes in `attributes` are added. You can write several `group_reply` entries; they are added in the configured order, after `reply_attributes`.
- The attribute name must be in the dictionary and the value must have the right format: `check` encodes every value once, so a mistake is reported right away and does not wait for a user to log in.
- With `group_reply` but without `required_groups`, anyone can log in, and the group only decides which extra attributes they get.

`alice` is in `it-staff`, and the response in the `[log.trace]` entry above has `Class = it-staff, Session-Timeout = 28800`. `dave` is not, and his Access-Accept has neither attribute. **Verified**.

A reply attribute only has an effect if the NAS has a matching setting, for example a filter or group policy with the same name on the VPN gateway.

## Comparing a password attribute in the directory

`password_check = "attribute"` makes the handler not bind as the user, but read the user's password attribute and compare it itself:

```toml
# rradius.toml (inside the ldap handler)
password_check = "attribute"
password_attribute = "userPassword"
```

The comparison rules are the same as for the sql handler (the format table in [chapter 07](07-users-and-chains.md)): formats such as `{SSHA}` and `{crypt}` can only verify PAP; clear text or `{nthash}` can also verify MS-CHAPv2, and clear text can additionally verify CHAP. MS-CHAPv1 is compared by the same rules (`allow_mschapv1 = false` in `[auth]` rejects it before it reaches the handler, see [chapter 07](07-users-and-chains.md)); this point is **not verified on real equipment**.

Uses:

- The directory stores clear text or NT hashes, and the NAS sends MS-CHAPv2 or CHAP.
- The directory does not allow ordinary users to bind, but allows the service account to read the password attribute.

**Verified**: `alice` (stored as clear text in the directory) gets `accept 1` with PAP, CHAP and MS-CHAPv2; `dave` (`{SSHA}`) gets `accept 1` with PAP and `reject 1` with MS-CHAPv2.

> **Warning:** This mode requires the service account to be able to read everyone's password attribute. If this account leaks, every password leaks (a clear-text password or NT hash can be used to log in directly). Use bind mode whenever you can.

- If the user's entry has no value for `password_attribute`, the handler rejects.
- The value is read out and compared afterwards, and the comparison does not hold a connection. A comparison that goes over `timeout_ms` counts as "backend unavailable".

## Search filter for certificate logins

When EAP-TLS logs in with a certificate, there is no password to bind with. The ldap handler handles this kind of login only if it has `certificate_filter`:

```toml
# rradius.toml (inside the ldap handler)
certificate_filter = "(&(uid={user})(!(nsAccountLock=TRUE)))"
```

- After the certificate passes, the handler searches for the name on the certificate with `certificate_filter`. If it finds the entry (and the entry matches `required_groups`), it accepts, without binding as the user.
- Without `certificate_filter`, the handler ignores certificate logins and does not ask the directory. It writes a warning line `ignoring a certificate login: this handler has no certificate_filter`. The reason: a plain search cannot tell whether an account is disabled or locked, so you have to say so in the filter.
- `nsAccountLock` above is only an example; use whichever attribute your directory uses to mark disabled accounts. For the AD form, see the remote-access scenario below.

For the EAP-TLS configuration see [chapter 11](11-eap.md).

> **Not verified on real hardware:** The part of a certificate login that goes through the ldap handler was not run on the test directory in this chapter. It was only checked against the code and tests.

## When the directory cannot be reached

The ldap handler tries `urls` in order:

| Situation | What the handler does |
| --- | --- |
| Cannot connect, TLS fails, connection drops, timeout | Moves to the next URL |
| The service account bind is refused, or the server returns an error during the search | Does not try other URLs; counts as "directory unavailable" |
| All URLs have been tried | Counts as "directory unavailable"; the log says `no server reachable; rejecting` |
| The user bind returns busy (51), unavailable (52) or serverDown (81) | Counts as "directory unavailable" |
| The user bind is refused (wrong password, or another reason such as a disabled account) | Rejects this user; not a directory problem |
| Not in `required_groups` | Rejects; not a directory problem |

When the "directory is unavailable", RADIUS **does not respond** to the request and counts it in `rradius_radius_discarded_total{reason="database"}`; TACACS+ replies ERROR; the [login failure lockout](07-users-and-chains.md) does not count it. The NAS then retransmits or asks another RADIUS server, instead of telling the user the password is wrong.

### Verification

The first URL deliberately points to port 1389 where nothing is listening, and the second is the working directory:

```toml
urls = ["ldap://localhost:1389", "ldap://localhost:389"]
```

Logins succeed. The first one tries the first URL and the log has one line; for the next 30 seconds logins go straight to the second URL, and then the first is tried once more:

```text
… WARN rradius_core::ldap_handler: auth ldap handler: ldap://localhost:1389: I/O error: Connection refused (os error 61)
```

Stop the directory container and log in again: the `rradius-bench` result is `timeouts 1` (the server did not respond), and the log says:

```text
… WARN rradius_core::ldap_handler: auth ldap handler: ldap://localhost:389: I/O error: Connection refused (os error 61)
… WARN rradius_core::ldap_handler: auth ldap handler: ldap://localhost:1389: I/O error: Connection refused (os error 61)
… ERROR rradius_core::ldap_handler: auth ldap handler: no server reachable; rejecting
… WARN rradius_core::lua_engine: RADIUS Access-Request from client localhost not answered: the authentication backend could not be used
```

The URL that failed in the last 30 seconds is tried last. A host that does not answer, instead of refusing the connection, is logged after `timeout_ms` as `timeout: deadline has elapsed` on a new connection, or as `timed out` on a pooled one. `rradius_radius_discarded_total{reason="database"}` in `/metrics` goes up with it. **Verified**.

> **Note:** Requests try the URLs in the order of `urls`. For 30 seconds after a URL fails to connect (connection refused, TLS failure, drop, timeout), requests try the other URLs first and come back to it last. After 30 seconds it gets another chance, and if it has recovered, requests go back to it. So when the first server is down for a long time, only the one request every 30 seconds has to wait for it to fail (a refused connection is fast; a host that does not answer is waited for up to `timeout_ms`), and the others are answered directly by the later URLs. When all URLs have failed within those 30 seconds, they are tried in the listed order, and when several do not answer, one request waits "number of URLs × `timeout_ms`". An error reply from the server (for example a refused service account bind) does not count as unreachable and is not moved to the back. This memory lives only inside the handler and starts over after a reload. Put the most reliable server first. If a server will be down for a long time, you should still remove it from `urls` and reload first.

## The remote-access scenario: VPN with AD accounts

The `vpn` chain of the example scenario `remote-access` is the form for connecting to AD with the ldap handler:

```toml
# rradius.toml (remote-access, excerpt)
[auth.chains.vpn]
policy = "first_match"

[[auth.chains.vpn.handlers]]
type = "ldap"
urls = ["ldaps://dc1.corp.example.com", "ldaps://dc2.corp.example.com"]
bind_dn = "CN=svc-radius,OU=Service Accounts,DC=corp,DC=example,DC=com"
bind_password_file = "ldap.secret"
base_dn = "OU=People,DC=corp,DC=example,DC=com"
filter = "(&(sAMAccountName={user})(!(userAccountControl:1.2.840.113556.1.4.803:=2)))"
password_check = "bind"
required_groups = ["CN=vpn-users,OU=Groups,DC=corp,DC=example,DC=com"]

[[auth.chains.vpn.handlers.group_reply]]
group = "CN=it-staff,OU=Groups,DC=corp,DC=example,DC=com"
attributes = { "Filter-Id" = "vpn-it" }

[[auth.chains.vpn.handlers.group_reply]]
group = "CN=contractors,OU=Groups,DC=corp,DC=example,DC=com"
attributes = { "Filter-Id" = "vpn-contractor", "Session-Timeout" = "28800" }

[auth.chains.vpn.username]
strip_realm = "both"
lowercase = true

[[radius.clients]]
name = "vpn-gw"
address = "10.60.0.1"
secret_file = "vpn-gw.secret"
auth = "vpn"
```

Key points:

- The AD account name is in `sAMAccountName`. The second part of `filter` excludes disabled accounts (bit 2 of `userAccountControl`). A disabled account cannot be found by the search and is the same as not existing.
- Groups use the default `memberOf`, which AD writes on the user.
- Users may type `CORP\alice` or `alice@corp.example.com`; `strip_realm = "both"` normalizes both to `alice`.
- This is a named chain, and only the VPN gateway with `auth = "vpn"` goes through it. The jump host in the same scenario goes through a different `mfa` chain ([chapter 15](15-access-recipes.md)).
- If the directory server certificate is not signed by a CA the system trusts, add `ca_file`.

This configuration passes `check` (`check` does not connect to the directory). **Verified**: copy `rradius-server/examples/scenarios/remote-access/` as described in chapter 03, change `dictionary` to the real path of the dictionary and the password files to 0600, and `check` prints `configuration ok` (plus four warnings: `cluster.secret_raft`, `cluster.secret_api` and the shared secrets of the clients `vpn-gw` and `jump-host` are still the public values that come with the example).

> **Not verified on real hardware:** This scenario has not been run against a real AD. The filter that excludes disabled accounts is AD syntax and does not apply in the OpenLDAP test directory of this chapter.

## Active Directory MS-CHAPv2: the netlogon handler

Goal: Wi-Fi uses PEAP-MSCHAPv2, and users connect by typing their AD account and password, without installing Samba on the server.

The netlogon handler sets up a Netlogon secure channel with the domain controller as a **computer account**, and hands every MS-CHAPv2 response to the domain controller to verify. When the domain controller answers accept, it also returns the data needed to compute the MPPE keys. `MS-CHAP2-Success`, `MS-MPPE-Send-Key` and `MS-MPPE-Recv-Key` in the Access-Accept are exactly the same as when verified locally, so PEAP's EAP-Success produces the MSK as usual.

### Prepare the computer account

The handler needs:

- A computer account in the domain. Its name has at most 15 characters and in the configuration it is written **without** the trailing `$` (for example it is called `RRADIUS1$` in the domain and `RRADIUS1` in the configuration).
- The password of this computer account, in a file (`machine_password_file`); the trailing newline is removed.
- We recommend each node use its own computer account. The domain controller keeps only one set of secure channel state per computer account. When several hosts share one account, the one set up later invalidates the one set up earlier. This comes from the behavior of the Netlogon protocol itself; the code and spec do not say it, and it is **not verified on real equipment**.

Creating the computer account and setting a known password in the domain is done with your domain administration tools. The Samba domain controller used for the tests in this chapter was set up like this (a test environment method only):

```console
$ samba-tool computer create RRADIUS1
$ samba-tool user setpassword 'RRADIUS1$' --newpassword='…'
```

In a Windows domain (this needs the ActiveDirectory module of RSAT) you can create it with PowerShell. The name has at most 15 characters, and each node uses its own account:

```powershell
$pw = Read-Host -AsSecureString "Password for RRADIUS1"
New-ADComputer -Name RRADIUS1 -SamAccountName 'RRADIUS1$' -AccountPassword $pw -Enabled $true
```

> **Not verified on real hardware:** This `New-ADComputer` snippet has not been run on Windows Server.

A Samba AD domain controller does not accept MS-CHAPv2 by default. On every domain controller, add the following line to `[global]` in `/etc/samba/smb.conf`, then restart samba:

```ini
ntlm auth = mschapv2-and-ntlmv2-only
```

The test image of this chapter (`docker/interop-netlogon/dc-start.sh`) provisions the domain with this option. Without it, Samba may return `STATUS_WRONG_PASSWORD` even for a correct password (see [Common mistakes](#common-mistakes)).

Store the computer account password on the node, readable only by the account that runs rradius:

```console
$ install -m 600 -o rradius /dev/null /etc/rradius/netlogon-rradius1.secret
$ read -rs pw && printf '%s' "$pw" > /etc/rradius/netlogon-rradius1.secret
```

> **Warning:** The computer account password is this server's identity in the domain. Set the file permissions to 0600; `check` and `serve` warn when others can read the file.

### Configuration

```toml
# rradius.toml (excerpt)
[auth]
[[auth.handlers]]
type = "netlogon"
domain = "CORP"
servers = ["dc1.corp.example", "dc2.corp.example"]
computer_name = "RRADIUS1"
machine_password_file = "machine.secret"
required_groups = ["S-1-5-21-3266951882-2843208589-237589800-1106"]

[[auth.handlers.group_reply]]
group = "S-1-5-21-3266951882-2843208589-237589800-1106"
attributes = { "Filter-Id" = "staff" }
```

| Key | Default | Description |
| --- | --- | --- |
| `domain` | — (required) | NetBIOS name of the domain |
| `servers` | — (required) | DNS or NetBIOS names of the domain controllers, tried in order |
| `port` | — (asks the endpoint mapper on port 135 first) | Fixed Netlogon port |
| `computer_name` | — (required) | Computer account, without `$`, at most 15 characters |
| `machine_password_file` | — (required) | File with the computer account password |
| `connections` | `4` | Most connections kept open per domain controller |
| `timeout_ms` | `2000` | Time limit for one call to a domain controller, in milliseconds |
| `required_groups` | — | Group SIDs; the user must belong to one of them |
| `group_reply` | — | Reply attributes added when the user belongs to a SID |

Every key can be reloaded.

- `servers` **cannot be IP addresses**: the logon request has to carry the name of the domain controller, and a Windows domain controller refuses requests that name it by address. `check` reports an error when it finds an address.
- Without `port`, the handler first connects to port 135 of the domain controller to ask the endpoint mapper where Netlogon is. If the domain controller changes its port, the handler asks again. When there is a firewall in between, open port 135 and the port it answers with, or fix the Netlogon port on the domain controller and write it in `port`. The dynamic RPC ports of a Windows domain controller default to TCP 49152 to 65535, and without a fixed port the whole range must be open (not verified on real equipment; the Samba test image fixes it at 49200).
- Groups are written as SIDs of the form `S-1-5-21-…`. If you write a group name (for example `Domain Users`), `check` fails. You can look up the SID of a group with your domain administration tools: on Windows use `Get-ADGroup wifi-staff | Select SID` (not verified on real equipment); the test used `samba-tool group show wifi-staff --attributes=objectSid`.
- `check` only checks the configuration and reads the password file. It does **not** connect to the domain controller. The secure channel is set up when the first request arrives.
- The secure channel always uses AES. If the domain controller does not agree to AES, or the server certificate that comes back fails verification, the handler treats that domain controller as unavailable and does not fall back to RC4 or DES.

### Verification

Test environment: a Samba AD domain controller, domain `CORP`; `alice` is in the `wifi-staff` group; `bob` is not; `carol` is disabled. The Netlogon port is fixed, so the configuration has `port`. The `mschapv2` load of `rradius-bench` sends MS-CHAPv2 Access-Requests directly:

```console
$ rradius-bench run … --user alice --password Alice-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 1, reject 0 …
$ rradius-bench run … --user 'CORP\alice' --password Alice-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 1, reject 0 …
$ rradius-bench run … --user alice --password Wrong-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user carol --password Carol-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user bob --password Bob-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user nobody --password Any-Pass-1 --load mschapv2,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user alice --password Alice-Pass-1 --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
```

Server log (excerpt):

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="netlogon" decision="accept"
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, MS-CHAP2-Success = <hidden>, MS-MPPE-Send-Key = <hidden>, MS-MPPE-Recv-Key = <hidden>, Filter-Id = staff"
… INFO rradius_core::netlogon::handler: auth netlogon handler: localhost: STATUS_WRONG_PASSWORD; rejecting account="alice"
… INFO rradius_core::netlogon::handler: auth netlogon handler: localhost: STATUS_ACCOUNT_DISABLED; rejecting account="carol"
… INFO rradius_core::netlogon::handler: auth netlogon handler: localhost: not in any of required_groups; rejecting account="bob"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="nobody" account="nobody" chain="default" handler="none" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="none" decision="reject"
```

- `alice` passes, and the response carries the MS-CHAPv2 proof, the MPPE keys, and the `Filter-Id` from `group_reply`.
- The name written as `CORP\alice` also passes: the handler always uses the configured `domain` and drops the domain part of the name.
- When the domain controller answers `STATUS_NO_SUCH_USER`, the handler ignores it (`nobody` ends up as `handler="none"`), which gives later handlers a chance.
- PAP is not the netlogon handler's business: it ignores it and does not ask the domain controller.

**Verified** (Samba AD domain controller). For the EAP part of PEAP-MSCHAPv2 going through this handler, see [chapter 11](11-eap.md); this chapter did not run PEAP separately.

### Domain controller replies

| Domain controller reply | Handler result | `result` of `rradius_netlogon_logons_total` |
| --- | --- | --- |
| Success | accept (reject if not in `required_groups`) | `accept` (`reject` if not in the group) |
| `STATUS_NO_SUCH_USER` | ignore | `no_such_user` |
| `STATUS_WRONG_PASSWORD`, `STATUS_ACCOUNT_DISABLED`, `STATUS_ACCOUNT_LOCKED_OUT`, `STATUS_PASSWORD_EXPIRED`, `STATUS_PASSWORD_MUST_CHANGE`, `STATUS_ACCOUNT_EXPIRED`, `STATUS_INVALID_LOGON_HOURS`, `STATUS_INVALID_WORKSTATION` | reject; the log gives the status name | `reject` |
| Any other failure | reject; the log gives the status code in hexadecimal | `reject` |
| Success as a guest account | reject | `reject` |
| No domain controller could answer | backend unavailable: RADIUS does not respond | `unavailable` |

- A user who is locked out or whose password has expired in AD is rejected here as well, and the log gives the reason.
- When an answer is marked "not authoritative", the handler asks the next domain controller. The account states in the table above are an exception: they are rejected directly, because every domain controller gives the same answer.

### Connections, fault tolerance and metrics

- Each domain controller keeps at most `connections` connections, which are reused. When a connection drops, the handler retries on a new one. When the domain controller has invalidated the secure channel, the handler discards it and, if the request still has a new connection to try, sets the channel up again and retries once; if the new channel is refused too, or the new connections fail as well, it moves to the next domain controller. So once a restarted domain controller is serving again, the first request sets the channel up again and is answered. Tested against a Samba domain controller (2026-11-28, one login after the restart): no request went unanswered. Several logins at once after a restart were tested only against a simulated domain controller.
- An idle connection in the pool that does not answer within `timeout_ms` (for example because a firewall dropped it silently) is closed, together with the other idle connections of that domain controller, and the handler retries with a new connection.
- The computer account password, the session key, the NT-Response and the MPPE keys do not appear in the log at any level.

```console
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_netlogon'
rradius_netlogon_logons_total{result="accept"} 2
rradius_netlogon_logons_total{result="reject"} 3
rradius_netlogon_logons_total{result="no_such_user"} 1
rradius_netlogon_logons_total{result="unavailable"} 0
rradius_netlogon_channel_setups_total{result="ok"} 1
rradius_netlogon_channel_setups_total{result="failed"} 0
rradius_netlogon_open_connections 1
```

| Metric | Meaning |
| --- | --- |
| `rradius_netlogon_logons_total{result}` | MS-CHAPv2 logins that were sent to the domain controller, by result |
| `rradius_netlogon_channel_setups_total{result}` | Number of secure channel setups, `ok` or `failed` |
| `rradius_netlogon_open_connections` | Netlogon connections currently open |

The seven logins above set up the secure channel only once and reused it afterwards. `status` also has a `Netlogon` section:

```console
$ rradiusd status --config rradius.toml
…
Netlogon
  Open connections         1
  Logons accepted          0.0/s (2)
  Logons rejected          0.0/s (3)
  Logons of unknown users  0.0/s (1)
  Logons without a DC      0.0/s (0)
…
```

**Verified**. These metrics appear whether or not a netlogon handler is configured, and are all 0 when none is.

### Wrong computer account password

Replace `machine_password_file` with a wrong password and start the server. On the first login:

```text
… ERROR rradius_core::lua_engine: auth netlogon handler: localhost: cannot set up the secure channel as RRADIUS1$: the domain controller refused the computer account's password (STATUS_ACCESS_DENIED)
… WARN rradius_core::netlogon::handler: auth netlogon handler: localhost: cannot set up the secure channel as RRADIUS1$: the domain controller refused the computer account's password (STATUS_ACCESS_DENIED)
… ERROR rradius_core::lua_engine: auth netlogon handler: no domain controller of CORP could answer; rejecting
… WARN rradius_core::lua_engine: RADIUS Access-Request from client localhost not answered: the authentication backend could not be used
```

This login gets no response (`rradius-bench` shows `timeouts 1`), and `rradius_netlogon_channel_setups_total{result="failed"}` and `rradius_netlogon_logons_total{result="unavailable"}` each go up by one. **Verified**.

> **Note:** AD periodically asks computer accounts to change their password. This handler only reads `machine_password_file` and does not change it on its own. If the password of the computer account is changed on the domain side, update this file at the same time and reload.

## Putting LDAP and netlogon in the same chain

The same AD users send PAP from the VPN and PEAP-MSCHAPv2 from Wi-Fi. Put both handlers in the same `first_match` chain, with **netlogon first**:

```toml
# rradius.toml (excerpt)
[auth]
policy = "first_match"

[[auth.handlers]]
type = "netlogon"
domain = "CORP"
servers = ["dc1.corp.example", "dc2.corp.example"]
computer_name = "RRADIUS1"
machine_password_file = "machine.secret"

[[auth.handlers]]
type = "ldap"
urls = ["ldaps://dc1.corp.example", "ldaps://dc2.corp.example"]
bind_dn = "CN=svc-radius,OU=Service Accounts,DC=corp,DC=example,DC=com"
bind_password_file = "ldap.secret"
base_dn = "OU=People,DC=corp,DC=example,DC=com"
filter = "(&(sAMAccountName={user})(!(userAccountControl:1.2.840.113556.1.4.803:=2)))"
```

The reasons for the order:

- A PAP login: the netlogon handler ignores it and passes it to the ldap handler to bind.
- An MS-CHAPv2 login: the netlogon handler decides. If you put ldap first instead, the ldap handler in bind mode **rejects** MS-CHAPv2 (it cannot verify it, but it does not ignore it), and netlogon never gets its turn for a user the ldap search finds.
- When the domain controller says there is no such user (`STATUS_NO_SUCH_USER`), netlogon ignores it. The ldap handler cannot find the person in the directory either and ignores the login too; with no further handler the result is a reject (`handler="none"` in the authentication record).

You can also split this into two named chains, one for the VPN gateway and one for the Wi-Fi controller ([chapter 07](07-users-and-chains.md)), each with only one kind of handler. That is easier to understand.

> **Not verified on real hardware:** This combination has only passed `check`. It has not been run with an LDAP server and a domain controller connected at the same time; the handler order rules come from the code.

## Common mistakes

**Symptom:** LDAP users can log in with PAP, but Wi-Fi (PEAP) or MS-CHAPv2 is always rejected, and the authentication record shows `handler="ldap" decision="reject"`.
**Cause:** bind mode can only verify PAP.
**Fix:** For AD, use the netlogon handler. For other directories that store clear text or NT hashes, use `password_check = "attribute"`.

**Symptom:** The login gets no response, and the log has `invalid peer certificate: UnknownIssuer` and `no server reachable; rejecting`.
**Cause:** The directory certificate was not signed by a trusted CA.
**Fix:** Put the issuing CA in `ca_file`. If the name does not match, write `urls` with the host name on the certificate.

**Symptom:** The login gets no response, and the log has `service account bind failed: …`.
**Cause:** The DN or password of the service account is wrong, or the account is disabled.
**Fix:** Check `bind_dn` and the contents of `ldap.secret`. In this case the handler does not try the next URL.

**Symptom:** The password is correct but the login is still rejected, and the log has no error.
**Cause:** The user is not in `required_groups`; or the groups are read from `memberOf`, but the directory does not maintain `memberOf` on users.
**Fix:** Check the group DN; if the directory has no `memberOf`, use `group_base_dn` and `group_filter`.

**Symptom:** `check` prints `auth.handlers[0].filter must contain exactly one {user} placeholder, found 0`.
**Cause:** `filter` (or `certificate_filter`) has no `{user}`, or has two or more.
**Fix:** Write exactly one `{user}`.

**Symptom:** `check` prints `auth.handlers[0].urls: 'localhost:636' must start with ldap:// or ldaps://`.
**Cause:** The URL has no scheme.
**Fix:** Write it as `ldaps://host:port`.

**Symptom:** `check` prints `auth.handlers[0].bind_password must not be empty: an empty password makes an anonymous bind`, or `bind_password: exactly one of bind_password or bind_password_file must be set`.
**Cause:** The service account password is empty, or both keys are written (or neither).
**Fix:** Keep only `bind_password_file`, with a non-empty password in the file.

**Symptom:** `check` prints `auth.handlers[0].group_attribute has no effect without required_groups or group_reply`.
**Cause:** A way to look up groups is written, but no group setting uses it.
**Fix:** Add `required_groups` or `group_reply`, or remove the key. `group_filter needs group_base_dn` and `required_groups must list at least one group; leave it out to require none` are the same kind of error.

**Symptom:** `check` prints `auth.handlers: group_reply: unknown attribute 'No-Such-Attr'`.
**Cause:** The attribute name in `group_reply` is not in the dictionary.
**Fix:** Correct the name ([chapter 06](06-dictionary.md)).

**Symptom:** `check` prints `auth.handlers[0].password_attribute is required when password_check = "attribute"`.
**Cause:** Attribute mode is used without saying which attribute.
**Fix:** Add `password_attribute = "userPassword"` (according to your directory).

**Symptom:** `check` prints `auth.handlers[0].servers: '192.0.2.10' is an address or has a port; give the domain controller's DNS or NetBIOS name, which the logon names it by (a fixed port goes in port)`.
**Cause:** `servers` of netlogon has an IP address or `host:port`.
**Fix:** Write the name of the domain controller; write a fixed port in `port`.

**Symptom:** `check` prints `auth.handlers[0].computer_name 'RRADIUS1$' must have 1 to 15 characters and no trailing $`.
**Cause:** `computer_name` has a `$`, or is longer than 15 characters.
**Fix:** Remove the `$`; if the name is too long, create a shorter computer account in the domain.

**Symptom:** `check` prints `auth.handlers[0].required_groups: 'Domain Users' is not a SID of the form S-1-<authority>-<sub-authority>...`.
**Cause:** Groups for netlogon must be SIDs, not names.
**Fix:** Look up the SID of the group and write it.

**Symptom:** MS-CHAPv2 logins get no response, and the log has `cannot set up the secure channel as RRADIUS1$: the domain controller refused the computer account's password (STATUS_ACCESS_DENIED)`.
**Cause:** The password in `machine_password_file` differs from the password of the computer account in the domain.
**Fix:** Reset the computer account password in the domain, update the file, and reload.

**Symptom:** The log has `cannot set up the secure channel as RRADIUS1$: the domain controller has no such computer account (STATUS_NO_TRUST_SAM_ACCOUNT)`.
**Cause:** The domain has no such computer account, or `computer_name` is mistyped.
**Fix:** Create the computer account in the domain ([Prepare the computer account](#prepare-the-computer-account)), or correct `computer_name`.

**Symptom:** The log has `no Netlogon endpoint registered yet; the domain controller may still be starting`.
**Cause:** Port 135 of the domain controller already answers, but the Netlogon service has not registered yet. This is common right after the domain controller boots.
**Fix:** Wait for it to come up; it usually recovers on its own. If it keeps appearing, check the Netlogon service on the domain controller.

**Symptom:** Two nodes take turns showing `STATUS_ACCESS_DENIED`.
**Cause:** The two nodes share the same computer account.
**Fix:** Each node uses its own computer account. (This is not verified on real equipment; for the reason see [Prepare the computer account](#prepare-the-computer-account).)

**Symptom:** On a Samba domain controller, a correct password is still rejected, and the log has `STATUS_WRONG_PASSWORD`.
**Cause:** `smb.conf` does not have `ntlm auth = mschapv2-and-ntlmv2-only`, so Samba does not accept MS-CHAPv2.
**Fix:** Add this line and restart samba. (Not verified on real hardware: the test image has this option from the start, and the result without it was not tried.)

**Symptom:** The log has `not in any of required_groups`, although the password is correct.
**Cause:** The user is not in a group listed in `required_groups`, or the SID was copied wrongly.
**Fix:** Compare with the group SIDs of the user.

## Related chapters

- [07 User accounts and auth chains](07-users-and-chains.md)
- [06 Dictionary and attributes](06-dictionary.md)
- [11 802.1X and EAP](11-eap.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [15 Scenarios: network access for organizations](15-access-recipes.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [20 Security hardening](20-security.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix C: Metrics reference](92-metrics-reference.md)
