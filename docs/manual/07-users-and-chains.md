# 07 User accounts and auth chains

> After this chapter you can:
> - Add, change the password of, disable and delete local accounts with the `user` command
> - Choose a password format to match the authentication protocol your NAS uses, and bring hashes over from an old system unchanged
> - Combine several handlers in an auth chain, and send different NAS to different chains
> - Normalize the case and realm of user names
> - Limit password guessing with login failure lockout and a cap on hash cost

Prerequisites: you finished [Chapter 03](03-quick-start.md) and have a working copy of quick-start (`~/rradius-lab/quick-start`) that starts, with the `[accounts]` section added as in Chapter 03. The LDAP and Active Directory handlers are in [Chapter 08](08-ldap-and-ad.md).

In this chapter:

- [How the auth chain decides](#how-the-auth-chain-decides)
- [Manage accounts with the user command](#manage-accounts-with-the-user-command)
- [Choose a password format](#choose-a-password-format)
- [Migrate password hashes from an old system](#migrate-password-hashes-from-an-old-system)
- [Limit the cost of hashing](#limit-the-cost-of-hashing)
- [Authentication protocols and the MS-CHAPv1 switch](#authentication-protocols-and-the-ms-chapv1-switch)
- [sql handler](#sql-handler)
- [lua handler](#lua-handler)
- [all_accept: add a policy on top of the password](#all_accept-add-a-policy-on-top-of-the-password)
- [Let different NAS use different chains](#let-different-nas-use-different-chains)
- [Normalize user names and realms](#normalize-user-names-and-realms)
- [Login failure lockout](#login-failure-lockout)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

Run the commands in this chapter in the directory of the quick-start copy, with the server running in another terminal. Test logins use `rradius-bench` as in Chapter 03. Below, the shared options are shortened to `…`:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
```

Only the `accept` and `reject` part of the output is copied.

## How the auth chain decides

When the configuration file has `[auth]`, every RADIUS Access-Request and every TACACS+ login goes to the auth chain for a decision. Without `[auth]`, the top-level script in `[lua]` decides on its own; that style is in [Chapter 10](10-lua-policies.md).

A chain is a series of handlers that run in order. The core has four kinds of handler:

| `type` | What it does | Covered in |
| --- | --- | --- |
| `sql` | Looks up the account in the cluster database or an external database and checks the password | This chapter, [Chapter 09](09-external-databases.md) |
| `lua` | Runs a Lua script, which decides | This chapter, [Chapter 10](10-lua-policies.md) |
| `ldap` | Finds the user in an LDAP directory and verifies the password | [Chapter 08](08-ldap-and-ad.md) |
| `netlogon` | Passes MS-CHAPv2 to an Active Directory domain controller for verification | [Chapter 08](08-ldap-and-ad.md) |

For each request, every handler gives one of four answers:

| Answer | Meaning | Example |
| --- | --- | --- |
| accept | Accept | The password is correct |
| reject | Reject; the chain stops at once | Wrong password; two accounts with the same name found |
| ignore | Cannot tell; pass to the next handler | The account is not found; this login method is not its job |
| challenge | Ask the NAS to question the user once more (Access-Challenge); the chain stops at once | A Lua script asks for a verification code |

The `policy` in `[auth]` decides how the answers are combined:

- `first_match` (default): the first answer that is not ignore is the result.
- `all_accept`: every handler must run; any reject means reject; at the end there must be at least one accept.

When every handler ignores, the result is reject. So "account not found" and "wrong password" both look like Access-Reject to the NAS; the difference is only in the log. The server writes one authentication log line for every decision, and `handler` names who decided.

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="nobody" account="nobody" chain="default" handler="none" decision="reject"
```

In the first line the sql handler rejected `alice` (wrong password). In the second, no handler knew `nobody`, so `handler="none"`. `handler` can also take the values below, all of which come up later in this chapter:

| `handler` | Meaning |
| --- | --- |
| `sql`, `lua`, `ldap`, `netlogon` | This handler made the decision |
| `none` | Every handler ignored |
| `normalization` | The user name does not match `[auth.username]`; no handler ran |
| `no_password_check` | In an `all_accept` chain, no handler that checks passwords accepted |
| `lockout` | The account is blocked by [login failure lockout](#login-failure-lockout); the password was not checked |
| `mschapv1_refused` | MS-CHAPv1 is refused by default (not set, or `allow_mschapv1 = false`) |
| `length` | The RADIUS User-Password is not 16 to 128 octets or not a multiple of 16; no handler ran (see [Password length](#password-length)) |

### Rejection and "backend unavailable"

There is one more kind of result that is neither accept nor an ordinary reject: a handler fails because its backend is unavailable. For example, a query to the cluster database or an external database fails, all LDAP servers are unreachable, no domain controller answers, or a slow hash waits in the queue past the handler's `timeout_ms`.

In that case:

- RADIUS **does not answer** the request, and adds one to `rradius_radius_discarded_total{reason="database"}` (the label is always `database`, whatever kind of backend it was). The NAS gets no response and retransmits or asks another server.
- TACACS+ replies ERROR.
- This does not count as a failed login, so [login failure lockout](#login-failure-lockout) does not count it.

The reason for this design: if the server answered Access-Reject while a backend was down, the NAS would treat it as "wrong password". The user would be shut out, and might even be locked by the NAS's own failure counter. Staying silent lets the NAS ask another server. The `Discarded: database` line of `status` is this counter ([Chapter 17](17-monitoring.md)).

### The received name and the account name

The authentication log has two fields, `user` and `account`:

- `user` is the `User-Name` in the request, recorded as received.
- `account` is the account name the handler used for the lookup. With `[auth.username]`, it is the name after the realm is removed and the case is lowered (see [Normalize user names and realms](#normalize-user-names-and-realms)); when `default_user` was used, it is the account that stood in.

## Manage accounts with the user command

`rradiusd user` has six subcommands:

```text
rradiusd user add|passwd|enable|disable|delete|show [--config <path>] <name>
```

They do not touch the table structure themselves. They only run the SQL you wrote in `[accounts]`. So you decide what the account table looks like, and the `user` command writes to your table.

```toml
# rradius.toml (the section added in Chapter 03)
[accounts]
scheme = "bcrypt"
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
passwd = "UPDATE users SET password = ?2 WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled FROM users WHERE username = ?1"
```

| Key | Default | Used by | Description |
| --- | --- | --- | --- |
| `accounts.scheme` | `bcrypt` | `add`, `passwd` | The format the password is stored in; see the next section |
| `accounts.add` | — | `user add` | `?1` is the account name, `?2` the password value to store |
| `accounts.passwd` | — | `user passwd` | Same as above |
| `accounts.enable`, `disable`, `delete` | — | The subcommand of the same name | `?1` is the account name |
| `accounts.show` | — | `user show` | The query result is printed as it is, one column per line |

A subcommand whose statement is not set cannot be used. Only the `user` command reads `[accounts]`, and it reads it once each time it runs, independent of reload (Appendix A marks it "command"). See [Appendix A](90-config-reference.md) for the full fields.

Things to watch for:

- `disable` only changes a column. For it to have any effect, the `user_query` of the sql handler must skip disabled accounts, for example `… WHERE username = ? AND enabled = 1` in quick-start.
- Do not select the password column in `show`; it is printed to the terminal as it is.
- The command writes through the cluster API port, so **the server must be running**. The write goes through the cluster, every node sees it at once, and the next login uses the new data; no reload is needed. The command does not start a node and does not create `data_dir`.
- With `[auth.username]`, the command normalizes the name first, as a login does (see [Normalize user names and realms](#normalize-user-names-and-realms)).

### Goal: add, change the password of, disable and delete an account

```console
$ printf 'erin-password\n' | rradiusd user add --config rradius.toml erin
added erin
$ printf 'erin-new-password\n' | rradiusd user passwd --config rradius.toml erin
password changed for erin
$ rradiusd user disable --config rradius.toml erin
disabled erin
$ rradiusd user enable --config rradius.toml erin
enabled erin
$ rradiusd user delete --config rradius.toml erin
deleted erin
$ rradiusd user show --config rradius.toml erin
no account erin
```

The password is always read from standard input, never from a command-line argument (arguments show up in the process list). When you run `add` or `passwd` directly on a terminal, the command turns echo off and asks twice; if the two entries differ, nothing is written and the exit code is 2. When input comes through a pipe, only the first line is read, without its trailing newline. An empty password is always refused.

Exit codes:

| Exit code | Meaning | Example |
| --- | --- | --- |
| 0 | Done | `added erin` |
| 1 | No matching account, or the database rejected the SQL | `no account nobody`; `add` fails when the account already exists |
| 2 | Error in the command, configuration or input; nothing was sent | `accounts.add is not set in rradius.toml`, `the password is empty; nothing was changed` |
| 3 | Cannot reach the cluster | `the cluster did not answer within 30 s` |

**Verified:** every line of output above, and exit code 1 when `user add` is run for an account that already exists, with the database's error as the message (`Connect: Error code 1555: constraint failed Some("UNIQUE constraint failed: users.username")`).

## Choose a password format

### The four formats the user command stores

`accounts.scheme` decides what `user add` and `user passwd` store. It only affects passwords written afterwards; values already stored are not changed.

| `scheme` | Stored as | PAP | CHAP | MS-CHAPv1, MS-CHAPv2, PEAP |
| --- | --- | --- | --- | --- |
| `bcrypt` (default) | `{crypt}$2b$…` | Yes | No | No |
| `sha512-crypt` | `{crypt}$6$…` | Yes | No | No |
| `nthash` | `{nthash}` plus 32 hexadecimal digits | Yes | No | Yes |
| `clear` | `{clear}` plus the plaintext | Yes | Yes | Yes |

Why these limits exist:

- The CHAP response is computed from the plaintext password, so the server must hold the plaintext.
- MS-CHAPv1 and MS-CHAPv2 (also the inner method of PEAP) use the NT hash of the password, so storing the NT hash or the plaintext both work.
- bcrypt and SHA-512 crypt are one-way hashes and can only verify the plaintext password that PAP sends.

How to choose:

1. The NAS uses only PAP (most VPN gateways, web logins, TACACS+ PAP): use the default `bcrypt`.
2. Wi-Fi 802.1X with PEAP, or a NAS that uses MS-CHAPv2: use `nthash`.
3. CHAP is required: only `clear` works.
4. `bcrypt` costs a lot of CPU on every check (cost 10). With a large login volume and PAP only, `sha512-crypt` checks much faster.

**Verified:** one account was added with each of the four formats, and the stored values start as in the table; then each was tried with PAP, CHAP and MS-CHAPv2, with the results in the table (`rradius-bench` has no MS-CHAPv1 load, so that column follows the code and the spec).

> **Warning:** Anyone who gets the database can log in with `nthash` and `clear`: the NT hash alone is enough to compute the MS-CHAPv2 response, without knowing the original password. Use them only when needed, and protect the cluster's data directory and backups.

### All formats the server understands

The `user` command writes only the four formats above, but the sql handler understands more when it checks a password, which makes it easier to bring accounts over from other systems. A stored value is written as `{format}value`, and the prefix is not case-sensitive.

| Prefix | Content of the value | Protocols it can verify |
| --- | --- | --- |
| (no prefix), `{clear}` | Plaintext | PAP, CHAP, MS-CHAPv1, MS-CHAPv2 |
| `{nthash}` | 32 hexadecimal digits (MD4 of UTF-16LE) | PAP, MS-CHAPv1, MS-CHAPv2 |
| `{crypt}` | The result of crypt(3): traditional DES, `$1$` (MD5-crypt), `$5$`, `$6$`, `$2a$`/`$2b$`/`$2y$` (bcrypt) | PAP |
| `{argon2}` | A PHC string, for example `$argon2id$v=19$…` | PAP |
| `{SHA}`, `{MD5}`, `{SHA224}`, `{SHA256}`, `{SHA384}`, `{SHA512}` | Base64 of the digest | PAP |
| `{SSHA}`, `{SMD5}`, `{SSHA224}`, `{SSHA256}`, `{SSHA384}`, `{SSHA512}` | Base64 of the digest followed by the salt; the digest is computed over the password followed by the salt, and the salt is at least 1 octet | PAP |
| `{PBKDF2}`, `{PBKDF2-SHA1}`, `{PBKDF2-SHA256}`, `{PBKDF2-SHA512}` | `<iterations>$<salt>$<digest>`, with the salt and digest in adapted Base64 (`.` in place of `+`, no `=` padding); `{PBKDF2}` is the same as `{PBKDF2-SHA1}` | PAP |
| `{X-PBKDF2}` | `<algorithm>:<iterations>:<salt>:<digest>`; the algorithm is `HMACSHA1` or `HMACSHA2+224`/`+256`/`+384`/`+512`, and the iterations are 4 octets in Base64 | PAP |
| `{NS-MTA-MD5}` | 32 hexadecimal digits followed by a 32-character salt (a format of old mail servers) | PAP |

Rules:

- An unknown prefix, or a value that cannot be parsed, always fails the check and never makes the server fail.
- `{clear}` with nothing after it, a NULL column and an empty string all mean "no password", and every login is rejected. An empty PAP password does not get through because of that either.
- The deliberately slow formats, `{crypt}`, `{argon2}` and PBKDF2, are checked on threads other than the request-handling ones, with at most as many checks at the same time as there are CPUs. If queueing plus checking exceeds the `timeout_ms` of the sql handler (default 5000 ms), the request counts as "backend unavailable" and gets no answer (see [Rejection and "backend unavailable"](#rejection-and-backend-unavailable)).

### A hash without a prefix is not a password

If a value has no prefix but starts with `$2a$`, `$2b$`, `$2y$`, `$1$`, `$5$`, `$6$` or `$argon2`, the server does **not** compare it as plaintext, and every login is rejected. Such a value usually comes from an import that forgot `{crypt}`. If it were compared as plaintext, anyone who got hold of the hash could log in by sending the hash itself as the password.

The first time the server meets such an account, it writes one warning to the log (once per account, and without the stored value):

```text
… WARN rradius_core::auth_chain: the stored password looks like a hash with no scheme prefix and is refused; add {crypt} or {argon2} before it, or {clear} if it is the password itself user="gina"
```

Fix: add `{crypt}` or `{argon2}` in front of the value. If the string really is someone's plaintext password, write it as `{clear}$2y$…`.

Another case: the value has no prefix and looks like a digest (32, 40, 56, 64, 96 or 128 hexadecimal digits, or Base64 that decodes to 16, 20, 28, 32, 48 or 64 octets). Such a value is still **compared as plaintext**, but the first time it produces a warning:

```text
… WARN rradius_core::auth_chain: the stored password looks like a digest with no scheme prefix and is compared as plaintext; add its prefix ({nthash}, {MD5}, {SHA}, {SHA256}, ...), or {clear} if it is the password itself user="frank"
```

`frank` has a hexadecimal MD5 stored, and the user sends the original password. Compared as plaintext, they naturally do not match, so the login is rejected. Add the right prefix (a hexadecimal MD5 must first be converted to Base64; see the next section) and it works.

This rule is the same for PAP, CHAP, MS-CHAPv1 and MS-CHAPv2: CHAP and MS-CHAP also use the string as the plaintext password to compute the value to compare. The warning list remembers at most 1024 users per process; after that, no more warnings are written, but the check goes on as before. So "no such warning in the log" does not mean the database has no such values. To find them, check whether `ok` for `rradius_password_checks_total{scheme="none"}` is unreasonably high.

**Verified:** both warnings and the refusals were tested after adding test accounts to the quick-start copy.

### Metrics that show check results

`rradius_password_checks_total` counts every check by the format of the stored value and by result:

```console
$ curl -s http://127.0.0.1:9812/metrics | grep 'password_checks_total{scheme="crypt"'
rradius_password_checks_total{scheme="crypt",result="failed"} 6
rradius_password_checks_total{scheme="crypt",result="ok"} 4
```

`scheme` takes a fixed set of values: `clear`, `none` (no prefix), `crypt`, `md5`, `smd5`, `sha`, `ssha`, `sha224`, `ssha224`, `sha256`, `ssha256`, `sha384`, `ssha384`, `sha512`, `ssha512`, `pbkdf2-sha1` (`{PBKDF2}` counts here too), `pbkdf2-sha256`, `pbkdf2-sha512`, `x-pbkdf2`, `ns-mta-md5`, `nthash`, `argon2` and `unknown` (an unknown prefix). `result` is `ok` or `failed`.

While you migrate old accounts, use it to see how many logins still use the old formats. For the full list of metrics see [Appendix C](92-metrics-reference.md).

## Migrate password hashes from an old system

Goal: put the password hashes from an old directory, an old RADIUS server or `/etc/shadow` into the table as they are, so users do not have to reset their passwords; when a user later changes the password, it is stored in the new format.

The example scenario `migrate-users` does exactly this. Copy it to a working directory as in Chapter 03 (change the `dictionary` path and replace the `.secret` files). The key parts are the configuration and the migration:

```toml
# rradius.toml (migrate-users, excerpt)
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[auth.username]
strip_realm = "both"
lowercase = true

[accounts]
scheme = "bcrypt"
add = "INSERT INTO users (username, password, origin) VALUES (?1, ?2, 'rradius')"
passwd = "UPDATE users SET password = ?2, origin = 'rradius' WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled, origin FROM users WHERE username = ?1"
# …(other settings omitted)
```

- The account table has an extra column `origin`, which records where the password of this account came from. When the password is changed, `passwd` also sets it to `rradius`. Later, the rows whose `origin` is not `rradius` tell you how many accounts are still in the old format.
- `[auth.username]` unifies the case and removes the domain, because names in old systems are often written inconsistently (see [Normalize user names and realms](#normalize-user-names-and-realms)).

`migrations/2_imported_users.sql` holds a demo account for each source; every password is "account name-password":

| Account | Source | Stored value (start) |
| --- | --- | --- |
| `ldap1` | `userPassword` of an old LDAP directory | `{SSHA}…` |
| `ldap2` | Same as above | `{SSHA512}…` |
| `ldap3` | Same as above | `{PBKDF2-SHA256}10000$…` |
| `ldap4` | Same as above | `{X-PBKDF2}HMACSHA2+256:…` |
| `shell1` | The second field of `/etc/shadow`, with `{crypt}` in front | `{crypt}$1$…` |
| `mail1` | An old mail server | `{NS-MTA-MD5}…` |
| `old1` | A hexadecimal MD5 stored in an old database, converted to Base64 | `{MD5}kfiBqWi4k+QJLsoSpSKT7w==` |
| `win1` | An NT hash stored by an old system | `{nthash}…` |

### Check it

After the server starts, log in with PAP, one account at a time:

```console
$ rradius-bench run … --user ldap3 --password ldap3-password --load pap,concurrency=1 --requests 1
…
  pap: …, sent 1, accept 1, reject 0, …
```

**Verified:** all eight accounts get `accept 1` with PAP; `win1` also gets `accept 1` with MS-CHAPv2, while `ldap1` (`{SSHA}`) gets `reject 1` with MS-CHAPv2; `CORP\Shell1` and `win1@corp` both log in after name normalization.

Change a password once and watch `origin` change with it:

```console
$ rradiusd user show --config rradius.toml ldap1
username=ldap1
enabled=1
origin=ldap
$ printf 'new-pass\n' | rradiusd user passwd --config rradius.toml ldap1
password changed for ldap1
$ rradiusd user show --config rradius.toml ldap1
username=ldap1
enabled=1
origin=rradius
```

### Conversions when exporting old data

For most formats you only add the prefix in front of the original value. The ones that need conversion:

- **Hexadecimal digests:** after prefixes such as `{MD5}` and `{SHA}`, Base64 is expected, not hexadecimal. Convert with `xxd` and `base64`:

  ```console
  $ printf '%s' 91f881a968b893e4092eca12a52293ef | xxd -r -p | base64
  kfiBqWi4k+QJLsoSpSKT7w==
  ```

  Store it as `{MD5}kfiBqWi4k+QJLsoSpSKT7w==`. This is exactly the value of `old1`.
- **`/etc/shadow`:** add `{crypt}` in front of the second field (`$1$…`, `$5$…`, `$6$…`, `$2y$…`). Without the prefix it is treated as a [hash without a prefix](#a-hash-without-a-prefix-is-not-a-password) and rejected.
- **Systems that store only an NT hash:** add `{nthash}` in front. These accounts work with both PAP and MS-CHAPv2.

> **Note:** The old formats can verify only PAP (except `{nthash}` and plaintext). If a NAS of the old system logs these accounts in with CHAP or MS-CHAPv2, they are rejected after the move. Have the users change their passwords once, and set `scheme` to a suitable format.

### What can and cannot be migrated

**A hash can only be used with login methods that send the plaintext password to the server.** Before you migrate, check which one your devices use:

| Login method | Can a hash be used? |
| --- | --- |
| PAP (VPN, most web logins, TACACS+ PAP and ASCII) | All formats |
| EAP-TTLS with inner PAP (Wi-Fi; see [Chapter 11](11-eap.md)) | All formats |
| CHAP (common in PPPoE) | Plaintext only (`{clear}` or no prefix) |
| MS-CHAPv2, PEAP (the most common account-and-password method on Wi-Fi) | Plaintext or `{nthash}` only |
| MS-CHAPv1 (PPTP and old dial-up devices; refused by default, enabled with `[auth] allow_mschapv1 = true`) | Plaintext or `{nthash}` only |

(This table follows the rules in "Choose a password format" above and in [Authentication protocols and the MS-CHAPv1 switch](#authentication-protocols-and-the-ms-chapv1-switch); for TACACS+ see [Chapter 12](12-tacacs.md#authentication-types-ascii-pap-chap-ms-chap).) So:

- **The old system stores NT hashes:** the migrated accounts can use PEAP right away (`win1` in the example).
- **The old system has only other hashes, and Wi-Fi must use account and password:** there are two ways. Switch Wi-Fi to EAP-TTLS with inner PAP (support differs between operating systems, so check first; not verified in this chapter), or ask users to change their password once, with `scheme` set to `nthash` when they do.
- **CHAP for PPPoE:** without plaintext it cannot be done; the only way is to reset the passwords.
- **For salted hashes, the salt must come after.** Formats like `{SSHA}` are Base64 of "digest followed by salt", where the digest is computed over the password followed by the salt. If the old system uses "salt followed by password", or mixes them another way, there is no matching format, and those accounts can only be reset.
- **When you are not sure it is right, try a test account whose password you know first.** A value in the wrong format fails the check and lets nobody in.

**Verified** (scenario test of `migrate-users`, see the example directory): all eight demo accounts are accepted with PAP, and the `{SSHA}` account is rejected with CHAP.

### Import SQL

Turn the data exported from the old system into a new numbered migration file (in the example, `migrations/2_imported_users.sql`):

```sql
-- 2_imported_users.sql
INSERT INTO users (username, password, origin) VALUES
  ('ldap1', '{SSHA}ZvqZAfdyjd1PQiq8rurmdodN8ihkZW1vc2FsdA==', 'ldap'),
  ('shell1', '{crypt}$1$demosalt$EhA8qeu4qSun8gh9DajmS1', 'shadow'),
  ('o''brien', '{nthash}9d232eea31049319fc8ab84300855a34', 'nt');
```

- **Have the file ready before the first start.** A migration file that has been applied cannot be changed (a change gives `migration file '…' changed after it was applied`; see [Chapter 18](18-operations.md)); to add accounts later, add a file with the next number.
- **Convert account names to lower case and remove the domain first,** to match the settings in [Normalize user names and realms](#normalize-user-names-and-realms). If names with different case stay in the table, the lookup does not find them: with `lowercase = true`, the row `Alice` is never matched.
- **Two accounts that differ only in case** become the same name after lowercasing. In the example, `users.username` is a `PRIMARY KEY`, so the `INSERT` of the row that is now a duplicate fails (`UNIQUE constraint failed`), and the whole migration file fails with it. Find them before the import and decide which to keep.
- **Single quotes in SQL strings are written twice** (`o''brien` above). When a program generates the file, handle the quotes in both account names and hashes.
- **The `origin` column is not required.** The example uses it to record where each account's password came from, so that you can count how many old formats remain.
- **When the file is very large** (hundreds of thousands of rows), the import takes time: raise `[cluster] migrations_wait_ms` (default `60000`) to more than the import time. If the wait runs out at startup, it fails with `cluster was not ready to apply migrations: … (the limit is migrations_wait_ms; raise it if a migration file takes longer)`. Note that a cluster node waits only 30 seconds for the write of one migration file: past that, the message changes to `the cluster node gives one migration file 30 s to be answered, whatever migrations_wait_ms is`, the file keeps running inside the cluster, and once it finishes you start again with a longer `migrations_wait_ms`. This chapter did not measure how long an import of hundreds of thousands of rows takes.
- **The exported file holds everyone's password hash.** Weak hashes can be cracked offline. Protect it in transit and at rest, and delete temporary copies after the import; the same goes for the applied migration file, which stays in `migrations_dir`.
- **`user add` cannot be used to move existing hashes.** It accepts only a plaintext password (from standard input) and hashes it itself according to `scheme`.

### Plaintext without a prefix: MySQL and `CONCAT`

When an old system stores plaintext in a column, a value without a prefix is used directly as plaintext. The exceptions are plaintext that itself starts with `{` (it would be mistaken for a format prefix), or starts with something like `$2a$`, `$1$` or `$argon2` (it would be treated as a hash with a missing prefix; see [A hash without a prefix is not a password](#a-hash-without-a-prefix-is-not-a-password)). In that case add `{clear}` in the SQL:

```sql
-- SQLite, PostgreSQL: concatenate with ||
SELECT '{clear}' || value FROM radcheck WHERE username = ?
-- MySQL: use CONCAT
SELECT CONCAT('{clear}', value) FROM radcheck WHERE username = ?
```

(In MySQL, `||` is a logical OR by default, not concatenation, so write `CONCAT`. The two lines above were not run on a real database.)

### Before going live

- **Run both in parallel for a while.** Point a few NAS at rradius and leave the others on the old system; when the login results match, switch all of them.
- **Reply attributes from the old system** (VLAN, rate, privilege) must be moved too; put them in `user_reply` (the example's table, which `reply_query` reads). Attribute names must be names in the rradius dictionary ([Chapter 06](06-dictionary.md)); after the move, try a few accounts with real logins, and do not rely on `check` alone.
- **Shared secrets from the old system** can be kept, and the NAS only needs the new server address; better still, use this chance to switch to long secrets that differ per device.
- **Slow hashes queue.** `{crypt}`, `{argon2}` and PBKDF2 are slow on purpose, so many simultaneous logins queue up and get slower at peak. Values with too high a cost are stopped by `[auth.password_limits]` (see [Limit the cost of hashing](#limit-the-cost-of-hashing)); if the migrated hashes naturally cost a lot, raise the limits first.
- **Set a deadline.** For accounts that have not changed their password by the deadline, use a new migration file to set the password to `NULL` (for example `UPDATE users SET password = NULL WHERE origin <> 'rradius'`). An account whose password column is `NULL` can never log in (log: `no stored password; rejecting`), and the user must ask an administrator for a reset. This `UPDATE` is written for the example table and was not run.
- **Not tested:** the actual flow of exporting from, and importing out of, any specific old system. The hashes in the example were computed from the definition of each format, and the scenario test confirms that rradius verifies them.

If you want the accounts to stay in their original PostgreSQL or MySQL and keep being maintained by the old system, the sql handler can query the external database directly with `database`; `external-db.toml` in the scenario is written this way, see [Chapter 09](09-external-databases.md).

> **Warning:** The accounts and passwords in `2_imported_users.sql` are public demo values. When you turn this scenario into a production setup, delete this file and replace it with the data you exported yourself.

## Limit the cost of hashing

The value of a slow hash itself carries its computation cost, for example the iteration count of PBKDF2, the cost of bcrypt and the memory use of argon2. Someone who can write to the account table can put in a value with an extremely high cost, so that every login holds a check slot for a very long time or uses a lot of memory.

`[auth.password_limits]` sets limits. Before the check, the cost in the value is read, and if it exceeds the limit the hash is not computed and the password is simply treated as wrong:

| Key | Default | Format it limits |
| --- | --- | --- |
| `auth.password_limits.pbkdf2_iterations` | `5000000` | Iterations of `{PBKDF2*}` and `{X-PBKDF2}` |
| `auth.password_limits.crypt_rounds` | `1000000` | Rounds of `$5$`, `$6$`, `$sha1$` and extended DES in `{crypt}` |
| `auth.password_limits.bcrypt_cost` | `15` | The bcrypt cost in `{crypt}` |
| `auth.password_limits.argon2_memory_kib` | `262144` (256 MiB) | Memory of `{argon2}`, in KiB |
| `auth.password_limits.argon2_iterations` | `10` | Iterations of `{argon2}` |
| `auth.password_limits.argon2_lanes` | `16` | Parallelism of `{argon2}` |

- Without this section, the limits are still these defaults. Every value must be greater than 0.
- It can be reloaded; if a reload fails, the old limits stay.
- Formats with a fixed cost (DES, `$1$`, the SHA and MD5 digests, `{nthash}`) are not limited.
- Going over a limit counts as one failed login: `rradius_password_checks_total` records it as `failed`, and [login failure lockout](#login-failure-lockout) counts it too. It is not "backend unavailable".

### Goal: lower the PBKDF2 limit to 100,000 iterations

```toml
# rradius.toml (add after [[auth.handlers]])
[auth.password_limits]
pbkdf2_iterations = 100000
```

The account `erin`, which stores `{PBKDF2-SHA256}` with 200,000 iterations, can log in before the limit is lowered; after a reload, even the correct password is rejected, and the log gets one warning (once per account, without the stored value):

```text
… WARN rradius_core::auth_chain: the stored password asks for more than [auth.password_limits].pbkdf2_iterations allows and is refused; raise the limit if the hash is meant to be used user="erin" format="pbkdf2-sha256" parameter="pbkdf2_iterations"
```

**Verified.** To keep such an account able to log in, raise the limit, or have the user change the password so that it is stored in the `scheme` format.

> **Note:** `argon2_memory_kib` is the limit for one check. The number of simultaneous checks equals the number of CPUs, so the worst-case memory use is about this value times the number of CPUs.

## Authentication protocols and the MS-CHAPv1 switch

The auth chain decides which kind of login it is from what the request carries:

| Protocol | Attributes in the request | Stored values it can verify |
| --- | --- | --- |
| PAP | `User-Password` | All formats in the previous section |
| CHAP | `CHAP-Password` (`CHAP-Challenge` or the Request Authenticator) | Plaintext, `{clear}` |
| MS-CHAPv2 | The Microsoft vendor attributes `MS-CHAP-Challenge` (16 octets) and `MS-CHAP2-Response` | Plaintext, `{clear}`, `{nthash}` |
| MS-CHAPv1 | `MS-CHAP-Challenge` (8 octets) and `MS-CHAP-Response` | Plaintext, `{clear}`, `{nthash}` |

TACACS+ logins go through the same chain; for the `[tacacs]` settings see [Chapter 12](12-tacacs.md).

When MS-CHAPv2 succeeds, the Access-Accept carries `MS-CHAP2-Success`, `MS-MPPE-Send-Key` and `MS-MPPE-Recv-Key`; when it fails, the Access-Reject carries `MS-CHAP-Error` (error 691). When the user name is written as `domain\account`, the response is computed with the part after the backslash. MS-CHAPv1 checks only the NT-Response and does not accept a response that has only an LM-Response.

MS-CHAPv1 is a very old protocol, and with an `[auth]` chain it is **not accepted by default** (a setup without `[auth]`, where a Lua script handles everything, is outside the scope of this switch). Turn it on explicitly only when a NAS cannot do without it (for example an older PPTP gateway):

```toml
# rradius.toml (inside [auth])
[auth]
policy = "first_match"
allow_mschapv1 = true
```

`auth.allow_mschapv1` is `false` by default and can be reloaded. When the key is not set, or is `false`, an MS-CHAPv1 login is rejected before any handler runs, and `handler` in the authentication log is `mschapv1_refused`; the database is not queried, and it does not count as a failed login. PAP, CHAP, MS-CHAPv2 and EAP are not affected.

> **Upgrade note:** This default used to be `true`. After an upgrade, old devices that use only MS-CHAPv1 fail every login; `handler="mschapv1_refused"` in the authentication log shows which NAS they are, and you set `allow_mschapv1 = true` only after confirming it is needed.

> **Not verified on real hardware:** `rradius-bench` has no MS-CHAPv1 load, so this part was checked only against the code and tests; the setting itself passed `check`.

## sql handler

The sql handler is the most commonly used handler: it looks up the account's password with one SQL statement, and after the check passes, looks up the reply attributes with another.

```toml
# rradius.toml
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"
```

| Key | Default | Description |
| --- | --- | --- |
| `user_query` | — (required) | Exactly one `?`, filled with the account name. The first column is the stored password |
| `reply_query` | — | Exactly one `?`. Each row is `(attribute name, value)`, added to the Access-Accept in order |
| `certificate_query` | — | The query used instead after an EAP-TLS certificate passes; if it finds a row, the login is accepted ([Chapter 11](11-eap.md)) |
| `default_user` | — | When the account is not found, verify against this account's password instead |
| `database` | — (cluster database) | Query a `[databases.<name>]` instead ([Chapter 09](09-external-databases.md)) |
| `timeout_ms` | `5000` | Time limit for one query, in milliseconds; it also covers the queueing and checking time of slow hashes |

The whole of `[auth]` can be reloaded (except the connection that `database` points to: a change in `[databases]` needs a restart).

How the query results are read:

| Result of `user_query` | Answer of the handler |
| --- | --- |
| No row | ignore (with `default_user`, that account is used instead) |
| One row, password check passes | accept, then `reply_query` runs |
| One row, password does not match | reject |
| One row, the password column is NULL or empty | reject (log: `no stored password; rejecting`) |
| Two or more rows | reject (log: `user_query returned 2 rows; rejecting`) |
| Query fails or times out | Backend unavailable; no answer |

The account name is always bound as a parameter and never joined into the SQL string.

The attribute names in `reply_query` must be in the dictionary ([Chapter 06](06-dictionary.md)). `alice` in quick-start has two rows, `Session-Timeout = 3600` and `Filter-Id = staff`, and the Access-Accept of a successful login carries these two attributes, in the order the query returns them.

### default_user: a shared account when none is found

`default_user` makes a name that is not found verify against the password of a shared account, for example one shared password for a guest network:

```toml
# rradius.toml (excerpt)
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"
default_user = "DEFAULT"
```

The table needs a `DEFAULT` account. In the test it stores `{clear}guest-password`, and the handler is in the `lab` chain of the next section:

- Name `visitor`, password `guest-password`: `accept 1`. In the authentication log `user` is `visitor` and `account` is `DEFAULT`, and `reply_query` also looks up the reply attributes of `DEFAULT`.
- Name `visitor`, wrong password: `reject 1`. Note that the result is reject, not ignore: with `default_user`, any name that is not found becomes a check against the `DEFAULT` password, and the handlers after it never get a chance.

**Verified.**

> **Warning:** Anyone who knows the `DEFAULT` password can log in with any name. `default_user` does not apply inside EAP (the EAP name is recorded and returned to the device), nor to EAP-TLS certificate logins.

## lua handler

The lua handler runs a separate Lua script, for rules that tables and SQL cannot express. For how to write scripts and the functions available, see [Chapter 10](10-lua-policies.md) and [Appendix D](93-lua-reference.md).

```toml
# rradius.toml (excerpt)
[[auth.handlers]]
type = "lua"
script = "policy.lua"
```

| Key | Default | Description |
| --- | --- | --- |
| `script` | — (required) | The script file, relative to the directory of the configuration file |
| `verifies_password` | `false` | Set to `true` when the script checks the password itself |

The return value of the script maps to the handler's answer: `"accept"`, `"ignore"`, `"challenge"` (RADIUS only); any other value, or a script error, is reject. The global variable `auth_user` in the script is the normalized account name, while `get_attr('User-Name')` is still the name as received.

`verifies_password` matters:

- The default is `false`, meaning the script only checks policy (for example the time of day or the source device), and its accept does not mean the password is correct.
- With `true`, the script verifies the password itself (for example with `password_verify`). In `all_accept`, only then does its accept count as "someone checked the password"; in EAP, only then does its accept count as proof of the password.

`check` reads and compiles the script of every lua handler, and reports an error if it cannot be read or does not compile.

## all_accept: add a policy on top of the password

Goal: the sql handler checks the password, and a script then blocks one NAS. The login is let through only when both conditions pass.

```lua
-- policy.lua
if get_attr('NAS-Identifier') == 'lab-blocked' then
  return 'reject'
end
set_attr('Reply-Message', 'policy ok')
return 'accept'
```

```toml
# rradius.toml (excerpt)
[auth.chains.lab]
policy = "all_accept"

[[auth.chains.lab.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[[auth.chains.lab.handlers]]
type = "lua"
script = "policy.lua"
```

This is a named chain `lab`; the next section explains how to make a client use it. For the test, set the `localhost` client to `auth = "lab"`:

```console
$ rradius-bench run … --user bob --password bob-password --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
$ rradius-bench run … --user bob --password bob-password --nas-identifier lab-blocked --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
```

The server's authentication log:

```text
… user="bob" account="bob" chain="lab" handler="lua" decision="accept"
… user="bob" account="bob" chain="lab" handler="lua" decision="reject"
```

When both handlers accept, the Access-Accept carries the reply attributes of both, in the order they ran (the `reply_query` ones first, then the `set_attr` ones).

**Verified.**

### When nobody checks the password

`all_accept` has a protective rule: if the chain has any handler that checks passwords (sql, ldap, netlogon, or lua with `verifies_password = true`), one of them must accept for the result to be accept. Otherwise, in a chain like the one above, the sql handler would not find the account (ignore) and the policy script would accept anyone, so the result would be "everyone gets in".

So, with the same chain, log in with a name that does not exist:

```text
… WARN rradius_core::lua_engine: auth chain: rejecting a request from client localhost: all_accept without checking a password
… user="visitor" account="visitor" chain="lab" handler="no_password_check" decision="reject"
```

The result is reject, and the policy script does not run at all: no handler that checks passwords is left after it, and any data the script wrote would stay with a request that is bound to be rejected.

Other details:

- If the policy script returns `challenge` before a handler that checks passwords has accepted, it is also rejected, and no Access-Challenge is sent.
- If the chain has only lua handlers that do not check passwords, their answers decide.
- This kind of rejection does not count as a failed login.

**Verified** (a name that does not exist is rejected, `handler="no_password_check"`).

## Let different NAS use different chains

`[[auth.handlers]]` is the default chain, called `default` in the authentication log. You can also define any number of named chains, and a client picks one with `auth`:

```toml
# rradius.toml (excerpt)
[auth.chains.lab]
policy = "all_accept"
# …(handlers as in the previous section)

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
auth = "lab"
```

Rules:

- A client without `auth` uses the default chain. When there is no default chain (`[auth]` has only named chains), this client is decided by the top-level script of `[lua]`.
- An `auth` that points to a chain that does not exist makes `check` fail: `radius client 'localhost': unknown auth chain 'vpn' (no [auth.chains.vpn])`.
- TACACS+ clients also pick a chain with `auth` ([Chapter 12](12-tacacs.md)). When RADIUS and TACACS+ clients point to the same chain, they share one set of handlers, including the LDAP connection pool.
- Each chain is built once, at startup and at reload.
- A named chain must set its own `policy` and `username`; it does not inherit them from `[auth]`. If `[auth]` sets `policy` or `[auth.username]` but has no `[[auth.handlers]]`, `check` fails.

The example scenario `remote-access` works this way: the VPN gateway uses the `vpn` chain (LDAP), and the jump host uses the `mfa` chain (Lua two-step verification). For the LDAP part see [Chapter 08](08-ldap-and-ad.md), and for two-step verification see [Chapter 15](15-access-recipes.md).

A change of chain only needs a reload. Run it outside the terminal that runs the server:

```console
$ kill -HUP <server PID>
```

The server log writes `configuration reloaded from rradius.toml`, and later requests use the new chain. For the details of reload see [Chapter 04](04-configuration.md).

## Normalize user names and realms

The names users send are often inconsistent: `alice`, `Alice`, `CORP\alice`, `alice@corp.example`. Before handing it to the handlers, `[auth.username]` turns the received name into the account name:

```toml
# rradius.toml (add after [[auth.handlers]])
[auth.username]
strip_realm = "both"
lowercase = true
allowed = "[a-z0-9._-]+"
```

| Key | Default | Description |
| --- | --- | --- |
| `strip_realm` | `none` | The realm to remove: `none`, `suffix`, `prefix`, `both` |
| `lowercase` | `false` | Convert to lower case |
| `allowed` | — | A regular expression that the normalized name must match **entirely** |

The order is fixed: remove the realm first, then lower the case, and check `allowed` last.

| `strip_realm` | `lowercase` | Received | Account name |
| --- | --- | --- | --- |
| `none` | `false` | `Alice@Corp` | `Alice@Corp` |
| `suffix` | `false` | `Alice@Corp` | `Alice` (removed from the last `@` on) |
| `suffix` | `false` | `a@b@c` | `a@b` |
| `suffix` | `false` | `CORP\Alice` | `CORP\Alice` |
| `prefix` | `false` | `CORP\Alice` | `Alice` (removed up to the last backslash) |
| `prefix` | `false` | `A\B\c` | `c` |
| `both` | `true` | `CORP\Alice@Corp` | `alice` (prefix first, then suffix) |

If the result is an empty string, or does not match `allowed`, the chain rejects at once, runs no handler, and `handler` in the authentication log is `normalization`. The log gives only the reason and not the received name, because it comes from outside and may contain control characters:

```text
… WARN rradius_core::lua_engine: auth chain: rejecting a request from client localhost: the user name does not match the allowed pattern
… user="alice smith" account="" chain="default" handler="normalization" decision="reject"
… WARN rradius_core::lua_engine: auth chain: rejecting a request from client localhost: the user name is empty after normalization
… user="@corp" account="" chain="default" handler="normalization" decision="reject"
```

Tested with the settings above: `CORP\Alice@corp.example` and `ALICE` both log in as `alice`; `alice smith` (it has a space) and `@corp` (empty after normalization) are rejected. **Verified.**

### Effects elsewhere

- **Handlers look up the account name:** `user_query` and `reply_query` of sql and the search condition of ldap all use the normalized name. Always store lower case in the table.
- **The password check uses the received name:** the MS-CHAPv2 response is computed over the name the user sent, so verification uses it as it was.
- **The `user` command applies it too:** when the default chain has `[auth.username]`, `user add 'CORP\Henry@Corp'` stores `henry` (the output is `added henry`); a name that does not match `allowed`, for example `user add 'bad name'`, prints `'bad name': the user name does not match the allowed pattern`, exits with code 2 and writes nothing. The `username` of a named chain does not affect the `user` command.
- **lua handler:** `auth_user` is the account name, and `get_attr('User-Name')` is the received name.
- **EAP-TLS certificates:** the certificate proves the whole name. If normalization changes anything besides ASCII case (for example removing the realm), the login is rejected ([Chapter 11](11-eap.md)).

> **Note:** `allowed` gets anchors at the start and end automatically. Writing `[a-z0-9._-]+` already means "the whole name can have only these characters", so you do not need to write `^…$` yourself.

## Password length

According to section 5.2 of RFC 2865, the RADIUS User-Password is always 16 to 128 octets, and a multiple of 16 (the NAS pads the password with zeros to whole blocks). When a request arrives with a wrong length, rradius answers Access-Reject before decrypting, runs no handler or script, and does not count it in [login failure lockout](#login-failure-lockout), because this is not password guessing; `handler` in the authentication log is `length`.

Each rejection writes one WARN line with only the client name and the length, without the password, and it is rate limited, so a flood of requests does not flood the log:

```text
… WARN rradius_core::lua_engine: RADIUS client nas1: Access-Request refused, its User-Password is 240 octets, not 16 to 128 in whole blocks of 16
```

> **Upgrade note:** A User-Password longer than 128 octets used to be decrypted and verified as usual; now it is rejected. A NAS that does not pad to a multiple of 16 is rejected too. Before you upgrade, check the authentication log for lines with `handler="length"`; if there are any, it is a NAS configuration problem, not a wrong password.

## Login failure lockout

`[auth.lockout]` limits password guessing: when the same account fails too many times from the same origin within a period, it is locked first, and during the lock logins are rejected at once, without even checking the password.

```toml
# rradius.toml (add after [[auth.handlers]])
[auth.lockout]
max_failures = 3
lock_seconds = 60
```

| Key | Default | Description |
| --- | --- | --- |
| `max_failures` | `10` | How many failures within the window cause a lock |
| `window_seconds` | `300` | The window, counted from the first failure, in seconds |
| `lock_seconds` | `300` | How long the lock lasts, in seconds |
| `max_entries` | `100000` | The most keys each node remembers |
| `account_max_failures` | — (no total limit per account) | The limit on the total failures of one account from all origins; must be greater than `max_failures` |
| `max_in_flight` | 5 times `max_failures` | The most logins that can wait for a result at the same time for one key; counted failures take slots too (see below) |

Every value must be greater than 0; it can be reloaded. Without this section nothing is counted at all.

### The unit of counting

The lockout "key" is the **account name plus the origin**:

- The account name is the name the chain normalized and looks up, then converted to ASCII lower case. So `Alice` and `alice` are the same key, and changing the case does not get around it.
- The origin is the `Calling-Station-Id` of the RADIUS request (usually the MAC address of the user's device), or the `rem_addr` of TACACS+. If there is none, it is empty.

So if `alice` is locked on her own laptop, a login from her phone (another `Calling-Station-Id`) is not affected. With `account_max_failures` set, the failures of one account from all origins are also added up separately, and when the limit is reached, the account is locked for all origins.

> **Note:** `account_max_failures` lets an attacker lock someone else's account for all origins just by sending wrong passwords on purpose. Set it only when you are sure you need it.

What counts as one failure: a login the chain rejects, and not because the backend is unavailable. These do **not** count:

- The backend is unavailable (a database, LDAP or domain controller failure, or a slow-hash queue timeout)
- The name does not match `[auth.username]` (`normalization`), no one checked the password in `all_accept` (`no_password_check`), `mschapv1_refused`
- Access-Challenge, and EAP-TLS certificate logins

A successful login clears the failure count of this key. Accounts that do not exist are counted and locked just the same, so you cannot tell from the responses whether an account exists.

`max_in_flight` deals with another way of guessing: send a large batch at once, so that all of them arrive before any one fails. Every login takes a slot before the handlers run and returns it when the result is out; when one key has used all `max_in_flight` slots, later logins are rejected at once.

The failures counted in the window take slots as well: the logins waiting plus the failures counted must stay below `max_in_flight` (below `max_failures`, if that is larger). So within one window, wrong passwords are checked at most that many times before the lock (50 with the default values), whether they arrive together or one after another. A key with `k` counted failures can have at most 50 − `k` logins waiting; a successful login clears the count. A key that is not locked and has no login waiting always gets one slot, so after a reload that lowers the limits below the failures already counted, the next failure locks it.

### Goal: lock a device that keeps entering wrong passwords

With the `max_failures = 3` settings above, `--attr 31=text:…` makes `rradius-bench` send `Calling-Station-Id` (attribute 31):

```console
$ rradius-bench run … --user alice --password wrong --attr 31=text:aa-bb-cc-dd-ee-01 --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
```

Three wrong passwords lock it. After that, even the **correct** password from the same device is rejected; with another `Calling-Station-Id` the login works:

```console
$ rradius-bench run … --user alice --password alice-password --attr 31=text:aa-bb-cc-dd-ee-01 --load pap,concurrency=1 --requests 1
…  accept 0, reject 1 …
$ rradius-bench run … --user alice --password alice-password --attr 31=text:aa-bb-cc-dd-ee-02 --load pap,concurrency=1 --requests 1
…  accept 1, reject 0 …
```

The server log; the lock starts at the third failure:

```text
… INFO rradius_core::auth_chain: auth lockout: client localhost: account "alice" from "aa-bb-cc-dd-ee-01" is locked (this origin)
… user="alice" account="alice" chain="default" handler="lockout" decision="reject"
```

Metrics:

```console
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_auth_lockout'
rradius_auth_lockout_rejected_total 1
rradius_auth_lockouts_total 1
rradius_auth_lockout_untracked_total 0
```

| Metric | Meaning |
| --- | --- |
| `rradius_auth_lockouts_total` | How many locks have started |
| `rradius_auth_lockout_rejected_total` | Logins rejected because of a lock or because no slot was free |
| `rradius_auth_lockout_untracked_total` | Failures not counted because the table was full of locked keys |

These three metrics appear only when `[auth.lockout]` is set. **Verified.**

### Where the counts are kept

- The counts are kept in **each node's own memory**. With three cluster nodes and a NAS that asks them in turn, the number of tries an attacker really gets is at most the limit of each of the three nodes.
- A reload keeps the counts; a restart resets them to zero.
- When the table is full (`max_entries`), expired entries go first, then the oldest unlocked ones; locked keys are never dropped. When all of them are locked, new keys are not tracked and are counted in `rradius_auth_lockout_untracked_total`, and the log writes at most once a minute.

> **Note:** When a Lua script checks the password itself (for example `two-step-totp.lua` uses `password_verify`), `[auth.lockout]` has no control over that check inside the script. To limit password guessing, let the password be checked in the auth chain.

## Common mistakes

**Symptom:** The user is sure the password is right, the authentication log shows `handler="sql" decision="reject"`, and the log has `looks like a hash with no scheme prefix and is refused`.
**Cause:** The stored hash has no `{crypt}` or `{argon2}` prefix.
**Fix:** Add the prefix; see [A hash without a prefix is not a password](#a-hash-without-a-prefix-is-not-a-password).

**Symptom:** PAP logs in, but CHAP or MS-CHAPv2 (including PEAP) is rejected, and the Access-Reject of MS-CHAPv2 carries `MS-CHAP-Error`.
**Cause:** A one-way hash is stored (bcrypt, SHA-512 crypt, `{SSHA}` and so on). CHAP can check only plaintext, and MS-CHAP only plaintext or `{nthash}`.
**Fix:** Change `accounts.scheme` to `nthash` (MS-CHAP) or `clear` (CHAP), and have the user reset the password once with `user passwd`.

**Symptom:** A migrated account has the right password but is rejected.
**Cause:** The prefix is wrong (for example `{MD5}` was added directly to a hexadecimal MD5, without converting to Base64); the position of the salt or the format differs from the definition; or the hash was truncated or got extra spaces in the import.
**Fix:** Take an account whose password you know and go through [All formats the server understands](#all-formats-the-server-understands) item by item.

**Symptom:** Only accounts whose names have capital letters cannot log in.
**Cause:** `lowercase` is on, but the names in the table were not converted to lower case.
**Fix:** Make the table all lower case (see [Import SQL](#import-sql)).

**Symptom:** PAP works, but PEAP on Wi-Fi or CHAP for PPPoE does not.
**Cause:** PEAP needs plaintext or `{nthash}`, and CHAP needs plaintext; other hashes cannot do it.
**Fix:** See [What can and cannot be migrated](#what-can-and-cannot-be-migrated).

**Symptom:** `user disable` prints `disabled erin`, but `erin` can still log in.
**Cause:** `user_query` does not skip disabled accounts.
**Fix:** Add `AND enabled = 1` to `user_query` (according to your table).

**Symptom:** `user add` prints `bad.toml: accounts.scheme: 'md5' is not one of bcrypt, sha512-crypt, nthash, clear` and exits with code 2.
**Cause:** `scheme` can be only these four values. The server understands formats such as `{MD5}`, but the `user` command does not write them.
**Fix:** Change it to one of the four values.

**Symptom:** `check` prints `radius client 'localhost': unknown auth chain 'vpn' (no [auth.chains.vpn])`.
**Cause:** The `auth` of a client points to a chain that is not defined.
**Fix:** Add `[auth.chains.vpn]`, or correct the name in `auth`.

**Symptom:** `check` prints `auth.username is set but the default chain has no [[auth.handlers]] entries`, or `auth.policy is set but …`.
**Cause:** `[auth]` has only named chains, but `[auth.username]` or `policy` is written in `[auth]`.
**Fix:** Write them in the named chain: `[auth.chains.<name>.username]`, `[auth.chains.<name>] policy = …`.

**Symptom:** `check` prints `[auth] needs [[auth.handlers]] entries or at least one [auth.chains.<name>] (auth.handlers is empty)`.
**Cause:** There is an `[auth]`, but no handler at all.
**Fix:** Add handlers; if you do not want an auth chain, remove `[auth]` altogether and let the script in `[lua]` decide.

**Symptom:** `check` prints `auth.chains.lab.handlers[0].user_query must contain exactly one ? placeholder, found 2`.
**Cause:** `user_query`, `reply_query` and `certificate_query` can each have only one `?`; the account name is filled in only once.
**Fix:** Rewrite the query to keep only one `?`.

**Symptom:** `check` prints `auth.lockout.account_max_failures must be greater than max_failures (3), not 2`, or `auth.password_limits.pbkdf2_iterations must be greater than 0`.
**Cause:** A value is out of range.
**Fix:** `account_max_failures` must be greater than `max_failures`; every limit must be greater than 0.

**Symptom:** `check` prints `auth.username.strip_realm: 'left' is not none, suffix, prefix or both`, or `auth.username.allowed: regex parse error: …`.
**Cause:** The value of `strip_realm` is wrong, or `allowed` is not a valid regular expression.
**Fix:** Correct it according to [Normalize user names and realms](#normalize-user-names-and-realms).

**Symptom:** `check` prints `auth.chains.lab: cannot read auth handler script 'missing.lua': No such file or directory (os error 2)`.
**Cause:** The `script` of a lua handler cannot be found. A relative path is relative to the directory of the configuration file.
**Fix:** Correct the path.

**Symptom:** Some accounts occasionally get no answer, `Discarded: database` in `status` goes up, and those accounts store bcrypt or PBKDF2.
**Cause:** The number of slow-hash checks at the same time equals the number of CPUs; at peak, queueing plus checking exceeds `timeout_ms` and counts as backend unavailable.
**Fix:** Check whether the cost of the stored values is too high (`[auth.password_limits]`); switch accounts that use only PAP to the faster `sha512-crypt`; or add CPUs or nodes.

**Symptom:** A device is locked, the lock time is not over, and you want to unlock it now.
**Cause:** The counts are only in the node's memory, and there is no unlock command.
**Fix:** Wait for `lock_seconds` to pass; or remove `[auth.lockout]` and reload (this clears all counts), then add it back later.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [04 Configuration file basics](04-configuration.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [06 Dictionary and attributes](06-dictionary.md)
- [08 LDAP and Active Directory](08-ldap-and-ad.md)
- [09 External databases and named queries](09-external-databases.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [11 802.1X and EAP](11-eap.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix C: Metrics reference](92-metrics-reference.md)
