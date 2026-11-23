# 18 日常維運

> 本章讓你做到：
> - 照「`check` → `config diff` → reload → 看日誌與指標」改設定
> - 分辨 reload 就生效與要重新啟動的設定，安全地重啟與關機
> - 升級程式、新增 migration，知道哪些事情必須停機
> - 知道核心能怎麼保護資料，以及它沒有備份功能
> - 在 systemd、logrotate、Docker Compose、Kubernetes、Synology 上做這些事

前置條件：做過[第 03 章](03-quick-start.md)；讀過[第 04 章](04-configuration.md)，知道設定檔、`include` 與 `[shutdown]`。叢集的部分要先讀[第 16 章](16-cluster.md)。

本章小節：

- [改設定的標準流程](#改設定的標準流程)
- [reload 會套用什麼](#reload-會套用什麼)
- [關機與重新啟動](#關機與重新啟動)
- [叢集的滾動重啟](#叢集的滾動重啟)
- [升級程式](#升級程式)
- [新增 migration](#新增-migration)
- [帳號、session 與查詢](#帳號session-與查詢)
- [資料目錄與資料保護](#資料目錄與資料保護)
- [日誌與 logrotate](#日誌與-logrotate)
- [用 systemd 管理](#用-systemd-管理)
- [在 Docker Compose 與 Synology 上](#在-docker-compose-與-synology-上)
- [在 Kubernetes 上](#在-kubernetes-上)
- [哪些事情必須停機](#哪些事情必須停機)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

本章的例子用 quick-start 情境（[第 03 章](03-quick-start.md)），另外加了計費記錄檔與 `[shutdown]`。埠都用預設值：RADIUS 1812、1813，指標 9812。

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
$ rradius-server check --config rradius.toml
configuration ok
```

`check` 讀設定檔、被引入的檔案、字典、腳本與祕密檔，結束碼 0 表示可以載入，2 表示有錯。它不開埠、不碰資料目錄，伺服器在跑的時候也能執行。

### 2. config diff

`config diff` 把兩份設定都填上預設值，再逐鍵比較：

```console
$ rradius-server config diff rradius.toml.orig rradius.toml
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

兩個鍵都是 reload 就生效，以收到停止訊號那一刻的設定為準。說明見[第 04 章](04-configuration.md)。

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

一次只停一個，三個節點的叢集始終有過半數，寫入不中斷；停到 leader 時，會有一段寫入變慢的時間（約 7 到 8 秒：選舉 6 到 7 秒，加上約 1 秒的用戶端重連；2026-11-23 在一台主機的三節點 Docker 上量，核心版。企業版在升級核心版本、載入同一份修改之前仍約 15 秒）。如果你知道哪個是 leader，把它留到最後，選舉只會發生一次。

> **注意：** 改 `[cluster]` 的 `secret_raft` 或 `secret_api` 時，新舊密碼的節點互相不認得。這種改動沒辦法一個一個來，要整個叢集一起停、一起改、一起啟動。這是依程式行為推論的，本書沒有實測。

## 升級程式

核心的版本號用 `--version` 看：

```console
$ rradius-server --version
rradius-server 0.1.0
default config: /etc/rradius/rradius.toml
```

第二行是沒有給 `--config` 時讀的設定檔，建置時就決定了。

升級的步驟：

1. 用新版的程式對現在的設定跑 `check`。新版拒絕的設定鍵，`check` 會指出來，不會等到啟動才失敗。
2. 照[叢集的滾動重啟](#叢集的滾動重啟)一次換一個節點。單節點就是停機、換程式、啟動。
3. 每換一個，看 `status` 的 `Version` 與日誌。

> **未實機驗證：** 不同版本的節點混在同一個叢集裡運作。本書只有一個版本可測，滾動升級的步驟是依程式的啟動與加入行為寫的。新版如果改了叢集資料的格式，要看那一版的說明。

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
$ rradius-server check --config rradius.toml
configuration ok
```

`check` 會檢查檔名與編號（跳號時印出跟下面[跳號](#跳號)一樣的訊息，結束碼 2），但不會套用，也不會比對已套用檔案的內容；只有啟動時才會套用。所以加了檔案之後要重新啟動：reload 不會套用（`[cluster]` 要重啟）。重啟後日誌沒有特別的一行；套用失敗時啟動會結束，結束碼 2，訊息指出哪個檔案。

叢集裡，新檔案要放到每個節點的 `migrations_dir`，然後照滾動重啟做。第一個重啟的節點套用它；還沒拿到新檔案的節點重啟時，會發現叢集套用過的檔案比自己的目錄多，警告後照常服務。

### 改了套用過的檔案

```console
$ printf -- '-- edited\n' >> migrations/1_schema.sql
$ rradius-server server --config rradius.toml
…
migration file '1_schema.sql' changed after it was applied
$ echo $?
2
```

連一行註解都不能加。把檔案改回原樣才能啟動。

### 跳號

```console
$ mv migrations/3_user_note.sql migrations/5_user_note.sql
$ rradius-server server --config rradius.toml
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

各指令的用法：`user` 在[第 07 章](07-users-and-chains.md)，`session` 在[第 13 章](13-accounting.md)，`query` 在[第 09 章](09-external-databases.md)。

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
| 外部資料庫 | 想把資料放在自己管理的資料庫 | 驗證與腳本可以用 `[databases]` 的 PostgreSQL、MySQL（[第 09 章](09-external-databases.md)），那些資料庫用它們自己的備份方式 |
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
| `ExecStart` | `/usr/bin/rradius-server server` | 讀預設設定檔 |
| `ExecReload` | `/bin/kill -HUP $MAINPID` | `systemctl reload rradius` 就是 reload |
| `Restart`、`RestartSec` | `on-failure`、`2` | 結束碼不是 0 時，2 秒後重啟 |
| `TimeoutStopSec` | `60` | 等關機的時間 |
| `AmbientCapabilities` | `CAP_NET_BIND_SERVICE` | 可以聽 1024 以下的埠，例如 TACACS+ 的 49 |
| `ProtectSystem`、`ReadWritePaths` | `strict`、`/var/lib/rradius /var/log/rradius` | 只能寫這兩個目錄 |
| `LimitNOFILE` | `65536` | 檔案描述元上限 |

所以：

- `data_dir`、計費 spool 要放在 `/var/lib/rradius` 底下，計費記錄檔放在 `/var/log/rradius` 底下，否則寫不進去。
- `Restart=on-failure` 會接住伺服器主動以結束碼 1 結束的情況，例如叢集節點的 Raft 核心停止（[第 16 章](16-cluster.md)）。設定錯誤的結束碼 2 也會觸發重啟，但重啟也一樣會失敗；改設定後先 `check`。
- 常用的指令：`systemctl start|stop|restart|reload rradius`、`systemctl status rradius`、`journalctl -u rradius -f`。

> **未實機驗證：** 本書沒有在 systemd 下實際跑過這個 unit。表中的值照檔案抄錄，行為依 systemd 的一般規則與本章的實測推論。

## 在 Docker Compose 與 Synology 上

原始碼的 `compose.yaml`（單節點）、`compose.cluster.yaml`（三節點，[第 16 章](16-cluster.md)）與 `deploy/synology/compose.yaml`（Synology 的 Container Manager 專案）都用同一個 image：

- 程式以 `ENTRYPOINT ["/usr/local/bin/rradius-server"]` 執行，預設參數 `server --config /etc/rradius/rradius.toml`，以 `rradius` 使用者執行；資料在 volume `/var/lib/rradius`。
- `stop_grace_period: 30s`：給關機足夠的時間。Docker 預設的 10 秒不夠。
- `restart: unless-stopped`：異常結束後自動重啟。
- 根檔案系統唯讀，設定以唯讀方式掛進 `/etc/rradius`。

日常操作對應：

| 要做的事 | 指令 |
| --- | --- |
| reload | `docker compose kill -s HUP <服務>` |
| 重新啟動 | `docker compose restart <服務>` |
| 看日誌 | `docker compose logs -f <服務>` |
| 看狀態 | `docker compose exec <服務> rradius-server status --config <設定檔>` |
| 新增帳號 | `docker compose exec -T <服務> rradius-server user add --config <設定檔> <帳號>`，從標準輸入給密碼 |

Synology 上的專案目錄是 `/volume1/docker/rradius/`：`compose.yaml`、`config/`（從原始碼的 `docker/config/` 複製來，改 `rradius.toml`）、`secrets/`（三個密碼檔）。設定是唯讀掛載的，所以改設定要在 NAS 上改 `config/` 裡的檔案，再對容器送 SIGHUP 或重啟專案。資料放在 Docker 的具名 volume `data`，不是 `config/` 旁邊的資料夾；刪除專案時注意不要連 volume 一起刪。

> **未實機驗證：** 本書沒有建置 image，也沒有在 Docker 或 Synology 上實際執行。上表的指令是 Docker Compose 的一般用法，對應到本章在本機實測的訊號行為。

## 在 Kubernetes 上

原始碼的 `deploy/kubernetes/` 範例（[第 16 章](16-cluster.md)）的維運方式：

- **改設定：** 設定來自 kustomize 產生的 ConfigMap。內容一改，ConfigMap 的名稱就變（`rradius-config-<雜湊>`），StatefulSet 跟著更新，pod 依 `RollingUpdate` 一個一個重啟。也就是說，在 Kubernetes 上改設定等於滾動重啟，不是 reload。改之前先在本機對 `config/` 的檔案跑 `check`。
- **新增 migration：** 除了把檔案放進 `config/`，還要在 `kustomization.yaml` 的 `files` 加一行，並在 `statefulset.yaml` 的 `items` 加一行，把它掛到 `migrations/` 底下。漏了後者，檔案不會出現在 pod 的 `migrations/` 裡。
- **一次只動一個 pod：** PodDisruptionBudget `maxUnavailable: 1` 讓排空 Kubernetes 節點這類操作一次只停一個 pod。
- **下指令：** `kubectl exec -i rradius-0 -- rradius-server user add --config /etc/rradius/rradius-0.toml alice`。
- **看日誌：** `kubectl logs rradius-0`。
- **資料：** 每個 pod 一個 PVC（`data-rradius-N`）。重建一個節點的做法（[第 16 章](16-cluster.md)）在這裡就是刪掉那個 pod 的 PVC 與 pod，讓 StatefulSet 用空的 volume 重建它。

> **未實機驗證：** 以上在 Kubernetes 上的操作都沒有實際跑過。本書只用 `kubectl kustomize` 確認範例能產生資源，並對三份設定跑過 `check`。

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
