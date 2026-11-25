# Synology example

One `rradius-server` node as a Container Manager project on a Synology NAS. The
manual, [docs/manual.zh-TW/02-install.md](../../docs/manual.zh-TW/02-install.md#synology-nas) (the Synology NAS section), explains the steps.

| File | What it is |
| --- | --- |
| `compose.yaml` | The project: the image, the two RADIUS ports, the config and secrets folders, the data volume |

The configuration is `docker/config/` of this repository, copied to the NAS; the secrets are
made by `docker/init-secrets.sh`.
