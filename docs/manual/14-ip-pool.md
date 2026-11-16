# 14. 位址池（由認證伺服器分配 IP）

**情境：** 寬頻用戶登入時，要由 rradius 從資料表的池裡分配一個 IP 位址，放進 Access-Accept 的 `Framed-IP-Address`。用戶上線後 NAS 送計費，rradius 確認位址在用；用戶下線或 NAS 當機，位址回到池裡。

- 同時登入的人拿到不同的位址，不會發出同一個。
- 同一個用戶重新登入，盡量拿到他上次用過的位址（黏著）。
- 位址用盡時拒絕這次登入，記錄裡寫明是哪個池。
- NAS 當機沒送 Stop 時，位址一段時間後自己回收。

**先判斷要不要用這一章。** 很多 BNG／NAS 自己有位址池，認證伺服器只要回 `Framed-Pool` 的池名，NAS 就從自己的池發位址；這時不需要這一章，在 `reply_query` 回 `Framed-Pool` 即可（[第 2 章](02-quick-start.md)）。在下面這些情況，才要由認證伺服器分配：

| 情況 | 為什麼 |
| --- | --- |
| NAS 沒有位址池，或池太小、功能不夠 | 位址只能由認證伺服器給 |
| 同一批位址要給好幾台 NAS 共用 | 池在中央，才不會各台各發各的而衝突 |
| 要查「這個位址現在是誰在用」、稽核、限額 | 位址的歸屬都在資料表裡 |

範例目錄：[`rradius-server/examples/scenarios/ip-pool/`](../../rradius-server/examples/scenarios/ip-pool/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔：`sql` 處理器驗密碼，接著 `lua` 處理器分配位址 |
| `migrations/1_schema.sql` | `subscribers`（用戶和他的池）、`ip_pool`（位址）、`housekeeping`（定期工作的紀錄） |
| `migrations/2_demo_data.sql` | 示範用戶和兩個池 |
| `allocate.lua` | 認證時分配位址 |
| `accounting.lua` | 計費時確認、延長、釋放位址，並回收過期的 |
| `*.secret` | 叢集和 NAS 的共用密碼（示範值） |

示範資料：`residential` 池 8 個位址（10.77.0.2 到 10.77.0.9），用戶 alice、bob、carol；`business` 池 3 個位址（10.88.0.2 到 10.88.0.4），用戶 dave、frank、gina、hank，所以第 4 個人會遇到池用盡；erin 是停用的。

## 位址的狀態

| 狀態 | 意思 | 怎麼離開 |
| --- | --- | --- |
| `free` | 沒人用 | 認證時被搶走，變 `reserved` |
| `reserved` | 認證時預留給某個用戶，還沒收到 Accounting-Start | 收到 Start 變 `active`；`expires`（預設 120 秒）到了還沒確認就回 `free` |
| `active` | NAS 回報在用 | Stop 回 `free`；每個 Interim-Update 延長 `expires`；到期沒收到就回 `free` |

放回 `free` 時，`last_user` 記下最後用的人，`released` 記下時間，之後用來黏著。

## 步驟

### 1. 建置、檢查、啟動

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server check --config examples/scenarios/ip-pool/rradius.toml
./target/release/rradius-server server --config examples/scenarios/ip-pool/rradius.toml
```

### 2. 登入

```bash
radtest alice alice-password 127.0.0.1 0 testing123-change-me
```

收到 Access-Accept，帶 `Framed-IP-Address`（10.77.0.2 到 10.77.0.9 其中一個）和 `Acct-Interim-Interval = 300`。再試幾個：

| 指令 | 結果 | 原因 |
| --- | --- | --- |
| `radtest alice alice-password ...`（再一次） | 同一個位址 | 她已經有綁定，不多佔一個 |
| `radtest bob bob-password ...` | 不同的位址 | 每個用戶一個位址 |
| `radtest dave dave-password ...` | 10.88.0.x | dave 的池是 `business` |
| `radtest alice wrong ...` | Reject，沒有位址 | 密碼錯，來不到分配這一步 |
| `radtest erin erin-password ...` | Reject | erin 停用 |

### 3. 計費

```bash
echo 'User-Name=alice,Acct-Session-Id=s1,Acct-Status-Type=Start,Framed-IP-Address=10.77.0.2,NAS-IP-Address=10.0.0.1' | radclient 127.0.0.1:1813 acct testing123-change-me
```

`Framed-IP-Address` 換成 alice 拿到的位址。`ip_pool` 裡這個位址變成 `active`，`rradius-server session list` 會列出她。送 `Acct-Status-Type=Stop`（同樣的 `Acct-Session-Id`）後位址回到 `free`；alice 再登入，拿到同一個位址。

### 4. 池用盡

business 池只有三個位址，dave、frank、gina 各拿一個後，hank 登入會收到 Access-Reject，記錄裡有一行：

```
ip pool business is exhausted: no address for hank
```

NAS 只看到 Reject，原因在記錄裡（Access-Reject 不帶處理器設的回覆屬性）。

## 設定說明

```toml
[auth]
policy = "all_accept"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM subscribers WHERE username = ? AND enabled = 1"

[[auth.handlers]]
type = "lua"
script = "allocate.lua"
```

- **`all_accept` 依序執行，任一個拒絕就停。** 所以密碼錯的人不會執行到 `allocate.lua`，不會替沒通過的人佔位址。`allocate.lua` 自己也會查 `subscribers`：停用或不存在的帳號回 `reject`，不分配。
- **最上層腳本 `accounting.lua` 只處理計費。**

### `allocate.lua` 做了什麼

1. **已經有綁定就沿用。** 這個用戶在他的池有 `reserved` 或 `active` 的位址時，直接回那一個，預留的延長期限。重新認證不會多佔位址。
2. **黏著。** 沒有綁定時，優先找他上次用過（`last_user`）、現在空閒、而且放回不到一天（`HOLD_SECONDS`）的位址。
3. **搶位址。** 從空閒的位址裡讀 8 個（起點在空閒位址的 `n` 範圍內隨機取，從那裡往後讀；後面不夠就從最小的 `n` 讀），逐個用比較後更新去搶：

   ```sql
   UPDATE ip_pool SET state = 'reserved', username = ?, expires = ?, updated = ?
   WHERE pool = ? AND ip = ? AND state = 'free'
   ```

   `sql_execute` 回傳受影響的列數：1 表示搶到，0 表示剛被別人拿走，換下一個。寫入都經過叢集的領導節點，所以兩個請求不可能同時搶到同一個位址；候選是從本機副本讀的，可能稍舊，頂多多試一個。8 個都搶不到再讀一輪，最多四輪。
4. **隨機起點的原因。** 同時進來的請求如果都從同一個位址開始搶，大部分會輸；隨機起點讓它們散開。位址用 `n`（位址在池裡的序號）排序，所以挑候選只走索引，不用掃全部空閒的列。

### `accounting.lua` 做了什麼

- **Start、Interim-Update：** 把用戶的位址確認成 `active`，到期時間設成現在起三個 Interim 間隔（900 秒）。位址的歸屬用 NAS 回報的 `Framed-IP-Address` 加用戶名稱對。如果預留已經過期放回了，位址還空著，就拿回來；已經被別人拿走，就記一行並照常回覆 NAS。
- **Stop：** 釋放位址，只動這個 session 的，所以晚到的舊 session 的 Stop 動不到新的綁定；重送的 Stop 不再改任何東西。
- **Accounting-On／Off（NAS 重開機）：** 這台 NAS 的 `active` 位址全部放回。
- **回收過期的：** `reserved` 沒被確認、`active` 沒收到 Interim-Update，到期就放回 `free`。沿用[第 11 章](11-operations.md#清除舊資料)的做法：請求先讀本機副本判斷該不該跑（60 秒一次），再用 `UPDATE housekeeping ... WHERE last_run < ?` 搶一次，只有搶到的那個請求去回收，所以多個節點每個週期只做一次。每次最多回收 1000 個，太多留到下一次。
- **每個寫入都有條件，** 同一個封包處理兩次的結果和一次相同。

### 要調的數字

| 數字 | 在哪 | 預設 | 說明 |
| --- | --- | --- | --- |
| `RESERVE_SECONDS` | `allocate.lua` | 120 | 預留多久；NAS 通常在登入後幾秒內送 Start |
| `HOLD_SECONDS` | `allocate.lua` | 86400 | 黏著多久 |
| `INTERIM_SECONDS` | 兩個腳本 | 300 | 要求 NAS 多久送一次 Interim-Update；到期是它的三倍 |
| `CANDIDATES`、`ATTEMPTS` | `allocate.lua` | 8、4 | 一次讀幾個候選、最多讀幾輪 |
| `REAP_SECONDS` | `accounting.lua` | 60 | 回收多久最多跑一次（`allocate.lua` 內固定 60） |

`allocate.lua` 和 `accounting.lua` 是兩個獨立的腳本，沒辦法共用常數，所以 `INTERIM_SECONDS` 要兩邊一致。

## 維運

查每個池的使用率：

```sql
SELECT pool, state, count(*) FROM ip_pool GROUP BY pool, state;
```

查「這個位址現在是誰在用」：`SELECT * FROM ip_pool WHERE ip = '10.77.0.2'`。查某個池有沒有洩漏：`SELECT ip, username, expires FROM ip_pool WHERE state != 'free' AND expires < strftime('%s', 'now')`，這些列在下一次回收時會放回。

新增位址就是新增列（`INSERT INTO ip_pool (pool, ip, n) VALUES (...)`，`n` 的編法見「換成自己的資料」，寫成新編號的 migration 檔）；縮小池要先確認那些位址是 `free`，再刪列。

## 換成自己的資料

1. 刪掉 `migrations/2_demo_data.sql`，改成自己的用戶和位址；**正式環境第一次啟動前就決定好，** 套用過的 migration 檔不能再改，要改結構就新增下一個編號的檔案。
2. 換掉所有 `*.secret` 的內容，見[第 11 章](11-operations.md#更換密碼)。
3. 把 `[[radius.clients]]` 改成你的設備位址。
4. 新增位址時 `n` 要接著池裡現有最大的往下編，從 1 開始、盡量連續（1..N）。`n` 不連續時（例如拿 IP 的整數值當 `n`，兩段位址相隔很遠），隨機起點大多落在空隙裡，同時進來的請求會讀到同一批候選而互相搶，量測用的是連續編號。
5. 池很大時的速度見下面的「規模」。

## 規模

登入風暴的量測（一台 14 CPU 的 Docker，伺服器 7 個 CPU、量測程式 7 個 CPU，64 個並行的 PAP 登入，每個用戶第一次登入）：

| 池 | 登入 | 每秒 | 結果 |
|---|---|---|---|
| 2.2 萬位址、2 萬用戶 | 5,000 | 約 7,500 | 無重複、無拒絕 |
| 11 萬位址、10 萬用戶 | 2 萬 | 約 6,600 | 無重複、無拒絕 |
| 11 萬位址、10 萬用戶，整批重連 | 10 萬 | 約 6,500（共約 15 秒） | 10 萬人各拿一個不同的位址，無拒絕 |

這個結果靠兩件事：挑候選走 `(pool, state, n)` 索引；登入時的每個查詢，條件都被某個索引的前幾欄涵蓋（計費腳本的 `WHERE nas = ? AND state = 'active'` 沒有索引，但只在 NAS 重開機送 Accounting-On 時才跑，不是每次登入）。SQLite 沒有統計資料時，如果索引只涵蓋條件的一部分，查詢可能改走主鍵，把整個池掃一遍；先前版本的「沿用綁定」查詢就是這樣（它有 `ORDER BY ip`，主鍵正好是排好序的），11 萬位址時只有每秒 64 個登入。改自己的查詢時用 `EXPLAIN QUERY PLAN` 看它走哪個索引。

量測只做認證；計費的寫入（Start、Interim、Stop）另外算，上限見[第 11 章](11-operations.md)。預留位址 120 秒內沒收到 Accounting-Start 會放回，整批重連如果超過這個時間，量測的「預留數」會少於登入數。

## 已知限制

- **一個用戶一個位址。** 同一個帳號同時有兩個 session（例如兩台設備）會拿到同一個位址；要限制同時登入數是另一回事，需要自己加計數。
- **沒有 IPv6 前綴池。** DHCPv6-PD 的 /56、/60、/64 要對齊分配，這個範例不處理。
- **沒有內建的使用率指標和管理指令。** 現在靠上面的 SQL 查；之後如果需要，會考慮內建函式、指標和指令。
- **一次一個位址。** 沒有「分配一整段」。
- **NAS 當機到位址回收之間，** 位址最多保留到期限（預設 15 分鐘），之後才會給別人；NAS 回來送 Accounting-On 會立刻釋放。
- **用盡時 NAS 只看到 Reject。** 原因在 rradius 的記錄，監控記錄裡的 `is exhausted`。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 登入成功但沒有 `Framed-IP-Address` | 設定沒有 `lua` 處理器，或 `policy` 不是 `all_accept` |
| 一直是 Reject，記錄有 `is exhausted` | 池用盡：看有多少列是 `reserved` 或 `active`，有沒有洩漏的（過期還沒回收） |
| 位址一直不放回 | NAS 沒送 Stop 也沒送 Interim-Update，等到期；或沒有任何請求進來，回收是請求觸發的 |
| 同一個位址給了兩個人 | 不應該發生；檢查 `ip_pool` 的主鍵 `(pool, ip)` 有沒有被改過，或兩個池有重疊的位址卻被當成同一個 |
| NAS 回報的位址和資料表對不上 | `Framed-IP-Address` 要等於分配的位址；記錄有 `is held by someone else` 表示該位址已經是別人的 |
