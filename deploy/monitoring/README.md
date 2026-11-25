# Monitoring example

Alerting rules for `rradius-server` for Prometheus, and a way to get the alerts to a monitoring
system that takes SNMP traps. The manual,
[docs/manual.zh-TW/17-monitoring.md](../../docs/manual.zh-TW/17-monitoring.md#告警規則與-alertmanager), explains what each alert
means and what to do about it.

| File | What it is |
| --- | --- |
| `rradius.rules.yml` | 19 alerting rules, in five groups: availability, traffic, accounting, dependencies, operations |
| `rradius.rules.test.yml` | `promtool test rules` cases: which alerts fire for synthetic series, and which do not |
| `alertmanager.yml` | A route and a receiver that send the alerts to `snmp_notifier` |

```bash
promtool check rules rradius.rules.yml
promtool test rules rradius.rules.test.yml     # run in this directory
```

## Prometheus

```yaml
scrape_configs:
  - job_name: rradius                  # the rules look for job="rradius"
    static_configs:
      - targets: ["10.0.0.11:9812", "10.0.0.12:9812", "10.0.0.13:9812"]
rule_files:
  - rradius.rules.yml
alerting:
  alertmanagers:
    - static_configs:
        - targets: ["alertmanager:9093"]
```

`[metrics]` has no authentication: bind it to an internal address that only Prometheus reaches.
The rules for the external databases, the accounting log and the named queries only match when
the feature is configured; the metrics are not there otherwise.

## Traps

`snmp_notifier` (an independent project, <https://github.com/maxwo/snmp_notifier>) receives the
webhook of Alertmanager and sends an SNMP trap for each group of alerts:

```bash
snmp_notifier --snmp.destination=nms.example.net:162 --snmp.community=<community> \
  --trap.default-oid=<your enterprise OID>.1 \
  --trap.description-template=/etc/snmp_notifier/description-template.tpl
```

`snmp.version=V3` with the `--snmp.authentication-*` and `--snmp.private-*` options sends v3
traps. The OID `1.3.6.1.4.1.98789.1` it uses by default is not yours: set your own enterprise
number. A label `oid` on an alert (or `--trap.oid-label`) chooses the trap OID per alert.

Checked: `amtool check-config` accepts `alertmanager.yml`, and with Alertmanager,
`snmp_notifier` and `snmptrapd` in containers an alert added with `amtool alert add` reached
`snmptrapd` as a v2c trap carrying the alert name, the instance, the severity and the summary.
Not checked: v3 traps, and a real monitoring system as the receiver.
