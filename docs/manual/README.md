# rradius User Manual

This manual is organized by topic. It starts with getting to know the program, installing it and the first login. It then goes through the features one chapter at a time, and ends with reference appendices for settings, commands and metrics. Common deployment scenarios (enterprise Wi-Fi, wired MAC authentication, remote access, broadband and ISP) are written up as scenario chapters. Each scenario has a configuration you can start as it is, in [`rradius-server/examples/scenarios/`](../../rradius-server/examples/scenarios/), and an automated test (`rradius-server/tests/scenarios_test.rs`) starts them the way the manual describes.

You can also ask the program how a command works: `rradiusd help`, `rradiusd schema` and `rradiusd config show`.

## First steps

| Chapter | Contents |
| --- | --- |
| [01 Getting to know rradius](01-overview.md) | What it is, how a request travels, how the configuration file is laid out, where the data lives, how to test with external tools |
| [02 Installation](02-install.md) | Packages, containers, Kubernetes, Synology NAS, and building from source |
| [03 First start and first login](03-quick-start.md) | A single node, accounts in a table, your first Access-Accept within ten minutes |

## What I want to do, and which chapter to read

| I want to… | Chapter |
| --- | --- |
| Write the configuration file, and know which changes need a restart | [04](04-configuration.md); every key is described in [Appendix A](../manual.zh-TW/90-config-reference.md) |
| Register a NAS (a RADIUS client), or replace a shared secret | [05](05-radius-clients.md), [18](../manual.zh-TW/18-operations.md#更換密碼) |
| Add attributes or change the dictionary | [06](06-dictionary.md); the functions Lua can use are in [Appendix D](../manual.zh-TW/93-lua-reference.md) |
| Keep accounts in a table, choose password hashes, set up auth chains; move accounts from an old system | [07](../manual.zh-TW/07-users-and-chains.md) |
| Use accounts in a company directory (LDAP, Active Directory) | [08](../manual.zh-TW/08-ldap-and-ad.md) |
| Use accounts in another database or an HTTP API, and watch named queries with metrics | [09](../manual.zh-TW/09-external-databases.md) |
| Write policies in Lua | [10](../manual.zh-TW/10-lua-policies.md) |
| Enterprise Wi-Fi: 802.1X, certificates, revocation | [11](../manual.zh-TW/11-eap.md), [15](../manual.zh-TW/15-access-recipes.md) |
| Put devices such as printers and IP phones on the wired network (MAB) | [15](../manual.zh-TW/15-access-recipes.md) |
| Log in to a VPN with the company directory, or add a one-time code | [15](../manual.zh-TW/15-access-recipes.md) |
| Control who can log in to routers and which commands they can run (TACACS+) | [12](../manual.zh-TW/12-tacacs.md) |
| Accounting and sessions | [13](../manual.zh-TW/13-accounting.md) |
| Broadband and ISP: address pools, prefix pools, usage control, line identity, fixed addresses, rate attributes, dual stack | [14](../manual.zh-TW/14-isp-recipes.md) |
| Keep service running when any one machine stops; span data centers; Kubernetes | [16](../manual.zh-TW/16-cluster.md) |
| Check status, metrics and logs, and get alerts (including SNMP traps) | [17](../manual.zh-TW/17-monitoring.md); the list of metrics is in [Appendix C](../manual.zh-TW/92-metrics-reference.md) |
| Day-to-day work after go-live: changes, backups, upgrades, purging old data | [18](../manual.zh-TW/18-operations.md) |
| Performance tuning and load testing | [19](../manual.zh-TW/19-performance.md) |
| Security hardening and pre-launch checks | [20](../manual.zh-TW/20-security.md) |
| Something went wrong: find the cause from the symptom | [21](../manual.zh-TW/21-troubleshooting.md) |

## Reference appendices

| Appendix | Contents |
| --- | --- |
| [A Configuration reference](../manual.zh-TW/90-config-reference.md) | The default value of every setting, and whether it can be reloaded |
| [B Command reference](../manual.zh-TW/91-command-reference.md) | Every command and flag, and the exit codes |
| [C Metrics reference](../manual.zh-TW/92-metrics-reference.md) | Every Prometheus metric and label |
| [D Lua API reference](../manual.zh-TW/93-lua-reference.md) | The functions a script can call |
| [E Glossary and RFC cross-reference](../manual.zh-TW/94-glossary.md) | Terms and the RFCs they correspond to |

## Conventions used in the examples

The way the examples work (relative paths, running one scenario at a time, the executable path when you use a release file, the demo passwords) is described at [the start of chapter 14](../manual.zh-TW/14-isp-recipes.md#這些情境共同的做法) and in [chapter 03](03-quick-start.md#prepare-a-working-directory).

Enterprise features (RadSec, proxying, CoA forwarding, DPSK, dynamic clients and so on) are not in this manual. See the enterprise manual.
