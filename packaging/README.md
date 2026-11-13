# rradius

RADIUS、TACACS+ 的 AAA 伺服器，認證和計費邏輯用 Luau 腳本與 SQL 寫成，資料放在內建的 hiqlite 叢集。

## 內容

| 路徑 | 內容 |
| --- | --- |
| `bin/rradius-server` | 伺服器，也是管理指令（`check`、`user`） |
| `share/` | RADIUS 字典，設定檔的 `dictionary` 指向 `share/dictionary` |
| `rradius-server/examples/scenarios/` | 使用手冊各情境的設定、資料表 migration 和 Lua 腳本，可以直接啟動 |
| `rradius-server/examples/` | 其他範例設定和腳本 |
| `docs/manual/` | 使用手冊，從 `docs/manual/README.md` 開始讀 |
| `docs/usage.md` | 所有設定和功能的參考 |
| `VERSION` | 版本和建置它的 commit；Linux 版另有一行 `glibc <版本>`，是它需要的最低 glibc 版本 |

目錄結構和原始碼 repo 相同，所以範例設定和文件裡的相對路徑都照樣成立。

RadSec、依 realm 轉送、中斷連線與變更權限（CoA）、動態 client、EAP-AKA'、OCSP 這些企業功能需要 rradius-enterprise，它的壓縮檔另外附企業功能的文件和情境。

## 快速開始

```bash
cd rradius-server
../bin/rradius-server check --config examples/scenarios/quick-start/rradius.toml
../bin/rradius-server server --config examples/scenarios/quick-start/rradius.toml
```

使用手冊的指令以原始碼 repo 的 `rradius-server/` 目錄為準，寫成 `./target/release/rradius-server`；用這個壓縮檔時，在 `rradius-server/` 目錄下改成 `../bin/rradius-server`，其他部分都一樣。見 `docs/manual/02-quick-start.md`。

- 範例的密碼檔（`*.secret`）都是示範值，正式環境一定要換掉。
- 範例啟動後會在範例目錄產生 `data/` 等目錄，要從頭再來就停止伺服器後刪掉。
- 組叢集不需要另外安裝 CA 憑證（`ca-certificates`）。要用 LDAP over TLS 或腳本的 `http_request` 連 HTTPS 時，主機要有系統的 CA 憑證，或在設定裡指定 `ca_file`。
- 正式部署時，把字典放在固定的位置（例如 `/usr/share/rradius`），設定檔的 `dictionary` 指向那裡。
