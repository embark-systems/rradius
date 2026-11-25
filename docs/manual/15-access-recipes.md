# 15 Scenarios: network access for organizations

> After this chapter you can:
> - Let devices that cannot do 802.1X, such as printers and IP phones, log in by MAC address and place them in their own VLANs
> - Let a VPN gateway use accounts and groups from the company directory to decide who may connect and which set of rules applies
> - Add a two-step login to a jump host: a password, then a TOTP code
> - Ask an HTTP API on every login when the accounts live in another system

Prerequisites: you have read [chapter 03](03-quick-start.md) and "Prepare a lab directory" in [chapter 14](14-isp-recipes.md), and you know the auth chain from [chapter 07](07-users-and-chains.md). The details of connecting to LDAP and Active Directory are in [chapter 08](08-ldap-and-ad.md); the HTTP and TOTP functions for scripts are in [chapter 10](10-lua-policies.md).

In this chapter:

- [Prepare the lab directory](#prepare-the-lab-directory)
- [MAC authentication on wired networks: wired-mab](#mac-authentication-on-wired-networks-wired-mab)
- [Remote access: remote-access](#remote-access-remote-access)
- [Accounts in an external system: external-api](#accounts-in-an-external-system-external-api)
- [Single-file examples](#single-file-examples)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Prepare the lab directory

The three scenarios in this chapter are in `rradius-server/examples/scenarios/` in the source tree. You copy them, change the dictionary path and replace the demo secrets the same way as in [chapter 14](14-isp-recipes.md):

```console
$ cp -R rradius-server/examples/scenarios/wired-mab ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/wired-mab/rradius.toml
$ cd ~/rradius-lab/wired-mab
$ for f in *.secret; do openssl rand -hex 32 > $f; done; chmod 600 *.secret
$ rradiusd check --config rradius.toml
configuration ok
```

The client addresses in all three scenarios are subnets of a real network (switches, VPN gateway, hotspot), but `rradius-bench` sends its requests from `127.0.0.1`. In the lab directory only, change the `address` of the client you want to test to `"127.0.0.1"`. Otherwise the request is dropped as coming from an unknown client, and `rradius-bench` shows only `timeouts 1`. To see the reply attributes, add `[log.trace]` as in chapter 14, with `clients` set to the client name of this scenario.

## MAC authentication on wired networks: wired-mab

### Goal

Printers, IP phones and cameras cannot do 802.1X. The switch uses MAC authentication (MAB) instead: it sends the device's MAC address as the account to be checked. Known devices are placed in their own VLANs; unknown devices are rejected.

### Tables

```sql
-- migrations/1_schema.sql
CREATE TABLE mac_devices (
  mac TEXT PRIMARY KEY,
  vlan TEXT NOT NULL,
  note TEXT
);
```

`mac` is always stored as 12 lowercase hexadecimal characters, however the switch writes it (`00-11-22-AA-BB-CC`, `00:11:22:aa:bb:cc` and `0011.22aa.bbcc` are all normalized to `001122aabbcc`). `note` is for people; the script does not read it. The demo data has one printer (VLAN 30) and one meeting-room phone (VLAN 40).

### Key settings

```toml
# rradius.toml (excerpt, other settings omitted)
[lua]
script = "mac-auth.lua"

[[radius.clients]]
name = "access-switches"
address = "10.40.0.0/24"
secret_file = "switches.secret"
```

- There is no `[auth]`. Authentication and accounting are both handled by `mac-auth.lua`, and accounting is always answered.
- All switches in the management subnet share one client (chapter 05).

### Key points of the script

`mac-auth.lua` (that is, `rradius-server/examples/mac-auth.lua`):

1. It first turns away protocols other than RADIUS. It is the top-level script, so if the same server also has `[tacacs]` enabled, accepting everything would authorize every command.
2. It normalizes both `Calling-Station-Id` and `User-Name` to 12 hexadecimal characters, and the two must be equal. `User-Name` can be anything the device says; `Calling-Station-Id` is the device the switch actually sees.
3. It looks the address up in `mac_devices`. If it finds it, it returns three attributes that place the device in a VLAN:

   ```lua
   set_attr('Tunnel-Type:1', 'VLAN')
   set_attr('Tunnel-Medium-Type:1', 'IEEE-802')
   set_attr('Tunnel-Private-Group-Id:1', rows[1].vlan)
   ```

   `:1` is the tag (RFC 2868). The three attributes use the same tag, which says that they describe the same tunnel.

The script does not check the password: whatever password the switch sends makes no difference, and only the address counts. So MAB works with both PAP and CHAP.

> **Warning:** A MAC address is not a secret. Anyone who can read it from the device label or from the wire can impersonate the device. Put these devices in a VLAN that can reach only the services they need.

**What about unknown devices.** The example script returns reject for an address it does not know, and the switch then decides what happens to that port (put it in a guest VLAN, keep it isolated, or cut the power). This is switch configuration, not rradius, and the commands differ from vendor to vendor.

> **Not verified on real hardware:** The switch-side setting "after a RADIUS reject, put the port in a guest or quarantine VLAN" has not been tried on a real switch.

To have rradius place unknown devices in a guest VLAN itself, change the script: when the lookup finds nothing, do not return reject, but return accept with a guest VLAN:

```lua
-- mac-auth.lua (the changed part)
local GUEST_VLAN = '99'
local rows = sql_query('SELECT vlan FROM mac_devices WHERE mac = ?', {mac})
if #rows ~= 1 then
  set_attr('Tunnel-Type:1', 'VLAN')
  set_attr('Tunnel-Medium-Type:1', 'IEEE-802')
  set_attr('Tunnel-Private-Group-Id:1', GUEST_VLAN)
  return 'accept'
end
```

Verified: with the example changed this way, `00-ff-ff-ff-ff-ff`, which is not in the table (`User-Name` and `Calling-Station-Id` the same), gets accept, and the trace line shows `Tunnel-Private-Group-Id = 0x013939` (tag 1 plus the text `99`). The known printer is still in VLAN 30. A request whose `User-Name` and `Calling-Station-Id` differ is still rejected.

> **Warning:** After this change, anyone who plugs a computer into the network cable gets into the guest VLAN. First make sure the guest VLAN reaches only the internet and cannot reach internal services.

When the same switch serves both users who log in with an account and password and MAB devices, they are the same client, so you cannot separate them by client. Instead, use `mac-auth.lua` as a lua handler in the auth chain, after the handler that checks passwords, with `first_match`:

```toml
# rradius.toml (excerpt, other settings omitted)
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"

[[auth.handlers]]
type = "lua"
script = "mac-auth.lua"
```

For an account the sql handler knows, the sql handler decides, and a wrong password is a reject that goes no further; only names it does not know are passed to `mac-auth.lua` to be matched by address. When PEAP or TTLS of EAP also goes through this chain, remember that an accept from a lua handler does not count as proof of the password: an sql or ldap handler must really have compared it. So the MAB rule does not let PEAP through without a password (see [chapter 11, "What happens inside the tunnel"](11-eap.md#what-happens-inside-the-tunnel)). Measured (the `users` table has `alice`, and `mac_devices` has the wired-mab demo data): `alice` with the right password is accept, and with a wrong password is reject (even when the request carries the printer's `Calling-Station-Id`); the printer's address is accept and returns VLAN 30.

### How to test, and notes

Verified. First change the `address` of `access-switches` to `"127.0.0.1"`. Write the MAC address as both the account and the password, and carry `Calling-Station-Id` with `--attr 31=text:…`:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat switches.secret)" \
    --user 00-11-22-AA-BB-CC --password 00-11-22-AA-BB-CC \
    --attr 31=text:00-11-22-AA-BB-CC --load pap,concurrency=1 --requests 1
…
  pap: 1430/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

Results for different cases:

| Request | Result |
| --- | --- |
| Printer, with `User-Name` and `Calling-Station-Id` both `00-11-22-AA-BB-CC` | accept, VLAN 30 |
| Phone, with `User-Name` `00a0c9123456` and `Calling-Station-Id` `00:a0:c9:12:34:56` | accept, VLAN 40 (written differently, equal after normalization) |
| `User-Name` `00a0.c912.3456` (dotted form), `Calling-Station-Id` `00:a0:c9:12:34:56` | accept, VLAN 40 |
| The printer's address sent with `--load chap` | accept |
| `User-Name` is the printer, `Calling-Station-Id` is another address | reject |
| No `Calling-Station-Id` | reject |
| An address that is not in the table | reject |

In the trace line, the three reply attributes look like this:

```text
… rradius_trace: response client="access-switches" source=127.0.0.1 user="00-11-22-AA-BB-CC" code=2 attributes="Message-Authenticator = <hidden>, Tunnel-Type = 16777229, Tunnel-Medium-Type = 16777222, Tunnel-Private-Group-Id = 0x013330"
```

The trace shows the raw values with the tag: `16777229` is `0x0100000d` in hexadecimal, where the leading `01` is tag 1 and `0d` (13) is `VLAN`; `16777222` is tag 1 plus `IEEE-802` (6); `0x013330` is tag 1 plus the text `30`.

Notes:

- The script looks the table up on every login, so adding a device row does not need a reload. For now, add data with a new migration file, for example `3_add_cameras.sql`:

  ```sql
  -- migrations/3_add_cameras.sql
  INSERT INTO mac_devices (mac, vlan, note) VALUES ('0011223344aa', '50', 'Front-door camera');
  ```

  Put it in `migrations/` and restart the server for the migration to be applied (restart a cluster one node at a time). Verified: after adding this file and restarting, a request with `User-Name` `0011.2233.44aa` and `Calling-Station-Id` `00:11:22:33:44:aa` got VLAN 50. When there are many devices and they change often, writing them one by one as migrations is inconvenient, and you need a separate process to manage this table (for example an import from an external system).
- `chain="script" handler="script"` in the authentication record means that the top-level script made this decision.

## Remote access: remote-access

### Goal

Two kinds of remote login on the same server:

- VPN gateway: employees log in with accounts from the company directory (LDAP or Active Directory). Only people in the `vpn-users` group may connect, and different access rules are returned depending on the group.
- Jump host: after the password, an Access-Challenge asks for the TOTP code from the authenticator app on the user's phone.

The two devices are two clients, and each names an auth chain (`auth`). Both share `accounting.lua` for accounting, written as in the quick start.

### Tables

The VPN accounts are in the directory and are not stored here. The jump host uses two tables:

| Table | Content |
| --- | --- |
| `mfa_users` | `password` (a stored password with a prefix, for example `{crypt}$6$…` or `{argon2}$…`), `totp_secret` (Base32, the same as in the authenticator on the user's phone), `last_step` (the time step used last), `failed_attempts`, `locked_until` |
| `mfa_pending` | Challenges waiting for a code: `state` (a random value), `username`, `expires` |

There is also `accounting` for accounting. The demo account `ops1` has the password `ops1-password` (stored as a SHA-512 crypt hash), and its TOTP key is the well-known demo value `JBSWY3DPEHPK3PXP`.

> **Warning:** `JBSWY3DPEHPK3PXP` is a public demo key, and anyone can compute its codes. In production every user must have their own randomly generated key.

### Key settings

```toml
# rradius.toml (excerpt, other settings omitted)
[lua]
script = "accounting.lua"

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

[auth.chains.mfa]
policy = "first_match"

[[auth.chains.mfa.handlers]]
type = "lua"
script = "two-step-totp.lua"

[[radius.clients]]
name = "vpn-gw"
address = "10.60.0.1"
secret_file = "vpn-gw.secret"
auth = "vpn"

[[radius.clients]]
name = "jump-host"
address = "10.60.0.20"
secret_file = "jump-host.secret"
auth = "mfa"
```

How the settings fit together:

| Setting | Default | reload | How it is used here |
| --- | --- | --- | --- |
| `auth.chains.<name>` | none | yes | Two named auth chains, with the same fields as `[auth]` |
| `radius.clients[].auth` | none | yes | Which named auth chain the client uses |
| `auth.chains.<name>.username.strip_realm` | `none` | yes | `both`: a user who types `CORP\alice` or `alice@corp.example.com` becomes `alice` |
| `auth.chains.<name>.username.lowercase` | `false` | yes | Convert to lower case |
| `password_check` of the ldap handler | `bind` | yes | First find the user with the service account, then check the password by binding as the user |
| `required_groups` of the ldap handler | no group required | yes | Accept only if the user belongs to one of the groups |
| `group_reply` of the ldap handler | none | yes | Reply attributes added when the user belongs to a group |

For the full description of each key, see [Appendix A](../manual.zh-TW/90-config-reference.md); for the other keys of the ldap handler (`ca_file`, timeouts, number of connections and so on), see [chapter 08](08-ldap-and-ad.md).

- The second half of `filter` makes disabled accounts in AD (bit 2 of `userAccountControl`) impossible to find, the same as if they did not exist. For the OpenLDAP form (`pwdAccountLockedTime`) see "Log in with directory accounts" in [chapter 08](08-ldap-and-ad.md); check it on your own directory first.
- `required_groups` looks at `memberOf` by default and counts only direct members. To count nested groups in AD, use `group_base_dn` plus `group_filter = "(member:1.2.840.113556.1.4.1941:={dn})"`; see "Decide by group whether a user may log in" in [chapter 08](08-ldap-and-ad.md) (this form has not been tested on AD).
- `password_check = "bind"` needs the plain-text password, so the VPN gateway must send passwords with PAP.
- The `Filter-Id` of `group_reply` must match a rule of the same name on the VPN gateway (a filter or group policy).
- When the directory server's certificate is not signed by a CA the system trusts, add `ca_file`.

Both clients name an auth chain, so authentication never reaches `accounting.lua` under `[lua]`, which handles accounting only.

### Key points of the script

`two-step-totp.lua` (that is, `rradius-server/examples/two-step-totp.lua`) works in two steps:

1. The request has no `State`: it compares `mfa_users.password` with `password_verify`, the same way the auth chain compares stored passwords. If it matches, it makes a random value with `random_token()`, writes it to `mfa_pending` (valid for 60 seconds), and returns `challenge` with `State` and `Reply-Message = Enter your code`.
2. The request carries `State`: it claims the challenge with `DELETE … WHERE state = ? AND username = ? AND expires >= ?`, and it counts only if 1 row was deleted, so a challenge can be used only once. Then it checks the code with `totp_verify`: 6 digits, 30-second steps, one step of tolerance either way. Finally it updates `last_step` only if this step is newer, so the same code cannot be used twice.

After 5 wrong codes in a row, the account is locked for 5 minutes. The lock is written in the cluster database, so every node sees it. The answer to a challenge may go to another node, and that node's copy may be slightly old.

> **Note:** The script limits only the number of guesses at the code, not guesses at the password: if guessing the password also locked the account, anyone who knew an account name could lock it on purpose. A script that checks passwords itself is not protected by `[auth.lockout]`. To limit password guesses as well, check the password in the auth chain (`[auth.lockout]` counts failures, chapter 07) and leave only the code to the script.

> **Enterprise edition:** The enterprise edition has a built-in TOTP/HOTP second factor, so you do not need to write this script.

When you switch to your own accounts, use Base32 for the code key, one key per person. After generating it, give it to the user as a QR code (`otpauth://totp/…`) to scan. After you replace the demo key, regenerate the QR code and the `otpauth` URL too.

### How to test, and notes

**Jump host: verified.** Change the `address` of `jump-host` in the lab directory to `"127.0.0.1"`, and send the first step:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat jump-host.secret)" \
    --user ops1 --password ops1-password --load pap,concurrency=1 --requests 1
…
  pap: 614/s, sent 1, accept 0, reject 0, challenge 1, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`challenge 1` is the Access-Challenge returned by the script; with a wrong password you get `reject 1`. `rradius-bench` does not answer a challenge, so for the second step use a client that handles Access-Challenge, for example FreeRADIUS's `radclient`, or a real jump host. With `radclient`, compute the code with `oathtool` (for the demo key `JBSWY3DPEHPK3PXP`: SHA-1, 6 digits, 30-second steps, the same as `totp_verify` in the code) and send it with the `State` value received in the first step:

```console
$ oathtool --totp -b JBSWY3DPEHPK3PXP
$ echo 'User-Name=ops1,User-Password=<code>,State=<State value>,Message-Authenticator=0x00' \
    | radclient 127.0.0.1:1812 auth "$(cat jump-host.secret)"
```

> **Not verified on real hardware:** These two lines with `oathtool` and `radclient` have not been run in this version; the algorithm part was checked against `rradius-core/src/totp.rs`.

On a real jump host: after you enter the password, it shows `Enter your code`, and it sends the code back as the next password together with `State`. For an SSH jump host that uses pam_radius, write one line in `/etc/pam_radius.conf` with the rradius address, the shared secret (the content of `jump-host.secret`) and a timeout; a timeout of 5 seconds or more is recommended, and with push approval it must be longer, see "Single-file examples".

> **Not verified on real hardware:** The pam_radius setting has not been run on a real jump host. It comes from the previous version of the manual.

For this chapter the second step was verified with a test client that handles challenges: the correct code gets an Access-Accept, and sending the same `State` again gets an Access-Reject. The server's authentication records:

```text
… rradius_auth: authentication protocol="radius" client="jump-host" source=127.0.0.1 user="ops1" account="ops1" chain="mfa" handler="lua" decision="challenge"
… rradius_auth: authentication protocol="radius" client="jump-host" source=127.0.0.1 user="ops1" account="ops1" chain="mfa" handler="lua" decision="accept"
… rradius_auth: authentication protocol="radius" client="jump-host" source=127.0.0.1 user="ops1" account="ops1" chain="mfa" handler="lua" decision="reject"
```

`chain="mfa"` means that the request went through the auth chain named by `jump-host`.

**VPN gateway:**

> **Not verified on real hardware:** The VPN auth chain needs a real directory server. For this chapter, `check` was used only to confirm that the configuration is valid, and the behavior when the directory cannot be reached was confirmed (see below). The actual results for groups and reply attributes have not been run against a directory.

Change the `address` of `vpn-gw` to `"127.0.0.1"` and change `jump-host` back to its original address: when both clients are `127.0.0.1`, `check` gives `radius clients: duplicate client network '127.0.0.1/32'`. If you log in while the directory does not exist, `rradius-bench` waits until it times out, and the server log says:

```text
… WARN rradius_core::ldap_handler: auth ldap handler: ldaps://dc1.corp.example.com: I/O error: failed to lookup address information: …
… WARN rradius_core::ldap_handler: auth ldap handler: ldaps://dc2.corp.example.com: I/O error: failed to lookup address information: …
… ERROR rradius_core::ldap_handler: auth ldap handler: no server reachable; rejecting
… WARN rradius_core::lua_engine: RADIUS Access-Request from client vpn-gw not answered: the authentication backend could not be used
```

When the directory cannot be reached, the server does not answer instead of returning Access-Reject: the NAS retransmits or asks the next RADIUS server, and users are not treated as having a wrong password because the directory is briefly down. When DNS cannot find the name, the second half of the error message depends on the operating system.

Notes:

- This scenario has no `[sessions]`, so `session list` gives `sessions.list is not set in rradius.toml`. To use it, add it as in the quick start (chapter 13).
- The time the user spends typing the code is between two requests and does not use up the NAS's RADIUS timeout; but a challenge is valid for only 60 seconds.

## Accounts in an external system: external-api

### Goal

A membership or billing system already has accounts and passwords. rradius does not store accounts; on every login it asks the system's API over HTTPS. If the API says yes, rradius returns the plan name given by the API as `Filter-Id`. Accounting is still written to a local table.

### Tables

Only `accounting` for accounting, written as in the quick start.

### Key settings

```toml
# rradius.toml (excerpt, other settings omitted)
[lua]
script = "accounting.lua"

[lua.http]
allowed_hosts = ["members.example.com"]
timeout_ms = 1500
max_concurrent = 64

[auth]
policy = "first_match"

[[auth.handlers]]
type = "lua"
script = "member-api.lua"
verifies_password = true
```

| Setting | Default | reload | How it is used here |
| --- | --- | --- | --- |
| `lua.http.allowed_hosts` | required | yes | The script may call only the hosts listed here. A bare host name allows only HTTPS on port 443; `host:port` allows that port; only `http://host:port` allows plain HTTP |
| `lua.http.timeout_ms` | `2000` | yes | Time limit for one call, from connecting to reading the whole response. Make it shorter than the NAS's retransmit time |
| `lua.http.max_concurrent` | `64` | yes | Number of calls in progress at the same time per host; requests over the limit fail at once |
| `auth.handlers[].verifies_password` | `false` | yes | Tells the auth chain that this script really compared the password |

- Without `[lua.http]`, there is no `http_request` in the script.
- `verifies_password = true` lets the auth chain have only this one lua handler: its accept also counts as proof of the password, so the inner PAP of EAP-TTLS is let through (chapter 11). Set it only when the script really verifies the password.
- A call to a URL that is not listed in `allowed_hosts` is refused, and the metric `rradius_script_http_requests_total{result="denied"}` goes up by one.

### Key points of the script

The API that `member-api.lua` assumes (replace it with what your system really provides):

- `POST https://members.example.com/v1/radius/login`, with the header `Authorization: Bearer <API key>` and the body `username=<account>&password=<password>` (`application/x-www-form-urlencoded`).
- 200: the account and password are correct and the user may go online; the response header `X-Plan` is the plan name (optional).
- 401, 403: the account or password is wrong, or the user may not go online.

The script has no JSON function, so ask the API to answer with status codes and headers. The key points of the script:

- Only PAP carries the plain-text password. A CHAP or MS-CHAPv2 request has no `User-Password`, so the script refuses it directly and does not ask the API.
- The account and password come from the user, so always encode them with `url_encode` before putting them in the body.
- When the API gives no answer (timeout, cannot connect, too busy), the script rejects and writes one log line; do not write the password to the log.
- `X-Plan` is put into `Filter-Id` only if it looks like a name (letters, digits, underscore, hyphen); the script also returns `Session-Timeout = 14400`.
- The API key is written in the script (`API_KEY`); let only the account that runs rradius read this file.

### How to test, and notes

Verified, with a fake API on the local machine. For the lab, replace the API in the lab directory with plain HTTP on the local machine:

```toml
# rradius.toml (changed in the lab directory)
[lua.http]
allowed_hosts = ["http://127.0.0.1:8080"]
timeout_ms = 1500
max_concurrent = 64
```

Change `API` in `member-api.lua` to `'http://127.0.0.1:8080/v1/radius/login'`, and change the `address` of `hotspot` to `"127.0.0.1"`. The fake API is written in Python. It accepts only `alice`/`alice-password` and returns `X-Plan: gold`.

> **Note:** `member_api.py` is **not a tool provided by rradius**; it is only a demonstration. It uses only the Python 3 standard library and runs once you save it as a file; do not use it as a real service.


```python
# member_api.py
import http.server, urllib.parse

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get('Content-Length', 0))
        form = urllib.parse.parse_qs(self.rfile.read(length).decode())
        ok = (self.headers.get('Authorization') == 'Bearer member-api-key-change-me'
              and form.get('username') == ['alice'] and form.get('password') == ['alice-password'])
        self.send_response(200 if ok else 401)
        if ok:
            self.send_header('X-Plan', 'gold')
        self.send_header('Content-Length', '0')
        self.end_headers()

http.server.HTTPServer(('127.0.0.1', 8080), Handler).serve_forever()
```

Start it with `python3 member_api.py` in another terminal, then start rradius. Results:

| Request | Result |
| --- | --- |
| `alice`/`alice-password`, PAP | accept, `Filter-Id = gold, Session-Timeout = 14400` |
| Wrong password | reject (the API returns 401, nothing is logged) |
| `alice`, `--load chap` | reject, the API is not asked |
| `--load session` | login accept, and both Start and Stop get an Accounting-Response |
| Stop the fake API, then log in | reject, and the log says `member API gave no answer: the connection was refused` |

The `rradius_script_http_requests_total` metric in `/metrics` is split into the results `ok`, `timeout`, `error`, `denied`, `too_large` and `busy`. This round gave `ok` 3 and `error` 1. A 401 from the API also counts as `ok`: the HTTP call succeeded, and the answer was "no".

Notes:

- When the API is slow, every waiting login holds one in-flight slot (`radius.max_auth_inflight`, default `4096`, chapter 19). `timeout_ms` must be shorter than the NAS's retransmit time, so the NAS does not send again while the script is still waiting.
- When the API is down, this script rejects everyone, and users see a wrong-password result. Watch `timeout` and `error` of `rradius_script_http_requests_total` (chapter 17) to tell that the problem is the API.

## Single-file examples

Three scripts under `rradius-server/examples/` are related to this chapter:

| File | Description | Verified |
| --- | --- | --- |
| `mac-auth.lua` | The same as the script of the wired-mab scenario | Verified (same as wired-mab) |
| `two-step-totp.lua` | The same as the script of the remote-access scenario | Verified (same as remote-access) |
| `push-approval.lua` | Push approval: after the password, the user taps "approve" on the phone | Configuration checked with `check` only |

`push-approval.lua` goes last in an `all_accept` auth chain, and runs only after the handlers before it have checked the password:

```toml
# conf.d/push.toml
[auth]
policy = "all_accept"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"

[[auth.handlers]]
type = "lua"
script = "push-approval.lua"

[lua.http]
allowed_hosts = ["push.example.com"]
timeout_ms = 7000
```

This snippet passes `check` when you put it in the base file from `config init` (chapter 04). The push service that the script assumes:

- `POST https://push.example.com/v1/push`, with the body `username=<account>`, returns 201 and the header `X-Push-Id`, which means the push has reached the user's phone.
- `GET https://push.example.com/v1/push/<id>?wait=<seconds>` waits until the user answers or the time runs out, and returns 200 with the header `X-Push-Result`: `approved`, `denied` or `pending`.

The script waits at most `WAIT_SECONDS` (30 seconds), and each request lets the service wait at most `POLL_SECONDS` (5 seconds), so `timeout_ms` must be longer than 5 seconds; the snippet uses 7000.

> **Not verified on real hardware:** `push-approval.lua` was checked with `check` only. It has not been run against a push service.

Waiting takes time, so think about three things first:

- The NAS's RADIUS timeout must be longer than `WAIT_SECONDS`. Otherwise the NAS gives up or retransmits while the user is still looking at the phone. For example, if `WAIT_SECONDS` is 30 seconds, set the timeout on the pam_radius server line to 35 seconds; on a VPN gateway, set its RADIUS server timeout.
- Every waiting login holds one slot of `max_auth_inflight`. When many people wait at the same time, raise this value.
- `lua.max_execution_ms` (default `1000`) counts only the time the script itself runs, not the time spent waiting for an HTTP response.

## Common mistakes

**Symptom:** The result of `rradius-bench` is `timeouts 1`, and the server log has `RADIUS packet from unknown client 127.0.0.1 discarded`.
**Cause:** The client address in the scenario is a real subnet, and `127.0.0.1` is not in it. `Discarded: unknown client` in `status` goes up.
**Fix:** Change the `address` of the client you want to test to `"127.0.0.1"` in the lab directory only.

**Symptom:** MAB is always rejected, and the log has no error.
**Cause:** The switch did not send `Calling-Station-Id`, or it is not the same address as `User-Name`.
**Fix:** Turn on `[log.trace]` to see the attributes of the request, and compare the two values.

**Symptom:** The VPN login gets no answer, and the log has `no server reachable; rejecting` and `not answered: the authentication backend could not be used`.
**Cause:** No directory server can be reached: the name cannot be found, the port is closed, or the certificate is not trusted.
**Fix:** Make sure the server can reach the addresses listed in `urls`; if the directory's certificate is not signed by a CA the system trusts, add `ca_file` (chapter 08).

**Symptom:** The jump host rejects a correct code.
**Cause:** The challenge was answered after more than 60 seconds; the same code was already used; the clocks of the server and the phone differ by more than one step (30 seconds); or there were 5 wrong codes in a row and the account is locked for 5 minutes.
**Fix:** Make sure the clocks on both sides are synchronized, and wait for the next code before trying again.

**Symptom:** All `external-api` logins are rejected, and the log has `script http_request refused: https://… is not in lua.http.allowed_hosts`.
**Cause:** The URL the script calls does not match `allowed_hosts`: the host, the port, or http/https is different.
**Fix:** Write `allowed_hosts` to match the URL: for HTTPS on port 443 write only the host, for other ports write `host:port`, and for plain HTTP write `http://host:port`.

**Symptom:** All CHAP or MS-CHAPv2 logins of `external-api` are rejected.
**Cause:** These two methods do not carry a plain-text password, so the script has nothing to give to the API.
**Fix:** Make the NAS use PAP; or use the inner PAP of EAP-TTLS (chapter 11), which is protected by TLS on the outside.

## Related chapters

- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [08 LDAP and Active Directory](08-ldap-and-ad.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [11 802.1X and EAP](11-eap.md)
- [13 Accounting and sessions](13-accounting.md)
- [14 Scenarios: broadband and ISP](14-isp-recipes.md)
- [Appendix A: Configuration reference](../manual.zh-TW/90-config-reference.md)
- [Appendix D: Lua API reference](../manual.zh-TW/93-lua-reference.md)
