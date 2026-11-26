# 認識 rradius

> 本章讓你做到：
> - 說出 rradius 能做什麼、不做什麼
> - 看懂一個 RADIUS 請求進到伺服器之後走過哪些地方
> - 分清楚設定檔、資料目錄、字典這三樣東西各自負責什麼
> - 判斷你要的功能在核心版就有，還是要用企業版

前置條件：不需要。這一章只有觀念，不必先裝好任何東西。

本章小節：

- [rradius 是什麼](#rradius-是什麼)
- [支援的協定](#支援的協定)
- [一個程式，內建叢集資料庫](#一個程式內建叢集資料庫)
- [請求怎麼走](#請求怎麼走)
- [設定檔、資料目錄與字典](#設定檔資料目錄與字典)
- [指令一覽](#指令一覽)
- [用外部工具測試](#用外部工具測試)
- [核心與企業版](#核心與企業版)
- [本書的讀法](#本書的讀法)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## rradius 是什麼

rradius 是一套 AAA 伺服器：它替 NAS（網路存取設備，例如 AP 控制器、交換器、BNG、VPN 閘道）回答「這個使用者能不能進來」，並記下「這個使用者用了多久、多少流量」。

它有三個特點，後面各章都會用到：

1. **登入規則寫在設定檔、SQL 與 Lua 腳本裡。** 簡單的情況只要在設定檔寫一段 SQL 查詢；要判斷時段、配額、群組這類規則時，寫一個 Lua 腳本（實際上是 Luau 方言）。
2. **資料放在內建的叢集資料庫。** 帳號、計費、session 都存在程式自己帶的資料庫裡，單機就能跑，要高可用時把幾台組成叢集，不必另外架資料庫伺服器。
3. **只有一個執行檔。** 伺服器和管理指令都是 `rradiusd`：`rradiusd serve` 啟動服務，`rradiusd user add` 新增帳號，`rradiusd status` 看狀態。

rradius **不做**的事：

- 不提供網頁管理介面。所有管理都在命令列與設定檔完成。
- 不替你決定資料表的樣子。資料表由你自己用 migration（SQL 檔）建立，查詢也是你寫的。範例情境附了一套可以直接用的表。
- 不自帶憑證管理。EAP 要用的伺服器憑證、CA 憑證由你準備（第 11 章）。
- 管理指令透過叢集的 API 埠和指標埠工作，沒有獨立的管理 API 服務。

## 支援的協定

核心版 `rradiusd` 支援：

| 協定 | 用途 | 預設埠 | 說明在 |
| --- | --- | --- | --- |
| RADIUS 認證（RFC 2865） | 使用者登入：PAP、CHAP、MS-CHAP、MS-CHAPv2 | UDP 1812 | 第 05、07 章 |
| RADIUS 計費（RFC 2866） | Start、Interim-Update、Stop 等計費紀錄 | UDP 1813 | 第 13 章 |
| Status-Server（RFC 5997） | 監控系統確認伺服器還活著，不用測試帳號 | 同上兩個埠 | 第 5 章 |
| EAP over RADIUS | 802.1X：EAP-TLS、PEAP、EAP-TTLS | 走 RADIUS 認證埠 | 第 11 章 |
| TACACS+（RFC 8907） | 網路設備的管理員登入、指令授權、計費 | TCP 49 | 第 12 章 |

另外還有兩個不是 AAA 協定、但每台都會用到的埠：

- 指標埠：設了 `[metrics]` 才開，預設 `127.0.0.1:9812`，提供 `/metrics`、`/livez`、`/readyz`（第 17 章）。`status` 指令讀的就是這個埠。
- 叢集埠：Raft 埠與 API 埠，`config init` 產生的底稿寫的是 `127.0.0.1:8100` 與 `127.0.0.1:8200`。節點之間同步資料、管理指令寫入資料都走這裡（第 16 章）。

> **注意：** 指標埠沒有認證，只能綁在內部位址。叢集埠只靠共用密碼保護，也不要開到外面去。

## 一個程式，內建叢集資料庫

rradius 的資料庫是 hiqlite：每個節點各有一份 SQLite 資料庫，節點之間用 Raft 協定同步。對你來說，這代表：

- **單機就是一個節點的叢集。** 設定裡一定有 `[cluster]` 這一節，就算只有一台也一樣。單機時不用管 leader、follower 這些事。
- **寫入要經過 leader。** 叢集裡有一個節點是 leader，所有寫入都要它確認。叢集暫時選不出 leader 時，需要寫入的請求就等不到結果。
- **資料表由 migration 建立。** `[cluster]` 的 `migrations_dir` 指向一個放 SQL 檔的目錄，伺服器啟動時依檔名編號依序套用，已經套用過的不會再跑。要改結構，就新增下一個編號的檔案，不要改舊檔。單一檔案不能超過 2,000,000 位元組（一次寫入叢集的上限）：migration 放結構與少量初始資料，大量資料不要寫成 migration。超過時伺服器啟動失敗，訊息是 `migration file '3_big.sql' is 3000001 bytes, more than the 2000000 the cluster takes in one write: …`。`check` 不會發現這件事，要到啟動才知道。幾個檔案加起來超過 2,000,000 位元組沒有關係，伺服器會自己分批寫入。
- **管理指令透過 API 埠寫入。** `user add` 這類指令不直接開資料庫檔，而是連到叢集的 API 埠，所以伺服器要先啟動，指令才有對象。

除了內建的資料庫，你也可以讓 sql 處理器去查外部的 PostgreSQL 或 MySQL（第 9 章），或用 LDAP、Active Directory 驗證（第 8 章）。

## 請求怎麼走

知道一個請求在伺服器裡經過哪些關卡，排錯時才知道要看哪裡。下面以 RADIUS 為例，依程式碼實際的順序說明。

### 認證請求（Access-Request）

1. **Status-Server 先處理。** 如果封包是 Status-Server，直接回應，不跑任何腳本，也不經過重送偵測。
2. **比對用戶端（client）。** 用封包的來源 IP 位址，在 `[[radius.clients]]` 裡找對應的用戶端。一個用戶端可以是一個位址，也可以是整個網段（第 5 章）。找不到就丟棄，不回應，日誌最多每秒記一行 `RADIUS packet from unknown client … discarded`。
3. **檢查封包。** 用這個用戶端的共用密碼檢查 Message-Authenticator。算出來不對（通常是共用密碼設錯）就丟棄。用戶端要求一定要有 Message-Authenticator、封包卻沒帶時，也丟棄（`require_message_authenticator`，第 5 章）。
4. **重送偵測。** NAS 沒收到回應時會重送（retransmit）同一個封包。伺服器記得最近處理過的請求：處理中的重送直接丟掉，已經回過的就把同一個回應再送一次。
5. **沒有 leader 時不回應。** 如果設了 `cluster.stop_without_leader_ms`，叢集失去 leader 超過這段時間，認證請求就不回應，讓 NAS 改問別台（第 16 章）。
6. **EAP。** 封包帶 EAP-Message、而且設了 `[radius.eap]` 時，交給 EAP 處理：先跟用戶端完成 TLS 交握，過程中會來回好幾次 Access-Challenge（第 11 章）。EAP 裡面的身分最後還是交給下一步判斷：
   - EAP-TLS：憑證驗證通過後，用憑證證明的名稱去問驗證鏈；沒有驗證鏈時，問最上層的 Lua 腳本。
   - PEAP、EAP-TTLS：通道裡的密碼一定要由驗證鏈檢查。用戶端沒有驗證鏈時，這種登入一律拒絕。
7. **驗證鏈或 Lua 腳本，二選一。** 這一步只走其中一條：
   - 設了 `[auth]` 時，用驗證鏈。用戶端有 `auth = "<名稱>"` 時用 `[auth.chains.<名稱>]`，否則用 `[auth]` 本身那條（名稱叫 `default`）。驗證鏈由一個或多個處理器組成（sql、lua、ldap、netlogon），依 `policy` 決定結果（第 7 章）。
   - 沒有 `[auth]` 時，執行 `[lua]` 的 `script`，`phase` 是 `authentication`，由腳本回傳 `accept`、`reject` 或 `challenge`（第 10 章）。
8. **資料庫無法使用時不回應。** 驗證時資料庫或外部後端無法使用，伺服器不回 Access-Reject，而是不回應，日誌寫 `not answered: the authentication backend could not be used`。這樣 NAS 會重送或改問別台，使用者不會因為後端暫時故障被當成密碼錯誤。
9. **回覆。** 結果是接受時，把回覆屬性（例如 `Session-Timeout`、`Filter-Id`）依字典編碼，送回 Access-Accept；拒絕時送 Access-Reject。每次認證都在日誌寫一筆認證紀錄（`[log]` 的 `auth`，預設開啟），裡面有用戶端、使用者、驗證鏈、處理器和結果。

> **注意：** 設了 `[auth]` 之後，最上層的 Lua 腳本就不再參與 RADIUS 的密碼認證（EAP-TLS 沒有驗證鏈的情況除外）。很多人以為腳本會在驗證鏈之後再跑一次，其實不會。要在驗證鏈裡加規則，用 lua 處理器（第 7 章）。

### 計費請求（Accounting-Request）

計費走另一條比較短的路：

1. 和認證一樣，先比對用戶端，再用共用密碼檢查 Request Authenticator，接著做重送偵測。
2. **一律交給最上層的 Lua 腳本**，`phase` 是 `accounting`。計費請求不經過驗證鏈。
3. 腳本回傳 `accept`，伺服器才回 Accounting-Response。沒有回應的話，NAS 會重送，所以腳本裡的寫入要能重複執行。
4. 腳本寫資料庫失敗、而且設了 `[radius.accounting_spool]` 時，先把請求寫進計費 spool（寫在磁碟上、等待重送（replay）的暫存），再回應 NAS。等資料庫恢復，伺服器自己把 spool 裡的請求重新交給腳本（第 13 章）。

### TACACS+

TACACS+ 走 TCP，流程類似：用來源位址比對 `[[tacacs.clients]]`；登入的密碼交給驗證鏈或腳本；指令授權由 `authorize_groups` 的規則或腳本決定；計費交給腳本。細節在第 12 章。

## 設定檔、資料目錄與字典

rradius 跑起來需要三樣東西，各自的角色不同：

| 東西 | 是什麼 | 誰寫入 | 放在哪裡 |
| --- | --- | --- | --- |
| 設定檔 | 一個 TOML 檔，加上 `include` 帶進來的檔案。寫監聽的埠、用戶端、驗證鏈、腳本位置、叢集設定 | 你 | 沒有 `--config` 時是 `/etc/rradius/rradius.toml` |
| 資料目錄 | `[cluster]` 的 `data_dir`：叢集資料庫與 Raft 記錄 | 伺服器 | 套件與 Docker 用 `/var/lib/rradius`，範例情境用設定檔旁的 `data` |
| 字典 | 頂層的 `dictionary`：屬性名稱、編號與型別的對照表 | 不要改 | 套件與 Docker 是 `/usr/share/rradius/dictionary` |

幾個容易搞錯的地方：

- **設定檔裡的相對路徑以設定檔所在目錄為基準。** 不是以你執行指令時所在的目錄為基準。所以 `data_dir = "data"` 指的是設定檔旁邊的 `data`。
- **資料目錄只能給一個節點用。** 它是伺服器自己的狀態，不要手動改裡面的檔案，也不要讓兩個程式同時用同一個目錄。
- **字典決定腳本與設定裡能用哪些屬性名稱。** 伺服器啟動時讀字典，`Filter-Id`、`Session-Timeout` 這些名稱都是從字典來的。rradius 附的字典是 FreeRADIUS 專案 v3.2.x 字典檔的原樣複本（授權是 CC-BY 4.0，見字典目錄裡的 `NOTICE`）。要加自訂屬性的做法見第 6 章。

除了這三樣，通常還有：

- Lua 腳本（`[lua]` 的 `script`）：設定裡一定要有 `[lua]` 這一節，腳本檔也要存在。
- migration 目錄（`cluster.migrations_dir`）：建立資料表的 SQL 檔。
- 祕密檔：共用密碼、叢集密碼放在獨立的檔案裡，用 `secret_file`、`secret_raft_file`、`secret_api_file` 指過去，權限設成 0600 或 0640（第 4 章）。

## 指令一覽

不帶參數執行 `rradiusd`，或執行 `rradiusd help`，會印出全部指令：

```console
$ rradiusd help
usage: rradiusd serve [--config <path>]
usage: rradiusd check [--config <path>]
usage: rradiusd status [--config <path>] [--watch] [--json] [--cluster]
usage: rradiusd --version
usage: rradiusd schema [--lang en|zh-TW]
usage: rradiusd config show [--config <path>]
usage: rradiusd config diff <a.toml> <b.toml>
usage: rradiusd config init [--full] [--lang en|zh-TW]
usage: rradiusd tuning [--config <path>]
usage: rradiusd user add|passwd|enable|disable|delete|show [--config <path>] <name>
  add and passwd read the password from standard input
usage: rradiusd session list [--config <path>] [--json]
usage: rradiusd session show [--config <path>] [--json] <name>
usage: rradiusd query list [--config <path>] [--json]
usage: rradiusd query <name> [--config <path>] [--json]
usage: rradiusd cluster members|remove [--config <path>] [--force] [<node_id>]
  remove takes a node id, and --force to leave one or two voters
without --config, the configuration is /etc/rradius/rradius.toml
```

| 指令 | 做什麼 | 說明在 |
| --- | --- | --- |
| `serve` | 啟動伺服器 | 第 3 章 |
| `check` | 檢查設定檔，不啟動任何服務 | 第 4 章 |
| `config show`／`diff`／`init` | 印出帶預設值的完整設定、比較兩份設定、產生底稿 | 第 4 章 |
| `schema` | 印出設定檔的 JSON Schema，含每個鍵的說明與預設值 | 第 4 章 |
| `status` | 讀指標埠，整理成一頁狀態 | 第 17 章 |
| `tuning` | 依這台機器的狀況給效能調校建議 | 第 19 章 |
| `user` | 新增、改密碼、停用、啟用、刪除、查看帳號 | 第 7 章 |
| `session` | 列出在線的 session | 第 13 章 |
| `query` | 執行設定裡的具名查詢 | 第 9 章 |
| `cluster` | 查看與移除叢集成員 | 第 16 章 |

另外有一個獨立的壓測程式 `rradius-bench`，可以送 RADIUS 和 TACACS+ 請求。本書第 3 章拿它當登入測試工具，第 19 章用它做壓測。

## 用外部工具測試

本書的登入測試都用 `rradius-bench`（第 3 章）。要用別的用戶端工具時：

- **RADIUS：** FreeRADIUS 附的 `radtest`、`radclient`（Debian、Ubuntu 在 `freeradius-utils` 套件，macOS 可用 Homebrew 的 `freeradius-server`）。
- **802.1X：** wpa_supplicant 附的 `eapol_test`（第 11 章）。
- **TACACS+：** Python 套件 `tacacs_plus` 附的 `tacacs_client`（第 12 章）。

rradius 預設丟棄沒有 Message-Authenticator 的 Access-Request（`require_message_authenticator`，第 5 章），不送的用戶端工具會得到逾時，伺服器日誌是 `discarded … Access-Requests lacking a Message-Authenticator`。遇到這種情況改用 `radclient`，在輸入加上 `Message-Authenticator=0x00`，它會算出正確的值：

```console
$ echo 'User-Name=alice,User-Password=alice-password,Message-Authenticator=0x00' \
    | radclient 127.0.0.1:1812 auth "$(cat localhost.secret)"
```

```console
$ radtest alice alice-password 127.0.0.1 0 "$(cat localhost.secret)"
$ radtest -t chap alice alice-password 127.0.0.1 0 "$(cat localhost.secret)"
$ echo 'User-Name=alice,Acct-Session-Id=s1,Acct-Status-Type=Start' \
    | radclient 127.0.0.1:1813 acct "$(cat localhost.secret)"
$ echo 'Message-Authenticator=0x00' | radclient 127.0.0.1:1812 status "$(cat localhost.secret)"
```

最後一行是 Status-Server，`radius.status_server` 預設開啟（第 5 章）。

> **未實機驗證：** 這一節的外部工具指令沒有在本版重跑；先前用 FreeRADIUS 3.2.10 的 `radclient` 測過（較舊的版本可能不會自動加 Message-Authenticator）。`Message-Authenticator` 被預設要求這一點，則對照了程式碼與第 05、21 章的實測。

## 核心與企業版

rradius 有兩個程式：

- **`rradiusd`（核心版）：** 本書的主角。原始碼公開，授權是 BSD-2-Clause。
- **`rradius-enterprise`（企業版）：** 包含核心版的全部功能，另外加上一些企業功能。它有自己的一本手冊。

兩個程式讀同一種設定檔格式。核心版遇到企業版才有的設定或指令時，**不會默默忽略**，而是明確拒絕並說明原因，結束碼是 2：

```console
$ rradiusd check --config rradius.toml
rradius.toml: proxy: not supported by this program (proxying needs rradius-enterprise)
$ rradiusd dpsk show alice
dpsk: not supported by this program (DPSK needs rradius-enterprise)
```

下表是核心版會拒絕、只有企業版才有的項目，依據是程式碼裡的三張對照表：

| 企業功能 | 設定 | 指令 |
| --- | --- | --- |
| RadSec（RADIUS over TLS） | `[radius.tls]` | — |
| 動態用戶端 | `[radius.dynamic_clients]`、`[accounts.clients]` | `client` |
| 第二因素（TOTP／HOTP） | `[mfa]` | `otp` |
| EAP-AKA'、EAP-SIM | `[radius.eap.aka]`、`[accounts.aka]`，EAP 方法 `aka-prime` | `aka`、`aka-seal` |
| Diameter | `[diameter]` | — |
| DPSK（每人一把 Wi-Fi 密碼） | `[radius.dpsk]`、`[accounts.dpsk]` | `dpsk`、`dpsk-seal` |
| TACACS+ over TLS | `[tacacs.tls]` | — |
| 計費匯出 | `[accounting_export]` | — |
| 依 realm 轉送 | `[proxy]` | — |
| CoA 與 Disconnect | `[radius.coa]` | `coa`、`disconnect` |
| 叢集備份 | `[cluster.backup]` | — |

上表之外，`[radius.eap]` 與 `[[radius.clients]]` 裡也有幾個只有企業版才能用的設定鍵，例如 EAP 的 OCSP 撤銷檢查與 session 恢復。核心版遇到其中大多數同樣會拒絕，訊息也以 `needs rradius-enterprise` 結尾；少數幾個會被接受，但在核心沒有作用。這些鍵列在[附錄 A](90-config-reference.md)，標「核心不支援」，本書正文不說明。

判斷原則很簡單：用 `rradiusd check` 檢查你的設定，通過就代表核心版能跑；出現 `needs rradius-enterprise` 就代表那一項要企業版。

## 本書的讀法

依你要做的事挑著讀：

- **第一次接觸：** 照順序讀第 01 到 03 章，讓伺服器回出第一個 Access-Accept。接著讀第 04、05 章，學會寫設定檔和加入 NAS。
- **要接上帳號來源：** 第 7 章（內建資料表與驗證鏈），第 8 章（LDAP、AD），第 9 章（外部資料庫），第 10 章（Lua 腳本）。
- **要做 802.1X：** 第 11 章。
- **要管網路設備的登入：** 第 12 章。
- **要記計費、看誰在線：** 第 13 章。
- **想照著現成的情境做：** 第 14 章（寬頻與 ISP），第 15 章（組織網路存取）。每個情境都對應 `rradius-server/examples/scenarios/` 底下的一個目錄。
- **要上線：** 第 16 章（叢集），第 17 章（監控），第 18 章（日常維運），第 19 章（效能），第 20 章（安全）。
- **出問題時：** 第 21 章（排錯）。
- **查鍵名、指令、指標、Lua 函式：** 附錄 A 到 E。

書中的格式：

- 設定範例的第一行註明它放在哪個檔案，例如 `# rradius.toml`。
- 指令以 `$ ` 開頭，下面是實際的輸出；太長的輸出刪節處標 `…`。
- `> **注意：** ` 是容易做錯的地方；`> **警告：** ` 會影響安全或造成服務中斷；`> **未實機驗證：** ` 表示這段只對照了程式碼與測試，沒有在真的設備或服務上跑過。
- 共用密碼、叢集密碼一律放在檔案裡，用 `openssl rand -hex 32` 產生，範例不寫明文密碼。

## 常見錯誤

**症狀：** `check` 印出 `…: not supported by this program (… needs rradius-enterprise)`，結束碼 2。
**原因：** 設定檔或 `conf.d/` 裡用到企業版才有的設定，或執行了企業版才有的指令。
**解法：** 拿掉那一節，或改用 `rradius-enterprise`。對照上面的表確認是哪一項。

**症狀：** 設了 `[auth]` 之後，Lua 腳本裡寫的認證規則沒有作用。
**原因：** 有 `[auth]` 時，RADIUS 的密碼認證只走驗證鏈，最上層腳本只處理計費（以及 TACACS+ 的授權與計費）。
**解法：** 把規則改寫成驗證鏈裡的 lua 處理器（第 7 章），或拿掉 `[auth]`，全部交給腳本（第 10 章）。

**症狀：** PEAP 或 EAP-TTLS 登入一律被拒絕，日誌說 `this client has no chain to check a password with`。
**原因：** 通道裡的密碼只能由驗證鏈檢查，這個用戶端沒有驗證鏈。
**解法：** 設定 `[auth]`（第 7 章、第 11 章）。

**症狀：** NAS 說 RADIUS 伺服器沒回應，伺服器日誌卻沒有認證紀錄。
**原因：** 請求在比對用戶端或檢查封包的階段就被丟掉了，例如來源位址不在 `[[radius.clients]]` 裡，或共用密碼不一致。
**解法：** 看日誌有沒有 `from unknown client`，看 `status` 的 `Discarded: unknown client`、`Discarded: invalid` 計數（第 21 章）。

## 相關章節

- [02 安裝](02-install.md)
- [03 第一次啟動與登入](03-quick-start.md)
- [04 設定檔的基本功](04-configuration.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [16 叢集與高可用](16-cluster.md)
- [附錄 E：術語與 RFC 對照](94-glossary.md)
