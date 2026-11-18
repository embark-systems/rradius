# 22. 雙堆疊線路（線路身分加固定位址、IPv4 位址池與 IPv6 前綴池）

**情境：** 固網寬頻 IPoE 的用戶沒有帳號密碼，靠 BNG 轉送的 DHCP Option 82 認出是哪一條線（[第 16 章](16-line-identity.md)）。這一章在同一個登入裡回完整的雙堆疊：

- 有**固定資料**的線路（商業用戶）：固定的 IPv4 位址、路由給用戶的子網段、WAN 前綴、委派前綴，和[第 17 章](17-static-ip.md)一樣，但身分是線路，不是帳號密碼。
- 沒有固定資料的線路：從方案指定的 **IPv4 位址池**（[第 14 章](14-ip-pool.md)）拿一個位址，從**前綴池**（[第 19 章](19-prefix-pool.md)）拿一個委派前綴，計費確認與釋放。
- IPv4 與 IPv6 各自決定：只固定 IPv4 的線路，前綴仍從池拿。

這一章是第 14、16、17、19 章合起來的完整做法，附合併後的認證與計費腳本和測試；那幾章說「合併腳本沒有附」的缺口就是這一章。池與前綴的原理（黏著、雜湊起點、released、使用率）不重複，請看那兩章。

範例目錄：[`rradius-server/examples/scenarios/dual-stack-lines/`](../../rradius-server/examples/scenarios/dual-stack-lines/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔：認證串接只有一個 `lua` 處理器；`[lua]` 的腳本處理計費；`[[radius.clients]]` 只列 BNG；兩個命名查詢監控池 |
| `line.lua` | 認證：線路身分、固定資料的檢查與回覆、IPv4 池、前綴池、前綴用盡時放回剛預留的 IPv4 |
| `accounting.lua` | 計費：同時確認、延長、釋放位址與前綴 |
| `migrations/1_schema.sql` | `plans`、`lines`（含固定資料欄位）、`ip_pool`、`prefix_pools`、`prefix_leases`、`housekeeping` |
| `migrations/2_demo_data.sql` | 三個示範方案、十三條示範線路、三個前綴池與兩個位址池 |
| `*.secret` | 叢集和 BNG 的共用密碼（示範值） |

## 步驟

### 1. 建置、檢查、啟動

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server check --config examples/scenarios/dual-stack-lines/rradius.toml
./target/release/rradius-server server --config examples/scenarios/dual-stack-lines/rradius.toml
```

範例的 `[[radius.clients]]` 是 `10.60.0.0/24`（BNG 的位址），測試時要從那個網段送請求，或暫時改成 `127.0.0.1`。

### 2. 試兩條線

用 `rradius-bench` 送請求（`--secret` 是 `bng.secret` 的內容；`26=hex:` 是廠商 3561 的子屬性，編號 1 是 circuit-id、2 是 remote-id，寫法見[第 16 章](16-line-identity.md)）。線路 `eth 1/1/1:100`（alice，全動態）：

```bash
./target/release/rradius-bench run --server 127.0.0.1 --secret "$(cat examples/scenarios/dual-stack-lines/bng.secret)" \
  --user x --load pap,concurrency=1 --requests 1 \
  --attr 26=hex:00000de9010f65746820312f312f313a313030 \
  --attr 26=hex:00000de902076f6c742d61 \
  --attr 31=text:00:11:22:aa:bb:cc
```

結果是 accept；回覆帶一個 `10.77.0.x` 的 `Framed-IP-Address`、一個 `2001:db8:1xx::/56` 的 `Delegated-IPv6-Prefix`、`Framed-IPv6-Pool` `res6`、`Filter-Id` `home-100m`，以及 `Class` `alice`。線路 `eth 1/1/1:103`（dave，全固定）把第一個 `--attr` 的最後三個位元組 `313030` 換成 `313033`：回 `Framed-IP-Address` `203.0.113.10`、`Framed-Route` `203.0.113.8/29 0.0.0.0 1`、`Framed-IPv6-Prefix` `2001:db8:1:1::/64`、`Delegated-IPv6-Prefix` `2001:db8:f00::/56`，沒有 `Framed-IPv6-Pool`。（`rradius-bench` 只顯示 accept 或 reject 的數量；回覆屬性的內容看 `tests/scenarios_test.rs` 的 `test_dual_stack_lines`。）

### 3. 看結果

拒絕的原因都寫在記錄裡：線路的原因是 `dual stack:` 開頭，池用盡是 `ip pool <名稱> is exhausted` 或 `prefix pool <名稱> is exhausted`（Access-Reject 不帶屬性，BNG 只看到拒絕）。

## 設定說明

### 為什麼一個腳本做完所有事

認證串接（`[[auth.handlers]]`）的處理器之間**不傳值**：前一個處理器設的回覆屬性，後一個讀不到；`auth_user` 是 `User-Name` 正規化後的值，不是處理器能指定的，腳本也不能共用模組（`require` 被關掉）。IPoE 的用戶沒有可用的 `User-Name`，所以第 14 章和第 19 章「一個處理器分配位址、一個處理器分配前綴、`all_accept` 串起來」的做法在這裡行不通：兩個池腳本都不知道這是哪個用戶。另外，只有同一個腳本能在前綴用盡時，把剛為這個登入預留的 IPv4 位址放回去；兩個處理器時，前面預留的位址會留著直到到期回收。

所以 `line.lua` 一個腳本依序做：

1. 線路身分（[第 16 章](16-line-identity.md)）：正規化 circuit-id 與 remote-id、查 `lines`、停用與 MAC 鎖。
2. IPv4：`lines.ipv4` 有值就回固定的（加 `Framed-IP-Netmask`，有 `routed_v4` 就加 `Framed-Route`），沒有就從方案的 IPv4 池拿一個（沿用綁定、黏著、隨機候選）。
3. IPv6：`wan_v6` 或 `pd_v6` 有任何一個值，就只回有值的（`Framed-IPv6-Prefix`、`Delegated-IPv6-Prefix`），沒有就從方案的前綴池拿一個（沿用、雜湊起點、released、搶最舊的 released），並回 `Framed-IPv6-Pool`。
4. 前綴用盡時，把這個登入剛預留的 IPv4 放回去（只動這個請求剛預留、還沒被確認的那一列，不動原本就有的綁定），然後預設拒絕。

池的鍵是 `lines.subscriber`，所以一個 subscriber 只能有一條線（`lines_subscriber` 唯一索引）。回給 BNG 的 `Class` 也是它。

### 計費靠 Class

IPoE 的計費封包沒有可靠的 `User-Name`。`line.lua` 回的 `Class` 是 subscriber 的名稱，BNG 在計費封包帶回它，`accounting.lua` 靠它找到這條線和它的兩個池。**BNG 必須在計費封包帶回 `Class`**（這是標準做法，多數 BNG 預設就這樣）；沒帶時封包照常回覆，但不處理，記一行 `dual stack accounting: no Class`。

一個計費封包可以同時帶 `Framed-IP-Address` 與 `Delegated-IPv6-Prefix`，有哪個就做哪個的部分：

- Start、Interim-Update：位址與前綴都確認成 active，到期時間延長到從現在起三個 Interim 間隔。
- Stop：位址放回空閒（記下上一個用的人，黏著用）；前綴改成 released，留給這條線 24 小時。
- Accounting-On、Off：這台 BNG 的 active 位址與前綴全部放回。

固定資料的線路在池裡沒有列，對應的部分不做任何事，也不記錄，所以固定線路的計費封包一樣正常回覆。

### 示範資料

| 方案 | IPv4 池 | 前綴池 | 說明 |
| --- | --- | --- | --- |
| `residential` | `res4`（8 個位址） | `res6`（`2001:db8:100::/48`，256 個 /56） | 住宅 |
| `business` | `biz4`（3 個位址） | `biz6`（`2001:db8:200::/52`，256 個 /60） | 商業 |
| `metro` | `res4` | `tiny6`（2 個 /64） | 前綴池很小，用來看前綴用盡 |

`olt-a` 的線路：`:100` alice、`:101` bob（住宅，全動態）；`:102` carol（MAC 鎖）；`:103` dave（商業，IPv4 與 IPv6 全固定）；`:104` erin（停用）；`:105` frank（只固定 IPv4）；`:106` 到 `:109`（商業，全動態，第 4 個遇到 IPv4 用盡）；`:110` 到 `:112`（metro，第 3 個遇到前綴用盡）。

### 一邊用盡時

預設（`ALLOW_V4_ONLY = false`）整個登入拒絕，記錄寫 `prefix pool <名稱> is exhausted: no prefix for <用戶>`（或 IPv4 池的 `ip pool ... is exhausted`）。要「有 IPv4 沒前綴也放行」，把 `line.lua` 開頭的 `local ALLOW_V4_ONLY = false` 改成 `true`：前綴拿不到時保留 IPv4，只回 IPv4 的屬性，用戶沒有 IPv6 但能上網；用戶之後登入時再試著拿前綴。IPv4 用盡時一律拒絕，因為沒有 IPv4 的線路通常也上不了網。

### 資料怎麼維護

```sql
-- 新增一條動態線路
INSERT INTO lines (remote_id, circuit_id, subscriber, plan) VALUES ('olt-a', 'eth 1/1/2:100', 'zoe', 'residential');
-- 給一條線固定的 IPv4 與前綴（欄位是 NULL 的就不是固定的）
UPDATE lines SET ipv4 = '203.0.113.30', pd_v6 = '2001:db8:f10::/56' WHERE subscriber = 'zoe';
-- 停用
UPDATE lines SET enabled = 0 WHERE subscriber = 'zoe';
```

`remote_id`、`circuit_id` 要寫成正規化後的值（見第 16 章）；IPv6 前綴請一律寫成小寫、壓縮的寫法。固定的 `ipv4`、`wan_v6`、`pd_v6` 各有唯一索引，兩條線不能登記同一個值。

線路從動態改成固定（或相反）時，它在池裡原有的列不會立刻消失：位址或前綴等計費停了、到期後由請求觸發的回收放回；在那之前它佔著池的一個位置。

### 叢集

認證與計費的寫入都經過叢集的領導節點（登入寫 `lines` 的最後看到的 MAC 與時間，動態線路還要預留位址與前綴）。叢集沒有領導節點時，登入會失敗，連固定資料的線路也一樣；不要求「最後看到」的資訊時，把腳本最後那個 `UPDATE lines SET last_mac ...` 拿掉，固定線路就不再依賴領導節點。

### 監控

`rradius.toml` 有兩個命名查詢：`pool_usage`（每個 IPv4 池、每個狀態的位址數）和 `prefix_usage`（每個前綴池、每個狀態的前綴數），成為指標 `rradius_query_pool_usage{pool,state}` 與 `rradius_query_prefix_usage{pool,state}`。更完整的前綴池指標（容量、released 的年齡）與告警，照[第 19 章](19-prefix-pool.md)「監控」加進來。

## 換成自己的資料

1. 刪掉 `migrations/2_demo_data.sql`，換成自己的方案、池和線路；**正式環境第一次啟動前就決定好**池的常數（前綴池的 `base`、`plen`、`dlen`，見第 19 章），套用過的 migration 檔不能再改。
2. 換掉所有 `*.secret` 的內容，見[第 11 章](11-operations.md#更換密碼)。
3. 把 `[[radius.clients]]` 改成你的 BNG 位址。
4. 用你的 BNG 實際送出的值填 `lines`（看第 16 章「Option 82 怎麼到 RADIUS」），確認 BNG 在計費封包帶回 `Class`。
5. 池的大小要留空間：使用率超過 90% 之前就擴充（第 14、19 章）。

## 規模

量測的是**合併後每次登入多出的成本**：同一台機器、同一次量測，三個範例各自的登入吞吐（Apple M4 Max，14 CPU，macOS，伺服器與量測程式同一台，單節點；64 個並行的 PAP 登入，各 2,000 個不同的用戶第一次登入，`rradius-bench`；IPv4 池與前綴池各 16,384 個，事先分散地佔了 43%，量測結束時約 55%；各跑兩次）：

| 範例 | 每秒登入 | p50 | p99 |
| --- | --- | --- | --- |
| 第 14 章位址池 | 約 14,500（14,473、14,674） | 約 3.6 ms | 約 21 ms |
| 第 19 章前綴池 | 約 11,200（11,075、11,288） | 約 4.8 ms | 約 22 ms |
| 這一章，動態線路（IPv4 加前綴） | 約 6,100（6,102、6,172） | 約 9.4 ms | 約 34 ms |

動態線路每次登入約 163 微秒，大約是兩個單獨的池（約 69 加 90 微秒）的和，再加線路的查詢與最後看到的寫入；沒有拒絕、沒有重複分配。比較慢的單獨範例（前綴池）的一半是約 5,600，所以沒有低於一半。這些是同一台機器上的相對數字，不是 BNG 現場的容量；量測時線路用 `User-Name` 當名稱（bench 會輪流換它），不是 Option 82 屬性，兩者對成本的差別只是取一個屬性。

固定線路不碰池，登入只有查詢加一次寫入，比動態線路便宜。

## 已知限制

- **沒有對真實 BNG 驗證。** 屬性用標準的 `Framed-*` 和 RFC 4818 的 `Delegated-IPv6-Prefix`；BNG 是否接受、`Framed-Route` 的寫法、是否帶回 `Class`，要看你的 BNG 文件。
- **固定值不和池比對。** 固定的 `ipv4` 如果落在 `ip_pool` 的位址範圍內，或固定的 `pd_v6` 落在前綴池裡，池可能把同一個值再分給別人；固定位址請放在池以外的網段。
- **線路只在登入時檢查。** 停用一條線不會中斷已經在線的連線（要中斷需要 CoA／Disconnect，企業版）。
- **前綴不保證穩定。** 起點被別人佔走時拿到不同的，池越滿越不穩（第 19 章）。
- **一個 subscriber 一條線。** 要一個用戶多條線，要改池的鍵。
- **計費只認 `Delegated-IPv6-Prefix`。** 只帶 `Framed-IPv6-Prefix`（WAN 的前綴）的封包不會確認前綴。
- **沒有 WAN 的 /64 池。** `Framed-IPv6-Pool` 只是池的名稱，連線本身的位址由 BNG 的池決定。
- **只認 RFC 4679 的屬性和 `NAS-Port-Id`**，同第 16 章。
- **MAC 鎖和線路身分的安全限制**同第 16 章：線路身分不是祕密，client 清單只列 BNG。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 一直是 Reject，記錄有 `dual stack: unknown line` | 資料表裡的值和 BNG 送的不同，同第 16 章 |
| 記錄有 `has routed_v4 or netmask but no ipv4` | 線路登記了 `routed_v4` 或 `netmask`，卻沒有 `ipv4`；補上 `ipv4` 或清掉它們 |
| 記錄有 `dual stack: bad ... for` | 固定資料的值不合法（位址、子網段、遮罩、前綴），記錄寫出用戶、欄位和值；不會悄悄退回動態 |
| 記錄有 `prefix pool ... is exhausted` | 前綴池沒有空位；擴充池，或把 `ALLOW_V4_ONLY` 改成 `true` |
| 計費封包沒有改變池 | 記錄有 `no Class`：BNG 沒有在計費封包帶回 `Class` |
| 登入成功但沒有 `Delegated-IPv6-Prefix` | 線路的 `pd_v6` 是 NULL 又設了 `wan_v6`（只回有值的），或 `ALLOW_V4_ONLY` 是 `true` 而且前綴池滿了 |
