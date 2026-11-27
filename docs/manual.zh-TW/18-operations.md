# 日常維運

> 本章讓你做到：
> - 照「`check` → `config diff` → reload → 看日誌與指標」改設定
> - 分辨 reload 就生效與要重新啟動的設定，安全地重啟與關機
> - 升級程式、新增 migration，知道哪些事情必須停機
> - 知道核心能怎麼保護資料，以及它沒有備份功能
> - 在 systemd、logrotate、Docker Compose、Kubernetes、Synology 上做這些事

前置條件：做過[第 3 章](03-quick-start.md)；讀過[第 4 章](04-configuration.md)，知道設定檔、`include` 與 `[shutdown]`。叢集的部分要先讀[第 16 章](16-cluster.md)。

本章小節：

- [改設定的標準流程](#改設定的標準流程)
- [reload 會套用什麼](#reload-會套用什麼)
- [關機與重新啟動](#關機與重新啟動)
- [叢集的滾動重啟](#叢集的滾動重啟)
- [升級程式](#升級程式)
- [新增 migration](#新增-migration)
- [帳號、session 與查詢](#帳號session-與查詢)
- [更換密碼](#更換密碼)
- [清除舊資料](#清除舊資料)
- [資料目錄與資料保護](#資料目錄與資料保護)
- [日誌與 logrotate](#日誌與-logrotate)
- [用 systemd 管理](#用-systemd-管理)
- [在 Docker Compose 與 Synology 上](#在-docker-compose-與-synology-上)
- [在 Kubernetes 上](#在-kubernetes-上)
- [監控：先看這幾個指標](#監控先看這幾個指標)
- [上線前的檢查清單](#上線前的檢查清單)
- [哪些事情必須停機](#哪些事情必須停機)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

本章的例子用 quick-start 情境（[第 3 章](03-quick-start.md)），另外加了計費記錄檔與 `[shutdown]`。埠都用預設值：RADIUS 1812、1813，指標 9812。

## 改設定的標準流程

每次改設定都走這四步：

1. **`check`：** 新的設定能不能載入。
2. **`config diff`：** 跟現在跑的那份比，到底改了什麼。
3. **reload：** 送 SIGHUP，讓伺服器重新讀設定。
4. **看日誌與指標：** 確認套用了，或知道哪些要重啟。

### 1. check

先把新設定寫在另一個檔案，或先備份舊的，再檢查：

```console
$ cp rradius.toml rradius.toml.orig
$ vi rradius.toml
$ rradiusd check --config rradius.toml
configuration ok
```

`check` 讀設定檔、被引入的檔案、字典、腳本與祕密檔，結束碼 0 表示可以載入，2 表示有錯。它不開埠、不碰資料目錄，伺服器在跑的時候也能執行。

### 2. config diff

`config diff` 把兩份設定都填上預設值，再逐鍵比較：

```console
$ rradiusd config diff rradius.toml.orig rradius.toml
lua.max_execution_ms: 1000 | 500
radius.auth_port: 1812 | 1822
```

- 每行是 `鍵: 舊值 | 新值`；某一邊沒有就寫 `(absent)`。
- 祕密只寫 `differs (hidden)`，不印內容。
- 結束碼：有差異 1，沒有差異 0（印 `no differences`），讀不到設定 2。
- `cluster.node_id`、`cluster.listen` 的差異列在最後的 `node-specific:` 下面，不算差異。比較叢集裡兩個節點的設定時很好用（[第 16 章](16-cluster.md)）。
- 想看某一份設定帶入預設值後的全貌，用 `config show --config rradius.toml`。

上面的差異裡，`radius.auth_port` 要重啟才生效，`lua.max_execution_ms` reload 就生效。下一節的表說明怎麼分。

### 3. reload

送 SIGHUP 給伺服器的行程：

```console
$ kill -HUP <pid>
```

用 systemd 時是 `systemctl reload rradius`（`ExecReload` 就是送 SIGHUP）。

reload 全有或全無：伺服器先依新設定把要換的東西全部建好，全部成功才一起換上；任何一步失敗，就什麼都不換，照舊服務。處理中的請求用它開始時的腳本、驗證鏈與用戶端設定做完，不會被中斷。

### 4. 看日誌與指標

每次 reload 在日誌留一行。全部套用：

```text
INFO rradius_core::reload: configuration reloaded from rradius.toml
```

有些改動沒有套用、要重啟：

```text
WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: radius.auth_port
```

這時其他改動已經生效（例子裡的 `lua.max_execution_ms`），伺服器仍在舊的埠上。之後每次 reload，只要設定還跟執行中的不同，都會再列一次，直到重新啟動。

設定有錯：

```text
ERROR rradius_core::reload: reload failed, keeping the running configuration: rradius.toml: line 63: `x`: extra `=`, expected nothing
```

錯誤訊息會指出檔案與行號，但不會引用那一行的內容，因為那一行可能是密碼。修好之後再送一次 SIGHUP 就好。

指標上看得到 reload 的結果：

```text
rradius_reload_total{result="ok"} 1
rradius_reload_total{result="error"} 1
rradius_reload_last_success_timestamp_seconds 1795385561
```

`status` 的 `Reload` 區段顯示的是同樣的資料（[第 17 章](17-monitoring.md)）。監控 `rradius_reload_total{result="error"}` 的增加，就能發現沒人注意到的失敗 reload。

> **注意：** 叢集的每個節點各自讀自己的設定檔。改了設定，要每個節點都改、都 reload。

## reload 會套用什麼

reload 時套用與不套用的設定：

| reload 就生效 | 要重新啟動 |
| --- | --- |
| 最上層腳本 `lua.script`、驗證鏈與處理器的腳本 | `dictionary`（字典檔案的內容改了也不會重讀） |
| `[auth]` 的驗證鏈與設定 | `[cluster]` 的任何一個鍵，含 migration |
| `[lua]` 的時限（`max_memory_mb` 除外） | `lua.max_memory_mb` |
| RADIUS 與 TACACS+ 的用戶端清單與設定，含 `secret_file` 的內容 | `[metrics]`、`[log]`（`log.trace` 除外）、`[runtime]` |
| `log.trace`（[第 17 章](17-monitoring.md)） | `[databases]`、`[queries]` |
| `[shutdown]` | `radius.bind`、`auth_port`、`acct_port`、`sockets`、`recv_buffer_bytes`、`status_server`、`max_auth_inflight`、`max_acct_inflight` |
| `[accounts]`、`[sessions]`（指令每次執行時才讀） | `[radius.accounting_spool]`；`[radius.accounting_log]` 的 `queue` |
| `[radius.accounting_log]` 的 `file`（reload 時重新開檔） | `tacacs.bind`、`port`、`max_connections`、`max_connections_per_source`、`max_body_len`、`idle_timeout_ms` |
| | 新增或移除整個 `[radius]` 或 `[tacacs]` |

每個鍵能不能 reload，也寫在[附錄 A](90-config-reference.md) 的 reload 欄。

## 關機與重新啟動

### 關機的順序

送 SIGTERM（或在前景按 Ctrl+C，也就是 SIGINT）之後，伺服器依序：

1. `/readyz` 改回 503 `stopping`，但照常回答請求，持續 `shutdown.ready_delay_ms`。
2. 停止接收新請求：RADIUS 不再讀 socket，TACACS+ 關掉監聽，計費 spool 的重送停下來（已寫的留在磁碟上）。
3. 等處理中的請求做完，最多 `shutdown.drain_ms`。
4. 關閉叢集節點的資料庫，結束，結束碼 0。

| 鍵 | 預設 | 用途 |
| --- | --- | --- |
| `shutdown.ready_delay_ms` | 單節點 0；`cluster.nodes` 超過一個時 5000 | 讓負載平衡器看到 503、把流量移走的時間 |
| `shutdown.drain_ms` | 5000 | 等處理中請求的上限 |

兩個鍵都是 reload 就生效，以收到停止訊號那一刻的設定為準。說明見[第 4 章](04-configuration.md)。

實測：quick-start 設 `ready_delay_ms = 3000`，送 SIGTERM 後每 0.4 秒測一次：

```text
t=0.0 readyz=[stopping 503] accept 1, reject 0, challenge 0, accounting 0, timeouts 0
t=1.4 readyz=[stopping 503] accept 1, reject 0, challenge 0, accounting 0, timeouts 0
t=2.7 readyz=[stopping 503] accept 1, reject 0, challenge 0, accounting 0, timeouts 0
t=3.2 readyz=[ 000] accept 0, reject 0, challenge 0, accounting 0, timeouts 1
exited
```

前 3 秒 `/readyz` 已經是 503，登入仍然 accept；3 秒後不再接收，行程結束。日誌：

```text
INFO rradius_core::cli: stopping: /readyz answers 503; requests are answered for 3000 ms more, then those being handled are waited for, for at most 5000 ms
INFO rradius_core::cli: stopping: no more requests taken
INFO rradius_core::cli: stopping: every request taken was answered
WARN hiqlite::store::state_machine::sqlite::writer: SQL writer is shutting down
```

`drain_ms` 到了還有請求沒做完，會寫 WARN，列出剩下幾個 RADIUS 請求與 TACACS+ 連線，然後切斷它們。停止期間再送一次 SIGTERM 或 SIGINT，就不再等，直接進入關閉資料庫；資料庫的關閉不會被跳過。`/livez` 在停止期間仍是 200。

### 對負載平衡器的影響

- 負載平衡器用 `/readyz` 判斷時，`ready_delay_ms` 要比它的檢查間隔乘上失敗次數長，流量才會在伺服器停止接收之前移走。
- NAS 直接設多台 RADIUS 伺服器時，沒有人看 `/readyz`。停止接收後，NAS 的請求會逾時，再改問下一台。
- 單節點沒有別台可問，所以預設 `ready_delay_ms = 0`。

### 關機要多久

超過一個節點的叢集，節點在關閉資料庫之前還會先等約 9.5 秒，讓叢集交接（實測的日誌從 `every request taken was answered` 到 `SQL writer is shutting down` 正好隔 9.5 秒）。實測：

| 情況 | SIGTERM 到行程結束 |
| --- | --- |
| 單節點，`ready_delay_ms = 3000` | 約 3 秒 |
| 三節點叢集的 leader，預設值 | 約 14.5 秒 |
| 四節點叢集的 follower，預設值 | 約 15 秒 |

所以行程管理程式的等待時間要給夠：附帶的 systemd unit 是 `TimeoutStopSec=60`，Compose 與 Synology 的範例是 `stop_grace_period: 30s`，Kubernetes 的範例是 `terminationGracePeriodSeconds: 30`。Docker 預設只等 10 秒，不夠。

> **警告：** 等待時間不夠，行程會被 SIGKILL 強制結束。強制結束的節點，下次啟動時要從 Raft 記錄重建本機的資料庫，日誌寫 `Node did not shut down gracefully - auto-rebuilding State Machine`。資料不會因此遺失，但啟動會變慢。

### 重新啟動

重新啟動就是 SIGTERM 之後再啟動。啟動時節點會等 leader、套用 migration、追上叢集的進度，才開始服務（[第 16 章](16-cluster.md)）。單節點重啟期間不回應任何請求。

## 叢集的滾動重啟

套用要重啟的設定、升級程式時，一次只重啟一個節點：

1. 改好這個節點的設定（或換好程式），`check`。
2. 送 SIGTERM，等行程結束。
3. 啟動，等它的 `/readyz` 回 `ok`，日誌出現 `has caught up with the cluster`。
4. 用 `cluster members` 確認三個節點都是 voter、有 leader。
5. 再做下一個節點。

一次只停一個，三個節點的叢集始終有過半數，寫入不中斷；停到 leader 時，會有一段寫入變慢的時間（約 7 到 8 秒：選舉 6 到 7 秒，加上約 1 秒的用戶端重連；2026-11-23 在一台主機的三節點 Docker 上量，核心版與企業版都是）。如果你知道哪個是 leader，把它留到最後，選舉只會發生一次。跨機房、延遲較高時的滾動升級量測，見[第 16 章〈跨機房〉](16-cluster.md#跨機房)。

> **注意：** 改 `[cluster]` 的 `secret_raft` 或 `secret_api` 時，新舊密碼的節點互相不認得。這種改動沒辦法一個一個來，要整個叢集一起停、一起改、一起啟動。這是依程式行為推論的，本書沒有實測。

## 升級程式

核心的版本號用 `--version` 看：

```console
$ rradiusd --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
default dictionary: /usr/share/rradius/dictionary
```

第二行是沒有給 `--config` 時讀的設定檔，建置時就決定了。

升級的步驟：

1. 用新版的程式對現在的設定跑 `check`。新版拒絕的設定鍵，`check` 會指出來，不會等到啟動才失敗。
2. 照[叢集的滾動重啟](#叢集的滾動重啟)一次換一個節點。單節點就是停機、換程式、啟動。
3. 每換一個，看 `status` 的 `Version` 與日誌。

`--version` 印的是版本號，版本號相同的兩個建置分不出來：下面量測用的兩個建置都印 `rradiusd 0.1.0 (community)`。要看某個節點換過程式沒有，比對檔案（例如 `sha256sum /usr/bin/rradiusd`）。

**2026-11-26 的量測**（核心版）：一台 Mac 上的三台 Debian 12 arm64 虛擬機，用 deb 套件安裝、systemd 管理，以 `dpkg -i` 從 commit `782d193` 升到 commit `a33a028`。兩個建置的叢集資料格式相同，設定與 migration 檔沒有改。每個節點每秒收 20 筆計費和 20 筆 PAP 登入，不重送，1 秒內沒有回覆算遺失。每種情況各跑一次。

| 情況 | 被停的節點 | 另外兩個節點 |
| --- | --- | --- |
| 升級兩個 follower，各一次 | 約 12 秒沒有回覆；停止到追上 17.3、17.2 秒 | 沒有遺失 |
| 升級 leader，換好立刻啟動 | 16.4 秒沒有回覆；停止到追上 21.8 秒 | 計費遺失 0.85 秒，登入沒有遺失 |
| leader 停 20 秒才啟動 | 停機期間沒有回覆 | 計費遺失 7.15 秒，登入沒有遺失 |
| 新版 leader 帶兩個舊版 follower，60 秒 | 沒有停機 | 3,600 筆登入、3,600 筆計費全部有回覆 |

- 停一個節點每次都花 14.6 秒；收到停止指示後還會回應約 5 秒。
- 約 1 秒內就重新啟動的 leader，在別的節點開始選舉前就回來了：它仍然是 leader。
- 升級過程中舊版 leader 帶新版 follower 時，除了正在停的那個節點，沒有任何遺失。
- 有回覆的計費，三個節點的資料庫裡一筆不缺。沒有回覆的請求仍可能寫進去：leader 停止期間另外兩個節點沒回覆的計費，事後全部在資料庫裡。計費腳本要能重複執行（[第 13 章](13-accounting.md)）。

> **未實機驗證：** 不同版本分在多台實體主機上、或高負載下運作，以及改了叢集資料格式或新增 migration 的版本。遇到這種版本，看它的說明，先在測試叢集上試。

## 新增 migration

`cluster.migrations_dir` 裡的 SQL 檔，在啟動時、開始服務之前依序套用。規則：

- 檔名是 `<整數>_<名稱>.sql`，從 1 開始，不能跳號或重複。
- 每個檔案一個交易，套用後記下檔名與內容的 SHA-256，之後啟動就跳過。
- **套用過的檔案不能改，也不能改名。** 要改資料表，就新增下一個編號的檔案。
- 叢集裡每個檔案只套用一次，由第一個啟動、拿到 leader 的節點透過叢集寫入；其他節點收到複寫。
- 每個檔案最多執行 30 秒。超過的話這次啟動失敗，檔案在叢集裡繼續跑完，下次啟動會看到它已套用。
- `migrations_wait_ms` 同時是等 leader、套用、追上進度三段各自的時限，要比最慢的檔案長。

### 加一個檔案

```console
$ printf 'ALTER TABLE users ADD COLUMN note TEXT;\n' > migrations/3_user_note.sql
$ rradiusd check --config rradius.toml
configuration ok
```

`check` 會檢查檔名與編號（跳號時印出跟下面[跳號](#跳號)一樣的訊息，結束碼 2），但不會套用，也不會比對已套用檔案的內容；只有啟動時才會套用。所以加了檔案之後要重新啟動：reload 不會套用（`[cluster]` 要重啟）。重啟後日誌沒有特別的一行；套用失敗時啟動會結束，結束碼 2，訊息指出哪個檔案。

叢集裡，新檔案要放到每個節點的 `migrations_dir`，然後照滾動重啟做。第一個重啟的節點套用它；還沒拿到新檔案的節點重啟時，會發現叢集套用過的檔案比自己的目錄多，警告後照常服務。

### 改了套用過的檔案

```console
$ printf -- '-- edited\n' >> migrations/1_schema.sql
$ rradiusd serve --config rradius.toml
…
migration file '1_schema.sql' changed after it was applied
$ echo $?
2
```

連一行註解都不能加。把檔案改回原樣才能啟動。

### 跳號

```console
$ mv migrations/3_user_note.sql migrations/5_user_note.sql
$ rradiusd serve --config rradius.toml
migration numbering must start at 1 without gaps or repeats: expected number 3, found '5_user_note.sql'
$ echo $?
2
```

檔名與編號的錯誤，在啟動叢集節點之前就檢查，所以不管叢集是否就緒，都會馬上結束；`check` 也抓得到這類錯誤。內容被改過，則要等啟動、讀到已套用的紀錄時才發現。

## 帳號、session 與查詢

`user`、`session`、`query` 這些指令都透過叢集的 API 埠跟執行中的伺服器說話：

- 伺服器沒在跑時，它們等 30 秒，結束碼 3。
- 它們讀的是設定檔裡的 `[accounts]`、`[sessions]`、`[queries.<name>]`，每次執行時讀，改了不必 reload。
- 在叢集裡，用哪個節點的設定都可以；寫入會交給 leader。

各指令的用法：`user` 在[第 7 章](07-users-and-chains.md)，`session` 在[第 13 章](13-accounting.md)，`query` 在[第 9 章](09-external-databases.md)。

## 更換密碼

每一種密碼換的方式不同。重點是：哪些 reload 就生效，哪些要重新啟動，以及換的過程中誰的請求會失敗。

| 密碼 | 怎麼換 | 換的時候 |
| --- | --- | --- |
| RADIUS 用戶端的共用密碼（`secret_file`） | 改檔案，送 SIGHUP。NAS 那邊也要換，兩邊越接近越好 | 兩邊不同步的那一段，這台 NAS 的請求會被丟棄（`Discarded: invalid`）。每台 NAS 用不同的密碼，一次只影響一台。步驟與實測見[第 5 章](05-radius-clients.md) |
| TACACS+ 用戶端的共用密碼 | 同上 | 用戶端清單可以 reload；開著的連線也照新清單比對（[第 12 章](12-tacacs.md)） |
| LDAP 服務帳號（`bind_password_file`） | 先在目錄改服務帳號的密碼，再改檔案並送 SIGHUP | 目錄改了、rradius 還沒 reload 的那一段，LDAP 驗證會失敗（[第 8 章](08-ldap-and-ad.md)） |
| 外部資料庫（`[databases.<名稱>]` 的 `password_file`） | 先在資料庫改密碼，再改檔案，**重新啟動** | `[databases]` 不能 reload（見上面的表）。叢集用滾動重啟（[第 9 章](09-external-databases.md)） |
| `secret_raft`、`secret_api`（或 `_file`） | 每個節點都要相同，整個叢集一起換 | `[cluster]` 要重新啟動。各節點密碼不同就連不起來，所以這是要排維護時間的事（依程式行為推論，未實機驗證） |
| EAP 的憑證與私鑰 | 換檔案，送 SIGHUP | 新的對話用新的，進行中的不受影響；新檔案有錯時 reload 失敗，繼續用舊的（[第 11 章](11-eap.md)） |

產生新的密碼：

```console
$ (umask 077; openssl rand -hex 32 > nas.secret)
```

- 祕密檔的權限只給執行 rradius 的帳號讀（0600 或 0640）。權限太寬時 `check`、啟動與 reload 都有警告，細節見[第 20 章](20-security.md)。
- 共用密碼短於 16 個位元組，或含有 `change-me`（範例附的公開值）時也有警告，不會擋下啟動。有些舊設備限制密碼的長度或字元，換之前先確認設備接受。

## 清除舊資料

`accounting`、`tacacs_log` 這類表會一直變大，rradius 不會自動清除。可以在計費腳本裡定期刪除：每小時最多一次，整個叢集只有一個請求會真的去做。

先用 migration 建一張記錄上次執行時間的表。下面接在 quick-start 的 `1_schema.sql`（[第 3 章](03-quick-start.md)）後面，作為下一個編號的 migration：

```sql
-- 3_housekeeping.sql
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('purge-accounting', 0);
```

> **注意：** 檔名的編號要接在你現有的 migration 後面，不能跳號（見上面的〈新增 migration〉）。quick-start 有 `1_schema.sql`、`2_demo_users.sql`，所以這一個是 `3_`；已經刪掉示範檔的環境，編號依你的情況。

然後在計費腳本 `return 'accept'` 之前加上：

```lua
-- 每小時最多一次：先讀本機副本，不必每個請求都寫入叢集。
local now = math.floor(now_unix())
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'purge-accounting' AND last_run < ?", {now - 3600})
-- 寫入經過叢集，只有一個請求能把 last_run 改成功，它負責刪除。
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'purge-accounting' AND last_run < ?",
    {now, now - 3600}) == 1 then
  -- 一次最多刪 10000 列，太多就留到下一個小時，避免一次寫入太久。
  -- ORDER BY 讓每個節點刪到同樣的列。
  sql_execute([[DELETE FROM accounting WHERE rowid IN
    (SELECT rowid FROM accounting WHERE stopped IS NOT NULL AND stopped < ? ORDER BY rowid LIMIT 10000)]], {now - 180 * 86400})
end
```

怎麼運作：

- `sql_query` 讀本機副本，大部分請求在這一步就結束，不會寫入叢集。
- `sql_execute` 回傳受影響的列數。同一個小時裡許多請求同時看到「到期了」，但只有一個能把 `last_run` 更新成功（回傳 1），其他的回傳 0，不刪。
- `stopped IS NOT NULL` 只刪已經結束的 session，還開著的不動；`stopped < ?` 保留最近 180 天。
- `LIMIT 10000` 限制一次的量，`ORDER BY rowid` 讓叢集各節點刪到同樣的列。積壓很多時要好幾個小時才清完。
- 同樣的寫法可以清 `tacacs_log`（[第 12 章](12-tacacs.md)）：條件改成 `logged < ?`，另加一列 `housekeeping` 記錄它的 `name`。

注意事項：

- 保存多久（範例 180 天）依法規和公司政策決定。業者可能有最短保存期限，**刪除前請先確認**。
- 刪除在處理一個計費請求時執行，那個請求會晚一點回覆。刪除的量要能在 `[lua] query_timeout_ms` 內完成，否則 `sql_execute` 逾時。
- 要保留歷史資料時，先由外部的匯出流程把資料搬走，再刪除；計費記錄檔見[第 13 章](13-accounting.md)。

## 資料目錄與資料保護

### 資料目錄裡有什麼

`cluster.data_dir` 底下由 hiqlite 管理，實際看到的是：

```text
data/
├── logs/                    Raft 記錄（*.wal、meta.hql、lock.hql）
└── state_machine/
    ├── db/hiqlite.db        資料庫本身（含 -wal、-shm）
    ├── snapshots/           snapshot
    ├── backups/             （核心不使用）
    └── lock                 執行中的鎖
```

- 不要在伺服器執行時改這裡的檔案，也不要用其他程式寫資料庫。所有寫入都要經過叢集，否則各節點的資料會不一致。
- 伺服器正常結束時會移除 `lock`。下次啟動看到 `lock` 還在，就知道上次沒有正常結束，會從 Raft 記錄重建資料庫。
- 想從頭來（只適用於單節點的測試環境）：停掉伺服器，刪掉整個 `data_dir`，下次啟動會重新建立並重新套用 migration。

### 核心沒有備份功能

核心沒有內建的備份：沒有定時把資料庫複製出去的設定，也沒有還原的指令。核心能做、程式碼裡看得到的資料保護方式是這些：

| 方式 | 防的是什麼 | 說明 |
| --- | --- | --- |
| 叢集的複寫 | 一台主機壞掉 | 每個節點都有完整的副本。三個節點可以失去一個而不失去資料（[第 16 章](16-cluster.md)） |
| 從其他節點重建 | 一個節點的磁碟或資料目錄壞掉 | 停掉它、清空 `data_dir`、用原本的設定啟動，它會從其他節點抄回資料。第 16 章實測過 |
| 異常結束後自動重建 | 斷電、強制結束 | 下次啟動從 Raft 記錄重建本機資料庫 |
| 計費 spool | 寫不進資料庫時的計費 | 計費先寫在本機磁碟，之後重送（[第 13 章](13-accounting.md)） |
| 計費記錄檔 | 想另外保存每一筆計費 | `[radius.accounting_log]` 把每個計費請求寫成一行 JSON，可以用 logrotate 輪替、搬到別處保存（[第 13 章](13-accounting.md)） |
| 外部資料庫 | 想把資料放在自己管理的資料庫 | 驗證與腳本可以用 `[databases]` 的 PostgreSQL、MySQL（[第 9 章](09-external-databases.md)），那些資料庫用它們自己的備份方式 |
| 設定與 migration 放進版本控制 | 重建整個環境 | 資料表結構與示範資料都在 `migrations/`，加上設定檔就能重建一個空的叢集 |

這些都不能防「錯誤的寫入」：誤刪的帳號會立刻複寫到每個節點。叢集同時失去過半數節點的資料，資料也就沒了。

> **企業版：** 企業版有叢集備份與還原。

> **注意：** 停機後直接複製 `data_dir` 當備份，這個做法程式碼與測試都沒有驗證過，本書不把它當成備份程序。

## 日誌與 logrotate

### 伺服器的日誌

伺服器把日誌寫到標準錯誤輸出，一行一筆，有時間與等級。它不自己寫日誌檔：

- systemd 下，日誌進 journal：`journalctl -u rradius`。
- 容器裡，用 `docker logs` 或 `kubectl logs` 看。
- 等級、格式（`text` 或 `json`）、認證紀錄由 `[log]` 決定，改了要重啟；暫時追蹤某個使用者用 `log.trace`，reload 就生效。說明都在[第 17 章](17-monitoring.md)。

### 計費記錄檔的輪替

`[radius.accounting_log]` 的檔案會一直變長，要輪替。伺服器收到 SIGHUP 時會重新開檔：檔案被搬走了，就建一個新的。實測：

```console
$ mv acct.jsonl acct.jsonl.1
$ kill -HUP <pid>
$ rradius-bench run … --load acct-start,concurrency=1 --requests 1
$ wc -l acct.jsonl*
       1 acct.jsonl
       2 acct.jsonl.1
       3 total
```

SIGHUP 之後的計費寫進新的 `acct.jsonl`，舊檔不再增加。

套件附的 `/etc/logrotate.d/rradius`（原始碼的 `packages/common/rradius.logrotate`）就是這樣做：

```text
/var/log/rradius/*.jsonl {
    daily
    rotate 30
    missingok
    notifempty
    nocreate
    compress
    delaycompress
    postrotate
        systemctl kill -s HUP rradius.service >/dev/null 2>&1 || true
    endscript
}
```

- 每天輪替，保留 30 份，壓縮前一份以外的舊檔。
- `nocreate`：logrotate 不建新檔，讓伺服器自己建。
- 輪替後送 SIGHUP。這也會 reload 設定；設定沒改時，就只是一次沒有變化的 reload。
- 不要用 `copytruncate`：複製與清空之間寫進來的行會遺失。
- 它只處理 `/var/log/rradius/` 底下的 `*.jsonl`，所以計費記錄檔要放在那裡，例如 `file = "/var/log/rradius/accounting.jsonl"`。

## 用 systemd 管理

套件附的 unit（原始碼的 `packages/common/rradius.service`）的重點：

| 設定 | 值 | 意思 |
| --- | --- | --- |
| `ConditionPathExists` | `/etc/rradius/rradius.toml` | 沒有設定檔就不啟動 |
| `User`、`Group` | `rradius` | 用一般使用者執行 |
| `ExecStart` | `/usr/bin/rradiusd serve` | 讀預設設定檔 |
| `ExecReload` | `/bin/kill -HUP $MAINPID` | `systemctl reload rradius` 就是 reload |
| `Restart`、`RestartSec` | `on-failure`、`2` | 結束碼不是 0 時，2 秒後重啟 |
| `TimeoutStopSec` | `60` | 等關機的時間 |
| `AmbientCapabilities` | `CAP_NET_BIND_SERVICE` | 可以聽 1024 以下的埠，例如 TACACS+ 的 49 |
| `ProtectSystem`、`ReadWritePaths` | `strict`、`/var/lib/rradius /var/log/rradius` | 只能寫這兩個目錄 |
| `LimitNOFILE` | `65536` | 檔案描述元上限 |

所以：

- `data_dir`、計費 spool 要放在 `/var/lib/rradius` 底下，計費記錄檔放在 `/var/log/rradius` 底下，否則寫不進去。
- `Restart=on-failure` 會接住伺服器主動以結束碼 1 結束的情況，例如叢集節點的 Raft 核心停止（[第 16 章](16-cluster.md)）。已從叢集移除的節點也以結束碼 1 結束，但重啟不會恢復，要停用服務（[第 16 章](16-cluster.md#被移除的節點又啟動時)）。設定錯誤的結束碼 2 也會觸發重啟，但重啟也一樣會失敗；改設定後先 `check`。
- 常用的指令：`systemctl start|stop|restart|reload rradius`、`systemctl status rradius`、`journalctl -u rradius -f`。

> **未實機驗證：** 本書沒有在 systemd 下實際跑過這個 unit。表中的值照檔案抄錄，行為依 systemd 的一般規則與本章的實測推論。

## 在 Docker Compose 與 Synology 上

原始碼的 `compose.yaml`（單節點）、`compose.cluster.yaml`（三節點，[第 16 章](16-cluster.md)）與 `deploy/synology/compose.yaml`（Synology 的 Container Manager 專案）都用同一個 image：

- 程式以 `ENTRYPOINT ["/usr/local/bin/rradiusd"]` 執行，預設參數 `serve --config /etc/rradius/rradius.toml`，以 `rradius` 使用者執行；資料在 volume `/var/lib/rradius`。
- `stop_grace_period: 30s`：給關機足夠的時間。Docker 預設的 10 秒不夠。
- `restart: unless-stopped`：異常結束後自動重啟。
- 根檔案系統唯讀，設定以唯讀方式掛進 `/etc/rradius`。

日常操作對應：

| 要做的事 | 指令 |
| --- | --- |
| reload | `docker compose kill -s HUP <服務>` |
| 重新啟動 | `docker compose restart <服務>` |
| 看日誌 | `docker compose logs -f <服務>` |
| 看狀態 | `docker compose exec <服務> rradiusd status --config <設定檔>` |
| 新增帳號 | `docker compose exec -T <服務> rradiusd user add --config <設定檔> <帳號>`，從標準輸入給密碼 |

Synology 上的專案目錄是 `/volume1/docker/rradius/`：`compose.yaml`、`config/`（從原始碼的 `docker/config/` 複製來，改 `rradius.toml`）、`secrets/`（三個密碼檔）。設定是唯讀掛載的，所以改設定要在 NAS 上改 `config/` 裡的檔案，再對容器送 SIGHUP 或重啟專案。資料放在 Docker 的具名 volume `data`，不是 `config/` 旁邊的資料夾；刪除專案時注意不要連 volume 一起刪。

Synology 上的日常工作：

| 工作 | 做法 |
| --- | --- |
| 看日誌 | Container Manager 專案頁的「記錄」，或 SSH 上去 `sudo docker compose -p rradius logs` |
| 改設定 | 改 `config/` 的檔案後，重新啟動專案。設定錯誤時容器會馬上結束，日誌寫明原因；先在別的電腦上用 `rradiusd check` 檢查更保險 |
| 更新版本 | 在別的電腦上建新 image、匯入 NAS、重新啟動專案（建置與匯入見[第 2 章](02-install.md#synology-nas)）。資料在 `data` volume，不會因為更新而不見 |
| 停止 | 專案頁「停止」。`stop_grace_period` 是 30 秒，給資料庫完成關閉的時間；不要用強制停止，否則下次啟動要重建本機資料庫 |
| 備份 | 備份 `config/` 與 `secrets/` 資料夾。資料在 Docker 管理的 volume，名稱是 `<專案名稱>_data`（專案叫 `rradius` 時是 `rradius_data`）：先停專案，再用 Hyper Backup 備份那個 volume 所在的資料夾，或用 `docker run --rm -v rradius_data:/d -v $PWD:/b alpine tar czf /b/data.tgz -C /d .` |

備份的方式與限制見[資料目錄與資料保護](#資料目錄與資料保護)。DSM 上 volume 資料夾的位置、Hyper Backup 的操作，本書沒有驗證。

> **未實機驗證：** 本書沒有建置 image，也沒有在 Docker 或 Synology 上實際執行。上表的指令是 Docker Compose 的一般用法，對應到本章在本機實測的訊號行為。

## 在 Kubernetes 上

原始碼的 `deploy/kubernetes/` 範例（[第 16 章](16-cluster.md)）的維運方式：

- **改設定：** 設定來自 kustomize 產生的 ConfigMap。內容一改，ConfigMap 的名稱就變（`rradius-config-<雜湊>`），StatefulSet 跟著更新，pod 依 `RollingUpdate` 一個一個重啟。也就是說，在 Kubernetes 上改設定等於滾動重啟，不是 reload。改之前先在本機對 `config/` 的檔案跑 `check`。
- **新增 migration：** 除了把檔案放進 `config/`，還要在 `kustomization.yaml` 的 `files` 加一行，並在 `statefulset.yaml` 的 `items` 加一行，把它掛到 `migrations/` 底下。漏了後者，檔案不會出現在 pod 的 `migrations/` 裡。
- **一次只動一個 pod：** PodDisruptionBudget `maxUnavailable: 1` 讓排空 Kubernetes 節點這類操作一次只停一個 pod。
- **下指令：** `kubectl exec -i rradius-0 -- rradiusd user add --config /etc/rradius/rradius-0.toml alice`。
- **看日誌：** `kubectl logs rradius-0`。
- **資料：** 每個 pod 一個 PVC（`data-rradius-N`）。重建一個節點的做法（[第 16 章](16-cluster.md)）在這裡就是刪掉那個 pod 的 PVC 與 pod，讓 StatefulSet 用空的 volume 重建它。
- **升級：** 換 image 版本後再 `kubectl apply -k deploy/kubernetes`，一樣一次換一個 pod。StatefulSet 從 `rradius-2` 換到 `rradius-0`，不會替你挑 leader 最後換；換到 leader 時有一段選舉的寫入空檔（秒數見[第 16 章](16-cluster.md#跨機房)），計費進 spool。
- **節點維護：** PodDisruptionBudget 讓 `kubectl drain` 這類自願中斷一次只停一個 pod。**不要 `kubectl delete pod --all`：** 三個同時重啟，叢集會暫時失去過半數。
- **不能自動擴充，`replicas` 也不能當成節點數的旋鈕。** 叢集成員寫在每個 pod 的設定檔裡，pod 數量要等於 `[[cluster.nodes]]` 的數量，也等於 `kustomization.yaml` 與 `items` 裡 `rradius-N.toml` 的份數（沒有那份檔案的 pod 讀不到設定）。所以不要設 HPA。
  - `kubectl scale --replicas=1` 等於停掉兩個節點，剩下那台的行為就是[第 16 章](16-cluster.md)說的停兩台。再擴回 3 時，回來的 pod 可能各重啟一次才就緒：設了 `stop_without_leader_ms` 的節點，啟動時等不到 leader 會以結束碼 1 結束（訊息 `cluster was not ready: no usable leader within …, and stop_without_leader_ms is set`），由 Kubernetes 重啟，第二次就會歸隊。
  - 要改節點數，每份設定、`kustomization.yaml` 與 `items` 都要跟著改；叢集本身增減節點的做法見[加入節點](16-cluster.md#加入節點)與[移除節點](16-cluster.md#移除節點)。這個組合本書沒有在 Kubernetes 上試過。已移除的節點如果它的 pod 還在，會一直重啟又結束（[第 16 章](16-cluster.md#被移除的節點又啟動時)）。
- **限制誰能讀指標：** 指標埠沒有認證也沒有 TLS，綁 `0.0.0.0` 時叢集裡的人都讀得到。`networkpolicy.yaml` 只讓 RADIUS 埠對外、叢集埠只給其他 rradius pod、指標埠只給 `monitoring` namespace（改成 Prometheus 所在的 namespace）。要用時把它加進 `kustomization.yaml` 的 `resources`。kubelet 的探針從節點本身來，大部分網路外掛都會放行，**套用後確認 pod 還是就緒的。** 用 `hostNetwork` 的 pod 不受 NetworkPolicy 管（[第 16 章](16-cluster.md#沒有合適的負載平衡器時hostnetwork)）。

> **未實機驗證：** 以上在 Kubernetes 上的操作都沒有實際跑過。本書只用 `kubectl kustomize` 確認範例能產生資源，並對三份設定跑過 `check`。

## 監控：先看這幾個指標

完整的指標清單與告警規則在[第 17 章](17-monitoring.md)。日常維運先把下面這幾個接上，它們各自對應到一個要動手的情況：

| 指標 | 代表 | 增加或不正常時 |
| --- | --- | --- |
| `rradius_radius_responses_total{code="access_reject"}` 的增加速度 | 登入被拒絕 | 突然增加：有人在試密碼、目錄故障或設定錯誤。看認證紀錄的 `handler` |
| `rradius_radius_discarded_total{reason="unknown_client"}` | 來源不在用戶端清單 | 新設備還沒加，或有人在掃描 |
| `rradius_radius_discarded_total{reason="invalid"}` | 簽章或格式不對 | 通常是某台設備的共用密碼設錯 |
| `rradius_radius_discarded_total{reason="overload"}` | 同時處理的請求到了上限 | 大於 0 就要查：資料庫、目錄或腳本變慢，見[第 21 章](21-troubleshooting.md) |
| `rradius_radius_duplicates_total` 的增加速度 | NAS 在重送 | 處理比 NAS 的重送間隔慢 |
| `rradius_cluster_leader` | 這個節點有沒有 leader 可以寫入 | 0 持續幾秒以上：節點被隔開，或叢集沒有過半數（[第 16 章](16-cluster.md)） |
| `rradius_acct_spool_bytes` | spool 裡還沒寫進資料庫的計費 | 持續大於 0：資料庫或叢集有問題 |
| `rradius_acct_spool_rejected_total` | 重放時被放棄的計費 | 大於 0 就去看 spool 目錄的 `rejected.log` |
| `rradius_accounting_log_dropped_total`、`rradius_accounting_log_failures_total` | 計費記錄檔沒寫進去的 | 磁碟太慢、滿了或不能寫 |
| `rradius_reload_total{result="error"}` | reload 失敗的次數 | 大於 0 就看日誌，修好再送一次 SIGHUP |
| `rradius_log_dropped_lines_total` | 日誌來不及寫而丟掉的行 | 日誌的去處太慢（[第 17 章](17-monitoring.md)） |
| `rradius_radius_response_seconds`、`rradius_tacacs_response_seconds` 的 p99 | 回應花多久 | 變大：資料庫、目錄或腳本變慢 |
| `rradius_tacacs_connections_total{result="refused"}` | 因連線數上限被拒絕的 TACACS+ 連線 | 增加：到了 `max_connections` 或 `max_connections_per_source`（[第 12 章](12-tacacs.md)） |

- 計數器從啟動開始累計，每個節點各算各的，reload 不歸零。告警看增加的速度，不看絕對值。
- 設了外部資料庫、具名查詢或 EAP 的環境，再加上 `rradius_external_db_*`、`rradius_query_*`、`rradius_eap_sessions_total` 這幾組，說明在第 09、11、17 章。
- 沒有 Prometheus 時，在節點上跑 `rradiusd status --config rradius.toml --watch`，每秒列一次同樣的資料（[第 17 章](17-monitoring.md)）。
- 「活著與否」用 Status-Server 最準，它走和認證相同的路徑：`rradius-bench run --load status-auth` 或帶 Message-Authenticator 的 `radclient`（[第 5 章](05-radius-clients.md#status-server)）。
- 指標埠沒有認證也沒有 TLS，只綁內部位址。

## 上線前的檢查清單

這是營運面的整合清單。安全相關的項目逐項寫在[第 20 章〈檢查清單〉](20-security.md#檢查清單)，這裡只指到它，不重複。

- [ ] `rradiusd check --config …` 通過，而且沒有警告（祕密檔權限、共用密碼太短或含 `change-me`、`unencrypted` 等，見第 20 章）。
- [ ] 所有 `*.secret` 是新產生的隨機值，不是範例附的（見上面的〈更換密碼〉）。
- [ ] 憑證不是示範用的；伺服器憑證、NAS 與設備憑證的到期日有人盯。
- [ ] `migrations/` 裡的示範資料檔（例如 `2_demo_users.sql`）已刪掉或換成正式資料，要在第一次啟動之前做，因為套用過的檔案不能改。
- [ ] `[[radius.clients]]`、`[[tacacs.clients]]` 只列真的會來問的設備，不用 `0.0.0.0/0`。
- [ ] `[metrics]` 只綁內部位址；多節點時 `[cluster] listen` 綁內部網路或 WireGuard 的位址。
- [ ] 防火牆只開用到的埠，並限制來源：1812、1813（UDP）、49（TACACS+），叢集埠只在節點之間開。
- [ ] 跑過 `rradiusd tuning --config …`，照建議調好接收緩衝與檔案描述元上限（[第 19 章](19-performance.md)）。
- [ ] 用 systemd、Compose 或 Kubernetes 管理，停止時送 SIGTERM（不要 SIGKILL），等待時間給夠；`data_dir` 與計費 spool 放在持久的本機磁碟（見上面〈關機要多久〉與〈用 systemd 管理〉）。
- [ ] 設好自動重新啟動（systemd `Restart=on-failure`、Compose `restart: unless-stopped`）：節點的資料庫壞掉時，伺服器以結束碼 1 結束，等重啟來恢復。
- [ ] 啟動後看日誌，每個要開的埠都有一行 `listening`：`RADIUS Server listening on …`、`TACACS+ Server listening on …`、`Metrics listening on …`。RADIUS 或 TACACS+ 的埠綁不了時伺服器以結束碼 1 結束；只有指標埠綁不了時只記一行錯誤、程序照常跑，所以一定要看到 `Metrics listening on …`。
- [ ] 用實際的設備（不只是測試工具）各試一次登入與計費。
- [ ] 計費表有清除舊資料的做法（上面〈清除舊資料〉），並確認保存期限。
- [ ] 監控至少接上「活著」（Status-Server 或 `/livez`）和上面〈先看這幾個指標〉；log 輪替已設（〈日誌與 logrotate〉）。

## 哪些事情必須停機

| 改動 | 單節點 | 三節點叢集 |
| --- | --- | --- |
| 腳本、驗證鏈、用戶端、共用密碼 | 不必，reload | 不必，每個節點 reload |
| 帳號、session（`user` 指令、資料表） | 不必 | 不必 |
| 埠、監聽位址、`[metrics]`、`[log]`、`dictionary` 等要重啟的設定 | 要，重啟期間不服務 | 不必，滾動重啟 |
| 新增 migration | 要，重啟 | 不必，滾動重啟 |
| 升級程式 | 要 | 不必，滾動重啟（未實機驗證） |
| 加入或移除節點 | — | 不必（[第 16 章](16-cluster.md)） |
| 換 `secret_raft`、`secret_api` | 要 | 要，整個叢集一起（依程式行為推論） |
| 改已套用的 migration | 不可以，改用新檔案 | 不可以，改用新檔案 |

## 常見錯誤

**症狀：** reload 後日誌寫 `these need a restart to apply: radius.auth_port`，伺服器還在舊的埠上。
**原因：** 這個設定要重新啟動才生效。
**解法：** 找時間重啟（叢集用滾動重啟）。其他改動已經生效。

**症狀：** 日誌寫 `reload failed, keeping the running configuration: …`。
**原因：** 新的設定有錯，伺服器照舊服務。
**解法：** 照訊息指出的檔案與行號修正，`check` 通過後再送 SIGHUP。

**症狀：** 改了設定，但行為沒變，日誌也沒有 reload 的紀錄。
**原因：** 沒有送 SIGHUP；或在叢集裡只 reload 了一個節點。被引入的檔案也是 reload 時才重讀。
**解法：** 對每個節點送 SIGHUP，看每個節點的日誌。

**症狀：** 啟動時印出 `migration file '1_schema.sql' changed after it was applied`，結束碼 2。
**原因：** 改了已套用的 migration 檔。
**解法：** 把檔案改回原樣，要做的改動寫成下一個編號的新檔案。

**症狀：** 啟動時印出 `migration numbering must start at 1 without gaps or repeats: expected number 3, found '5_user_note.sql'`，結束碼 2。
**原因：** 檔名的編號跳號或重複。
**解法：** 照順序編號。

**症狀：** 啟動時結束，訊息提到 `migrations_wait_ms`，結束碼 1。
**原因：** 在時限內等不到 leader、套用不完 migration，或本機副本追不上。
**解法：** 確認叢集有過半數的節點在運作；檔案本身很慢時，加長 `migrations_wait_ms`。

**症狀：** 停止服務後，下次啟動日誌寫 `Node did not shut down gracefully - auto-rebuilding State Machine`。
**原因：** 上次被強制結束，例如等待時間不夠被 SIGKILL。
**解法：** 加長行程管理程式的等待時間（systemd 的 `TimeoutStopSec`、Compose 的 `stop_grace_period`、Kubernetes 的 `terminationGracePeriodSeconds`）。

**症狀：** logrotate 輪替後，計費記錄檔不再增加，或新檔沒出現。
**原因：** 輪替後沒有送 SIGHUP，伺服器還在寫被搬走的舊檔。
**解法：** 在 `postrotate` 送 SIGHUP，像套件附的設定那樣；不要用 `copytruncate`。

**症狀：** `user add` 等了 30 秒，印出 `the cluster did not answer within 30 s`，結束碼 3。
**原因：** 伺服器沒在跑，或叢集沒有 leader。
**解法：** 先啟動伺服器；叢集的話，先讓過半數節點恢復。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [04 設定檔的基本功](04-configuration.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [13 計費與 session](13-accounting.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [19 效能調校與壓測](19-performance.md)
- [21 排錯](21-troubleshooting.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 B：命令參考](91-command-reference.md)
