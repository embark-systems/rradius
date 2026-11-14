# 7. 高可用叢集

**情境：** 認證不能停。三台 rradius 組成叢集，資料自動複寫到每一台；任一台停機（故障、升級、重開機）時，另外兩台照常服務，NAS 自動改問活著的節點。

範例目錄：[`rradius-server/examples/scenarios/ha-cluster/`](../../rradius-server/examples/scenarios/ha-cluster/)

| 檔案 | 內容 |
| --- | --- |
| `node1.toml`、`node2.toml`、`node3.toml` | 三個節點的設定，只差 `node_id`、`listen` 和 `[metrics] bind` |
| `raft.secret`、`api.secret` | 叢集內部的密碼，三台相同 |
| `nas.secret` | NAS 的共用密碼，三台相同 |
| `accounting.lua` | 計費腳本，和[第 2 章](02-quick-start.md#accountinglua-做了什麼)說明的相同 |
| `migrations/1_schema.sql` | 資料表，和快速開始相同 |
| `migrations/2_demo_users.sql` | 示範帳號 alice、bob、carol，**正式環境要刪掉** |

帳號和計費和快速開始相同，所以這一章只講叢集本身。

## 架構

```
             NAS（主：node1，備：node2、node3）
              │ RADIUS 1812/1813
   ┌──────────┼───────────────┐
 node1      node2           node3       每台都有完整的資料
 10.99.0.1  10.99.0.2       10.99.0.3   （WireGuard 位址）
   └──── raft 8100 / api 8200 ────┘     節點之間只走 WireGuard
```

- **寫入（`sql_execute`、migration）經過 Raft，** 要多數節點（3 台中的 2 台）同意才成功，所以每台寫入的結果都相同。
- **讀取（`sql_query`、`sql` 處理器）讀本機副本，** 很快，但可能比剛寫入的資料慢一點。
- **三台是最小的高可用：** 停一台還有多數；兩台時停一台就沒有多數。五台可以同時停兩台。

| 狀況 | 認證 | 計費 |
| --- | --- | --- |
| 三台都在 | 正常 | 正常 |
| 停一台 | 正常（NAS 改問其他台）。停的剛好是 leader 時，選出新 leader 前的幾秒，要寫入的登入不回應、計費寫入逾時 | 正常；停的是 leader 時，那幾秒的計費進暫存（可能重複一次，見下方） |
| 停兩台，剩一台 | 只讀資料的認證（`sql`、`ldap`）讀本機副本，照常回應；要寫入的（驗證碼、IP 配發）**不回應**，讓 NAS 重送或改問別台。設了 `stop_without_leader_ms` 時，超過那段時間連只讀的認證和 Status-Server 都不回應 | 寫不進去，有計費暫存時先寫在本機並回覆，叢集恢復後重放 |

- **資料庫不能用時不回應，而不是拒絕。** 登入因為叢集資料庫逾時或找不到 leader 而無法決定時，RADIUS 不回覆、TACACS+ 回 ERROR，這樣 NAS 和設備會改問其他節點，使用者也不會看到「密碼錯」。計入 `rradius_radius_discarded_total{reason="database"}`。確定是帳號不存在、密碼錯，而且腳本的資料庫呼叫都成功，才回拒絕；腳本有一次資料庫呼叫失敗後才拒絕的，也算不能決定。腳本在資料庫失敗後回 `ignore` 時，照常交給下一個處理器。進行中的 EAP 對話會留著，NAS 重送時接著做。
- **`stop_without_leader_ms`（`[cluster]`，預設不設）：** 節點超過這段時間沒有 leader，就連只讀的登入和 Status-Server 都不回應（TACACS+ 回 ERROR），計入 `reason="no_leader"`，讓 NAS 和負載平衡器把它當成故障、改問被隔開的另一側。代價是叢集只剩這一台時，它也不服務。計費照常。改了要重啟。
- **斷線很久的節點回來後可能要重啟一次。** 它不在的期間寫入超過 10 萬筆（`snapshot_every`）時，回來後 hiqlite 讀不到自己的日誌，rradius 會記錄 `exiting so that a restart recovers it` 並以狀態碼 1 結束；重新啟動後它會追上叢集。所以每個節點都要設自動重啟（systemd `Restart=on-failure`、Docker `restart: unless-stopped`）。
- **剩一台時不要重啟它。** 正在跑的那台會繼續服務只讀的登入，但重新啟動後要等到有 leader 才起得來（這是 hiqlite 的行為），有 `migrations_dir` 時等 `migrations_wait_ms` 後以狀態碼 1 結束。先救回其他節點。
- **被隔開的節點讀的是舊資料。** 它和多數節點斷開時，在另一側停用的帳號、刪掉的 NAS、撤掉的金鑰，在它這裡都還有效，SIGHUP 也沒用，直到網路恢復。不能接受的話設 `stop_without_leader_ms`。
- **交給叢集的寫入會等結果。** 資料庫很慢（例如 leader 在做快照）或換 leader 時，腳本已經交給叢集的寫入不會中途放棄，會等到叢集回覆（最多 30 秒），所以這筆寫入不會「回報失敗、其實寫進去了」。但計費仍可能記兩次：30 秒內都沒有結果時，計費不回覆、由 NAS 重送；等得比時限久時，同一個請求之後的資料庫呼叫會立刻逾時，這筆進暫存、重放時再跑一次；帶 `RETURNING` 的語句照舊以時限為準。所以計費腳本仍要能重複執行（例如用 `INSERT OR IGNORE`、以 `Acct-Session-Id` 為主鍵）。

## 步驟

### 1. 每台主機的準備

1. 建 WireGuard 網路 `10.99.0.0/24`，三台互為 peer，並用防火牆讓 8100、8200 只收 `wg0` 的連線。完整步驟在 [usage 2.2 節](../usage.md#22-cluster)。
2. 把 `examples/scenarios/ha-cluster/` 整個目錄複製到每台（例如 `/etc/rradius/ha-cluster/`）。
3. **換成自己的帳號：** 第一次啟動前刪掉 `migrations/2_demo_users.sql`，裡面的示範帳號用公開的密碼。之後設定 `[accounts]` 用 `rradius-server user add` 新增，在任何一台節點上執行都會寫到整個叢集；或在 `migrations/` 新增 `2_users.sql`，格式見[第 2 章](02-quick-start.md)和[第 11 章](11-operations.md#帳號和資料)。
4. **每台的 `raft.secret`、`api.secret`、`nas.secret` 和 `migrations/` 要相同。** 先產生新的密碼再複製過去：

   ```bash
   (umask 077; openssl rand -base64 32 > raft.secret; openssl rand -base64 32 > api.secret)
   ```

### 2. 啟動

三台各自啟動自己的設定，順序不拘：

```bash
rradius-server server --config /etc/rradius/ha-cluster/node1.toml   # 在 node1
rradius-server server --config /etc/rradius/ha-cluster/node2.toml   # 在 node2
rradius-server server --config /etc/rradius/ha-cluster/node3.toml   # 在 node3
```

- 第一次啟動時，每台都等叢集組成（多數節點上線），再套用 migration，最多等 `migrations_wait_ms`（範例 120 秒）。一台先起來、其他的還沒起來時，它會等待，不是故障。
- 看到 `RADIUS Server listening` 就能服務。
- **NAS 的設定：** 三台都設成 RADIUS 伺服器，主備或輪流都可以，共用密碼相同。EAP 的一次登入要在同一台完成（第 3 章），所以用 EAP 時選主備，或讓負載平衡器依來源固定節點。

### 3. 確認

RADIUS 走節點的實體位址，不是 WireGuard 位址；請求也要從 client 名單內的主機送出（範例是 `10.80.0.0/16`），否則不會有回應。從那個網段的一台主機，用上面第 3 項加的帳號：

```bash
# 每台都能登入（192.0.2.11 到 .13 換成三台的實體位址）
for ip in 192.0.2.11 192.0.2.12 192.0.2.13; do radtest <帳號> <密碼> $ip 0 "$(cat nas.secret)"; done

# 每台的指標，在節點上或從 WireGuard 網路內
curl -s http://10.99.0.1:9812/metrics | grep rradius_
```

管理用的主機不在 client 網段時，暫時加一個只有它位址的 `[[radius.clients]]`（SIGHUP 套用），測完再移除。

在 node1 送 Start、在 node3 送 Stop，`accounting` 表裡是同一列：寫入經過叢集，哪一台收到都一樣。

## 設定說明

```toml
[cluster]
node_id = 1
listen = "10.99.0.1"
migrations_dir = "migrations"
migrations_wait_ms = 120000

[[cluster.nodes]]
id = 1
raft = "10.99.0.1:8100"
api = "10.99.0.1:8200"
# ...id 2、3 相同
```

- **每台都列出全部三個節點（包含自己），** `node_id` 指出自己是哪一個，一定要有編號 1。
- **`listen` 寫明 WireGuard 位址。** 有 `[[cluster.nodes]]` 時預設是 `0.0.0.0`，會在所有介面上開 raft 和 api，而這兩個埠的連線沒有加密。
- **`[lua] query_timeout_ms = 2000`：** 失去多數節點時，寫入要等這麼久才失敗、計費才寫進暫存。預設 5 秒比很多 NAS 的重送間隔長，重送會被當成處理中而丟棄。
- **`[radius.accounting_spool]`：** 各節點只重放自己的暫存檔，所以放在持久的磁碟。`rradius_acct_spool_bytes` 大於 0 代表有還沒寫進資料庫的計費，可以用來告警。
- **`[log] format = "json"`：** 三台的記錄送到同一個收集系統時比較好處理。

### 哪些東西是每個節點自己的

資料表會自動複寫，但下面這些不會。規劃多節點時逐項確認：

| 東西 | 每個節點 | 要注意 |
| --- | --- | --- |
| 設定檔、腳本、字典、`migrations/` | 各自一份 | 內容要相同。改了之後每台都要更新，並各自送 SIGHUP 或重啟 |
| `raft.secret`、`api.secret`、NAS 的共用密碼檔 | 各自一份 | 內容要相同 |
| 憑證、私鑰、撤銷清單 | 各自一份 | 更新時每台都要換並送 SIGHUP |
| 計費暫存 | 只重放自己的檔案 | 放在持久的磁碟；節點壞掉時，它還沒重放的就遺失了 |
| 進行中的 EAP 對話 | 只在收到它的節點 | 同一次登入要留在同一個節點 |
| 叢集狀態 | 各自看 | `/metrics` 的 `rradius_cluster_leader`、`rradius_cluster_is_leader`、`rradius_cluster_applied_index`、`rradius_cluster_seconds_without_leader` |
| 指標、`[log.trace]` 的計時 | 各自的 | 叢集的總和由監控系統加總；追蹤要對有關的每一台都設 |

### 調整

- **`read_connections`：** 讀本機資料庫的連線數，預設是 CPU 數的兩倍（最多 32）。登入大多是讀取，連線不夠時要排隊。記憶體很緊時可以調低，每條連線有自己的快取。
- **`snapshot_every`：** 每累積這麼多筆寫入，節點就做一次快照，預設 100000。調低的話，異常結束（斷電、SIGKILL）後重新啟動比較快，但寫入多時快照太頻繁會拖慢服務。
- **多節點不會提高寫入的上限。** 每筆寫入都要經過同一個 leader、等多數節點確認。登入（讀取）則是每個節點各自處理，節點越多越快。分開的主機之間，每筆寫入還要多等節點之間的往返。

這三項的量測和說明見 [usage 2.2 節](../usage.md#22-cluster)。改了都要重新啟動。

## 日常操作

### 升級或重開機

一次一台：停掉一台 → 升級 → 啟動 → 確認它回到叢集 → 再做下一台。每次只停一台，叢集一直保有多數。

- **確認回到叢集：** 記錄有 `cluster node N has caught up with the cluster`，`/metrics` 的 `rradius_cluster_leader` 是 1。節點啟動時會等有 leader、再等本機副本追上叢集已經寫入的資料，才開始回應，所以停機期間新增的帳號在它第一次回應時就看得到。等待的上限是 `migrations_wait_ms`；沒設 `migrations_dir` 又等不到時，它記一行警告後照常服務（可能用到舊資料）。
- **最後才停 leader。** `rradius_cluster_is_leader` 是 1 的那台最後做：停掉 leader 時要重新選舉，那幾秒的寫入會逾時。

- **用 SIGTERM 或 Ctrl+C 停止，** 節點先讓 `/readyz` 回 503、照常回應 5 秒讓負載平衡器移出它，再停止接收、等處理中的請求回應完，最後乾淨關閉資料庫（hiqlite 再等 9.5 秒），多節點時要 15 秒左右，所以 `docker stop` 要用 `-t 30` 或設 `stop_grace_period: 30s`。兩段等待可以用 `[shutdown]` 調整（[usage 2.16 節](../usage.md#216-停止-shutdown)）。SIGKILL（`kill -9`、容器被強制停止）不會，下次啟動時本機資料庫會從其他節點重建，比較慢。

### 加新的 migration

1. 把新檔案（例如 `3_add_column.sql`）放到**每一台**。
2. 逐台重啟。第一台重啟時套用到整個叢集，其他台重啟時看到已經套用。
3. 不要讓目錄內容不同的節點同時啟動，見 [usage 2.5 節](../usage.md#25-資料表-migration)。

### 換掉壞掉的節點

主機壞掉、資料救不回來時，換一台新主機頂替它：

1. 新主機用**同一個 `node_id`**、同樣的位址（`[[cluster.nodes]]` 裡它的 `raft`、`api`），設定檔、`raft.secret`、`api.secret`、`migrations/` 和其他台相同。增減過節點時，`[[cluster.nodes]]` 要列出目前所有成員，見〈增加節點〉。
2. `data/` 是空的。舊主機的資料還在但不能信任時，整個刪掉再啟動。
3. 啟動。它從其他節點取得整份資料，記錄出現 `cluster node N has caught up with the cluster` 後才開始回應。
4. 確認：`/metrics` 的 `rradius_cluster_applied_index` 和其他台相同，`rradius_cluster_leader` 是 1；用一個它停機期間新增的帳號在它上面登入一次。

- **其他兩台要在線上。** 新節點要從多數節點取得資料；三台壞兩台時，見 [usage 2.2 節](../usage.md#22-cluster)〈沒有多數節點時重新啟動〉。
- **壞掉的是 leader 也一樣。** 剩下兩台會自己選出新 leader：登入在 1 秒左右恢復，選舉那幾秒的寫入會逾時，計費進暫存後補送。
- **leader 已經做過快照、丟掉舊紀錄也沒關係，** 新節點會直接收到快照。
- **管理指令會等久一點。** leader 剛停止的那段時間，`rradius-server user` 這類指令會先連到舊 leader，等連線逾時才改連新的，可能要十幾秒才完成（上限 30 秒）。
- **測過的範圍：** 2026-11-13 用 `compose.cluster.yaml` 的三節點，在一台主機的 Docker 上模擬主機壞掉（強制停止，刪掉容器和資料 volume）。換掉 follower 約 2 秒追上；設 `snapshot_every = 50`、寫入約 200 筆讓 leader 做過快照後換掉 leader，約 5 秒追上，三台的 applied index 相同，期間新增的帳號在每台都能登入。沒有在多台實體主機或經過 WireGuard 的環境測過。

### 增加節點

叢集在執行中就能加一台，其他節點不必停：

1. 新主機用一個沒用過的 `node_id`，設定檔的 `[[cluster.nodes]]` 列出**目前所有成員，包含它自己**（用 `cluster members` 看現在有誰）；`raft.secret`、`api.secret`、`migrations/` 和其他台相同。
2. `data/` 是空的，啟動。它會自己向叢集申請加入，先複製資料，再成為投票成員，記錄出現 `cluster node N has caught up with the cluster` 後才開始回應。
3. 確認：`rradius-server cluster members --config /etc/rradius/rradius.toml` 列出它是 `voter`；用一個既有帳號在它上面登入一次。
4. 把它加進其他每台的 `[[cluster.nodes]]`（還有 WireGuard 的 peer 和防火牆，見 [usage 2.2 節](../usage.md#22-cluster)），之後重啟時生效。不急著改：其他節點不改設定也能照常運作和重啟，但 `user`、`cluster` 這些指令只問設定裡列的節點。

```
$ rradius-server cluster members --config /etc/rradius/rradius.toml
node 1  voter    leader  raft 10.0.0.1:8100  api 10.0.0.1:8200
node 2  voter            raft 10.0.0.2:8100  api 10.0.0.2:8200
node 3  voter            raft 10.0.0.3:8100  api 10.0.0.3:8200
node 4  voter            raft 10.0.0.4:8100  api 10.0.0.4:8200  (not in this configuration)
```

`(not in this configuration)` 表示這份設定還沒列它，提醒你做第 4 步。

- **空資料加入的節點一定要列出現在的 leader。** 它只向自己設定裡列的節點申請加入，其他節點只會叫它去找 leader；leader 不在它的清單裡就加不進去。實測的設定有 `migrations_dir`，它等到 `migrations_wait_ms` 後結束（記錄 `did not join within`），被 Docker 自動重啟後一直重來。所以列出全部現有成員最保險。2026-11-13 實測：leader 換成 node 4 後，設定只列 1 到 3 的空資料 node 1 加不進去；補上 node 4 後 8 秒就加入。
- **帶著資料重新啟動的節點不受影響。** 它從自己的資料知道成員，設定沒列 leader 也能照常回到叢集（同一次實測約 14 秒）。

### 移除節點

要淘汰一台時，順序是**先停機、再移除、最後改設定**：

1. 停掉那台（停止服務或關機），並且之後不再用原來的設定和資料啟動它。
2. 在任何一台還在叢集裡的節點上執行 `rradius-server cluster remove --config /etc/rradius/rradius.toml 4`，印出 `removed node 4` 就完成。指令會自己找到 leader。
3. 從其他每台的 `[[cluster.nodes]]` 刪掉它，之後重啟時生效。

指令會擋下這些情況，什麼都不送：

| 情況 | 結果 |
| --- | --- |
| 那台還在回應 | 拒絕，要你先停機（結束碼 2）；沒有旗標可以跳過 |
| 編號不在叢集裡 | `no node 9 in the cluster`（結束碼 1） |
| 移除後只剩 1 或 2 台投票成員 | 拒絕並說明風險，要加 `--force`（結束碼 2） |
| 它是最後一台投票成員 | 一律拒絕（結束碼 2） |

為什麼一定要先停機（2026-11-13 實測）：

- **被移除但還在執行的節點會用舊資料回答。** 它不再收到新的寫入，移除後新增的帳號在它上面登入會被拒絕，停用的帳號還能登入。NAS 還把它列為 RADIUS 伺服器時，就會得到錯的答案。
- **帶著舊資料重新啟動會卡住，** 登入沒有回應。
- **清空資料、設定還列著自己時重新啟動，會自動加回叢集。** 所以淘汰的主機要把 rradius 的服務關掉，不只是刪資料。
- **管理指令可能卡住。** 設定裡排第一的節點如果「已移除但還在執行」，`user` 這類指令會等到 30 秒逾時；先停機就不會發生。

其他注意事項：

- **維持奇數台（3 或 5）。** 3 台可以壞 1 台，5 台可以壞 2 台；4 台和 3 台一樣只能壞 1 台。只剩 2 台時任何一台停掉都不能寫入，所以指令要求 `--force`。
- **要移除的是 leader 也一樣，** 先停掉它：剩下的節點會選出新 leader（選舉那幾秒的寫入會逾時），再移除。
- **只停一台不必移除。** 升級、重開機、換硬體後用同一個 `node_id` 回來，見〈升級或重開機〉和〈換掉壞掉的節點〉；移除是給不會再回來的節點。
- **測過的範圍：** 2026-11-13 用 `compose.cluster.yaml` 的三節點，在一台主機的 Docker 上，用 `cluster` 指令操作：加第 4 台 1 到 3 秒成為投票成員、約 4 秒開始回應登入；第 4 台還在執行時移除被拒；停機後移除立刻完成；停掉 leader（約 10 秒）後移除它，連同選出新 leader 約 4 秒完成，之後每台都看得到新帳號；被移除的 node 1 以空資料、列出全部成員的設定重新加入約 8 秒。舊節點不改設定也能帶著資料重啟。沒有在多台實體主機或經過 WireGuard 的環境測過。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 啟動後一直沒有 `listening`，最後以狀態碼 1 結束 | 等不到多數節點：其他節點沒啟動、WireGuard 不通、防火牆擋住、`raft.secret` 不同 |
| 某台的登入結果和其他台不同 | 那台的副本落後（看 `rradius_cluster_applied_index` 和其他台比），或被隔開了（`rradius_cluster_leader` 是 0），或 `migrations/` 不同；看那台的記錄 |
| 停一台之後計費變慢 | 停的不是 leader 時不應該。停的是 leader 時，選舉的幾秒內寫入會逾時並進暫存。停兩台時，沒有 leader 超過 5 秒後寫入立刻失敗並進暫存 |
| 有些登入沒有回應，`reason="database"` 在增加 | 那台連不到叢集資料庫（沒有 leader、被隔開）或資料庫太慢。看 `rradius_cluster_leader` 和記錄 |
| 某一台完全不回應，`reason="no_leader"` 在增加 | 設了 `stop_without_leader_ms`，那台沒有 leader 太久。恢復它和其他節點之間的網路 |
| Wi-Fi 登入偶爾失敗，記錄提到 `State` | EAP 對話的後半段到了另一個節點。NAS 改成主備，或負載平衡器依來源固定節點 |
