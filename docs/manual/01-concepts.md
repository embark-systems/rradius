# 1. 基本概念

## rradius 做什麼

rradius 是 AAA 伺服器：網路設備（NAS：無線控制器、交換器、VPN 閘道、BNG、路由器）在有人要上網或登入時來問它三件事：

- **認證（Authentication）**：這個人是誰，密碼或憑證對不對。
- **授權（Authorization）**：通過之後給他什麼，例如 VLAN、速率、權限等級、能下哪些指令。
- **計費（Accounting）**：他什麼時候上線、下線，用了多少流量。

設備和 rradius 之間用兩種協定：

| 協定 | 用在哪裡 | 埠 |
| --- | --- | --- |
| RADIUS（UDP） | 使用者上網：Wi-Fi、有線 802.1X、VPN、PPPoE | 認證 1812、計費 1813 |
| TACACS+（TCP） | 管理員登入網路設備（第 6 章） | 49 |

RadSec（2083 埠）、TACACS+ over TLS（300 埠），以及接收其他系統送來的「踢人」「改權限」請求（Disconnect、CoA，3799 埠）需要 rradius-enterprise，見企業版手冊。

## 一個請求怎麼被處理

```
NAS ──Access-Request──► rradius
                          │
                          ├─ 來源不是設定的 client ─────────► 丟棄，不回應
                          ├─ Status-Server ─────────────────► 直接回答（status_server = true 時）
                          ├─ 帶 EAP-Message ────────────────► EAP（TLS、PEAP、TTLS）完成後
                          │                                    把名稱交給下面的串接（第 3、8 章）
                          ├─ client 有 auth = "名稱" ───────► 那條具名串接
                          ├─ 有 [[auth.handlers]] ──────────► 預設串接
                          └─ 都沒有 ────────────────────────► [lua].script
```

- **認證串接（`[auth]`）** 把幾個處理器（`sql`、`ldap`、`lua`）依序執行。`first_match` 由第一個有意見的處理器決定；`all_accept` 要每個處理器都同意，常用來「先比對密碼，再檢查政策」。見 [usage 第 3 節](../usage.md#3-認證串接-auth)。
- **最上層腳本（`[lua].script`）** 一定要有。它處理**所有計費**和 **TACACS+ 的授權**，也處理沒有串接的認證。所以有串接的情境，最上層腳本通常只寫計費。
- **計費（Accounting-Request）** 不經過串接，一律交給最上層腳本。
- **不回應是刻意的。** 來源不認得、簽章不對、太忙時，rradius 都不回應，讓 NAS 重送或改用備援。「沒有回應」怎麼查，見[第 12 章](12-troubleshooting.md#nas-送了請求完全沒有回應)。

## 設定檔的結構

一個設定檔就是一台伺服器。常見的區段：

| 區段 | 內容 | 必要 |
| --- | --- | --- |
| `dictionary` | RADIUS 字典檔，repo 的 `share/dictionary` | 是 |
| `[lua]` | 最上層腳本和腳本的時限 | 是 |
| `[cluster]` | 資料目錄、叢集密碼、節點、migration | 是 |
| `[auth]` | 認證串接 | 否 |
| `[radius]`、`[[radius.clients]]` | RADIUS 的埠和可以來問的設備 | 用 RADIUS 時 |
| `[radius.eap]` | 802.1X 用的 EAP 和憑證（第 3、8 章） | 用 EAP 時 |
| `[radius.accounting_spool]` | 資料庫寫不進去時暫存計費（[usage 2.11 節](../usage.md#211-計費暫存-radiusaccounting_spool)） | 否 |
| `[tacacs]`、`[[tacacs.clients]]` | TACACS+（第 6 章） | 用 TACACS+ 時 |
| `[lua.http]` | 讓腳本呼叫 HTTP API（第 10 章） | 否 |
| `[accounts]` | `rradius-server user` 指令執行的 SQL（第 11 章） | 否 |
| `[metrics]` | Prometheus 指標 | 否 |
| `[log]` | 記錄格式、等級、追蹤特定使用者 | 否 |

RadSec（`[radius.tls]`）、每人一把 Wi-Fi 密碼（`[radius.dpsk]`）、從資料表查 NAS（`[radius.dynamic_clients]`）、接收斷線請求（`[radius.coa]`）、TACACS+ over TLS（`[tacacs.tls]`）和依 realm 轉送（`[proxy]`）需要 rradius-enterprise，見企業版手冊。

`[radius]` 和 `[tacacs]` 至少要有一個。每個區段都拒絕不認得的欄位，所以欄位拼錯時啟動就會失敗並指出名稱。

- **相對路徑以設定檔所在目錄為準，** 和從哪裡啟動無關。
- **密碼一律放檔案**（`secret_file`），不要寫在設定檔裡。檔案結尾的換行會去掉。
- **設定錯誤在啟動時就失敗**（狀態碼 2），訊息會指出欄位。上線前用 `check` 檢查：

  ```bash
  rradius-server check --config /etc/rradius/rradius.toml
  ```

  它檢查設定、字典、憑證、migration 檔名和腳本語法，但不連資料庫和 LDAP，所以 SQL 寫錯、LDAP 密碼錯誤要啟動後才知道。

## client：誰可以來問

`[[radius.clients]]` 列出可以送請求的設備，每個有位址（單一 IP 或網段）和共用密碼。

```toml
[[radius.clients]]
name = "wlc"                 # 記錄、指標和腳本裡的 client 都用這個名稱
address = "10.30.0.10"       # 或 "10.30.0.0/24"
secret_file = "wlc.secret"
```

- **不在名單裡的來源完全不回應。** 這是新手最常遇到的「伺服器沒反應」，看記錄裡的 `unknown client`。
- **最長前綴優先：** 同時有 `10.0.0.0/8` 和 `10.30.0.10` 時，後者比較精確，用後者。
- **共用密碼要夠長，** 少於 16 個字元會有警告，建議 24 個以上的隨機字元。每台設備可以不同。
- **預設要求 Message-Authenticator。** 舊設備不送時會被丟棄，要在那個 client 設 `require_message_authenticator = "no"` 或 `"auto"`，見 [usage 2.6 節](../usage.md#26-radius-安全設定)。

## 資料表和 migration

rradius 內建資料庫（hiqlite：SQLite 加上 Raft 複寫），單機和叢集用法相同。帳號、回覆屬性、計費這些資料表由你自己定義，寫成 SQL 檔放在 `migrations_dir`，啟動時依編號套用：

```
migrations/
  1_schema.sql        建立資料表
  2_demo_users.sql    示範資料
```

- **檔名是 `<編號>_<名稱>.sql`，** 編號從 1 開始連續。
- **套用過的檔案不能改，** 改了會啟動失敗。要改結構或加資料，就新增下一個編號的檔案，再重新啟動（叢集要逐台重啟）。見 [usage 2.5 節](../usage.md#25-資料表-migration)。
- **腳本讀寫資料：** `sql_query` 讀本機副本，`sql_execute` 寫入並複寫到所有節點。來自請求的值一律用參數傳，不要接進 SQL 字串：

  ```lua
  local rows = sql_query("SELECT plan FROM subscribers WHERE username = ?", {get_attr('User-Name')})
  ```

帳號的新增、改密碼和停用不必寫 migration：設定 `[accounts]` 後用 `rradius-server user` 指令，整個叢集立刻生效，見[第 11 章](11-operations.md#帳號和資料)。

## 計費要能重複執行

NAS 沒收到 Accounting-Response 會重送，所以：

- **計費腳本回傳 `'accept'` 才會回覆。** 其他結果（含腳本錯誤）都不回覆，NAS 會重送，腳本會再跑一次。
- **同一個封包處理兩次，結果要和一次相同。** 範例都用 `INSERT ... ON CONFLICT DO UPDATE` 或 `INSERT OR IGNORE`，以 NAS 加 `Acct-Session-Id` 當主鍵（Session ID 只在同一台 NAS 內唯一）。一個 client 可以是整個網段，所以「哪一台 NAS」要再看 NAS 回報的 `NAS-IP-Address` 或 `NAS-Identifier`，不能只看 client 名稱。
- **不想記錄的封包也回傳 `'accept'`，** 否則 NAS 會一直重送。

## 用什麼工具測試

範例用 FreeRADIUS 的用戶端工具測試，Debian／Ubuntu 是 `freeradius-utils` 套件，macOS 可以用 Homebrew 的 `freeradius-server`。

**rradius 預設丟棄沒有 Message-Authenticator 的 Access-Request。** 專案用 FreeRADIUS 3.2.10 的 `radclient` 測試過，它會自動加上；較舊的版本可能不會，請求就會被丟棄而沒有回應。遇到這種情況，改用 `radclient` 並在輸入加上 `Message-Authenticator=0x00`，它就會計算正確的值：

```bash
echo 'User-Name=alice,User-Password=alice-password,Message-Authenticator=0x00' | radclient 127.0.0.1:1812 auth testing123-change-me
```

```bash
# PAP 登入：帳號、密碼、伺服器、NAS port、共用密碼
radtest alice alice-password 127.0.0.1 0 testing123-change-me

# CHAP 登入
radtest -t chap alice alice-password 127.0.0.1 0 testing123-change-me

# 計費
echo 'User-Name=alice,Acct-Session-Id=s1,Acct-Status-Type=Start' | radclient 127.0.0.1:1813 acct testing123-change-me

# 伺服器活著嗎（Status-Server）
echo 'Message-Authenticator=0x00' | radclient 127.0.0.1:1812 status testing123-change-me
```

- 802.1X（EAP）用 wpa_supplicant 的 `eapol_test`，設定範例在[第 3 章](03-enterprise-wifi.md#3-用-eapol_test-測試)。
- TACACS+ 可以用 `tacacs_client`（Python 套件 `tacacs_plus`）或直接用設備測試，見[第 6 章](06-device-admin.md)。
- 每次登入都會寫一行認證紀錄（`[log] auth = true`，預設開啟），寫明 client、名稱、哪條串接和哪個處理器做的決定，排查問題時先看這一行。
