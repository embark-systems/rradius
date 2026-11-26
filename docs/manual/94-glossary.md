# 94 Appendix E: Glossary and RFC cross-reference

> After this chapter you can:
> - Look up how every term in this manual is written, what it means, and which setting key or command it maps to
> - Tell apart terms that are easy to confuse, such as client and user, or authorize group and LDAP group
> - Find which RFCs and standards the core implements, and which feature uses each one
> - Recognize RFCs that the code mentions but the core does not implement

Prerequisites: none. You can look things up in this appendix on its own.

In this chapter:

- [How to read this cross-reference](#how-to-read-this-cross-reference)
- [Basic terms](#basic-terms)
- [Authentication and accounts](#authentication-and-accounts)
- [Accounting and sessions](#accounting-and-sessions)
- [Clusters](#clusters)
- [Operations and monitoring](#operations-and-monitoring)
- [Certificates and security](#certificates-and-security)
- [Terms for features the core does not support](#terms-for-features-the-core-does-not-support)
- [RFCs implemented by the core](#rfcs-implemented-by-the-core)
- [Other standards](#other-standards)
- [RFCs mentioned in the code but not implemented by the core](#rfcs-mentioned-in-the-code-but-not-implemented-by-the-core)
- [RFCs used by rradius-bench](#rfcs-used-by-rradius-bench)
- [Official RFC titles](#official-rfc-titles)
- [How to regenerate the RFC list](#how-to-regenerate-the-rfc-list)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

---

## How to read this cross-reference

The whole manual uses one set of wordings. The "Used in this manual" column is the wording used in the text. Setting keys, commands, attribute names and RFC names always stay as they are. Terms marked "not translated" are written in English in the manual, for example realm, session and leader.

The "Maps to" column shows what the term looks like in a configuration file or a command. For details, see the "Main chapter".

## Basic terms

| Term | Used in this manual | Meaning | Maps to | Main chapter |
| --- | --- | --- | --- | --- |
| NAS | NAS | The device that sends RADIUS requests to the server, for example an AP controller, a switch, a BNG or a VPN gateway. On first use, write it as "NAS (network access server, for example an AP controller, a switch, a BNG or a VPN gateway)" | — | [05](05-radius-clients.md) |
| client | client | One `[[radius.clients]]` or `[[tacacs.clients]]` entry in the configuration, that is, a NAS that is allowed to send requests. One entry can cover a whole network range, so one client is not always one device | `radius.clients`, `tacacs.clients` | [05](05-radius-clients.md), [12](12-tacacs.md) |
| user | user | The person or device that logs in | `User-Name` | [07](07-users-and-chains.md) |
| account | account | The row in a table that stands for one user | `user` command, `[accounts]` | [07](07-users-and-chains.md) |
| shared secret | shared secret | The same password configured on both the client and the server, used to sign packets and to hide password fields | `secret`, `secret_file` | [05](05-radius-clients.md) |
| authentication／authorization／accounting | authentication/authorization/accounting | Confirm who you are/decide what you can do/record how much you used. Together they are called AAA | — | [01](01-overview.md) |
| realm | realm | The part of an account name after the `@`, for example `@example.com` | `auth.username.strip_realm` | [07](07-users-and-chains.md) |
| dictionary | dictionary | The file that maps attribute names to numbers and types | `dictionary` | [06](06-dictionary.md) |
| vendor-specific attribute | vendor-specific attribute (VSA) | An attribute whose number is defined by a vendor, carried inside `Vendor-Specific` | `VENDOR` in the dictionary | [06](06-dictionary.md) |
| reply attribute | reply attribute | An attribute that Access-Accept carries back to the NAS, for example `Session-Timeout` or `Filter-Id` | `reply_query` of the sql handler, `set_attr` in Lua | [07](07-users-and-chains.md), [10](10-lua-policies.md) |

## Authentication and accounts

| Term | Used in this manual | Meaning | Maps to | Main chapter |
| --- | --- | --- | --- | --- |
| auth chain | auth chain | A series of handlers that are asked in order to decide whether to accept or reject one login | `[auth]`, `auth.chains` | [07](07-users-and-chains.md) |
| handler | handler | One step in an auth chain, for example the sql handler or the ldap handler | `auth.handlers`, `type` | [07](07-users-and-chains.md) |
| policy | `first_match`, `all_accept` | How an auth chain combines the results of its handlers; written as in the source | `auth.policy` | [07](07-users-and-chains.md) |
| lockout | lockout | Temporarily rejecting an account after too many wrong passwords | `auth.lockout` | [07](07-users-and-chains.md) |
| script | script | A policy written in Lua | `lua.script` | [10](10-lua-policies.md) |
| phase | phase | What the script is being called to handle this time: `authentication`, `authorization` or `accounting` | `phase` in Lua | [10](10-lua-policies.md) |
| enable (TACACS+) | enable | The step on a TACACS+ device that raises privilege; written as in the source | `tacacs.enable` | [12](12-tacacs.md) |
| authorize group | authorize group | A set of TACACS+ authorization rules in `authorize_groups`, grouped by **device**, not an LDAP group | `tacacs.authorize_groups` | [12](12-tacacs.md) |

## Accounting and sessions

| Term | Used in this manual | Meaning | Maps to | Main chapter |
| --- | --- | --- | --- | --- |
| session | session | One connection of a user from going online to going offline; it starts with Accounting Start and ends with Stop | `[sessions]`, `session` command | [13](13-accounting.md) |
| accounting spool | accounting spool | A temporary store on disk where accounting requests wait to be replayed while the database cannot be written to. Explain it this way on first use | `radius.accounting_spool` | [13](13-accounting.md) |
| replay | replay (a NAS retransmit is a different thing) | Handing a request in the accounting spool to the script once more | `replay_interval_ms`, `replay_window` | [13](13-accounting.md) |

## Clusters

| Term | Used in this manual | Meaning | Maps to | Main chapter |
| --- | --- | --- | --- | --- |
| cluster | cluster | A group of rradius nodes that share one database; a single machine is also a cluster of one node | `[cluster]` | [16](16-cluster.md) |
| node | node | One rradius process in a cluster | `cluster.node_id`, `cluster.nodes` | [16](16-cluster.md) |
| leader／follower | leader/follower | The node that does the writing, and the other nodes that copy its data. Not translated | `Leader` in `status` | [16](16-cluster.md) |
| voter／learner | voter/learner | A member that takes part in voting for the leader, and a member that only copies data and does not vote. Not translated | output of `cluster members` | [16](16-cluster.md) |
| snapshot | snapshot | The cluster saves the whole database state; later log entries continue from there | `cluster.snapshot_every` | [16](16-cluster.md) |
| migration | migration | The SQL files in `migrations_dir`, applied in numbered order to create and change tables | `cluster.migrations_dir` | [16](16-cluster.md) |

## Operations and monitoring

| Term | Used in this manual | Meaning | Maps to | Main chapter |
| --- | --- | --- | --- | --- |
| reload | reload | Reading the configuration again without stopping the server; triggered by sending SIGHUP | SIGHUP | [04](04-configuration.md) |
| drain | drain | On shutdown, stop accepting new requests and wait for requests in progress to finish | `shutdown.drain_ms` | [04](04-configuration.md) |
| metrics | metrics | Values in Prometheus format served by `/metrics` | `[metrics]` | [17](17-monitoring.md) |
| readiness／liveness | readiness/liveness | Can handle requests/the process is still alive; they map to `/readyz` and `/livez` | `[metrics]` | [17](17-monitoring.md) |

## Certificates and security

| Term | Used in this manual | Meaning | Maps to | Main chapter |
| --- | --- | --- | --- | --- |
| certificate | certificate | An X.509 certificate | `radius.eap.cert_file` | [11](11-eap.md) |
| CA | CA | The authority that issues certificates; the server uses it to check client certificates | `radius.eap.client_ca_file` | [11](11-eap.md) |
| CRL | CRL (certificate revocation list) | The list of revoked certificates that a CA publishes | `radius.eap.crl_file` | [11](11-eap.md) |
| revocation | revocation | Making a certificate invalid before it expires | `radius.eap.crl_file` | [11](11-eap.md) |

## Terms for features the core does not support

These terms appear in the core's error messages or in the configuration reference, but the feature itself exists only in the enterprise edition. The core manual uses them only when it says "not supported by the core":

| Term | Used in this manual | What the core does when it meets it |
| --- | --- | --- |
| RadSec | RadSec | `radius.tls: not supported by this program (RadSec needs rradius-enterprise)` |
| CoA, Disconnect-Request | CoA, Disconnect-Request | `radius.coa: not supported by this program (CoA needs rradius-enterprise)`; the Lua calls `send_disconnect` and `send_coa` raise an error when called |
| OCSP | OCSP | `radius.eap.ocsp: not supported by this program (OCSP needs rradius-enterprise)` |
| proxy | proxy | `proxy: not supported by this program (proxying needs rradius-enterprise)`; "proxy server" is used only when talking about upstream servers |
| upstream | upstream server | — |
| second factor／MFA | second factor | `mfa: not supported by this program (two-factor login needs rradius-enterprise)`; the setting key `mfa` stays as it is |
| sealed key／seal | sealed | Encrypted with another master key before it is written to the database; for example `aka-seal` means "produce a sealed value". The core has no setting or command that uses sealing, and `aka-seal` gets `not supported by this program` |

---

## RFCs implemented by the core

This list takes the RFC numbers from the code comments in `rradius-core/src`, then opens the code each comment is in and keeps only the RFCs that the core really implements. "What the code uses it for" follows the content of the comment, not the official RFC title.

**RADIUS itself**

| RFC | What the code uses it for | Source | Main chapter |
| --- | --- | --- | --- |
| RFC 2865 | RADIUS authentication: hiding and unhiding `User-Password`; Access-Challenge does not carry the reply attributes from earlier handlers, because most attributes cannot go in an Access-Challenge | `radius.rs`, `auth_chain.rs` | [05](05-radius-clients.md) |
| RFC 2866 | RADIUS accounting: the Request Authenticator of Accounting-Request | `radius.rs` | [13](13-accounting.md) |
| RFC 2868 | The tag of tunnel attributes, and the salt encryption of `Tunnel-Password` | `radius.rs` | [06](06-dictionary.md) |
| RFC 3162 | IPv6 attributes: the bytes after the prefix can be omitted | `radius.rs` | [06](06-dictionary.md) |
| RFC 6929 | Extended and long-extended attributes and Extended-Vendor-Specific; an invalid attribute is skipped as a whole | `radius.rs` | [06](06-dictionary.md) |
| RFC 5080 | Retransmit detection: how long an answered request stays in the duplicate cache, and how a retransmit is recognized | `radius.rs` | [05](05-radius-clients.md) |
| RFC 5997 | Status-Server: answered without running a script and without entering the duplicate cache | `radius.rs` | [05](05-radius-clients.md) |
| RFC 3579 | RADIUS carrying EAP, and the Message-Authenticator check | `radius.rs`, `eap.rs` | [05](05-radius-clients.md), [11](11-eap.md) |
| RFC 8045 | `IP-Port-Range`: the public address and port ranges that a CGN reports in accounting, kept as one group per range and searched by `accounting find` | `radius.rs`, `accounting_find.rs` | [13](13-accounting.md), [91](91-command-reference.md) |

**EAP**

| RFC | What the code uses it for | Source | Main chapter |
| --- | --- | --- | --- |
| RFC 3748 | EAP itself, for example a stale response is dropped | `eap.rs` | [11](11-eap.md) |
| RFC 5216 | EAP-TLS: key derivation for TLS 1.2, the length field in the first fragment | `eap.rs` | [11](11-eap.md) |
| RFC 9190 | Key derivation and the way EAP-TLS ends when it uses TLS 1.3 | `eap.rs` | [11](11-eap.md) |
| RFC 5281 | EAP-TTLS: AVPs inside the tunnel, key derivation | `eap.rs` | [11](11-eap.md) |
| RFC 4514 | Escaping rules when the subject (DN) of a client certificate is written as a string | `eap.rs` | [11](11-eap.md) |

**Passwords and challenge-response authentication**

| RFC | What the code uses it for | Source | Main chapter |
| --- | --- | --- | --- |
| RFC 2759 | The MS-CHAPv2 response and authenticator response, also used inside PEAP | `mschap.rs`, `eap.rs` | [07](07-users-and-chains.md) |
| RFC 2433 | The NT-Response check of MS-CHAPv1 | `mschap.rs`, `ldap_handler.rs` | [07](07-users-and-chains.md) |
| RFC 2548 | Microsoft vendor attributes: the MS-CHAP request and the reply attributes for MPPE keys | `mschap.rs`, `auth_chain.rs`, `reload.rs` | [07](07-users-and-chains.md) |
| RFC 3079 | Session key derivation for MPPE | `mschap.rs` | [07](07-users-and-chains.md) |

**TACACS+, LDAP and Lua functions**

| RFC | What the code uses it for | Source | Main chapter |
| --- | --- | --- | --- |
| RFC 8907 | TACACS+: the packet format, the arguments of authorization rules, the warning when a shared secret is too short | `tacacs.rs`, `tacacs_rules.rs`, `reload.rs` | [12](12-tacacs.md) |
| RFC 4513 | LDAP bind: an empty password becomes an unauthenticated bind that always succeeds, so only a non-empty PAP password is used to bind | `ldap_handler.rs` | [08](08-ldap-and-ad.md) |
| RFC 4515 | Escaping of `{user}` in LDAP filter strings | `ldap_handler.rs` | [08](08-ldap-and-ad.md) |
| RFC 6238 | TOTP, the Lua function `totp_verify` | `totp.rs` | [10](10-lua-policies.md) |
| RFC 4226 | HOTP, the basis of the TOTP calculation | `totp.rs` | [10](10-lua-policies.md) |
| RFC 4648 | Base32, the encoding of the TOTP secret | `totp.rs` | [10](10-lua-policies.md) |
| RFC 3986 | Which characters the Lua function `url_encode` leaves unencoded | `script_http.rs` | [10](10-lua-policies.md) |
| RFC 3339 | The time format of the accounting log (UTC, to the millisecond) | `accounting_log.rs` | [13](13-accounting.md) |

## Other standards

| Standard | What the code uses it for | Source | Main chapter |
| --- | --- | --- | --- |
| MS-NRPC | Netlogon for AD: setting up the secure channel, the `NetrLogonSamLogonEx` network logon | `netlogon/` | [08](08-ldap-and-ad.md) |
| PEAPv0 | PEAP, with inner EAP-MSCHAPv2. The code does not cite a standard number | `eap.rs` | [11](11-eap.md) |

The core code does not cite any 3GPP TS.

## RFCs mentioned in the code but not implemented by the core

These numbers appear in the core source or in `docs/spectra/specs/`, but the feature exists only in the enterprise edition. The core rejects the setting or raises an error when called:

| RFC | Topic | What the core does |
| --- | --- | --- |
| RFC 6614, RFC 6613 | RadSec (RADIUS over TLS/TCP) | `common/framing.rs` and `clients.rs` have shared code, but `radius.tls` is rejected |
| RFC 5176 | CoA and Disconnect | `dynamic_authorization.rs` is the hook; `radius.coa` is rejected, and `send_disconnect` and `send_coa` raise errors |
| RFC 9887 | TACACS+ over TLS | `tacacs_transport.rs` is the hook; `tacacs.tls` is rejected |
| RFC 6960 | OCSP | Only in specs; `radius.eap.ocsp` is rejected |
| RFC 4186 | EAP-SIM | Only in specs; `eap_methods = ["sim"]` gets `'sim' is not a method this server offers` |
| RFC 9048 | EAP-AKA' | Only in specs and the load-test tool; `eap_methods = ["aka-prime"]` gets `aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)` |
| RFC 5448 | The first version of EAP-AKA' (RFC 9048 updates it) | Only in specs |
| RFC 6733, RFC 3539 | Diameter, and the AAA transport profile (the watchdog is defined here) | Only in specs; `[diameter]` is rejected |
| RFC 7585 | Finding the RadSec servers of a realm through DNS | Only in specs; `[proxy]` is rejected |

The 3GPP TS documents in `docs/spectra/specs/` (29.273, 33.402, 35.205, 35.206, 35.208) also belong to EAP-AKA' and Diameter in the enterprise edition.

Also, `passwords.rs` cites RFC 7914 only because it publishes a set of PBKDF2-HMAC-SHA-256 test vectors; the core does not implement the subject of that RFC.

## RFCs used by rradius-bench

The load-test tool is a RADIUS and TACACS+ client, and its code cites RFC 2865, 2866, 2548, 2759, 3579, 3748, 5216, 5281, 9190 and 8907, the same as the server side. Three more depend only on whether the server under test has the feature:

| RFC | Use in the tool | For the core |
| --- | --- | --- |
| RFC 6614 | `--transport tls` | The core has no RadSec |
| RFC 4187, RFC 9048 | `aka-prime` load | The core replies with Reject |

## Official RFC titles

The official title of every RFC mentioned in this appendix, checked on 2026-11-23 against the public data at rfc-editor.org. The "Topic" and "What the code uses it for" columns in the tables above are this manual's own explanations, not titles.

| RFC | Official title |
| --- | --- |
| RFC 2433 | Microsoft PPP CHAP Extensions |
| RFC 2548 | Microsoft Vendor-specific RADIUS Attributes |
| RFC 2759 | Microsoft PPP CHAP Extensions, Version 2 |
| RFC 2865 | Remote Authentication Dial In User Service (RADIUS) |
| RFC 2866 | RADIUS Accounting |
| RFC 2868 | RADIUS Attributes for Tunnel Protocol Support |
| RFC 3079 | Deriving Keys for use with Microsoft Point-to-Point Encryption (MPPE) |
| RFC 3162 | RADIUS and IPv6 |
| RFC 3339 | Date and Time on the Internet: Timestamps |
| RFC 3539 | Authentication, Authorization and Accounting (AAA) Transport Profile |
| RFC 3579 | RADIUS (Remote Authentication Dial In User Service) Support For Extensible Authentication Protocol (EAP) |
| RFC 3748 | Extensible Authentication Protocol (EAP) |
| RFC 3986 | Uniform Resource Identifier (URI): Generic Syntax |
| RFC 4186 | Extensible Authentication Protocol Method for Global System for Mobile Communications (GSM) Subscriber Identity Modules (EAP-SIM) |
| RFC 4187 | Extensible Authentication Protocol Method for 3rd Generation Authentication and Key Agreement (EAP-AKA) |
| RFC 4226 | HOTP: An HMAC-Based One-Time Password Algorithm |
| RFC 4513 | Lightweight Directory Access Protocol (LDAP): Authentication Methods and Security Mechanisms |
| RFC 4514 | Lightweight Directory Access Protocol (LDAP): String Representation of Distinguished Names |
| RFC 4515 | Lightweight Directory Access Protocol (LDAP): String Representation of Search Filters |
| RFC 4648 | The Base16, Base32, and Base64 Data Encodings |
| RFC 5080 | Common Remote Authentication Dial In User Service (RADIUS) Implementation Issues and Suggested Fixes |
| RFC 5176 | Dynamic Authorization Extensions to Remote Authentication Dial In User Service (RADIUS) |
| RFC 5216 | The EAP-TLS Authentication Protocol |
| RFC 5281 | Extensible Authentication Protocol Tunneled Transport Layer Security Authenticated Protocol Version 0 (EAP-TTLSv0) |
| RFC 5448 | Improved Extensible Authentication Protocol Method for 3rd Generation Authentication and Key Agreement (EAP-AKA') |
| RFC 5997 | Use of Status-Server Packets in the Remote Authentication Dial In User Service (RADIUS) Protocol |
| RFC 6238 | TOTP: Time-Based One-Time Password Algorithm |
| RFC 6613 | RADIUS over TCP |
| RFC 6614 | Transport Layer Security (TLS) Encryption for RADIUS |
| RFC 6733 | Diameter Base Protocol |
| RFC 6929 | Remote Authentication Dial In User Service (RADIUS) Protocol Extensions |
| RFC 6960 | X.509 Internet Public Key Infrastructure Online Certificate Status Protocol - OCSP |
| RFC 7585 | Dynamic Peer Discovery for RADIUS/TLS and RADIUS/DTLS Based on the Network Access Identifier (NAI) |
| RFC 7914 | The scrypt Password-Based Key Derivation Function |
| RFC 8045 | RADIUS Extensions for IP Port Configuration and Reporting |
| RFC 8907 | The Terminal Access Controller Access-Control System Plus (TACACS+) Protocol |
| RFC 9048 | Improved Extensible Authentication Protocol Method for 3GPP Mobile Network Authentication and Key Agreement (EAP-AKA') |
| RFC 9190 | EAP-TLS 1.3: Using the Extensible Authentication Protocol with TLS 1.3 |
| RFC 9887 | Terminal Access Controller Access-Control System Plus (TACACS+) over TLS 1.3 |

## How to regenerate the RFC list

```console
$ grep -rhoE 'RFC ?[0-9]{3,5}' rradius-core/src | sed 's/RFC */RFC /' | sort -u
$ grep -rhoE 'RFC ?[0-9]{3,5}' docs/spectra/specs | sed 's/RFC */RFC /' | sort -u
```

Result on 2026-11-26: the first command prints 32 lines. One of them, `RFC 822`, is picked up by accident from the certificate field name `RFC822Name` and is not a citation of an RFC, so there are 31 numbers. The second command prints 31 numbers. Every number from both is placed in one of the tables in this appendix, except `RFC 1994`, which only the bench tool spec cites (the CHAP load of the TACACS+ benchmark) and no code comment does. For each new number, open the code that holds the comment and confirm that the core really implements it before you add it to [RFCs implemented by the core](#rfcs-implemented-by-the-core); a number that is only a hook or is rejected goes into [RFCs mentioned in the code but not implemented by the core](#rfcs-mentioned-in-the-code-but-not-implemented-by-the-core).

## Common mistakes

**Mixing up client and user.** A client is a NAS, written in `[[radius.clients]]`; a user is the person who logs in, written in the account table. For "client rejected", look at `Discarded: unknown client` in `status`; for "user rejected", look at `decision="reject"` in the authentication log.

**Treating an authorize group as an LDAP group.** `tacacs.authorize_groups` groups by TACACS+ device. Deciding permissions by the user's LDAP groups is the job of the ldap handler; see [08](08-ldap-and-ad.md).

**Mixing up the accounting spool and the accounting log.** The accounting spool (`radius.accounting_spool`) is temporary storage for when the database cannot be written to, and it is replayed later; the accounting log (`radius.accounting_log`) is a log file with one line per accounting request, and it is never replayed. Both are in [13](13-accounting.md).

## Related chapters

- [01 Getting to know rradius](01-overview.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [06 Dictionary and attributes](06-dictionary.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [11 802.1X and EAP](11-eap.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [13 Accounting and sessions](13-accounting.md)
- [16 Clusters and high availability](16-cluster.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix B: Command reference](91-command-reference.md)
