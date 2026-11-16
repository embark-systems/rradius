# 19. IPv6 前綴池（DHCPv6-PD 的委派前綴）

**情境：** IPv6 用戶（家用路由器）要從營運商拿到一個委派前綴，常見是 /56、/60 或 /64，再由路由器分給家裡的網路。前綴要**對齊**（/56 的前綴，位址的後 72 位元是 0）、不能發給兩個人、用戶離線後要能回收，最好**每次登入都拿到同一個**（不然用戶家裡的路由器要重新設定）。rradius 在認證時從池裡分配一個前綴，放進 Access-Accept 的 `Delegated-IPv6-Prefix`，計費確認與釋放。這一章和[第 14 章](14-ip-pool.md)（IPv4 位址池）用同一套做法，不需要改核心。

範例目錄：[`rradius-server/examples/scenarios/prefix-pool/`](../../rradius-server/examples/scenarios/prefix-pool/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔：`all_accept` 串接，先 `sql` 處理器驗密碼，再 `lua` 處理器分配前綴；三個命名查詢（各狀態的前綴數、池的容量、最舊的 `released` 的年齡） |
| `migrations/1_schema.sql` | `prefix_pools`（池的三個常數）、`prefix_leases`（在線與剛離線的前綴）、`subscribers`、`housekeeping` |
| `migrations/2_demo_data.sql` | 三個示範池（兩個 /48 與一個 /52，有一個是 ULA）和五個用戶 |
| `allocate.lua` | 認證時分配前綴 |
| `accounting.lua` | 計費確認、延長、釋放前綴，回收過期的 |
| `*.secret` | 叢集和 NAS 的共用密碼（示範值） |

## 池和前綴

**池只由三個常數決定，表只放已經分配出去的前綴：沒有那一列就是空閒。** `prefix_pools(pool, base, plen, dlen)`：

- `base`：前綴的**高 64 位元**，一個整數。委派前綴最長 /64，所以要分配的部分都在這裡。`2001:db8:100::` 寫成 `0x20010db801000000`。最高位元是 1 的（例如 `fd00::/8` 的 ULA）在 SQLite 是負數，沒有關係。
- `plen`：池的長度，例如 `2001:db8:100::/48` 寫 48。
- `dlen`：每個用戶的長度，例如 /56 寫 56，不超過 64。

池的第 `n` 個前綴（`n` 從 0 起）的高 64 位元是 `base | (n << (64 − dlen))`，所以一定對齊；池有 2 的（`dlen − plen`）次方個前綴。示範的 `residential` 是 `2001:db8:100::/48` 切 /56，256 個：`2001:db8:100::/56`、`2001:db8:100:100::/56`、`2001:db8:100:200::/56`……

`base` 必須對齊 `plen`（後面的位元都是 0），`dlen` 不能小於 `plen`，容量最多 2 的 30 次方：不合的池寫不進 `prefix_pools`（資料表的 CHECK 擋下）。池很大也沒有關係：/32 切 /56 有 1,600 萬個前綴，表裡一列都不用放。

`prefix_leases(pool, n, prefix, state, username, session_id, nas, expires, updated)` 一個前綴一列：

| 狀態 | 意思 | 什麼時候變 |
| --- | --- | --- |
| `reserved` | 認證時預留了，還沒收到 Accounting-Start | 收到 Start 變 `active`；`expires`（預設 120 秒）到了還沒確認就刪掉 |
| `active` | NAS 回報在用 | 每個 Interim-Update 延長 `expires`（預設 900 秒）；Stop、NAS 重開機、到期沒收到 Interim 都改成 `released` |
| `released` | 用戶離線了，前綴暫時留給他 | `expires`（釋放時間加 `HOLD_SECONDS`，預設 24 小時）到了才刪；他回來登入先拿到同一個前綴（改回 `reserved`）；別人只有在池裡找不到空位時才會搶最舊的 `released` |

沒有 `free`：沒有那一列就是空閒。一個用戶在一個池裡只有一個前綴，一個前綴只有一個人（唯一索引）。

**為什麼留著離線用戶的前綴：** 要讓用戶每次登入都拿到同一個前綴，光靠雜湊起點不夠：池越滿，起點越常被別人佔走，離線再回來就換了前綴（見下面的模擬與量測）。把剛離線的前綴留 24 小時，短斷線、NAS 或 BNG 重開機之後，用戶回來就拿回原來的。表的列數是「在線加最近 24 小時內離線」的前綴，上限是池的容量，不是用戶總數的歷史；`HOLD_SECONDS` 是有限的，不要設成接近無限，不然就成了每個用戶一列的歷史表。

### 前綴的文字一定要和伺服器解碼的一樣

計費確認靠 `Delegated-IPv6-Prefix` 的**文字**找到 `prefix_leases` 的那一列。伺服器把 NAS 送來的屬性解碼成文字的形式是壓縮、小寫：`2001:db8:100::/56`，`2001:db8:0:100::/56`（中間的 0 不壓縮，因為最長的零群在尾端），`fd00::/48`。表裡存的文字必須是同一種，否則 **Start 找不到那一列，前綴被當成沒人用而回收，發給第二個用戶，而且沒有任何錯誤**。

範例用 SQL 由 `n` 算出文字，不是人寫的：前綴的低 64 位元一定是 0，尾端至少四個零群，一定是被壓縮的那一段，所以文字是「不含尾端零群的各群（去前導零）加 `::/長度`」，全零是 `::/長度`。這個形式有測試對照（`test_prefix_pool_text_matches_what_the_server_decodes`：各種形狀的池、ULA、全零、最高位元是 1，每個 `n` 的文字都等於 Rust 的 `Ipv6Addr` 顯示），你自己改池的常數不會碰到這個問題；要自己寫別的方式存前綴時，一定要照這個形式。

## 步驟

### 1. 建置、檢查、啟動

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server check --config examples/scenarios/prefix-pool/rradius.toml
./target/release/rradius-server server --config examples/scenarios/prefix-pool/rradius.toml
```

### 2. 登入

用 `rradius-bench` 以 `alice` 登入（`--secret` 是 `localhost.secret` 的內容）：

```bash
./target/release/rradius-bench run --server 127.0.0.1 --secret "$(cat examples/scenarios/prefix-pool/localhost.secret)" \
  --user alice --password alice-password --load pap,concurrency=1 --requests 1
```

結果是 accept。bench 不印回覆的屬性；alice 拿到哪個前綴，看資料表 `prefix_leases`（`query prefix_leases` 只回每個池每個狀態的筆數），或看測試 `test_prefix_pool`（它把 NAS 收到的 18 個位元組的屬性解開來比對）。

### 3. 計費

NAS 在用戶上線時送 Accounting-Start，帶 `Delegated-IPv6-Prefix`（和 Access-Accept 給它的同一個）、`User-Name`、`Acct-Session-Id`。這時那一列變 `active`，`rradius-server session list` 就列得出來。Stop 把那一列改成 `released`（留 `HOLD_SECONDS`）。

## 設定說明

### `allocate.lua` 做了什麼

1. **已經有前綴就沿用**，不多佔一個：預留的延長期限；`released` 的改回 `reserved`（他回來了，前綴還在）。
2. **沒有就找一個：** 用用戶名稱的雜湊（FNV-1a，32 位元）決定一個起點 `n`，在往後 `WINDOW`（1,024）個 `n` 裡找空的（沒有那一列的）：起點自己空著就是它，否則走過連續已佔用的 `n` 到第一個空的。這是**一次讀**（走 `(pool, n)` 的主鍵，讀本機副本），不是一個一個去寫。
3. **窗口裡沒有空的，就搶最舊的 `released` 列**（離線最久的用戶）：讀最舊的 8 列（走部分索引），隨機挑一個，用一條有條件的 `UPDATE` 搶（條件：還是那個用戶、還是 `released`、還是那一次釋放）；受影響列數是 1 才算贏。他回來時就得到另一個前綴。
4. **都沒有**（沒有 `released` 列）才在整個池裡找空的；到池尾都滿了就從 0 找到起點。
5. **用 `INSERT OR IGNORE` 去佔空的 `n`：** `sql_execute` 回 1 就搶到，0 就是剛被別人拿走（主鍵擋住），所以兩個請求不可能拿到同一個前綴。前綴文字在同一個 `INSERT` 裡由 `n` 算出來。
6. **搶輸之後：** 同時進來的人、起點落在同一段的人，都會找到同一個空的 `n`，每一輪只有一個贏。所以搶輸後不是再拿「下一個」空的，而是讀往後最多 `width` 個空段的第一個 `n`，**隨機挑一個**，`width` 每輸一次加倍（最多 64）。最多試 `ATTEMPTS`（16）次。
7. **整個池都是在線的前綴才拒絕**，並在記錄寫一行：`prefix pool <池> is exhausted: no prefix for <用戶>`。NAS 只看到 Access-Reject。

**穩定性：** 用戶離線不超過 `HOLD_SECONDS`，而且前綴沒有被搶，回來就拿到同一個。搶 `released` 的條件是「起點往後 1,024 個 `n` 裡沒有空的」，不是「整個池沒有空的」，所以池很滿時，即使池裡別處還有空的，也可能搶最舊的 `released`；離線更久、或被搶了，拿到的可能不同。**不是保證**，下面有模擬與量測。

### `accounting.lua` 做了什麼

- **Start、Interim：** 用 `Delegated-IPv6-Prefix`（文字相等）加用戶名稱找到那一列，變成 `active`，到期時間設成從現在起 3 個 Interim 間隔（900 秒）。封包沒帶前綴時退回只用用戶名稱（一個用戶只有一個前綴）。前綴不是這個用戶的就不動，記一行。
- **Stop：** 把那一列改成 `released`（`session_id`、`nas` 清成空，留 `HOLD_SECONDS`），但只動這個 session 的 `active` 列；晚到的舊 session 的 Stop 動不到新的，也動不到還沒確認的預留（預留過期由回收刪掉）。Interim-Update 只更新沒有 session 或同一個 session 的列；`released` 的列只有「沒收到 Interim 而過期」的（還帶著同一個 session）能被它改回 `active`，Stop 與 NAS 重開機釋放的不行，所以 Stop 之後晚到的 Interim 不會把前綴改回來。Start 可以：快速重連時舊 session 的 Stop 先到、新 session 的 Start 後到，前綴還是他的。
- **Accounting-On、Off：** NAS 重開機，這台 NAS 的 `active` 全部改成 `released`。用戶重新連上來，先拿到原來的前綴。
- **請求觸發的回收：** 每分鐘由第一個請求處理：沒收到 Interim 而過期的 `active` 改成 `released`，過期的預留與 `released` 刪掉（多個節點只有一個去做）。
- **晚到的 Start 補不回來：** 預留過期刪掉之後才到的 Start 找不到那一列，只記一行，不會替它補回來（文字反推不出 `n`）。預留 120 秒夠 NAS 的 Start 正常送到。

## 池很滿的時候

池很滿有兩件事要看：登入會不會變慢或被拒絕，以及用戶離線再回來還拿不拿得到同一個前綴。前者用真正的伺服器量，後者主要靠模擬，兩者分開說。

### 登入的速度（伺服器實測）

容量 65,536（`2001:db8::/40` 切 /56），單節點（7 個 CPU 的伺服器），新用戶 64 並行第一次登入。池先用 `allocate.lua` 填到指定的使用率，分兩種：

- **全是預留的：** 剛填滿、還沒有 `released` 列（最壞的情況，窗口裡沒有空位又沒有 `released` 可搶，要整池找空位）。
- **全是 `released` 的：** 正常營運時用戶離線後的樣子，窗口裡沒有空位時搶最舊的 `released`。

| 使用率 | 池裡的前綴 | 新用戶每秒 | p50 | p99 | 被拒絕（池還有空位） |
| --- | --- | --- | --- | --- | --- |
| 90% | 預留 | 4,125 | 11 ms | 71 ms | 0 |
| 95% | 預留 | 1,643 | 24 ms | 155 ms | 0 |
| 99% | 預留 | 334 | 161 ms | 457 ms | 0 |
| 90% | `released` | 5,114 | 10 ms | 40 ms | 0 |
| 99% | `released` | 3,383 | 11 ms | 67 ms | 0 |
| 100% | `released` | 1,465 | 30 ms | 34 ms | 0 |

- **有 `released` 時比較快：** 窗口只看起點往後 1,024 個 `n`，窗口裡沒有就直接搶最舊的 `released`，不用整池掃。預留填滿的情況沒有 `released` 可搶，要整池找空位，使用率越高越慢（99% 每秒只有幾百個）。
- **越滿越慢，所以要告警：** 這是為什麼監控建議使用率超過 85% 就告警。
- **不會誤拒：** 整個池都是在線（`reserved`、`active`）的前綴才拒絕。如果搶輸就找「下一個」空的，同時進來的人每一輪都搶同一個 `n`，每輪只有一個贏：在 90% 使用率就有 2.5% 的登入被拒，池還有六千多個空位。所以搶輸再來時，從往後的候選裡隨機挑一個，候選數每輸一次加倍；搶 `released` 也一樣（從最舊的 8 列起，每輸一次加倍）。
- **沒有重複：** 每次量測後資料表的列數都等於不同前綴的數目，每個用戶一個前綴（唯一索引擋著）。
- **沒量到的：** 多節點叢集（寫入經領導節點，搶輸的行為可能不同）、64 並行搶 `released` 的叢集重試、容量更大的池（模擬到 100 萬個前綴趨勢相同，沒有用伺服器量過）。

### 用戶回來拿到同一個前綴嗎（模擬）

這部分是模擬（容量 65,536，用戶的在線時間是指數分佈，平均一天；離線時間是指數分佈，或 80% 是平均 3 分鐘的短斷線、20% 是長離線；暖機之後量每次再登入是不是拿到上一次的前綴），由兩位獨立寫的模擬互相核對過，不是伺服器實測。重點：

- **只靠雜湊起點（沒有 `released`）不穩定。** 有真實的上下線流動時，使用率 90% 約 39–87% 拿回同一個（取決於在線比例與離線時間的分佈），95% 約 62–68%，99% 約 39–53%；使用率 50–70% 也只有約 76–93%。容量放大到 100 萬結果相同。
- **NAS 或 BNG 重開機最差。** 一批在線用戶同時斷線、隨機順序回來，只靠雜湊起點：使用率 70% 約 51% 拿回同一個，90% 約 28%，95% 約 18%。
- **保留 `released` 後（`HOLD_SECONDS` 24 小時）：** 同樣的重開機情況 100% 拿回；一般上下線流動，90% 使用率約 93–98%（視保留時間與離線分佈），95% 約 89%，99% 約 82%。短斷線（保留期內回來）幾乎都拿回；超過保留期、或池滿到前綴被搶走的用戶，拿到的是另一個。
- **穩定性的上限取決於總用戶數與池容量的比：** 離線的用戶越多、空位越少，被搶走的越多。總用戶數不超過池容量時，保留能接近 100%；總用戶數遠超過容量，一定有人要換前綴。要穩定的前綴，**池的容量要大於用戶總數**，不只是使用率要低。
- **不做的：** 雙重雜湊探測（只多 1–10 個百分點）、分區塊（持平到更差）、使用率超過門檻就拒絕新用戶（需求 95%、門檻 90% 時 35% 的登入被拒，比換前綴更糟）；只告警。

模擬與量測的差別、第一版的問題與修正的細節在這個變更的紀錄裡。

## 監控

範例設定檔有三個命名查詢（[usage 2.20 節](../usage.md#220-命名查詢-queries)）：

```toml
[queries.prefix_leases]
sql = "SELECT pool, state, count(*) AS prefixes FROM prefix_leases GROUP BY pool, state"
labels = ["pool", "state"]
value = "prefixes"

[queries.prefix_capacity]
sql = "SELECT pool, 1 << (dlen - plen) AS capacity FROM prefix_pools"
labels = ["pool"]
value = "capacity"

[queries.prefix_hold_age]
sql = "SELECT pool, strftime('%s', 'now') - min(updated) AS age FROM prefix_leases WHERE state = 'released' GROUP BY pool"
labels = ["pool"]
value = "age"
```

成為指標 `rradius_query_prefix_leases{pool,state}`（各狀態的前綴數）、`rradius_query_prefix_capacity{pool}`（容量）與 `rradius_query_prefix_hold_age{pool}`（最舊的 `released` 釋放了多久，單位秒）。每個池的**使用率只算 `reserved` 與 `active`**（叢集用 `max()`，不要 `sum()`）：`released` 是留給離線用戶的，要用時可以被搶，把它算進去，使用率在穩態會一直接近 100%。

```promql
sum by (pool) (max by (pool, state) (rradius_query_prefix_leases{state=~"reserved|active"})) / max by (pool) (rradius_query_prefix_capacity)
```

- **使用率超過 85% 就告警**：池越滿，能留給離線用戶的越少。
- **`prefix_hold_age` 就是現在實際保證的保留時間。** 池沒有 `released` 列時沒有樣本；有 `released` 列但最舊的只有幾分鐘，表示池已經滿到前綴一釋放就被搶，保留不到 `HOLD_SECONDS`，穩定性會掉。這個值遠小於 `HOLD_SECONDS` 就該擴充池。
- 這些查詢在池完全空的時候沒有樣本，告警要用 `absent()` 補，或把這個池當作 0。

這些查詢都不掃整個池：`prefix_leases` 的 `GROUP BY pool, state` 走覆蓋索引 `prefix_leases_pool_state`，搶最舊的 `released` 走部分索引 `prefix_leases_released`，`prefix_hold_age` 走 `prefix_leases_expiry`（Accounting-On／Off 釋放一台 NAS 的前綴走 `prefix_leases_nas`），`prefix_pools` 只有幾列。表只有在線與最近離線的用戶那麼大，所以查詢的成本和池的容量無關。

## 和位址池（第 14 章）合併成雙堆疊

很多用戶要同時有 IPv4 位址和委派前綴。伺服器只有**一個最上層腳本**處理計費，所以兩個範例不能直接一起跑：

1. **認證：** `[[auth.handlers]]` 依序列 `sql`（驗密碼）、第 14 章的 `allocate.lua`、這一章的 `allocate.lua`。`all_accept` 會一個一個執行，任一個拒絕就停。兩個腳本要放在不同的檔名（例如 `ip-allocate.lua`、`prefix-allocate.lua`）。
2. **資料表：** 兩份 migration 的檔案編號不能撞，兩邊都有 `subscribers` 和 `housekeeping`，合併成一張：`subscribers` 要有 `pool`（IPv4 的池）和前綴池的 `pool`（兩個欄位，或兩個池同名）。
3. **計費：** 把兩個 `accounting.lua` 合併成一個。一個計費封包可以同時有 `Framed-IP-Address` 和 `Delegated-IPv6-Prefix`：位址的部分照第 14 章處理（有 `Framed-IP-Address` 才做），前綴的部分照這一章處理；Accounting-On、Off 兩邊的釋放都要做。請求觸發的回收兩邊各用自己的 `housekeeping` 列。
4. **一個認證兩邊都拿不到時的處理：** 其中一邊用盡時整個登入被拒絕（`all_accept`）；要「有 IPv4 沒前綴也放行」，就把前綴腳本的 `return 'reject'` 改成不設屬性就 `return 'accept'`。

這一章沒有附合併後的腳本，因為沒有測過；改的時候用兩邊現有的測試當底。

## 換成自己的資料

1. 刪掉 `migrations/2_demo_data.sql`，換成自己的池和用戶；**正式環境第一次啟動前就決定好**池的常數，套用過的 migration 檔不能再改，要加池就新增下一個編號的檔案。
2. 池的常數：`base` 用前綴高 64 位元的整數（`python3 -c "print(hex(int.from_bytes(__import__('ipaddress').ip_network('2001:db8:100::/48').network_address.packed[:8], 'big')))"` 可以算出來），`plen` 與 `dlen` 照你的位址計畫。
3. 換掉所有 `*.secret` 的內容，見[第 11 章](11-operations.md#更換密碼)。
4. 把 `[[radius.clients]]` 改成你的 BNG 位址。
5. **池的大小要留空間：** 使用率超過 90% 之前就擴充（新增一個池，或換更長的 `plen`）。

## 已知限制

- **前綴不保證穩定。** 起點被別人佔走時拿到不同的；池越滿越不穩（見上面）。沒有歷史表記錄「上次用過哪一個」。
- **池很滿時很慢。** 使用率超過 95% 時新用戶每秒約三千個以下，99% 約一千個。
- **回收一次最多處理 1,000 列（過期的 `active` 轉成 `released`、過期的預留與 `released` 刪除各一批），每分鐘一次。** 大量租約同時到期（例如整台 BNG 當機）時，到期還沒被刪的列仍佔著前綴，要幾分鐘才清完；池很滿時這段時間可能誤判用盡。
- **亂序的 Start 可能讓已離線用戶的前綴多佔一段時間：** Stop 先到、Start 後到時，Start 會把 `released` 改回 `active`，到期沒有 Interim 才再轉成 `released`（多佔約 900 秒，再重算保留）。封包沒帶前綴、退回用用戶名稱找列時，舊 session 晚到的 Start 或 Interim 也可能確認新的預留，新 session 的 Start 會再覆蓋。
- **池用盡時的拒絕要掃整個池：** 沒有 `released` 列、窗口沒有空位時，要整池找空位才能確定用盡，成本和已佔用的列數成正比；池一直在用盡的狀態時，這會讓被拒絕的登入變貴，要靠使用率告警避免。
- **晚到的 Start 補不回過期的預留**（見計費）。
- **沒有 WAN 的 /64 池。** 回的 `Framed-IPv6-Pool` 只是池的名稱，由 BNG 自己的池決定連線本身的位址。
- **沒有附雙堆疊的合併腳本。**
- **量測是容量 65,536 的池。** 更大的池沒有用伺服器量過。
- **沒有對真實 BNG 驗證。** 屬性用 RFC 4818 的 `Delegated-IPv6-Prefix`，計費封包要帶它。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 一直是 Reject，記錄有 `is exhausted` | 池真的沒有空位：查 `rradius_query_prefix_leases` 與容量；刪掉過期的、擴充池 |
| 一直是 Reject，記錄有 `is not in prefix_pools` | `subscribers.pool` 在 `prefix_pools` 沒有對應的池 |
| 登入成功，但那一列一直是 `reserved`，120 秒後被刪掉、前綴發給別人 | Start 沒有到，或 Start 的 `Delegated-IPv6-Prefix` 和表裡的文字不同（壓縮、大小寫）：看記錄裡 `is not in prefix_leases` 那行的前綴 |
| 寫不進池：`CHECK constraint failed` | `base` 沒有對齊 `plen`，或 `dlen` 小於 `plen`、超過 64，或容量超過 2 的 30 次方 |
| 用戶每次登入拿到不同的前綴 | 池使用率太高，起點被別人佔走（見「池很滿的時候」），或預留過期後那個位置被別人拿走了 |
