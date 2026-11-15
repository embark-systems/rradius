# 15. 用戶資源控制（同時連線數與流量上限）

**情境：** 寬頻與 Wi-Fi 服務的每個用戶都有方案。rradius 在登入時依方案檢查兩件事：

- **同時連線數：** 方案允許幾個連線同時在線，超過的登入被拒絕。同時進來的登入也不會突破上限。
- **流量上限：** 最近一段時間（滾動的 N 天）用掉的流量。沒超過回正常方案的 `Filter-Id`；超過之後依方案降速（登入成功，回降速的 `Filter-Id`）或拒絕。

這一章和[第 14 章](14-ip-pool.md)用同一套做法：資料表、`sql` 處理器驗密碼、`lua` 處理器做決定，不需要改核心。

範例目錄：[`rradius-server/examples/scenarios/usage-control/`](../../rradius-server/examples/scenarios/usage-control/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔：`sql` 處理器驗密碼，接著 `lua` 處理器做資源控制 |
| `migrations/1_schema.sql` | `plans`（方案）、`subscribers`（用戶）、`session_slots`（連線位）、`accounting`（每個 session 一列）、`housekeeping` |
| `migrations/2_demo_data.sql` | 三個示範方案和五個示範用戶 |
| `limits.lua` | 登入時檢查流量和同時連線數 |
| `accounting.lua` | 記錄流量，確認、延長、釋放連線位，回收過期的 |
| `*.secret` | 叢集和 NAS 的共用密碼（示範值） |

示範資料：

| 方案 | 同時連線 | 流量上限（30 天） | 超過之後 | 用戶 |
| --- | --- | --- | --- | --- |
| `basic` | 1 | 不限 | — | alice（erin 停用） |
| `family` | 2 | 100 GiB | 降速：登入成功，`Filter-Id` 是 `family-slow` | bob、dave |
| `strict` | 1 | 5 GiB | 拒絕 | carol |

## 步驟

### 1. 建置、檢查、啟動

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server check --config examples/scenarios/usage-control/rradius.toml
./target/release/rradius-server server --config examples/scenarios/usage-control/rradius.toml
```

### 2. 同時連線數

```bash
radtest alice alice-password 127.0.0.1 0 testing123-change-me
```

收到 Access-Accept，帶 `Filter-Id = "basic"` 和 `Acct-Interim-Interval = 300`。alice 的方案只有一個連線，再試一次：

| 指令 | 結果 | 原因 |
| --- | --- | --- |
| `radtest alice alice-password ...`（再一次） | Reject | 第一個登入還在等 NAS 的 Start，已經佔住唯一的連線位 |
| 送 `Accounting-Start` 之後再登入 | Reject | 連線位被在線的 session 佔住 |
| 送 `Accounting-Stop` 之後再登入 | Accept | 連線位放回 |
| `radtest bob bob-password ...` 三次 | Accept、Accept、Reject | bob 的方案有兩個連線位 |
| `radtest erin erin-password ...` | Reject | erin 停用 |

```bash
echo 'User-Name=alice,Acct-Session-Id=a1,Acct-Status-Type=Start,NAS-IP-Address=10.0.0.1' | radclient 127.0.0.1:1813 acct testing123-change-me
echo 'User-Name=alice,Acct-Session-Id=a1,Acct-Status-Type=Stop,NAS-IP-Address=10.0.0.1' | radclient 127.0.0.1:1813 acct testing123-change-me
```

`rradius-server session list` 列出現在在線（`active` 而且沒到期）的連線位。

### 3. 流量上限

流量來自 NAS 的 Interim-Update 和 Stop（`Acct-Input-Octets`、`Acct-Output-Octets`，超過 4 GiB 時進位在 Gigawords）。dave 的方案是 100 GiB：

```bash
echo 'User-Name=dave,Acct-Session-Id=d1,Acct-Status-Type=Start,NAS-IP-Address=10.0.0.1' | radclient 127.0.0.1:1813 acct testing123-change-me
echo 'User-Name=dave,Acct-Session-Id=d1,Acct-Status-Type=Interim-Update,NAS-IP-Address=10.0.0.1,Acct-Input-Gigawords=25' | radclient 127.0.0.1:1813 acct testing123-change-me
radtest dave dave-password 127.0.0.1 0 testing123-change-me
```

25 個 Gigawords 剛好是 100 GiB，已經用到上限，所以登入還是成功，但 `Filter-Id` 變成 `family-slow`。24 個 Gigawords 加一個位元組以下，還是 `family`。carol 的方案超過 5 GiB 就拒絕；NAS 只看到 Access-Reject，原因在記錄裡：

```
usage control: quota of plan strict is used up by carol - rejected
```

## 設定說明

```toml
[auth]
policy = "all_accept"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM subscribers WHERE username = ? AND enabled = 1"

[[auth.handlers]]
type = "lua"
script = "limits.lua"
```

- **`all_accept` 依序執行，任一個拒絕就停。** 密碼錯的人不會執行到 `limits.lua`，不會佔連線位。
- **方案放在 `plans` 表。** `quota_octets` 為 0 表示不限；`period_days` 是計量期間；`over_quota` 是 `throttle` 或 `reject`；`normal_filter`、`throttle_filter` 是回給 NAS 的 `Filter-Id`，NAS 把它對應成頻寬或 ACL（要用廠商的速率屬性，改成在 `limits.lua` 設那個屬性）。改了方案，下一次登入就依新方案。

### `limits.lua` 做了什麼

1. **查方案。** 停用或不存在的帳號回 `reject`。
2. **流量。** 加總這個用戶最近 `period_days` 天的流量，包含還開著的 session 目前的累計量：

   ```sql
   SELECT coalesce(sum(input_octets + output_octets), 0) FROM accounting WHERE username = ? AND started >= ?
   ```

   沒超過就用 `normal_filter`；超過依方案降速或拒絕。**先檢查流量，再搶連線位，** 所以被拒絕的人不佔連線位。
3. **同時連線數。** 每個用戶有 `max_sessions` 個連線位（`session_slots`），第一次登入時用 `INSERT OR IGNORE` 補齊。登入時逐個空的連線位用「比較後更新」去搶：

   ```sql
   UPDATE session_slots SET state = 'reserved', expires = ?, updated = ?
   WHERE username = ? AND slot = ? AND state = 'free'
   ```

   `sql_execute` 回傳受影響的列數：1 表示搶到，0 表示剛被別人拿走，換下一個。寫入都經過叢集的領導節點，所以兩個同時進來的登入不可能搶到同一個連線位，連線數不會超過上限。全部搶不到就拒絕，記錄一行含用戶與方案。

### `accounting.lua` 做了什麼

- **每個 session 一列** 寫進 `accounting`（寫法同[第 2 章](02-quick-start.md)）：Start 建立，Interim-Update 和 Stop 更新，同一個封包處理兩次的結果相同。
- **連線位：** Start 把一個預留的連線位確認成 `active`，到期時間設成現在起三個 Interim 間隔（900 秒）；Interim-Update 延長這個 session 的連線位；Stop 釋放；Accounting-On／Off（NAS 重開機）釋放那台 NAS 的所有連線位。
- **晚到的封包：** Interim-Update 找不到自己的連線位（預留已經過期放回），就拿一個空的；都沒有，記一行並照常回覆 NAS，不因為我們的資料而讓計費失敗。
- **回收過期的：** `reserved` 沒被確認、`active` 沒收到 Interim-Update，到期就放回。沿用[第 11 章](11-operations.md#清除舊資料)的做法：請求先讀本機副本判斷該不該跑（60 秒一次），再用 `UPDATE housekeeping ... WHERE last_run < ?` 搶一次，只有搶到的請求去回收，所以多個節點每個週期只做一次。

### 為什麼用連線位，不是登入時數 `accounting` 表

登入時數「這個用戶還開著幾個 session」比較簡單，但有兩個洞：同時進來的登入彼此看不到對方，Start 還沒到之前預留的也數不到，上限會被突破。連線位把「佔一個位子」做成一次有條件的寫入，才能保證不超過。

### 要調的數字

| 數字 | 在哪 | 預設 | 說明 |
| --- | --- | --- | --- |
| `RESERVE_SECONDS` | `limits.lua` | 120 | 預留多久；NAS 通常在登入後幾秒內送 Start |
| `INTERIM_SECONDS` | 兩個腳本 | 300 | 要求 NAS 多久送一次 Interim-Update；連線位到期是它的三倍 |
| `REAP_SECONDS` | `accounting.lua` | 60 | 回收多久最多跑一次（`limits.lua` 內固定 60） |

兩個腳本是獨立的，沒辦法共用常數，所以 `INTERIM_SECONDS` 要兩邊一致。

## 維運

某個用戶的連線與流量：

```sql
SELECT slot, state, session_id, nas, expires FROM session_slots WHERE username = 'bob' ORDER BY slot;
SELECT sum(input_octets + output_octets) FROM accounting WHERE username = 'bob' AND started >= strftime('%s', 'now') - 30 * 86400;
```

換方案就是改 `subscribers.plan`；改方案的上限就是改 `plans`。縮小 `max_sessions` 時，編號超過新上限的連線位不再被分配，在線的 session 不受影響，結束後自然消失。

## 換成自己的資料

1. 刪掉 `migrations/2_demo_data.sql`，改成自己的方案和用戶；**正式環境第一次啟動前就決定好，** 套用過的 migration 檔不能再改，要改結構就新增下一個編號的檔案。
2. 換掉所有 `*.secret` 的內容，見[第 11 章](11-operations.md#更換密碼)。
3. 把 `[[radius.clients]]` 改成你的設備位址。
4. 帳號名稱的大小寫：連線位和流量都以登入名稱為準，要不分大小寫就在串接設定 `[auth.username]`（見 usage 第 3 節）。

## 已知限制

- **只在登入時檢查。** 不會中斷已經在線的連線：一個 session 在線期間用掉的流量，要等它下次登入才會被檢查。要中途強制（超過上限就斷線或改變權限），需要 CoA 與 Disconnect（rradius-enterprise）。
- **計量期間是滾動的最近 N 天，** 不是日曆月，沒有帳單日和時區。
- **流量加總隨用戶的 session 數變慢。** 有 `accounting_user` 索引、只看一個用戶，但用戶多、session 多時要先實測；必要時另存每個用戶的累計表。
- **用盡時 NAS 只看到 Reject，** 原因在 rradius 的記錄，監控記錄裡的 `session limit` 與 `quota`。
- **沒有預付餘額和線上計費，** 沒有內建函式、指標和管理指令。
- **NAS 當機沒送 Stop，** 連線位佔到期限（預設 15 分鐘）才回收；NAS 回來送 Accounting-On 會立刻釋放。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 登入成功但沒有 `Filter-Id` | 設定沒有 `lua` 處理器，或 `policy` 不是 `all_accept`，或方案的 `normal_filter` 是空字串 |
| 一直是 Reject，記錄有 `session limit` | 連線位被佔住：查 `session_slots`，看有沒有過期還沒回收的（沒有請求進來就不會回收） |
| 一直是 Reject，記錄有 `quota` | 方案是 `reject` 而且流量用完了；查 `accounting` 加總 |
| 流量沒有增加 | NAS 沒送 Interim-Update 或 Stop；`accounting` 的 `input_octets` 是累計值 |
| 超過上限的人沒有被降速 | 降速只在下一次登入時生效；`Filter-Id` 要由 NAS 對應成頻寬 |
