# rradius

RADIUS 與 TACACS+ 的 AAA 伺服器，用 Rust 寫成。認證和計費的邏輯用 Luau 腳本與 SQL 寫，資料放在內建的 hiqlite 叢集（以 Raft 複製的 SQLite），不需要另外架資料庫。

- **RADIUS：** 認證、計費、Status-Server；PAP、CHAP、MS-CHAPv2，EAP-TLS、PEAP-MSCHAPv2、EAP-TTLS；延伸屬性與廠商屬性。
- **TACACS+：** 認證、授權（依設備分組的允許與拒絕規則）、計費。
- **帳號來源：** 內建資料表（多種密碼雜湊格式）、LDAP／Active Directory、HTTP API，可以串接。
- **叢集：** 三台以上組成叢集，任何一台停機不中斷服務；設定檔改了送 SIGHUP 重讀。
- **維運：** Prometheus 指標、結構化記錄、`--check` 檢查設定、管理指令。

## 建置與啟動

需要 Rust（stable）。

```bash
cargo build --release
cd rradius-server
../target/release/rradius-server --config examples/scenarios/quick-start/rradius.toml
```

也可以用 Docker：先執行 `docker/init-secrets.sh`，再 `docker compose up --build`（單機）或用 `compose.cluster.yaml`（三台），見[使用文件 1.1 節](docs/usage.md#11-docker)。發行檔用 `packaging/release.sh` 產生，見 [`packaging/README.md`](packaging/README.md)。

## 文件

- [使用手冊](docs/manual/README.md)：依部署情境一步一步說明，每個情境都有可以直接啟動的設定檔，放在 [`rradius-server/examples/scenarios/`](rradius-server/examples/scenarios/)，並有自動測試。
- [使用文件](docs/usage.md)：所有設定與功能的參考。

## 專案結構

| 目錄 | 內容 |
| --- | --- |
| `rradius-core/` | 伺服器的函式庫：協定、認證串接、Lua 腳本、叢集、設定 |
| `rradius-server/` | 執行檔 `rradius-server`、範例與測試 |
| `rradius-bench/` | 負載產生工具 `rradius-bench` |
| `share/` | RADIUS 字典 |

測試：`cargo test --workspace --locked`。

## 企業版

RadSec、依 realm 轉送與漫遊、Disconnect／CoA、從資料表管理 NAS、每人一把 Wi-Fi 密碼、EAP-AKA'、OCSP、EAP 快速重新連線、TACACS+ over TLS 由另外提供的 rradius-enterprise 支援。rradius-server 遇到這些設定時會直接說明需要 rradius-enterprise。

## 授權

BSD 2-Clause，見 [`LICENSE`](LICENSE)。`share/` 的 RADIUS 字典另有授權，見 [`share/LICENSE`](share/LICENSE) 與 [`share/NOTICE`](share/NOTICE)。
