# 06 Dictionary and attributes

> After this chapter you can:
> - Explain what a dictionary is and which dictionaries rradius ships
> - Read the dictionary file syntax and the attribute types
> - Add your own vendor-specific attributes (VSAs) and read and write them from a script
> - Set reply attributes from the database or from a script, and stay within the value and packet size limits

Prerequisites: build a configuration that starts, as in [chapter 04](04-configuration.md). This chapter uses `[log.trace]` to see the attributes that are actually sent. It is described in full in [chapter 17](../manual.zh-TW/17-monitoring.md).

In this chapter:

- [Concepts](#concepts)
- [Setting the dictionary location](#setting-the-dictionary-location)
- [Bundled dictionaries](#bundled-dictionaries)
- [Dictionary file syntax](#dictionary-file-syntax)
- [Attribute types](#attribute-types)
- [Extended attributes, TLVs and vendor formats](#extended-attributes-tlvs-and-vendor-formats)
- [Adding your own vendor attributes](#adding-your-own-vendor-attributes)
- [Setting reply attributes](#setting-reply-attributes)
- [Value and packet size limits](#value-and-packet-size-limits)
- [Restart after changing a dictionary](#restart-after-changing-a-dictionary)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Concepts

An attribute in a RADIUS packet is just "number + length + value". The **dictionary** maps the number to a name and a type: number 1 is `User-Name`, type `string`; number 27 is `Session-Timeout`, type `integer`. With a dictionary, scripts, the database and logs can write `Session-Timeout = 3600` instead of a string of bytes.

- **Standard attributes**: defined by RFCs, numbers 1 to 255, plus the extended attributes of RFC 6929.
- **Vendor-specific attributes (VSAs)**: carried inside number 26 (`Vendor-Specific`), prefixed with the vendor's IANA enterprise number. Each NAS vendor uses them to pass its own settings for VLANs, bandwidth, privilege levels and so on.
- **VALUE**: gives a name to a value of an integer attribute, for example 2 of `Service-Type` is called `Framed-User`.

The server only knows the attributes that are in the dictionary. An attribute in a request that the dictionary does not have is invisible to scripts; using a name the dictionary does not have in a reply is an error.

## Setting the dictionary location

| Key | Default | reload | Description |
| --- | --- | --- | --- |
| `dictionary` | The default dictionary path decided at build time (`/usr/share/rradius/dictionary`; if the build set `RRADIUS_PREFIX=<prefix>`, it is `<prefix>/share/rradius/dictionary`) | restart | Path of the main dictionary file; a relative path is relative to the directory of the main configuration file; may be omitted |

```toml
# rradius.toml (excerpt)
dictionary = "/usr/share/rradius/dictionary"
```

This is a top-level key. It can only be written in the main configuration file, and it must come before the first `[table]`. See [Appendix A](../manual.zh-TW/90-config-reference.md).

## Bundled dictionaries

rradius ships a full set of dictionaries. After installation they are here:

| Installation method | Location |
| --- | --- |
| deb and rpm packages and the Docker image | `/usr/share/rradius/` |
| FreeBSD port | `/usr/local/share/rradius/` |
| Source code | `share/` in the repo |

The directory contains:

- `dictionary`: the main file. The directory has 246 `dictionary.*` files, and the main file reads 232 of them with `$INCLUDE`; for the ones it does not read, `$INCLUDE` them yourself when you need them.
- `dictionary.rfc2865`, `dictionary.rfc2866` and so on: the standard attributes of each RFC.
- `dictionary.<vendor>`: the VSAs of each vendor, for example `dictionary.cisco`, `dictionary.mikrotik`, `dictionary.microsoft`.
- `NOTICE`, `LICENSE`: source and license.

These dictionaries are unmodified copies of the `share/` directory of the FreeRADIUS project's v3.2.x branch. They are licensed under CC-BY 4.0 (not the license of rradius itself). The full text is in `LICENSE` in the same directory, and the source and copyright holders are in `NOTICE`. When you distribute them, include both files.

> **Note:** Do not edit the bundled files directly: they belong to the package and are replaced on upgrade. To add attributes, write a separate file as described in [Adding your own vendor attributes](#adding-your-own-vendor-attributes), and use your own main file to read both.

## Dictionary file syntax

A dictionary is plain text with one directive per line; fields are separated by spaces or tabs. From `#` to the end of the line is a comment, and it can appear anywhere on a line.

| Directive | Syntax | Description |
| --- | --- | --- |
| `ATTRIBUTE` | `ATTRIBUTE <name> <number> <type> [options]` | Defines an attribute |
| `VALUE` | `VALUE <attribute> <value name> <number>` | Gives a name to one value of an integer attribute |
| `VENDOR` | `VENDOR <name> <enterprise number> [format=t,l[,c]]` | Defines a vendor |
| `BEGIN-VENDOR`, `END-VENDOR` | `BEGIN-VENDOR <name> [format=<attribute>]` | The `ATTRIBUTE` lines in between belong to this vendor |
| `BEGIN-TLV`, `END-TLV` | `BEGIN-TLV <attribute>` | The `ATTRIBUTE` lines in between are sub-attributes of this `tlv` attribute |
| `$INCLUDE` | `$INCLUDE <file>` | Reads another dictionary file |

- Numbers can be decimal or hexadecimal with a `0x` prefix; nested attribute numbers are written with dots (for example `241.1`).
- A relative path in `$INCLUDE` is relative to the directory of **the file that contains the line** (unlike the rule for configuration files). A loop of files that `$INCLUDE` each other is detected.
- When `VALUE` refers to an attribute that is not defined yet, that line is skipped and it is not an error.
- The options of `ATTRIBUTE` are separated by commas:
  - `has_tag`: the attribute carries a tag (used by the tunnel attributes of RFC 2868).
  - `encrypt=1`: encrypted the way User-Password is.
  - `encrypt=2`: encrypted with the salted method of Tunnel-Password.
  - `encrypt=3`: can be read, but `set_attr` cannot send it.
  - Any other value is an error.
- `format=t,l` of `VENDOR`: t is how many bytes the vendor attribute number takes (1, 2 or 4), and l is how many bytes the length field takes (0, 1 or 2). The default is `1,1`. `c` (the continuation bit) can only be used with `1,1`.
- An error names the file and line; when the error is in an `$INCLUDE`d file, the chain is listed level by level, for example `dictionary:3: dictionary.example:14: unsupported directive 'FLAGS'`.

## Attribute types

The type decides what `get_attr` returns and what `set_attr` expects:

| Type | Length | `get_attr` returns | `set_attr` accepts |
| --- | --- | --- | --- |
| `integer`, `date` | 4 | A number | A number, a string of digits, or a `VALUE` name of this attribute |
| `byte`, `short`, `integer64` | 1, 2, 8 | A number | Same; out of range is an error |
| `signed` | 4 | A number (can be negative) | A number or a string of digits |
| `ipaddr` | 4 | `"192.0.2.1"` | A string with an IPv4 address |
| `ipv6addr` | 16 | `"2001:db8::1"` | A string with an IPv6 address |
| `ipv6prefix`, `ipv4prefix` | variable | `"2001:db8:1::/64"` | A string with a prefix; the host part must be 0 |
| `ifid` | 8 | `"0011:2233:4455:6677"` | A string in the same format |
| `ether` | 6 | `"00:11:22:33:44:55"` | A string with a MAC address |
| `combo-ip` | 4 or 16 | A string with an IPv4 or IPv6 address | A string with an IPv4 or IPv6 address |
| `string`, `text`, `octets` | variable | The byte string as is | A string, sent as is; for a number, its decimal text is sent |
| `octets[N]` | N | The byte string as is | A string of exactly N bytes |

- For integer-type attributes, `get_attr` always returns a **number**, not a `VALUE` name: to test `Service-Type`, write `get_attr('Service-Type') == 2`. `set_attr` accepts both: `set_attr('Service-Type', 'Framed-User')` and `set_attr('Service-Type', 2)` are the same.
- For a value whose length does not match its type (for example a 5-byte `integer`), `get_attr` returns the bytes as they are.
- Container types such as `tlv`, `evs` and `vsa` cannot be set with `set_attr` themselves; you set the attributes under them.
- For an attribute with a tag, write the tag as `name:tag`, with the tag from 1 to 31, for example `Tunnel-Type:1`. Only attributes marked `has_tag` in the dictionary can be written this way.

## Extended attributes, TLVs and vendor formats

Standard attributes have only 255 numbers, and each vendor formats its attributes differently. The dictionary describes them in the ways below. Scripts do not need to know how they are laid out in the packet; they always read and write by attribute name.

**Extended attributes (RFC 6929).** Numbers 241 to 246 are containers, of type `extended` (241 to 244) or `long-extended` (245, 246). The real attributes are written as "container number.sub-number":

```text
# bundled dictionary.rfc* (excerpt)
ATTRIBUTE	Extended-Attribute-1			241	extended
ATTRIBUTE	Frag-Status				241.1	integer
ATTRIBUTE	Operator-NAS-Identifier			241.8	octets
```

- When the value of a `long-extended` attribute is longer than one attribute can hold, it is cut into pieces of 251 bytes when sent and joined again when received.
- An `evs` (Extended-Vendor-Specific) under a container carries vendor attributes; define them with `BEGIN-VENDOR <vendor> format=<evs attribute>`.

**TLV.** An attribute of type `tlv` contains sub-attributes. Write a sub-attribute as "parent number.sub-number", or between `BEGIN-TLV <parent attribute>` and `END-TLV`. For example, the bundled WiMAX dictionary:

```text
# bundled dictionary.wimax (excerpt)
ATTRIBUTE	WiMAX-Capability			1	tlv
ATTRIBUTE	WiMAX-Release				1.1	string
ATTRIBUTE	WiMAX-Accounting-Capabilities		1.2	byte
```

- A script sets the sub-attribute directly: `set_attr('WiMAX-Release', '2.1')`.
- When you set sub-attributes of the same TLV one after another, they are packed into the same TLV; a new one is started only when they do not fit.
- The value of a sub-attribute must also fit in the TLV and attribute around it. When it does not, `set_attr` fails: `value of 'WiMAX-Release' is too long`.
- When a received TLV is malformed, the whole attribute is skipped; sub-attributes that are not recognized are skipped.

**Vendor formats.** For most vendors the attribute number and length take 1 byte each, which is the default `format=1,1`. Some vendors differ, and the `VENDOR` line says so:

```text
# bundled dictionaries (excerpt)
VENDOR		Lucent				4846	format=2,1
VENDOR		WiMAX				24757	format=1,1,c
```

- `format=2,1`: the attribute number takes 2 bytes.
- `format=1,1,c`: has a continuation bit; a value too long for one Vendor-Specific is sent in several and joined again when received.

**Verify:** have a script set all of these kinds of attributes, then look at the response with trace:

```lua
-- auth.lua (excerpt)
set_attr('WiMAX-Release', '2.1')
set_attr('WiMAX-Accounting-Capabilities', 1)
set_attr('Operator-NAS-Identifier', 'nas-01')
set_attr('Lucent-PPP-Circuit-Name', 'circuit-7')
set_attr('WiMAX-AAA-Session-Id', string.rep('s', 300))
return 'accept'
```

```text
… INFO rradius_trace: response client="localhost" … code=2 attributes="Message-Authenticator = <hidden>, WiMAX-Release = 2.1, WiMAX-Accounting-Capabilities = 1, Operator-NAS-Identifier = nas-01, Lucent-PPP-Circuit-Name = circuit-7, WiMAX-AAA-Session-Id = sss…"
```

**What it looks like:** the login is accepted. `WiMAX-AAA-Session-Id` is 300 bytes, more than the 253 bytes of an ordinary attribute, and it is sent in pieces using the WiMAX continuation bit; with a vendor that has no continuation bit, the same value gives `value of '…' is too long`. Trace shows only the attributes and values, not how they are split in the packet.

> **Not verified on real hardware:** whether a NAS joins the split packets correctly depends on the NAS implementation. This chapter did not verify it with a real NAS.

## Adding your own vendor attributes

**Goal:** a NAS sends a VSA of its own that says which group the user belongs to. The server decides the privilege level from it and sends it back to the NAS. The bundled dictionaries do not have this vendor.

**Setup:** write your own main dictionary file that reads the bundled dictionary and then your own file. The vendor and the number 32473 below are an enterprise number that IANA reserves for documentation examples; replace them with the number and attribute definitions of your NAS vendor (usually in the vendor's documentation or in the dictionary file it ships).

```text
# dictionary: the main dictionary file of this server
$INCLUDE /usr/share/rradius/dictionary
$INCLUDE dictionary.example
```

```text
# dictionary.example: your own vendor attributes
VENDOR		Example-Corp	32473

BEGIN-VENDOR	Example-Corp
ATTRIBUTE	Example-Group		1	string
ATTRIBUTE	Example-Level		2	integer
ATTRIBUTE	Example-Gateway		3	ipaddr

VALUE	Example-Level		Guest			1
VALUE	Example-Level		Staff			2
VALUE	Example-Level		Admin			3
END-VENDOR	Example-Corp
```

Put both files next to the main configuration file, and point `dictionary` to your own main file:

```toml
# rradius.toml (excerpt)
dictionary = "dictionary"
```

The script reads `Example-Group` from the request, and replies with `Example-Level`, `Example-Gateway` and the standard `Session-Timeout`:

```lua
-- auth.lua
if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end
if get_attr('User-Name') ~= 'alice' or get_attr('User-Password') ~= 'alice-password' then
  return 'reject'
end
if get_attr('Example-Group') ~= 'staff' then
  return 'reject'
end
set_attr('Example-Level', 'Staff')
set_attr('Example-Gateway', '192.0.2.254')
set_attr('Session-Timeout', 3600)
return 'accept'
```

To see the attributes that are actually sent, turn on trace for this user for a while:

```toml
# conf.d/90-trace.toml
[log.trace]
users = ["alice"]
minutes = 30
```

**Verify:** run `check` first, then start the server. `rradius-bench` does not know dictionary names, so use `--attr 26=hex:…` to send a VSA directly: `00007ed9` is the enterprise number 32473, `01` is the attribute number, `07` is the length, and the rest is the ASCII of `staff`.

```console
$ rradiusd check --config rradius.toml
configuration ok
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 \
    --attr 26=hex:00007ed901077374616666
…
  pap: 1844/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 4417/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

**What it looks like:** the request with `Example-Group = staff` is accepted, and the one without it is rejected. The trace log:

```text
… INFO rradius_trace: request client="localhost" source=127.0.0.1 user="alice" code=1 attributes="Message-Authenticator = <hidden>, User-Name = alice, User-Password = <hidden>, NAS-IP-Address = 192.0.2.1, NAS-Identifier = rradius-bench, Example-Group = staff"
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Example-Level = 2, Example-Gateway = 192.0.2.254, Session-Timeout = 3600"
```

- The VSA in the request is shown with its dictionary name.
- `'Staff'` written in the reply is sent as 2 according to `VALUE`, and trace shows the number.

When you are done, remember to delete `90-trace.toml` and reload: trace writes personal data to the log.

> **Note:** trace shows attributes with a tag as raw values. For example, `Tunnel-Type:1` set to `VLAN` (13) is shown as `Tunnel-Type = 16777229`, which is tag 1 in the highest byte and the value 13 in the low three bytes; a string with a tag is shown in hexadecimal, for example `Tunnel-Private-Group-Id = 0x01313030` (tag 1 plus `100`). Encrypted attributes are always shown as `<hidden>`.

### Encrypted attributes

Some VSAs carry keys or passwords. The dictionary marks them with `encrypt=1` or `encrypt=2` after the type. `set_attr` takes the plain text as usual, and the server encrypts it with the shared secret of this client before sending it:

```text
# dictionary.example (add before END-VENDOR)
ATTRIBUTE	Example-Key		5	string	encrypt=1
```

The script writes `set_attr('Example-Key', 'session-key-0123')`, the login is accepted, and trace shows `Example-Key = <hidden>`, so the plain text is not written to the log.

## Setting reply attributes

Reply attributes (the attributes sent to the NAS in an Accept) have two sources, and you can use both together:

1. **Scripts**: `set_attr(name, value)`, as in the previous section. For how to write scripts, see [chapter 10](../manual.zh-TW/10-lua-policies.md).
2. **The database query of the auth chain**: the `reply_query` of the `sql` handler returns rows with two columns, "attribute name, value", and each row becomes one reply attribute ([chapter 07](../manual.zh-TW/07-users-and-chains.md)).

The values that `reply_query` returns are always text, converted according to the dictionary type: an integer attribute can be written as a number or as a `VALUE` name, and an attribute with a tag is written `name:tag`. For example, with this data:

```sql
-- migrations/1_users.sql (excerpt)
INSERT INTO user_reply VALUES
  ('alice', 'Session-Timeout', '3600'),
  ('alice', 'Example-Level', 'Staff'),
  ('alice', 'Tunnel-Type:1', 'VLAN'),
  ('alice', 'Tunnel-Medium-Type:1', 'IEEE-802'),
  ('alice', 'Tunnel-Private-Group-Id:1', '100'),
  ('alice', 'Framed-IP-Address', '192.0.2.10'),
  ('bad', 'Example-Level', 'Boss');
```

With `reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"`, `alice` gets these six attributes when she logs in (`Example-Level` is sent as 2).

**When a value does not fit the dictionary, the whole login becomes a reject.** `Boss` for `bad` is not a `VALUE` of `Example-Level`, so the result is an Access-Reject and one error line in the log:

```text
… ERROR rradius_core::radius: RADIUS response attribute error: 'Boss' is not a value of 'Example-Level'
```

The server prefers to reject rather than send an Accept that is missing an attribute: an accept without the VLAN or bandwidth limit could let a user into a network they should not be in.

When `set_attr` in a script hits an error, it raises a Lua error. If you do not catch it with `pcall`, the script stops there and the login is rejected:

```text
… ERROR … Lua script error (radius/authentication): runtime error: unknown attribute 'Example-Nope'…
```

Common `set_attr` error messages:

| Call | Message |
| --- | --- |
| `set_attr('Example-Nope', 'x')` | `unknown attribute 'Example-Nope'` |
| `set_attr('Example-Level', 'Boss')` | `'Boss' is not a value of 'Example-Level'` |
| `set_attr('Example-Gateway', 'gateway')` | `'Example-Gateway' needs an IPv4 address` |
| `set_attr('Filter-Id:1', 'x')` | `attribute 'Filter-Id' takes no tag` |
| `set_attr('Framed-IPv6-Prefix', '2001:db8::1/64')` | `'2001:db8::1/64' is not a valid ipv6prefix for 'Framed-IPv6-Prefix'` |

## Value and packet size limits

| Limit | When exceeded |
| --- | --- |
| The value of an ordinary attribute is at most 253 bytes | `set_attr` error `value of 'Reply-Message' is too long` |
| The value of `encrypt=1` is 1 to 128 bytes and cannot carry a tag | `set_attr` error |
| The value of `encrypt=2` is at most 239 bytes | `set_attr` error, for example `'WiMAX-MSK': value is longer than 239 octets` |
| The value of a TLV sub-attribute must fit in the TLV and attribute around it | `set_attr` error `value of '…' is too long` |
| The whole response packet is at most 4096 bytes | An Access-Reject with no attributes is sent instead, and one error line is logged |

- `long-extended` attributes, and vendor attributes of a vendor whose dictionary marks the continuation bit (`format=1,1,c`), are not subject to the 253-byte limit and are split automatically (see [Extended attributes, TLVs and vendor formats](#extended-attributes-tlvs-and-vendor-formats)).
- An example of a response that is too large is an Accept that carries 20 `Reply-Message` attributes of 250 bytes each:

  ```text
  … ERROR rradius_core::radius: RADIUS response to localhost is longer than 4096 octets
  ```

  The NAS receives a reject. Split the many attributes up, or use shorter values.

## Restart after changing a dictionary

The dictionary is read only once, when the server starts.

- Change the **path** of `dictionary` and reload: the reload still succeeds, and a `WARN` in the log lists `these need a restart to apply: dictionary`.
- Change only the **contents** of a dictionary file (for example, add an `Example-Room` to `dictionary.example`) and reload: after `configuration reloaded`, the log has one more `WARN` line: `dictionary files changed since the server started; restart to apply: <file path>` (at most five files are listed, and the rest are written as `and N more`; it is written on every reload as long as a file still differs from what it was at startup). The server still uses the old dictionary, and a script that uses the new attribute gets `unknown attribute 'Example-Room'`. It takes effect after a restart.

> **Warning:** after changing a dictionary, always restart the server; do not just send SIGHUP. First use `check` to confirm that the new dictionary can be read: `check` reads the dictionary from the files every time.

## Common mistakes

**Symptom:** `check` prints `dictionary:3: dictionary.example:14: unsupported directive 'FLAGS'`.
**Cause:** the dictionary uses a directive that rradius does not support. The message lists the levels: it was read through the `$INCLUDE` on line 3 of the main file, and the problem is on line 14 of `dictionary.example`.
**Fix:** delete or rewrite that line; for the supported directives see [Dictionary file syntax](#dictionary-file-syntax).

**Symptom:** `… unknown vendor 'Nobody'`.
**Cause:** `BEGIN-VENDOR` or `ATTRIBUTE` uses a vendor that has no `VENDOR` definition yet.
**Fix:** add the `VENDOR` line before it, or correct the name.

**Symptom:** `… cannot include 'dictionary.missing': No such file or directory (os error 2)`.
**Cause:** the file in `$INCLUDE` does not exist. A relative path is relative to the directory of the dictionary file that contains the `$INCLUDE`.
**Fix:** correct the path, or use an absolute path.

**Symptom:** `… unknown encryption 'encrypt=9' in: ATTRIBUTE Example-Secret 9 string encrypt=9`.
**Cause:** `encrypt=` can only be 1, 2 or 3.
**Fix:** correct it according to the definition of the NAS vendor.

**Symptom:** `… invalid vendor format 'format=3,1'`.
**Cause:** in `format=`, the attribute number length can only be 1, 2 or 4, and the length field can only be 0, 1 or 2.
**Fix:** correct it according to the definition of the vendor.

**Symptom:** `… ATTRIBUTE needs a name, number and type: ATTRIBUTE Short 9`, `… END-TLV without matching BEGIN-TLV`, `… include cycle: 'dictionary' is already being loaded`.
**Cause:** a field is missing, `BEGIN-TLV`/`END-TLV` are not paired, or dictionaries `$INCLUDE` each other.
**Fix:** correct it according to the file and line in the message.

**Symptom:** the script test `get_attr('Service-Type') == 'Framed-User'` is never true.
**Cause:** for an integer attribute, `get_attr` returns a number, not a `VALUE` name.
**Fix:** compare with the number: `get_attr('Service-Type') == 2`.

**Symptom:** a user is rejected, and the log has `RADIUS response attribute error: …`.
**Cause:** a reply attribute in the database has a value or name that does not fit the dictionary.
**Fix:** correct the data of that row according to the message; the name must match the dictionary exactly (including case).

**Symptom:** you added a new attribute and the reload succeeded, but the script gets `unknown attribute`.
**Cause:** the contents of the dictionary are read only at startup.
**Fix:** restart the server.

## Related chapters

- [04 Configuration file basics](04-configuration.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [07 User accounts and auth chains](../manual.zh-TW/07-users-and-chains.md)
- [10 Writing policies in Lua](../manual.zh-TW/10-lua-policies.md)
- [14 Scenarios: broadband and ISP](../manual.zh-TW/14-isp-recipes.md)
- [15 Scenarios: network access for organizations](../manual.zh-TW/15-access-recipes.md)
- [17 Monitoring, status and logs](../manual.zh-TW/17-monitoring.md)
- [Appendix A: Configuration reference](../manual.zh-TW/90-config-reference.md)
- [Appendix D: Lua API reference](../manual.zh-TW/93-lua-reference.md)
