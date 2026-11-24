# 14 情境實作：寬頻與 ISP

> 本章讓你做到：
> - 從資料表的位址池分配 IPv4 位址與 IPv6 委派前綴，用計費確認與釋放
> - 用 DHCP Option 82 的線路身分認證沒有密碼的 IPoE 用戶
> - 回固定位址、路由子網段與固定前綴，資料有錯時拒絕而不是悄悄改成動態
> - 依方案限制同時連線數與一段期間的流量
> - 同一個方案依 BNG 的廠商回不同的速率屬性

前置條件：讀過[第 03 章](03-quick-start.md)，會複製情境、啟動伺服器、用 `rradius-bench` 登入；知道[第 07 章](07-users-and-chains.md)的驗證鏈與[第 10 章](10-lua-policies.md)的 Lua 腳本在做什麼。本章全部在一台機器上做，不需要真的 BNG。

本章小節：

- [這些情境共同的做法](#這些情境共同的做法)
- [準備實驗目錄](#準備實驗目錄)
- [IPv4 位址池：ip-pool](#ipv4-位址池ip-pool)
- [IPv6 前綴池：prefix-pool](#ipv6-前綴池prefix-pool)
- [依線路身分認證：line-identity](#依線路身分認證line-identity)
- [雙堆疊線路：dual-stack-lines](#雙堆疊線路dual-stack-lines)
- [固定位址與前綴：static-ip](#固定位址與前綴static-ip)
- [方案與用量控制：usage-control](#方案與用量控制usage-control)
- [依 BNG 回速率：rate-limit](#依-bng-回速率rate-limit)
- [單檔範例](#單檔範例)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 這些情境共同的做法

本章的七個情境都在原始碼的 `rradius-server/examples/scenarios/` 底下，都是單機、資料放在叢集資料庫裡。它們用的是同一組做法，先看懂這幾點，後面每一節只講各自不同的地方。

**驗證鏈先驗密碼，再跑政策。** 有密碼的情境用 `[auth]` 的 `policy = "all_accept"`，第一個處理器是 sql 處理器驗密碼，第二個是 lua 處理器做分配或檢查。`all_accept` 依序執行，任一個拒絕就停，所以密碼錯的人不會走到 lua 處理器，也就不會替他佔位址或連線位。lua 處理器設的回覆屬性會放進 Access-Accept。

**計費腳本負責確認與釋放。** 有 `[auth]` 時，認證交給驗證鏈，`[lua]` 的 `script` 只處理計費。計費腳本回傳 `accept` 伺服器才回 Accounting-Response；其他結果不回應，NAS 會重送。所以計費腳本的每個寫入都寫成「同一個封包處理兩次，結果和一次相同」。

**狀態三段：預留、在用、放回。** 位址、前綴、連線位都有一個狀態欄：

| 狀態 | 什麼時候進入 | 什麼時候離開 |
| --- | --- | --- |
| `reserved`（預留） | 認證通過時 | 收到 Accounting-Start 變成 `active`；120 秒內沒收到就放回 |
| `active`（在用） | Accounting-Start 或 Interim-Update | Accounting-Stop、NAS 送 Accounting-On／Off，或 3 個 Interim 間隔沒收到消息 |
| 放回 | 上面任一種結束 | 下一個人拿走 |

腳本在 Access-Accept 裡回 `Acct-Interim-Interval = 300`，要求 NAS 每 300 秒送一次 Interim-Update。NAS 當機沒送 Stop 的 session，3 個 Interim 間隔（900 秒）沒有消息就算過期，在下一次回收時放回。

**用「比較後更新」搶資源。** 腳本先從本機的資料庫副本讀出幾個候選，再用一條有條件的 `UPDATE`（例如 `WHERE state = 'free'`）去搶。`sql_execute` 回傳受影響的列數，是 1 才算搶到。寫入都要經過叢集的 leader 一個一個做，所以兩個同時進來的請求不可能搶到同一個；本機副本可能稍舊，頂多多試一個。這個做法在多個節點上一樣成立，見[第 16 章](16-cluster.md)。

**請求觸發的回收。** 過期的預留與沒收到 Interim 的 session 不會自己消失，要有人放回。這些情境沒有排程器，而是由請求順便做：`housekeeping` 表每個工作一列，記著上次執行的時間；腳本發現超過 60 秒，就用 `UPDATE housekeeping … WHERE last_run < ?` 搶一次，搶到的那個請求才去回收，一次最多 1000 列。所以多個節點每個週期也只做一次。

**拒絕的原因寫在日誌。** Access-Reject 不帶處理器設的回覆屬性，NAS 只知道被拒絕。腳本用 `log()` 把原因寫進伺服器日誌，形式是：

```text
… INFO rradius_core::lua_engine: script allocate.lua (radius/authentication, client localhost): ip pool business is exhausted: no address for hank
```

**監控用具名查詢。** 有池的情境都有 `[queries.<name>]`：每個節點每 30 秒（`interval_ms` 預設 `30000`）查一次自己的副本，結果變成指標 `rradius_query_<name>`；`rradiusd query <name>` 則是馬上查一次，印成表格。具名查詢改了要重啟才生效，細節見[第 09 章](09-external-databases.md)。

本章情境用到的設定鍵（完整說明見[附錄 A](90-config-reference.md)）：

| 設定 | 預設值 | reload | 本章的用法 |
| --- | --- | --- | --- |
| `auth.policy` | `first_match` | 可以 | 有密碼的情境都是 `all_accept` |
| `auth.handlers` | 無 | 可以 | 一個 sql 處理器加一個 lua 處理器，或只有一個 lua 處理器 |
| `lua.script` | 必填 | 可以 | 有 `[auth]` 時只處理計費 |
| `sessions.list`、`show` | 無 | 指令 | 直接查池或連線位的表 |
| `queries.<name>.sql`、`labels`、`value` | 無、`[]`、`value` | 要重啟 | 池的使用量變成指標 |
| `queries.<name>.interval_ms` | `30000` | 要重啟 | 情境都用預設 |
| `log.trace.clients`、`users`、`minutes` | `[]`、`[]`、必填 | 可以 | 實驗時看回覆屬性 |

> **注意：** 示範資料都在 `migrations/2_demo_*.sql`，帳號、密碼、池都是公開的示範值。拿情境改成正式環境時，刪掉這個檔案，改用你自己的資料。套用過的 migration 檔不能再改，要改結構就新增下一個編號的檔案。

**用發行檔跑範例，與一次只跑一個。** 發行壓縮檔 `rradius-<版本>-<平台>.tar.gz` 解開後，目錄結構和原始碼 repo 相同（執行檔在 `bin/`，範例在 `rradius-server/examples/`），第 03 章的 `export PATH="$PWD/target/release:$PATH"` 在解開後的根目錄改成 `export PATH="$PWD/bin:$PATH"`，其他指令和路徑照寫。情境都用標準埠（1812、1813、TACACS+ 的 49），同一台機器上同時啟動兩個會互相衝突：一次只跑一個，或改埠。範例都有自動測試（`rradius-server/tests/scenarios_test.rs` 照這幾章的方式啟動每個情境，送出請求並比對結果），所以照著複製的設定檔能啟動；`.secret` 檔都是 `change-me` 一類的示範值，正式環境要換掉（見第 03 章〈換掉範例密碼〉與第 18 章〈更換密碼〉）。

## 準備實驗目錄

每個情境都照第 03 章的方式複製出來再跑，不要在原始碼目錄裡直接啟動。以 ip-pool 為例，在原始碼 repo 的根目錄執行：

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab
$ cp -R rradius-server/examples/scenarios/ip-pool ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/ip-pool/rradius.toml
$ cd ~/rradius-lab/ip-pool
$ for f in *.secret; do openssl rand -hex 32 > $f; done; chmod 600 *.secret
$ rradiusd check --config rradius.toml
configuration ok
```

最後一行之前的 `for` 迴圈把情境附的公開密碼換成隨機值；不換的話，`check` 會對每個 `.secret` 檔印出權限與公開值的警告，設定本身還是好的。

`rradius-bench` 不會印出回覆屬性。要看伺服器回了什麼，在 `rradius.toml` 最後加上追蹤，名稱寫這個情境的用戶端名稱：

```toml
# rradius.toml（加在檔案最後）
[log.trace]
clients = ["localhost"]
minutes = 60
```

`[log.trace]` 可以 reload，60 分鐘後自己停止。開了之後，每個請求與回應都會在日誌多一行 `rradius_trace`，密碼顯示成 `<hidden>`：

```text
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Framed-IP-Address = 10.77.0.4, Acct-Interim-Interval = 300"
```

`code=2` 是 Access-Accept、`3` 是 Access-Reject、`5` 是 Accounting-Response。追蹤的完整說明見[第 17 章](17-monitoring.md)。

接著在一個終端機啟動伺服器（`rradiusd serve --config rradius.toml`），在另一個終端機做每一節的測試。本章用到 `rradius-bench` 的三種負載：

- `pap`：送一筆登入。
- `acct-start`：送一筆 Accounting-Start，session ID 每次執行都不同。
- `session`：依序送登入、Start、Stop，三個用同一個 session ID，而且把 Access-Accept 裡的 `Class` 與 `Framed-IP-Address` 帶進兩筆計費，就像真的 NAS 會做的那樣。

要多帶屬性時用 `--attr 類型編號=型別:值`，例如 `--attr 8=ip:10.77.0.4`、`--attr 31=text:00-11-22-33-44-55`、`--attr 52=int:2`。`--attr` 加在每一個送出的請求上，認證與計費都有。

## IPv4 位址池：ip-pool

### 目標

用戶通過密碼驗證後，從他所屬的池拿一個 IPv4 位址，放在 Access-Accept 的 `Framed-IP-Address`。同一個人重新登入拿回同一個位址；離線後，原用戶在一天內優先拿回上次的位址（黏著）；池用完了就拒絕，並在日誌寫明原因。

### 要不要由認證伺服器分配位址

很多 BNG 與 NAS 自己有位址池。認證伺服器只要回 `Framed-Pool` 的池名稱，NAS 就從自己的池發位址；這時不需要這個情境，在 sql 處理器的 `reply_query` 回 `Framed-Pool` 即可（見[第 03 章](03-quick-start.md)與[第 07 章](07-users-and-chains.md)）。下面幾種情況才要由認證伺服器分配：

| 情況 | 為什麼 |
| --- | --- |
| NAS 沒有位址池，或池太小、功能不夠 | 位址只能由認證伺服器給 |
| 同一批位址要給好幾台 NAS 共用 | 池在中央，才不會各台各發各的而衝突 |
| 要查「這個位址現在是誰在用」、稽核、限額 | 位址的歸屬都在資料表裡 |

### 資料表

`migrations/1_schema.sql` 建三個表：

| 表 | 內容 |
| --- | --- |
| `subscribers` | 帳號、密碼、`pool`（從哪個池拿位址）、`enabled` |
| `ip_pool` | 每個位址一列，主鍵是 `(pool, ip)`，所以不同的池可以有重疊的位址（例如不同的 VRF） |
| `housekeeping` | 回收工作上次執行的時間 |

`ip_pool` 的欄位：

- `state`：`free`、`reserved`、`active`，有 `CHECK` 限制。
- `username`、`session_id`、`nas`：目前用的人、計費確認的 session、哪一台 NAS。
- `expires`：到期時間。預留是 120 秒，在用是 3 個 Interim 間隔。
- `last_user`、`released`：位址放回時記下上一個用的人和時間，黏著用。
- `n`：位址在池裡的序號。同一個池從 1 起連續編號，新增位址時接著往下編。

`n` 不連續時，挑位址的隨機起點會集中在某些位址。四個索引（`ip_pool_user`、`ip_pool_state`、`ip_pool_last_user`、`ip_pool_expiry`）涵蓋每一條查詢的條件。SQLite 沒有統計資料時，條件只用到索引前幾欄的話，可能改走主鍵掃整個池，所以加欄位或改查詢時，索引要跟著改。

示範資料：`residential` 池是 `10.77.0.2` 到 `10.77.0.9`，共 8 個；`business` 池只有 3 個位址（`10.88.0.2` 到 `10.88.0.4`），卻有 `dave`、`frank`、`gina`、`hank` 四個用戶，用來看池用盡；`erin` 停用中。

### 設定重點

```toml
# rradius.toml（節錄，其他設定略）
[lua]
script = "accounting.lua"

[auth]
policy = "all_accept"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM subscribers WHERE username = ? AND enabled = 1"

[[auth.handlers]]
type = "lua"
script = "allocate.lua"

[sessions]
list = "SELECT username, nas, session_id, ip, updated FROM ip_pool WHERE state = 'active' AND expires > strftime('%s', 'now') ORDER BY ip"
show = "SELECT username, nas, session_id, ip, updated FROM ip_pool WHERE username = ?1 AND state = 'active' AND expires > strftime('%s', 'now') ORDER BY ip"

[queries.pool_usage]
sql = "SELECT pool, state, count(*) AS addresses FROM ip_pool GROUP BY pool, state"
help = "Addresses of each pool by state: free, reserved or active."
labels = ["pool", "state"]
value = "addresses"
```

- `[sessions]` 直接查池：位址池本身就記著誰在用哪個位址，不需要另一個計費表。`[sessions]` 只有 `session` 指令會讀，改了不必 reload。
- `pool_usage` 變成指標 `rradius_query_pool_usage{pool,state}`。這個查詢走 `ip_pool_state` 索引，池再大也不掃整個表。

### 腳本重點

`allocate.lua`（lua 處理器）依序做四件事：

1. 這個人已經有 `reserved` 或 `active` 的位址就沿用，不多佔一個。
2. 沒有的話，先拿他上次用過、現在空閒、放回不到 `HOLD_SECONDS`（86400 秒）的位址。
3. 再從空閒的位址裡挑 `CANDIDATES`（8）個逐一去搶。起點在空閒位址的 `n` 範圍內隨機取，同時進來的請求才不會都搶同一批；最多讀 `ATTEMPTS`（4）輪候選。
4. 都搶不到就寫一行 `ip pool … is exhausted` 並拒絕。

`accounting.lua` 處理計費：

- Start 與 Interim-Update 把這個人的位址確認成 `active`，到期時間設成從現在起 3 個 Interim 間隔。如果預留已經過期放回、而位址還空著，就把它拿回來。
- Stop 只釋放這個 session 的位址，或從沒被確認過的預留，所以晚到的舊 session 的 Stop 動不到新的綁定。
- Accounting-On／Off 表示 NAS 重開機，把這台 NAS 所有 `active` 的位址放回。哪一台 NAS 由 `NAS-IP-Address`、`NAS-IPv6-Address`、`NAS-Identifier` 依序決定，都沒有時用用戶端名稱，因為一個用戶端可以是整個網段。

`RESERVE_SECONDS`、`HOLD_SECONDS`、`INTERIM_SECONDS` 在兩個腳本裡各有一份，改的時候兩邊要一致。

### 怎麼測與注意事項

已驗證。`alice` 登入兩次、`bob` 登入一次：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 639/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd query pool_usage --config rradius.toml
pool         state     addresses
business     free      3
residential  free      6
residential  reserved  2
```

`alice` 兩次的追蹤行都是 `Framed-IP-Address = 10.77.0.4, Acct-Interim-Interval = 300`，所以只佔了一個位址；另一個 `reserved` 是 `bob` 的。位址是隨機挑的，你的結果可能不同。

用 `dave`、`frank`、`gina`、`hank` 依序登入，前三個成功，`hank` 得到 `reject 1`，日誌寫著：

```text
… script allocate.lua (radius/authentication, client localhost): ip pool business is exhausted: no address for hank
```

送一筆 Start，位址從 `reserved` 變成 `active`，`session list` 就看得到。`acct-start` 不會自己帶位址，所以用 `--attr` 加上剛才拿到的那一個：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-start,concurrency=1 --requests 1 --attr 8=ip:10.77.0.4
…
$ rradiusd session list --config rradius.toml
username=alice
nas=192.0.2.1
session_id=e370000000000000
ip=10.77.0.4
updated=1795384802
```

`session` 負載走完一整個 session，Stop 之後位址放回：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user carol --password carol-password --load session,concurrency=1 --requests 1
…
  acct-stop: 776/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`carol` 再登入時拿回同一個位址（黏著）。

注意事項：

- 預留要 120 秒才會過期。測完池用盡想再測一次，要等兩分鐘，或停掉伺服器刪掉 `data/` 重來。
- `rradius_query_pool_usage` 每 30 秒才更新一次，剛做完的變化要等下一輪才看得到；要馬上看，用 `rradiusd query pool_usage`。
- 密碼錯、帳號停用或不存在都是 `reject`，而且不佔位址。

### 維運：查位址與增減位址

`ip_pool` 是一般的資料表，沒有專用的管理指令。以下 SQL 的欄位都是 `migrations/1_schema.sql` 的欄位。

查每個池的使用狀況，以及「這個位址現在是誰在用」：

```sql
SELECT pool, state, count(*) FROM ip_pool GROUP BY pool, state;
SELECT pool, ip, state, username, nas, session_id, expires FROM ip_pool WHERE ip = '10.77.0.2';
```

查洩漏：到期了卻還沒放回的列。這些列在下一次回收時放回；如果一直留著，是沒有請求進來觸發回收：

```sql
SELECT ip, username, expires FROM ip_pool WHERE state != 'free' AND expires < strftime('%s', 'now');
```

新增位址就是新增列。`n` 要接著池裡現有最大的往下編，才會連續（理由見上面〈資料表〉）：

```sql
INSERT INTO ip_pool (pool, ip, n)
VALUES ('residential', '10.77.0.10', (SELECT coalesce(max(n), 0) + 1 FROM ip_pool WHERE pool = 'residential'));
```

位址很多時，用程式產生這些語句，寫成新編號的 migration 檔，放到每個節點、逐台重啟（見[第 18 章](18-operations.md#新增-migration)）；已套用的檔案不能再改。縮小池時，先確認要刪的位址是 `free`，再刪列，而且從 `n` 最大的刪起，免得 `n` 出現空隙：

```sql
DELETE FROM ip_pool WHERE pool = 'residential' AND ip = '10.77.0.10' AND state = 'free';
```

### 監控與告警

`pool_usage` 的指標是 `rradius_query_pool_usage{pool,state}`，每個節點輸出同樣的數字，叢集要用 `max()`，不要 `sum()`。池快用完的告警：

```promql
max by (pool) (rradius_query_pool_usage{state="free"}) < 100
```

用掉幾成：

```promql
1 - (max by (pool) (rradius_query_pool_usage{state="free"})
     / sum by (pool) (max by (pool, state) (rradius_query_pool_usage)))
```

- `reserved` 一直很多而 `active` 很少，代表 NAS 沒有送 Accounting-Start；預留要 120 秒才回收。
- 查詢沒有符合的列就沒有樣本。池整個用盡（沒有任何 `free` 的列）時，`state="free"` 的樣本消失，上面第一條告警式不會觸發。要抓這種情況，用 `max by (pool) (rradius_query_pool_usage{state="active"}) unless on (pool) max by (pool) (rradius_query_pool_usage{state="free"})`，或在已知池名稱時用 `absent(rradius_query_pool_usage{pool="business",state="free"})`。
- 查詢失敗時指標保留上一次的值；看 `rradius_query_last_success_timestamp_seconds{query="pool_usage"}` 有沒有在前進（見[第 09 章](09-external-databases.md)）。
- 成本：這個查詢走 `ip_pool_state` 的覆蓋索引（`SCAN ip_pool USING COVERING INDEX ip_pool_state`）。2026-11-16 在 SQLite 3.54.1 的記憶體資料庫、11 萬位址上量到中位數約 3.7 毫秒；改寫成 `WHERE state = 'free' GROUP BY pool` 走 `ip_pool_expiry`，每列要回表，約 9.5 毫秒。所以一次取出每個池的每個狀態比較好。未重新量測。

### 規模

2026-11-16 的登入風暴量測（變更 ip-pool-scale；一台 14 CPU 的 Docker，伺服器 7 個 CPU、量測程式 7 個 CPU，64 個並行的 PAP 登入，每個用戶第一次登入；之後未重新量測）：

| 池 | 登入 | 每秒 | 結果 |
| --- | --- | --- | --- |
| 2.2 萬位址、2 萬用戶 | 5,000 | 約 7,500 | 無重複、無拒絕 |
| 11 萬位址、10 萬用戶 | 2 萬 | 約 6,600 | 無重複、無拒絕 |
| 11 萬位址、10 萬用戶，整批重連 | 10 萬 | 約 6,500（共約 15 秒） | 10 萬人各拿一個不同的位址，無拒絕 |

這個速度靠兩件事：挑候選走 `(pool, state, n)` 索引；登入時的每個查詢，條件都被某個索引的前幾欄涵蓋。計費腳本的 `WHERE nas = ? AND state = 'active'` 沒有索引，但只在 NAS 重開機送 Accounting-On／Off 時才跑，不是每次登入。SQLite 沒有統計資料時，索引只涵蓋條件的一部分，查詢可能改走主鍵、掃整個池；先前版本的「沿用綁定」查詢就是這樣（它有 `ORDER BY ip`，主鍵正好排好序），11 萬位址時只有每秒 64 個登入。改自己的查詢時，用 `EXPLAIN QUERY PLAN` 看它走哪個索引。

量測只做認證；計費的寫入（Start、Interim、Stop）另外算，上限見[第 19 章](19-performance.md)。整批重連超過 120 秒的話，沒收到 Start 的預留會先放回，「預留數」會少於登入數。

### 已知限制

- **一個用戶一個位址。** 同一個帳號同時有兩個 session（例如兩台設備）會拿到同一個位址，因為「已經有綁定就沿用」。要限制同時連線數，另外做，見〈方案與用量控制〉。
- **這個情境不處理 DHCPv6-PD。** IPv6 前綴要對齊分配，見〈IPv6 前綴池〉。
- **沒有內建的池函式和專用的管理指令。** 使用率靠 `pool_usage` 與 `rradiusd query`；挑位址、釋放都在腳本裡。
- **一次一個位址。** 沒有「分配一整段」。
- **NAS 當機之後，位址最多保留 15 分鐘**（3 個 Interim 間隔，900 秒）才會給別人；NAS 回來送 Accounting-On 會立刻釋放。
- **池用盡時 NAS 只看到 Reject。** 原因在日誌，監控日誌裡的 `is exhausted`。

## IPv6 前綴池：prefix-pool

### 目標

用 DHCPv6-PD 委派前綴給用戶：通過密碼驗證後，從池裡拿一個前綴，放在 `Delegated-IPv6-Prefix`；WAN 端的 /64 不在這裡分配，回池的名稱 `Framed-IPv6-Pool`，由 BNG 自己的池決定。用戶離線後，前綴保留給他一天，回來時拿到同一個。

### 資料表

IPv6 的池太大，不能每個前綴存一列。這個情境的做法是池只存三個常數，表裡只放已經分配出去的前綴：

| 表 | 內容 |
| --- | --- |
| `prefix_pools` | 每個池一列：`base`（前綴的高 64 位元，整數）、`plen`（池的長度）、`dlen`（每個用戶拿到的長度，不超過 64） |
| `prefix_leases` | 只放已分配的前綴，沒有那一列就是空閒。主鍵是 `(pool, n)` |
| `subscribers` | 帳號、密碼、`pool`，池名稱也是回給 BNG 的 `Framed-IPv6-Pool` |

第 `n` 個前綴（`n` 從 0 起）的高 64 位元是 `base | (n << (64 - dlen))`，所以一定對齊；一個池有 2 的 `dlen - plen` 次方個前綴，`CHECK` 限制最多 2 的 30 次方。`base` 也必須對齊 `plen`，沒對齊就寫不進去。`2001:0db8:0100:0000` 寫成 `0x20010db801000000`；高位元是 1 的位址（例如 `fd00::/8`）在 SQLite 裡是負數，沒有關係。

`prefix_leases.state` 多了一種 `released`：用戶離線了，前綴暫時留給他，`expires`（釋放時間加 `HOLD_SECONDS`）到了才刪。別人只有在找不到空的 `n` 時，才會搶最舊的 `released`。

`prefix` 欄存前綴的文字，形式必須和伺服器把 NAS 回報的 `Delegated-IPv6-Prefix` 解碼出來的一樣：壓縮、小寫，例如 `2001:db8:100::/56`。計費確認靠這個字串找到那一列。

示範資料有三個池：`residential` 是 `2001:db8:100::/48` 每人一個 /56，`business` 是 `2001:db8:200::/52` 每人一個 /60，`ula` 是 `fd12:3456:789a::/48` 每人一個 /56，各 256 個前綴。

### 設定重點

設定的組成和 ip-pool 相同：`all_accept` 先 sql 處理器、再 lua 處理器 `allocate.lua`，`[lua]` 是計費的 `accounting.lua`，`[sessions]` 查 `prefix_leases`。具名查詢有三個：

| 查詢 | 指標 | 內容 |
| --- | --- | --- |
| `prefix_leases` | `rradius_query_prefix_leases{pool,state}` | 每個池各狀態的前綴數 |
| `prefix_capacity` | `rradius_query_prefix_capacity{pool}` | 每個池的容量，`1 << (dlen - plen)` |
| `prefix_hold_age` | `rradius_query_prefix_hold_age{pool}` | 最舊的 `released` 放回到現在幾秒，也就是現在實際保證得到的保留時間 |

算使用率時只算 `reserved` 與 `active`：`released` 是留給離線用戶的，需要時可以被搶。

### 腳本重點

`allocate.lua`：

1. 已經有前綴就沿用；`released` 的改回 `reserved`，等他的 Accounting-Start。
2. 沒有的話，用帳號名稱的雜湊（FNV-1a）決定起點 `n`，在往後 `WINDOW`（1024）個 `n` 裡找空的，用 `INSERT OR IGNORE` 去佔：受影響列數是 1 就搶到，0 表示剛被別人拿走（主鍵擋住）。同一個人每次從同一個起點開始，所以那個位置空著時拿到同一個前綴，但這不是保證。
3. 窗口裡沒有空的，就從最舊的幾列 `released` 隨機挑一個搶。
4. 沒有 `released` 可搶，才在整個池裡找空的；整個池都滿了才拒絕，日誌寫 `prefix pool … is exhausted`。

前綴的文字在 SQL 裡算：Luau 沒有 64 位元整數，所以 `CLAIM` 這段 SQL 用位元運算與 `printf('%x:…')` 組出壓縮後的寫法。搶輸了再找時，候選數每輸一次加倍（最多 64），最多試 `ATTEMPTS`（16）次。

`accounting.lua`：

- Start 與 Interim-Update 把前綴確認成 `active`。
- Stop 把這個 session 的前綴改成 `released`，保留 `HOLD_SECONDS`。
- Accounting-On／Off 把這台 NAS 的 `active` 前綴全部改成 `released`。
- 封包帶了 `Delegated-IPv6-Prefix` 就要對得上；沒帶就只看帳號，因為一個人在一個池裡只有一個前綴。
- 預留過期被刪了才收到的 Start 不會補回來，因為前綴的文字不能反推出 `n`，只會在日誌寫一行 `is not in prefix_leases`。

### 怎麼測與注意事項

已驗證。四個人登入，各拿到自己池的前綴：

```text
… user="alice" code=2 attributes="Message-Authenticator = <hidden>, Delegated-IPv6-Prefix = 2001:db8:100:e700::/56, Framed-IPv6-Pool = residential, Acct-Interim-Interval = 300"
… user="carol" code=2 attributes="Message-Authenticator = <hidden>, Delegated-IPv6-Prefix = 2001:db8:200:120::/60, Framed-IPv6-Pool = business, Acct-Interim-Interval = 300"
… user="dave" code=2 attributes="Message-Authenticator = <hidden>, Delegated-IPv6-Prefix = fd12:3456:789a:df00::/56, Framed-IPv6-Pool = ula, Acct-Interim-Interval = 300"
```

起點由帳號名稱決定，所以同樣的示範資料，你拿到的前綴應該和這裡相同。`alice` 用 `session` 負載走完一次之後：

```console
$ rradiusd query prefix_leases --config rradius.toml
pool         state     prefixes
business     reserved  1
residential  active    1
residential  released  1
ula          reserved  1
```

`residential` 的 `released` 就是 `alice` 的前綴（`active` 那一個是另外用 `acct-start` 確認的 `bob`）。她再登入時拿回 `2001:db8:100:e700::/56`，那一列又變回 `reserved`。

注意事項：

- `rradius-bench` 的 `session` 負載只會把 `Class` 與 `Framed-IP-Address` 帶進計費，不帶 `Delegated-IPv6-Prefix`，所以測試時走的是「只看帳號」那條路。真的 BNG 會帶前綴。
- 池越滿，要走過的連續已佔用 `n` 越長，所以只看一個窗口，窗口裡沒有就搶 `released`。`prefix_hold_age` 變小，表示離線的人保留前綴的時間正在被壓縮，是該擴池的訊號。

### 池很滿的時候

池很滿有兩件事要看：登入會不會變慢或被拒絕，以及用戶離線再回來還拿不拿得到同一個前綴。前者用真正的伺服器量，後者主要靠模擬，分開說。**這一節的數字都取自舊版手冊第 19 章與變更 `ipv6-prefix-pool`（2026-11-16 至 17，紀錄在 `docs/spectra/changes/archive/2026-11-16-ipv6-prefix-pool/`），未重新量測；它們量的都是現在這個「只存已分配的前綴、有 `released` 保留」的設計**，不是更早的每個前綴預先產生一列的做法。

#### 登入的速度：Docker 單節點

容量 65,536（`2001:db8::/40` 切 /56），Docker 裡的單節點（伺服器 7 個 CPU，`rradius-bench` 另外 7 個），新用戶 64 並行第一次登入。池先用 `allocate.lua` 填到指定的使用率，分兩種：「預留」是剛填滿、沒有 `released` 列（最壞的情況：窗口裡沒有空位、也沒有 `released` 可搶，要整池找空位）；「`released`」是用戶離線後的正常樣子。

| 使用率 | 池裡的前綴 | 新用戶每秒 | p50 | p99 | 被拒絕（池還有空位） |
| --- | --- | --- | --- | --- | --- |
| 90% | 預留 | 4,125 | 11 ms | 71 ms | 0 |
| 95% | 預留 | 1,643 | 24 ms | 155 ms | 0 |
| 99% | 預留 | 334 | 161 ms | 457 ms | 0 |
| 90% | `released` | 5,114 | 10 ms | 40 ms | 0 |
| 99% | `released` | 3,383 | 11 ms | 67 ms | 0 |
| 100% | `released` | 1,465 | 30 ms | 34 ms | 0 |

- **有 `released` 時比較快：** 窗口只看起點往後 `WINDOW`（1,024）個 `n`，窗口裡沒有就直接搶最舊的 `released`，不用整池掃。預留填滿時沒有 `released` 可搶，使用率越高越慢。這也是監控建議使用率超過 85% 就告警的原因。
- **沒有重複：** 每次量測後資料表的列數都等於不同前綴的數目（唯一索引擋著）。

**隨機放寬重試的由來。** 第一版的做法是搶輸就去找「下一個」空的 `n`。用伺服器量時，使用率 90% 就有約 2.5%（3,000 個新用戶被拒絕 76 個）的登入被拒絕，池裡還有六千多個空位：同時進來、起點落在同一段的用戶都找到同一個空的 `n`，每一輪只有一個贏，輸的下一輪又都去搶同一個，池越滿空段越短。所以現在（`allocate.lua` 的 `free_near`、`steal`、`width`）搶輸之後，從往後的空段裡隨機挑一個，候選數每輸一次加倍（最多 64），搶 `released` 也一樣（從最舊的 8 列起，每輸一次加倍）；最多試 `ATTEMPTS`（16）次。單靠一次讀的模擬看不出這個問題，因為模擬沒有並行。

#### 登入的速度：更大的池、三節點叢集

另一台機器、另一次量測（Apple M4 Max，14 CPU，macOS，伺服器與 `rradius-bench` 同一台，PAP 登入，新用戶第一次登入；舊章記載日期 2026-11-18）。**只能和這一節的數字互相比，不能和上面的 Docker 表比。** 池先用真正的登入（`allocate.lua`）填到指定使用率；「`released`」是把填好的前綴全改成 `released`。用資料表直接寫入、前綴隨機散布填出來的池快很多（65,536 的 99% 預留約每秒七千個），因為真正的登入填出來的前綴與空位都是一段一段連在一起的；所以量測要用真正的登入填池。

單節點，64 並行：

| 容量 | 使用率 | 池裡的前綴 | 新用戶 | 每秒 | p50 | p99 | 被拒絕 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 65,536 | 90% | 預留 | 2,000 | 10,700 | 4.7 ms | 20 ms | 0 |
| 65,536 | 95% | 預留 | 2,000 | 3,737 | 7.4 ms | 108 ms | 0 |
| 65,536 | 99% | 預留 | 500 | 549 | 93 ms | 328 ms | 0 |
| 65,536 | 90% | `released` | 2,000 | 10,751 | 5.0 ms | 15 ms | 0 |
| 65,536 | 99% | `released` | 500 | 6,430 | 7.6 ms | 32 ms | 0 |
| 65,536 | 100% | `released` | 500 | 6,054 | 8.9 ms | 27 ms | 0 |
| 262,144 | 90% | 預留 | 2,000 | 10,334 | 4.8 ms | 22 ms | 0 |
| 262,144 | 99% | 預留 | 500 | 1,879 | 23 ms | 118 ms | 0 |
| 262,144 | 99% | `released` | 500 | 6,421 | 8.9 ms | 25 ms | 0 |
| 262,144 | 100% | `released` | 500 | 6,062 | 9.4 ms | 24 ms | 0 |

- **容量放大 4 倍，同一使用率的速度相同或更好。** 99% 預留在 262,144 比 65,536 快三倍多，因為同樣 1% 的空位，絕對數量多四倍（2,621 對 655）。
- **還是要在 90% 之前擴充：** 預留填滿又沒有 `released` 可搶的 99%，每秒只有幾百到一千多個。
- **資料表的列數都等於不同前綴數。** 262,144 的 100% 那組是 262,143 列，比預期少一列，沒有查出原因。

三節點叢集（三個 `rradiusd` 程序在同一台機器上，容量 65,536，三個量測程式各對一個節點，合計 63 並行）：

| 使用率 | 池裡的前綴 | 新用戶 | 三個節點合計每秒 | 各節點 p99 | 被拒絕 |
| --- | --- | --- | --- | --- | --- |
| 90% | 預留 | 2,100 | 6,654 | 25–28 ms | 0 |
| 99% | `released` | 600 | 5,448 | 27–34 ms | 0 |
| 100% | `released` | 600 | 4,696 | 26–32 ms | 0 |
| 99% | 預留 | 450 | 787 | 250–270 ms | 0 |

- 90% 與 `released` 的情況比單節點慢，約六到八成（6,654 對 10,700，5,448 對 6,430，4,696 對 6,054）；99% 預留反而比單節點快（787 對 549），那一組的瓶頸是找空位，不是寫入。寫入要經 leader。
- 三個節點同時搶 `released` 沒有被拒絕，也沒有重複：三個節點的資料表列數和不同前綴數都相等。
- **這是同一台機器上的三個程序**，沒有網路延遲；跨主機或跨機房的延遲會讓每次寫入更慢，沒有量。

### 用戶回來拿到同一個前綴嗎

**伺服器上的實測（舊章記載日期 2026-11-18）：** 容量 16,384，單節點。用戶依序登入並送 Start，NAS 送 Accounting-On，用戶隨機順序回來，看拿到的是不是原來那個。有保留（`HOLD_SECONDS` 24 小時）：使用率 70%、90%、95% 都是 100% 拿回同一個，沒有被拒絕。只靠雜湊起點（把 `HOLD_SECONDS` 設成 0，保留全部過期刪掉）：70% 約 55%，90% 約 41%，95% 約 37%。結論：**重開機後要穩定，靠 `released` 的保留，不是雜湊起點。**

**模擬：** 容量 65,536，用戶的在線時間是指數分佈（平均一天），離線時間是指數分佈，或 80% 是平均 3 分鐘的短斷線、20% 是長離線；暖機後量每次再登入是不是拿到上一次的前綴。由兩份獨立寫的模擬互相核對，不是伺服器實測，也沒有模擬並行與叢集。重點：

- **只靠雜湊起點（沒有 `released`）不穩定。** 有真實的上下線流動時，使用率 90% 約 39–87% 拿回同一個（取決於在線比例與離線時間的分佈），95% 約 62–68%，99% 約 39–53%；使用率 50–70% 也只有約 76–93%。容量放大到 100 萬結果相同。
- **NAS 或 BNG 重開機最差。** 一批在線用戶同時斷線、隨機順序回來，只靠雜湊起點：使用率 70% 約 51% 拿回同一個，90% 約 28%，95% 約 18%。
- **保留 `released` 後（24 小時）：** 同樣的重開機情況 100% 拿回；一般上下線流動，90% 使用率約 93–98%（視保留時間與離線分佈），95% 約 89%，99% 約 82%。短斷線（保留期內回來）幾乎都拿回；超過保留期、或前綴被搶走的用戶拿到另一個。
- 伺服器實測的 90%、95%（約 41%、37%）比模擬的 28%、18% 穩一些，原因沒有查；可能是伺服器在窗口內隨機挑空位，和模擬的演算法不完全相同。

#### 容量要求

**穩定性的上限取決於總用戶數與池容量的比：** 離線的用戶越多、空位越少，被搶走的越多。總用戶數不超過池容量時，保留能接近 100%；總用戶數遠超過容量，一定有人要換前綴。要穩定的前綴，**池的容量要大於用戶總數**，不只是使用率要低。

#### 否決的替代做法

這些都在研究階段試過、不採用（來自變更紀錄的模擬）：

- **雙重雜湊探測：** 只多 1–10 個百分點的穩定性。
- **分區塊：** 持平到更差。
- **使用率超過門檻就拒絕新用戶：** 需求 95%、門檻 90% 時 35% 的登入被拒，比換前綴更糟；所以只告警，不拒絕。
- **一個一個用 `INSERT OR IGNORE` 往後佔：** 90% 使用率時 p99 要走 400 到 1,100 個 `n`，每一個是一次經過 leader 的寫入；用 `ATTEMPTS` 限制又會在還有空位時誤判用盡。所以改成先一次讀找出空的 `n`、再佔。
- **每個前綴預先產生一列（由 migration 產生）：** 大池（例如 /32 切 /56 有 1,600 萬個前綴）要產生大量資料，100 萬列的 migration 能不能經叢集套用也沒把握；改成只存已分配的前綴後，池的大小不影響表的大小，代價是池很滿時要走過較長的連續已佔用區段（所以才有窗口與搶 `released`）。
- **從隨機起點找空位：** 沒有穩定性（同一個用戶每次起點不同），不採用。

### 監控與告警

具名查詢的指標見〈設定重點〉。每個池的使用率**只算 `reserved` 與 `active`**（叢集用 `max()`，不要 `sum()`：每個節點查到的是同一份資料）。`released` 是留給離線用戶的，需要時可以被搶，算進去的話使用率在穩態會一直接近 100%。

```promql
sum by (pool) (max by (pool, state) (rradius_query_prefix_leases{state=~"reserved|active"}))
  / max by (pool) (rradius_query_prefix_capacity)
```

**這個式子沒有對著真的 Prometheus 跑過**（指標名稱與標籤取自範例 `rradius.toml` 的具名查詢）；上線前先在你的 Prometheus 查詢一次，確認標籤對得上。

- **使用率超過 85% 就告警**（`> 0.85`）：池越滿，登入越慢，也越留不住離線用戶的前綴（見上面的數字）。
- **`rradius_query_prefix_hold_age` 是現在實際保證的保留時間（秒）。** 池沒有 `released` 列時沒有樣本；有但最舊的只有幾分鐘，表示池已經滿到前綴一釋放就被搶，保留不到 `HOLD_SECONDS`，穩定性會掉。它遠小於 `HOLD_SECONDS`（86,400）就該擴充。
- **查詢沒有符合的列就沒有樣本**，所以全空的池沒有 `prefix_leases` 的樣本。告警要用 `absent()` 補（例如 `absent(rradius_query_prefix_hold_age{pool="residential"})` 只代表現在沒有 `released`，不一定是問題），或把沒有樣本的池當作 0。
- 這些查詢都不掃整個池：`prefix_leases` 的 `GROUP BY pool, state` 走覆蓋索引 `prefix_leases_pool_state`，搶最舊的 `released` 走部分索引 `prefix_leases_released`，`prefix_hold_age` 走 `prefix_leases_expiry`，`prefix_pools` 只有幾列。表只有在線與最近離線的用戶那麼大，成本和池的容量無關。

### 換成自己的池

池的常數 `base` 是前綴高 64 位元的整數，用 Python 算（結果是 `0x20010db801000000`）：

```bash
python3 -c "print(hex(int.from_bytes(__import__('ipaddress').ip_network('2001:db8:100::/48').network_address.packed[:8], 'big')))"
```

`plen`、`dlen` 照你的位址計畫。**正式環境第一次啟動前就決定好池的常數**；套用過的 migration 檔不能再改，要加池就新增下一個編號的檔案。池的大小要留空間：使用率超過 90% 之前就擴充（新增一個池，或換更長的 `plen`）。

### 和位址池合併成雙堆疊（帳號密碼身分）

用戶用帳號密碼認證、又要同時有 IPv4 位址和委派前綴時，伺服器只有**一個最上層腳本**處理計費，所以兩個範例不能直接一起跑。做法如下，**這個合併版本沒有附範例，也沒有實際跑過**（附有腳本與測試的合併版本是〈雙堆疊線路〉，但它用線路身分）：

1. **認證：** `[[auth.handlers]]` 依序列 `sql`（驗密碼）、ip-pool 的 `allocate.lua`、這個情境的 `allocate.lua`。`all_accept` 一個一個執行，任一個拒絕就停。兩個腳本放在不同的檔名（例如 `ip-allocate.lua`、`prefix-allocate.lua`）。
2. **資料表：** 兩份 migration 的檔案編號不能撞；兩邊都有 `subscribers` 與 `housekeeping`，要合併成一張：`subscribers` 要有 IPv4 的 `pool` 和前綴池的名稱（兩個欄位，或兩個池同名），`housekeeping` 要同時有 `reap-ip-pool` 與 `reap-prefix-pool` 兩列。
3. **計費：** 把兩個 `accounting.lua` 合併成一個。一個封包可以同時有 `Framed-IP-Address` 與 `Delegated-IPv6-Prefix`：位址的部分照 ip-pool 處理，前綴的部分照這一節處理；Accounting-On／Off 兩邊的釋放都要做。
4. **一邊用盡：** 預設整個登入被拒絕（`all_accept`）。要「有 IPv4 沒前綴也放行」，把前綴腳本用盡時的 `return 'reject'` 改成不設屬性就 `return 'accept'`。注意這樣不會把剛預留的 IPv4 放回去；要放回去，要用〈雙堆疊線路〉那種單一腳本。

線路身分（Option 82）的用戶沒有可用的帳號，處理器之間又不傳值，不能這樣串，見〈雙堆疊線路〉。

### 已知限制

- **前綴不保證穩定。** 起點被別人佔走時拿到不同的；池越滿越不穩（見上面）。沒有歷史表記錄「上次用過哪一個」。
- **池很滿時很慢。** 預留填滿、沒有 `released` 時，Docker 單節點 95% 約每秒 1,600 個新用戶，99% 約 330 個；有 `released` 時 99% 仍有每秒 3,000 個以上（數字見上）。
- **回收一次最多處理 1,000 列，每分鐘一次**（過期的 `active` 轉 `released`、過期的預留與 `released` 刪除各一批）。大量租約同時到期（例如整台 BNG 當機）時，到期還沒被刪的列仍佔著前綴，要幾分鐘才清完；池很滿時這段時間可能誤判用盡。
- **亂序的 Start 可能讓已離線用戶的前綴多佔一段時間：** Stop 先到、Start 後到時，Start 把 `released` 改回 `active`，到期沒有 Interim 才再轉成 `released`（多佔約 900 秒，再重算保留）。封包沒帶前綴、退回用用戶名稱找列時，舊 session 晚到的 Start 或 Interim 也可能確認新的預留，新 session 的 Start 會再覆蓋。
- **池用盡時的拒絕要掃整個池：** 沒有 `released`、窗口沒有空位時，要整池找空位才能確定用盡，成本和已佔用的列數成正比；池一直在用盡的狀態時，被拒絕的登入很貴，要靠使用率告警避免。
- **晚到的 Start 補不回過期的預留**（前綴文字反推不出 `n`），只記日誌。
- **沒有 WAN 的 /64 池。** `Framed-IPv6-Pool` 只是池的名稱，連線本身的位址由 BNG 的池決定。
- **沒有對真實 BNG 驗證。** 屬性用 RFC 4818 的 `Delegated-IPv6-Prefix`，計費封包要帶它。
- **叢集：** 三個程序同機的量測以外沒有量過多主機叢集（見上）。

### 常見問題

| 現象 | 原因 |
| --- | --- |
| 一直是 Reject，日誌有 `is exhausted` | 池真的沒有空位：查 `rradius_query_prefix_leases` 與容量；擴充池 |
| 一直是 Reject，日誌有 `is not in prefix_pools` | `subscribers.pool` 在 `prefix_pools` 沒有對應的池 |
| 一直是 Reject，日誌有 `tries: others took the free ones as fast` | 同時進來的人太多、池太滿，16 次都搶輸；通常也是該擴池的訊號 |
| 登入成功，但那一列一直是 `reserved`，120 秒後被刪掉、前綴發給別人 | Start 沒有到，或 Start 的 `Delegated-IPv6-Prefix` 和表裡的文字不同（壓縮、大小寫）：看日誌裡 `is not in prefix_leases` 那行的前綴 |
| 寫不進池：`CHECK constraint failed` | `base` 沒有對齊 `plen`，或 `dlen` 小於 `plen`、超過 64，或容量超過 2 的 30 次方 |
| 用戶每次登入拿到不同的前綴 | 池使用率太高、起點被別人佔走；或離線超過 `HOLD_SECONDS`；或預留過期後那個位置被別人拿走了（見〈池很滿的時候〉） |

## 依線路身分認證：line-identity

### 目標

固網寬頻的 IPoE 用戶沒有帳號密碼，「哪一條線」就是身分：BNG 把 DHCP Option 82 的 circuit-id 與 remote-id 放進 RADIUS 請求（RFC 4679）。認得的線路回位址池名稱與設定檔名稱，不認得的、停用的拒絕；需要的話，還可以把線路綁定第一次看到的 MAC 位址。

### Option 82 怎麼到 RADIUS

這個情境要 BNG 把線路身分放進 Access-Request。`auth.lua` 認這幾個屬性：

| 屬性 | 用途 |
| --- | --- |
| `ADSL-Agent-Circuit-Id`（RFC 4679，廠商 3561 的子屬性 1） | circuit-id |
| `ADSL-Agent-Remote-Id`（RFC 4679，廠商 3561 的子屬性 2） | remote-id；沒有就當空字串 |
| `NAS-Port-Id` | 沒有 `ADSL-Agent-Circuit-Id` 時，當作 circuit-id |
| `Calling-Station-Id` | 用戶設備的 MAC 位址，`lock_mac` 要用；只取其中的 12 個十六進位字元，冒號、橫線不影響 |

不同廠商的 BNG 把 Option 82 放進哪個屬性、格式怎麼寫並不一致：有的用上面這組，有的用 `NAS-Port-Id`，有的把兩個值合成一個字串。**先確認你的 BNG 實際送什麼**：用不認識的線路登入一次，日誌會寫出腳本看到的值（`line identity: unknown line <remote-id> <circuit-id>`），再照它填 `lines`；也可以開 `[log.trace]` 看整個請求（見[第 17 章](17-monitoring.md)）。BNG 要怎麼設定，請看它的文件。

用戶名稱與密碼這個情境都不看：BNG 通常送線路字串或 MAC 當名稱、一個固定值當密碼，腳本只看上面的屬性。

### 資料表

| 表 | 內容 |
| --- | --- |
| `plans` | 每個方案：回給 BNG 的位址池名稱 `pool`、設定檔名稱 `profile`（速率等由 BNG 上同名的設定檔決定）、`interim` |
| `lines` | 每條線一列，主鍵是 `(remote_id, circuit_id)` |

circuit-id 通常只在同一台接入設備內唯一，remote-id 常是那台設備的名稱，所以兩個一起當主鍵；BNG 沒送 remote-id 時用空字串。兩個 id 都存正規化後的值：

- 去掉頭尾空白、轉小寫，例如 `eth 1/1/3:100`、`olt-a`。
- 含不可印字元（二進位）的值，寫成 `0x` 加小寫十六進位，不做其他處理，例如 `0x0004000a0001`。

`lines` 的其他欄位：

- `lock_mac`：`0` 只記錄最後看到的 MAC（`last_mac`、`last_seen`）；`1` 在第一次登入時把 MAC 綁進 `bound_mac`，之後換了 MAC 就拒絕。
- `bound_mac`：12 個小寫十六進位字元，沒有冒號或橫線。用戶換設備時，把它清成 `NULL`。

### 設定重點

這個情境沒有 `[auth]`，認證與計費都交給最上層腳本 `auth.lua`。用戶端只列 BNG：

```toml
# rradius.toml（節錄，其他設定略）
[lua]
script = "auth.lua"

[[radius.clients]]
name = "bng"
address = "10.60.0.0/24"
secret_file = "bng.secret"
```

> **警告：** 線路身分不是祕密。能讓 BNG 以外的來源送請求的人，只要在請求裡寫上任意的 Option 82，就能冒充任何一條線。用戶端清單只放 BNG，不要放整個管理網段；要多一層保護時用 `lock_mac`。

### 腳本重點

`auth.lua` 先擋掉非 RADIUS 的協定，計費一律回 `accept`。它是最上層腳本，同一台伺服器若也開了 `[tacacs]`，一律接受會授權每個指令，所以要先擋。認證時：

1. 線路取 `ADSL-Agent-Circuit-Id`，沒有的話用 `NAS-Port-Id`；remote-id 取 `ADSL-Agent-Remote-Id`，沒有就是空字串。兩者都先正規化再查 `lines`。
2. 找不到、停用、方案不存在，都拒絕並寫日誌。
3. `lock_mac = 1` 時比對 `Calling-Station-Id`：第一次用 `UPDATE … WHERE bound_mac IS NULL` 綁定，兩個請求同時來只有一個贏，另一個重讀後比較。
4. 更新 `last_mac`、`last_seen`，回 `Framed-Pool`、`Filter-Id`、`Acct-Interim-Interval`，以及 `Class`（值是用戶名稱）。BNG 會在計費封包帶回 `Class`，計費紀錄就知道這條線是誰。

### 怎麼測與注意事項

已驗證。`rradius-bench` 從 `127.0.0.1` 送請求，所以實驗時先把實驗目錄裡 `bng` 的 `address` 改成 `"127.0.0.1"`。

`ADSL-Agent-Circuit-Id` 與 `ADSL-Agent-Remote-Id` 是廠商屬性（vendor 3561 的第 1、2 號），`--attr` 要寫成屬性 26 的十六進位：4 個位元組的 vendor 編號、1 個位元組的子屬性編號、1 個位元組的長度（值的長度加 2），再接值。可以用 shell 組出來。下面的 `vsa` 是示範用的 shell 函式，**不是 rradius 提供的工具**，僅供示範；它在 bash、zsh 與 sh 都能用：

```console
$ vsa() { printf '26=hex:00000de9%02x%02x%s' "$1" $(( ${#2} + 2 )) "$(printf '%s' "$2" | od -An -tx1 | tr -d ' \n')"; }
$ rradius-bench run --server 127.0.0.1 --secret "$(cat bng.secret)" \
    --user line --password x --load pap,concurrency=1 --requests 1 \
    --attr "$(vsa 1 'eth 1/1/3:100')" --attr "$(vsa 2 'OLT-A')"
…
  pap: 1030/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`--user` 與 `--password` 隨便寫，腳本不看。remote-id 送的是大寫的 `OLT-A`，正規化後對上 `olt-a`。日誌的追蹤行：

```text
… rradius_trace: request client="bng" source=127.0.0.1 user="line" code=1 attributes="…, ADSL-Agent-Circuit-Id = eth 1/1/3:100, ADSL-Agent-Remote-Id = OLT-A"
… rradius_trace: response client="bng" source=127.0.0.1 user="line" code=2 attributes="Message-Authenticator = <hidden>, Framed-Pool = residential, Filter-Id = fiber-100m, Acct-Interim-Interval = 300, Class = alice"
```

其他情況的結果：

| 請求 | 結果 | 日誌 |
| --- | --- | --- |
| `bob` 的線（`eth 1/1/4:100`），帶 `--attr 31=text:00-11-22-33-44-55` | accept，MAC 綁定 | |
| 同一條線，換成 `00-11-22-33-44-66` | reject | `line olt-a eth 1/1/4:100 is locked to 001122334455 and the request has 001122334466` |
| `dave` 的線（`olt-b`、`eth 2/0/8:200`） | reject | `line olt-b eth 2/0/8:200 of dave is disabled` |
| 不存在的線 | reject | `unknown line olt-a eth 9/9/9:1` |
| 只帶 `--attr 87=hex:0004000a0001`（`NAS-Port-Id`，二進位） | accept，`Class = erin` | |

注意事項：

- 每次登入都會寫一次 `last_mac`、`last_seen`。不需要這份紀錄時，可以把 `auth.lua` 最後那段 `UPDATE` 拿掉，省下每次登入的一個寫入。
- 「接受」只回池名稱與設定檔名稱，位址由 BNG 的池決定。要由 rradius 分配位址，看下一節。

### 維護線路資料

`lines` 與 `plans` 是一般的資料表，沒有專用的管理指令。改資料有兩條路：

- **migration 檔。** 下面的 SQL 寫成新編號的 migration 檔，放到每個節點、逐台重啟（見[第 18 章](18-operations.md#新增-migration)）。適合線路不常變、想留下變更紀錄的情況；每換一次用戶設備就多一個檔案，換設備頻繁時不實際。
- **把線路資料放在別的地方。** 線路多、常變動（開通、停用、換設備）時，讓腳本用 `db_query` 查外部的 MySQL 或 PostgreSQL（[第 09 章](09-external-databases.md)），或用 `http_request` 問帳務系統（[第 10 章](10-lua-policies.md)），`lines` 只留 `bound_mac`、`last_mac` 這類 rradius 自己寫的欄位。這個情境沒有示範，要自己改 `auth.lua`。

不論用哪一條路，資料表的內容是這樣改的：

```sql
-- 新增方案和線路（兩個 id 用正規化後的值：小寫、沒有頭尾空白）
INSERT INTO plans (plan, pool, profile, interim) VALUES ('fiber-500', 'residential', 'fiber-500m', 300);
INSERT INTO lines (remote_id, circuit_id, subscriber, plan, lock_mac) VALUES ('olt-a', 'eth 1/1/9:100', 'frank', 'fiber-500', 1);

-- 停用、啟用
UPDATE lines SET enabled = 0 WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';
UPDATE lines SET enabled = 1 WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';

-- 換了用戶的設備：清掉綁定，下一個登入的設備會被綁
UPDATE lines SET bound_mac = NULL WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';

-- 用戶搬到別條線：停用舊線、新增新線，或直接改這條線的 subscriber
UPDATE lines SET subscriber = 'grace' WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';
```

`bound_mac` 要寫成 12 個小寫十六進位字元（`001122aabbcc`），不要有冒號或橫線；腳本是這樣比對的。

### 已知限制

- **線路身分不是祕密。** 能讓 BNG 送出任意 Option 82 的人，或能偽造 BNG 位址送請求的人，就能冒充任何一條線。用戶端清單只列 BNG，BNG 到 rradius 之間的網路要可信；要更強，加 `lock_mac`，或改用 RADIUS over TLS（RadSec，企業版）。
- **`lock_mac` 只擋換設備，擋不住偽造。** MAC 位址是明文，同一個二層網路的人可以抄。
- **每次登入寫一次資料庫（`last_mac`、`last_seen`）。** 寫入經過叢集的 leader；叢集沒有 leader 超過 5 秒後，寫入出錯，連不需要 MAC 鎖的線路也登不進去，登入得不到回應（見[第 10 章](10-lua-policies.md)）。不需要「最後看到」的資訊時，拿掉 `auth.lua` 最後那段 `UPDATE`，就不再依賴它。請求沒有 MAC 時只更新 `last_seen`，不蓋掉上一次的 `last_mac`。
- **首次綁定可能誤拒。** 綁定後的比較讀的是本機副本；跟隨節點還沒套用剛寫的綁定時，同一台設備的下一次登入可能被暫時拒絕，重試就會過。
- **文字與二進位的 id 可能撞在一起。** 二進位值寫成 `0x` 開頭的小寫十六進位，所以 BNG 若轉送用戶能控制的文字 id，用戶送 `0X0004000A0001`（轉小寫後是 `0x0004000a0001`）就等於送那個二進位值；結尾有 tab 或換行字元的文字也會被當成二進位。circuit-id 若是用戶能影響的字串，不要用這個情境。
- **只認 RFC 4679 的屬性和 `NAS-Port-Id`。** 其他格式要自己改 `auth.lua` 取值的那幾行。
- **只在登入時檢查。** 停用一條線不會中斷已經在線的連線。

### 常見問題

| 現象 | 原因 |
| --- | --- |
| 一直是 Reject，日誌有 `unknown line` | 資料表裡的值和 BNG 送的不同：看日誌裡腳本看到的 remote-id 與 circuit-id，注意要是正規化後的值；BNG 沒有送 remote-id 時，`remote_id` 要登記成空字串 |
| 日誌有 `no circuit-id and no NAS-Port-Id in the request` | BNG 沒有把 Option 82 放進請求；檢查 BNG 的設定，或確認它用的是別的屬性 |
| 日誌有 `is locked to` | `lock_mac = 1` 而且 MAC 和綁定的不同；換設備時清掉 `bound_mac` |
| 日誌有 `is locked to a MAC and the request has none` | `lock_mac = 1`，但請求沒有可用的 `Calling-Station-Id`（不是 12 個十六進位字元） |
| 日誌有 `the plan of line … does not exist` | `lines.plan` 在 `plans` 裡沒有對應的方案 |
| 登入成功但沒有 `Framed-Pool` | 這個腳本不是最上層腳本（`[lua]` 的 `script`），或 BNG 忽略這個屬性 |

## 雙堆疊線路：dual-stack-lines

### 目標

把前面幾個情境合起來：用線路身分認證，線路有固定位址就回固定的，沒有就從 IPv4 位址池與 IPv6 前綴池各拿一個；計費確認與釋放兩者。前綴池用盡時，剛替這個請求預留的 IPv4 要放回去，不留下沒人用的預留。

### 資料表

`migrations/1_schema.sql` 是前面三個情境的表的組合：

- `plans` 多了 `pool`（IPv4 池）與 `pool6`（前綴池）。
- `lines` 多了固定資料的欄位 `ipv4`、`netmask`、`routed_v4`、`wan_v6`、`pd_v6`，意思同下一節的 static-ip。
- `ip_pool`、`prefix_pools`、`prefix_leases`、`housekeeping` 同 ip-pool 與 prefix-pool，用 `lines.subscriber` 當帳號名稱，所以 `subscriber` 有唯一索引：一個用戶只能有一條線。

IPv4 與 IPv6 各自決定：只固定 IPv4 的線路，IPv6 前綴仍從池拿；`wan_v6`、`pd_v6` 只要設了任何一個，這條線的 IPv6 就不從池拿，只回設了的那個。

示範資料：`alice`、`bob` 全動態；`carol` 有 MAC 鎖；`dave` IPv4 與 IPv6 全固定；`frank` 只固定 IPv4；`metro` 方案的前綴池 `tiny6` 只有 2 個 /64，第三個人（`max`）會遇到前綴用盡。

三個方案與它們用的池（`migrations/2_demo_data.sql`）：

| 方案 | IPv4 池 | 前綴池 | 說明 |
| --- | --- | --- | --- |
| `residential` | `res4`（8 個位址，`10.77.0.2` 到 `10.77.0.9`） | `res6`（`2001:db8:100::/48` 切 /56，256 個） | 住宅 |
| `business` | `biz4`（3 個位址，`10.88.0.2` 到 `10.88.0.4`） | `biz6`（`2001:db8:200::/52` 切 /60，256 個） | 商業 |
| `metro` | `res4` | `tiny6`（`2001:db8:300::/63` 切 /64，2 個） | 前綴池很小，用來看前綴用盡 |

十三條線（remote-id 都是 `olt-a`，circuit-id 是 `eth 1/1/1:<號>`）：`:100` alice、`:101` bob（住宅，全動態）；`:102` carol（MAC 鎖）；`:103` dave（商業，IPv4 與 IPv6 全固定）；`:104` erin（停用）；`:105` frank（只固定 IPv4）；`:106` 到 `:109`（商業，全動態；`biz4` 只有 3 個位址，第 4 個遇到 IPv4 用盡）；`:110` 到 `:112`（metro，第 3 個遇到前綴用盡）。

### 設定重點

```toml
# rradius.toml（節錄，其他設定略）
[lua]
script = "accounting.lua"

[auth]

[[auth.handlers]]
type = "lua"
script = "line.lua"
```

沒有密碼，所以驗證鏈只有一個 lua 處理器。為什麼不是「IPv4 一個處理器、IPv6 一個處理器」：驗證鏈的處理器之間不傳值，前一個設的回覆屬性後一個讀不到，兩個池腳本要各自重算「這是哪個用戶」；而且只有同一個腳本，才能在前綴用盡時撤銷剛預留的 IPv4。

具名查詢 `pool_usage` 與 `prefix_usage` 分別看兩種池。

### 腳本重點

`line.lua` 依序做：

1. 線路身分，同 line-identity。
2. 固定資料的檢查：值不合法時拒絕並記錄，不悄悄改成動態（理由見下一節）。
3. IPv4：有固定的 `ipv4` 就回 `Framed-IP-Address`、`Framed-IP-Netmask`（沒設時 `255.255.255.255`），有 `routed_v4` 再回 `Framed-Route`；沒有就從方案的 IPv4 池拿。
4. IPv6：有固定的就回 `Framed-IPv6-Prefix`、`Delegated-IPv6-Prefix`；沒有就從前綴池拿，並回 `Framed-IPv6-Pool`。
5. 前綴用盡時，放回剛預留的 IPv4，然後拒絕。腳本開頭的 `ALLOW_V4_ONLY` 改成 `true` 的話，改成保留 IPv4、只回 IPv4 屬性：用戶沒有 IPv6 但能上網。

最後回 `Filter-Id`（方案的設定檔名稱）、`Acct-Interim-Interval` 與 `Class`。

`accounting.lua` 是 ip-pool 與 prefix-pool 兩個計費腳本的合併。IPoE 的計費封包沒有可靠的 `User-Name`，所以它靠 `Class` 找到這條線；封包沒有 `Class` 時照樣回覆 NAS，但不處理，並記一行日誌。固定位址的線路在池裡沒有列，對應的 `UPDATE` 不動任何列，也不是錯誤。

### 怎麼測與注意事項

已驗證。和 line-identity 一樣，先把 `bng` 的 `address` 改成 `"127.0.0.1"`，用上一節的 `vsa` 函式帶線路身分（remote-id 一律是 `olt-a`）。各條線的回覆：

```text
:100 alice  Framed-IP-Address = 10.77.0.9, Delegated-IPv6-Prefix = 2001:db8:100:e700::/56, Framed-IPv6-Pool = res6, Filter-Id = home-100m, Acct-Interim-Interval = 300, Class = alice
:103 dave   Framed-IP-Address = 203.0.113.10, Framed-IP-Netmask = 255.255.255.255, Framed-Route = 203.0.113.8/29 0.0.0.0 1, Framed-IPv6-Prefix = 2001:db8:1:1::/64, Delegated-IPv6-Prefix = 2001:db8:f00::/56, Filter-Id = biz-500m, Acct-Interim-Interval = 300, Class = dave
:105 frank  Framed-IP-Address = 203.0.113.20, Framed-IP-Netmask = 255.255.255.255, Delegated-IPv6-Prefix = 2001:db8:200:c30::/60, Framed-IPv6-Pool = biz6, Filter-Id = biz-500m, Acct-Interim-Interval = 300, Class = frank
```

（從追蹤行整理出來的；IPv4 位址是隨機挑的。）`:110`、`:111` 拿走 `tiny6` 的兩個 /64，`:112` 被拒絕：

```text
… script line.lua (radius/authentication, client bng): prefix pool tiny6 is exhausted: no prefix for max
```

這時 `rradiusd query pool_usage` 的 `res4 reserved` 仍是 3（`alice`、`kim`、`lee`），`max` 剛預留的 IPv4 已經放回。

計費用 `session` 負載測：它把 Access-Accept 的 `Class` 與 `Framed-IP-Address` 帶進 Start 與 Stop，就像 BNG 那樣。Stop 之後，那條線的 IPv4 回到 `free`，前綴變成 `released`。

注意事項：

- 測試時 `User-Name` 是 `line`，計費腳本完全不看它，只看 `Class`。真的 BNG 若不帶回 `Class`，這條線的計費不會被處理，要先確認 BNG 的設定。
- 三個常數（`RESERVE_SECONDS`、`HOLD_SECONDS`、`REAP_SECONDS`）在 `line.lua` 與 `accounting.lua` 各有一份，要一致。

### 資料怎麼維護

```sql
-- 新增一條動態線路
INSERT INTO lines (remote_id, circuit_id, subscriber, plan) VALUES ('olt-a', 'eth 1/1/2:100', 'zoe', 'residential');
-- 給一條線固定的 IPv4 與前綴（欄位是 NULL 就不是固定的）
UPDATE lines SET ipv4 = '203.0.113.30', pd_v6 = '2001:db8:f10::/56' WHERE subscriber = 'zoe';
-- 停用
UPDATE lines SET enabled = 0 WHERE subscriber = 'zoe';
```

改資料要寫成新編號的 migration 檔，放到每個節點、逐台重啟（見[第 18 章](18-operations.md#新增-migration)）。`remote_id`、`circuit_id` 要寫成正規化後的值（見〈依線路身分認證〉）；IPv6 前綴一律寫成小寫、壓縮的寫法。固定的 `ipv4`、`wan_v6`、`pd_v6` 各有唯一索引（`lines_ipv4`、`lines_wan_v6`、`lines_pd_v6`），兩條線不能登記同一個值。

**線路在動態與固定之間切換時，它在池裡原有的列不會立刻消失：** 位址或前綴要等計費停了、到期之後，才由請求觸發的回收放回；在那之前它仍佔著池的一個位置。

### 叢集

認證與計費的寫入都經過叢集的 leader：每次登入都會寫 `lines` 的 `last_mac`、`last_seen`，動態線路還要預留位址與前綴。叢集沒有 leader 時（見[第 16 章](16-cluster.md)），登入會失敗，連全固定資料的線路也一樣。不需要「最後看到」的資訊時，把 `line.lua` 最後那兩個 `UPDATE lines SET last_mac …`／`SET last_seen …` 拿掉，固定線路就不再依賴 leader；動態線路預留位址與前綴一定要寫，拿不掉。

### 規模

下面是**合併後每次登入多出的成本**，取自舊版手冊第 22 章，**未重新量測**（我沒有在變更紀錄裡找到這組原始檔，只有舊章的記載）。條件：Apple M4 Max（14 CPU，macOS），伺服器與 `rradius-bench` 同一台，單節點；64 個並行的 PAP 登入，各 2,000 個不同用戶第一次登入；IPv4 池與前綴池各 16,384 個，事先分散地佔了 43%，量測結束時約 55%；各跑兩次。

| 範例 | 每秒登入 | p50 | p99 |
| --- | --- | --- | --- |
| 位址池（ip-pool） | 約 14,500（14,473、14,674） | 約 3.6 ms | 約 21 ms |
| 前綴池（prefix-pool） | 約 11,200（11,075、11,288） | 約 4.8 ms | 約 22 ms |
| dual-stack-lines 的動態線路（IPv4 加前綴） | 約 6,100（6,102、6,172） | 約 9.4 ms | 約 34 ms |

動態線路每次登入約 163 微秒，大約是兩個單獨的池（約 69 加 90 微秒）的和，再加線路的查詢與最後看到的寫入；沒有拒絕、沒有重複分配。吞吐沒有低於較慢的單獨範例（前綴池）的一半（約 5,600）。這些是同一台機器上的相對數字，不是 BNG 現場的容量；量測時用 `User-Name` 當線路名稱（`rradius-bench` 會輪流換它），不是 Option 82 屬性，兩者的差別只是多取一個屬性。固定線路不碰池，登入只有查詢加一次寫入，比動態線路便宜。

### 已知限制

- **沒有對真實 BNG 驗證。** 屬性用標準的 `Framed-*` 和 RFC 4818 的 `Delegated-IPv6-Prefix`；BNG 是否接受、`Framed-Route` 的寫法、是否在計費封包帶回 `Class`，要看你的 BNG 文件。
- **固定值不和池比對。** 固定的 `ipv4` 若落在 `ip_pool` 的範圍內，或固定的 `pd_v6` 落在前綴池裡，池可能把同一個值再分給別人；固定位址請放在池以外的網段（見〈固定位址與前綴〉）。
- **線路只在登入時檢查。** 停用一條線不會中斷已經在線的連線（中斷要用 Disconnect，企業版功能）。
- **前綴不保證穩定。** 起點被別人佔走時拿到不同的，池越滿越不穩（見〈IPv6 前綴池〉）。
- **一個 subscriber 一條線。** 池與計費用 `lines.subscriber` 當鍵（`lines_subscriber` 唯一索引）；要一個用戶多條線，得改池的鍵。
- **計費只認 `Delegated-IPv6-Prefix`。** 只帶 `Framed-IPv6-Prefix`（WAN 前綴）的封包不會確認前綴。
- **沒有 WAN 的 /64 池。** `Framed-IPv6-Pool` 只是池的名稱，連線本身的位址由 BNG 的池決定。
- **只認 RFC 4679 的屬性和 `NAS-Port-Id`**，MAC 鎖與線路身分的安全限制同〈依線路身分認證〉：線路身分不是祕密，用戶端清單只列 BNG。

### 常見問題

| 現象 | 原因 |
| --- | --- |
| 一直是 Reject，日誌有 `dual stack: unknown line` | 資料表裡的值和 BNG 送的不同，處理方式同〈依線路身分認證〉 |
| 日誌有 `has routed_v4 or netmask but no ipv4` | 線路登記了 `routed_v4` 或 `netmask`，卻沒有 `ipv4`；補上 `ipv4` 或清掉它們 |
| 日誌有 `dual stack: bad … for` | 固定資料的值不合法（位址、子網段、遮罩、前綴）；日誌寫出用戶、欄位和值，不會悄悄退回動態 |
| 日誌有 `prefix pool … is exhausted` | 前綴池沒有空位；擴充池，或把 `ALLOW_V4_ONLY` 改成 `true` |
| 日誌有 `ip pool … is exhausted` | IPv4 池沒有空位；一律拒絕，不受 `ALLOW_V4_ONLY` 影響 |
| 計費封包沒有改變池 | 日誌有 `no Class`：BNG 沒有在計費封包帶回 `Class` |
| 登入成功但沒有 `Delegated-IPv6-Prefix` | 線路的 `pd_v6` 是 NULL 又設了 `wan_v6`（只回有值的），或 `ALLOW_V4_ONLY` 是 `true` 而且前綴池滿了 |

## 固定位址與前綴：static-ip

### 目標

商業用戶或特定方案要固定的 IPv4 位址、路由給他的子網段、固定的 IPv6 前綴。沒有固定資料的用戶照常回池名稱，由 BNG 分配。

### 資料表

`subscribers` 有 `pool`（沒有固定資料時回的池名稱）。`static_assignments` 只有有固定資料的用戶才有一列，欄位都可以是 `NULL`，`NULL` 的就不回那個屬性：

| 欄位 | 回成 | 例子 |
| --- | --- | --- |
| `ipv4` | `Framed-IP-Address`，加 `Framed-IP-Netmask` | `203.0.113.10` |
| `netmask` | `Framed-IP-Netmask`，沒設時 `255.255.255.255` | `255.255.255.252` |
| `routed_v4` | `Framed-Route`：`<子網段> 0.0.0.0 1` | `198.51.100.8/29` |
| `wan_v6` | `Framed-IPv6-Prefix`，連線本身的前綴，通常 /64 | `2001:db8:1:1::/64` |
| `pd_v6` | `Delegated-IPv6-Prefix`，委派給用戶內網的前綴 | `2001:db8:100::/56` |

`ipv4`、`wan_v6`、`pd_v6` 各有唯一索引，兩個用戶不能登記同樣的文字。

> **注意：** 唯一索引比的是文字。IPv6 前綴一律寫成小寫、壓縮的寫法（`2001:db8:1:1::/64`）；`2001:0db8:1:1::/64` 與 `2001:DB8:1:1::/64` 是不同的文字，索引擋不住。重疊的網段（某人的 /29 包含另一個人的位址）也擋不住，要在建資料的流程裡檢查。

### 唯一索引擋什麼、擋不住什麼

三個唯一索引（`static_assignments_ipv4`、`static_assignments_wan_v6`、`static_assignments_pd_v6`）擋的是：兩個用戶登記同樣的文字時，後寫的那個 `INSERT` 或 `UPDATE` 失敗，錯誤是 `UNIQUE constraint failed`。`NULL` 不算，很多人可以都沒有 `wan_v6`。擋不住的，除了上面注意事項說的前綴寫法與重疊網段，還有：

- **固定位址落在動態池裡。** ip-pool 情境的池不知道哪些位址被固定了，會把同一個位址發給別人。固定位址一定要放在池的範圍之外。兩個情境一起用時，可以查有沒有撞到（只比對完全相同的位址）：

  ```sql
  SELECT s.username, s.ipv4 FROM static_assignments s JOIN ip_pool p ON p.ip = s.ipv4;
  ```

  這是偶爾跑一次的維護查詢：`ip_pool` 沒有以 `ip` 開頭的索引，池很大時會掃整個表。不同的池（例如不同的 VRF）可以有相同的位址，所以結果要再看是不是同一個池。

### 設定重點

`all_accept`：sql 處理器驗密碼，lua 處理器 `static.lua` 回位址屬性。`[lua]` 的 `accounting.lua` 只擋掉非 RADIUS 的協定，計費一律回覆，不記錄。

### 腳本重點

`static.lua` 的原則是「資料有錯就拒絕」。付費要固定位址的用戶，如果因為資料打錯而悄悄拿到一個會變的位址，沒有人會發現。所以：

- 用 `pcall(set_attr, …)` 設屬性。`set_attr` 依字典檢查值的格式，不合法時丟錯；腳本接住，把用戶、欄位、值寫進日誌，然後拒絕。
- `Framed-Route` 在字典裡是字串，`Framed-IP-Netmask` 是位址，`set_attr` 擋不住內容不對的值（例如 `garbage`、`255.0.255.0`），所以腳本自己檢查：子網段要是 `a.b.c.d/n`、主機位元是 0；遮罩的 1 要連續在前面。
- 沒有那一列，或那一列沒有任何位址、網段、前綴（只有 `netmask` 也算沒有），就和沒有固定資料一樣，回 `Framed-Pool`。

### 怎麼測與注意事項

已驗證。五個示範帳號的回覆（從追蹤行整理）：

| 帳號 | 回覆 |
| --- | --- |
| `alice` | `Framed-Pool = residential` |
| `bob` | `Framed-IP-Address = 203.0.113.10, Framed-IP-Netmask = 255.255.255.255, Acct-Interim-Interval = 300` |
| `carol` | `Framed-IP-Address = 203.0.113.20, Framed-IP-Netmask = 255.255.255.252, Framed-Route = 198.51.100.8/29 0.0.0.0 1, Framed-IPv6-Prefix = 2001:db8:1:1::/64, Delegated-IPv6-Prefix = 2001:db8:100::/56, Acct-Interim-Interval = 300` |
| `dave` | `Framed-IPv6-Prefix = 2001:db8:1:2::/64, Delegated-IPv6-Prefix = 2001:db8:200::/56, Acct-Interim-Interval = 300` |
| `erin` | reject（停用） |

想看資料錯誤的行為，在啟動前多放一個 migration，例如 `migrations/3_bad.sql`：

```sql
UPDATE static_assignments SET netmask = '255.0.255.0' WHERE username = 'bob';
UPDATE static_assignments SET pd_v6 = '2001:db8:200::/200' WHERE username = 'dave';
```

`bob` 與 `dave` 都變成 reject，日誌寫著：

```text
… script static.lua (radius/authentication, client localhost): static ip: bad netmask for bob : 255.0.255.0
… script static.lua (radius/authentication, client localhost): static ip: bad pd_v6 for dave : 2001:db8:200::/200 - runtime error: '2001:db8:200::/200' is not a valid ipv6prefix for 'Delegated-IPv6-Prefix' …
```

這個 migration 只用來實驗，測完停掉伺服器、刪掉 `data/` 與這個檔案。

### 維護固定資料

`static_assignments` 沒有專用的管理指令。改資料要寫成新編號的 migration 檔，放到每個節點、逐台重啟（見[第 18 章](18-operations.md#新增-migration)）；用戶很多、常變動時，把這些資料放在外部資料庫或帳務系統，讓腳本用 `db_query` 或 `http_request` 查（[第 09 章](09-external-databases.md)、[第 10 章](10-lua-policies.md)）。SQL 是這樣：

```sql
-- 新增用戶和固定位址
INSERT INTO subscribers (username, password, pool, enabled) VALUES ('gina', 'gina-password', 'business', 1);
INSERT INTO static_assignments (username, ipv4, netmask, routed_v4, wan_v6, pd_v6)
VALUES ('gina', '203.0.113.30', '255.255.255.252', '198.51.100.16/29', '2001:db8:1:3::/64', '2001:db8:300::/56');

-- 改回動態：刪掉那一列
DELETE FROM static_assignments WHERE username = 'gina';
```

BNG 要接受這些屬性：哪些設備會用 `Framed-Route`、`Delegated-IPv6-Prefix`，取決於廠商與設定，請看 BNG 的文件；BNG 沒有路由出 `/29` 時，先確認這一項。

### 已知限制

- **不防止重疊網段，也不檢查固定位址在不在動態池裡**（見上）。
- **同一個用戶同時兩個 session 會拿到同一個固定位址。** 要限制同時連線數，把〈方案與用量控制〉的 `limits.lua` 接在前面：`[[auth.handlers]]` 的順序是 sql 處理器、`limits.lua`、`static.lua`。連線位要靠計費腳本確認與釋放，所以 `[lua]` 的 `script` 要換成 usage-control 的 `accounting.lua`，並把它用到的表（`plans`、`session_slots`、`accounting`、`housekeeping`）加進 migration；static-ip 自己的 `accounting.lua` 只回覆、不碰連線位，連線位預留 120 秒後就放回，上限形同虛設。
- **沒有動態的 IPv6 前綴。** 這個情境只回表裡寫的固定前綴；DHCPv6-PD 的對齊分配見〈IPv6 前綴池〉。
- **不依線路身分給固定位址。** 要做的話，在 `lines`（〈依線路身分認證〉）加欄位，或把 `static_assignments` 的主鍵換成線路。
- **只在登入時決定。** 改了資料不會影響已經在線的連線，要等它下次登入。
- **沒有內建的檢查工具和指標。** 位址與前綴的合法性只在登入時檢查。

### 常見問題

| 現象 | 原因 |
| --- | --- |
| 固定用戶一直是 Reject，日誌有 `static ip: bad` | 那個欄位的值不是合法的位址或前綴；日誌寫出用戶、欄位和值 |
| 登入成功但沒有固定位址 | `static_assignments` 沒有這個用戶的列，或欄位是 `NULL`（只有 `netmask` 也算沒有）；用戶名稱要和 `subscribers` 一致 |
| BNG 沒有路由出 `/29` | BNG 不接受 `Framed-Route` 的這個格式，或要另外設定 |
| 兩個用戶拿到同一個位址 | 固定位址落在動態池裡，或網段重疊；用上面的檢查查詢 |
| `INSERT` 失敗，`UNIQUE constraint failed` | 那個位址或前綴已經登記給別人 |

## 方案與用量控制：usage-control

### 目標

每個用戶有一個方案，方案限制兩件事：同時幾個連線，以及最近一段期間的流量。超量之後依方案降速（登入成功，但回另一個 `Filter-Id`）或拒絕。

### 資料表

| 表 | 內容 |
| --- | --- |
| `plans` | `max_sessions`、`quota_octets`（進加出的位元組，`0` 是不限）、`period_days`（最近幾天，滾動窗口，不是日曆月）、`over_quota`（`throttle` 或 `reject`）、`normal_filter`、`throttle_filter` |
| `subscribers` | 帳號、密碼、`plan`、`enabled` |
| `session_slots` | 每個用戶 `max_sessions` 個連線位，主鍵 `(username, slot)`。同時連線數就是被佔住的連線位數 |
| `accounting` | 每個 session 一列，寫法同 quick-start。主鍵是 `(client, nas, session_id)`，因為 session ID 只在同一台 NAS 內唯一 |

示範方案：`basic` 同時 1 個、不限流量；`family` 同時 2 個、100 GiB／30 天、超過降速成 `family-slow`；`strict` 同時 1 個、5 GiB／30 天、超過拒絕。

連線位在用戶第一次登入時用 `INSERT OR IGNORE` 補齊，所以不必事先建立。

### 設定重點

`all_accept`：sql 處理器驗密碼，lua 處理器 `limits.lua` 檢查方案。`[sessions]` 查 `session_slots` 裡 `active` 且未到期的列；具名查詢 `slot_usage` 變成指標 `rradius_query_slot_usage{state}`。

### 腳本重點

`limits.lua`：

1. 查用戶的方案，停用或不存在的拒絕。
2. 先檢查流量：加總這個用戶最近 `period_days` 天的 `input_octets + output_octets`，包含還開著的 session 目前累計的量。超過了就依 `over_quota` 降速或拒絕。先檢查流量，被拒絕的人就不會佔連線位。
3. 再搶一個空的連線位，搶到就預留 120 秒，回 `Filter-Id` 與 `Acct-Interim-Interval`；搶不到就拒絕。方案改小之後，超出 `max_sessions` 的連線位不再分配。

`accounting.lua`：

- 每個計費封包都寫進 `accounting`。流量取 `max(舊值, 新值)`，重送或晚到的封包不會讓數字倒退。
- 超過 4 GiB 的流量，NAS 會把進位放在 `Acct-Input-Gigawords`、`Acct-Output-Gigawords`，腳本把它們乘 4294967296 加回去。
- 用 `Acct-Delay-Time` 還原事件發生的時間，所以從計費 spool 重送或 NAS 晚送的封包，時間仍是對的。
- Start 把預留的連線位確認成 `active`；Interim-Update 延長；找不到連線位時，拿一個預留的或空的（預留已經過期、封包晚到的情況）。Stop 釋放；Accounting-On／Off 把這台 NAS 的全部放回。

### 為什麼用連線位，不是登入時數 accounting 表

登入時數「這個用戶還開著幾個 session」比較簡單，但有兩個洞：同時進來的登入彼此看不到對方；Accounting-Start 還沒到之前，預留的也數不到。上限會被突破。連線位把「佔一個位子」做成一次有條件的寫入（`UPDATE … WHERE state = 'free'`，影響 1 列才算搶到），寫入都經過叢集的 leader，所以兩個同時進來的登入不可能搶到同一個連線位。

### 要調的數字

| 數字 | 在哪 | 預設 | 說明 |
| --- | --- | --- | --- |
| `RESERVE_SECONDS` | `limits.lua` | 120 | 預留多久；NAS 通常在登入後幾秒內送 Start |
| `INTERIM_SECONDS` | `limits.lua`、`accounting.lua` | 300 | 要求 NAS 多久送一次 Interim-Update；連線位到期是它的三倍 |
| `REAP_SECONDS` | `accounting.lua` | 60 | 回收多久最多跑一次（`limits.lua` 內寫死 60） |

兩個腳本是獨立的檔案，沒辦法共用常數，所以 `INTERIM_SECONDS` 要兩邊一致。

### 怎麼測與注意事項

已驗證。同時連線數：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 819/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 3516/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

第二次被拒絕，日誌寫 `usage control: session limit of plan basic ( 1 ) reached by alice - rejected`。`bob` 的 `family` 方案可以登入兩次，第三次被拒絕。

> **注意：** 第一次登入雖然還沒有 Accounting-Start，連線位已經預留了 120 秒。測試時連續登入兩次，第二次一定被拒絕；這不是錯誤，是預留在擋。要讓位子空出來，送 Stop，或等預留過期。

流量：用 `session` 負載加上 `--attr 52=int:N`（`Acct-Input-Gigawords`），一次送出 N × 4 GiB 的用量：

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user carol --password carol-password --load session,concurrency=1 --requests 1 --attr 52=int:2
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user carol --password carol-password --load pap,concurrency=1 --requests 1
…
  pap: 3180/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`carol` 用了 8 GiB，超過 `strict` 的 5 GiB，日誌寫 `quota of plan strict is used up by carol - rejected`。`dave`（`family`）送 `--attr 52=int:25`（100 GiB）之後再登入，仍然成功，但 `Filter-Id` 從 `family` 變成 `family-slow`。

注意事項：

- 流量在登入時檢查。用戶在線時超量，這個情境不會把他踢下線，要等他下次登入才降速或拒絕。
- 期間是滾動的最近 `period_days` 天，以 session 的開始時間計算。要改成每月歸零，改 `limits.lua` 的那條 `SELECT`。

> **企業版：** 企業版可以在計費時對超量的用戶送 CoA 或 Disconnect-Request，不必等下次登入。

### 維運：查連線位與流量、換方案

```sql
-- 某個用戶的連線位
SELECT slot, state, session_id, nas, expires FROM session_slots WHERE username = 'bob' ORDER BY slot;
-- 他最近 30 天的流量（位元組）
SELECT sum(input_octets + output_octets) FROM accounting WHERE username = 'bob' AND started >= strftime('%s', 'now') - 30 * 86400;
-- 換方案：下一次登入就依新方案
UPDATE subscribers SET plan = 'family' WHERE username = 'bob';
-- 改方案的上限：family 改成 200 GiB（200 × 1073741824）
UPDATE plans SET quota_octets = 214748364800 WHERE plan = 'family';
```

縮小 `max_sessions` 時，編號超過新上限的連線位不再分配；在線的 session 不受影響，結束後那個連線位就不再被用到。`plans` 與 `subscribers` 的資料用新編號的 migration 檔改，或放在外部資料庫（見〈依線路身分認證〉的〈維護線路資料〉）。

### 監控

`slot_usage` 的指標是 `rradius_query_slot_usage{state}`，每個節點每 30 秒數一次：

```toml
# rradius.toml（節錄，其他設定略）
[queries.slot_usage]
sql = "SELECT state, count(*) AS slots FROM session_slots GROUP BY state"
help = "Session slots by state: free, reserved or active."
labels = ["state"]
value = "slots"
```

```console
$ rradiusd query slot_usage --config rradius.toml
```

- 現在有多少連線在線：`max(rradius_query_slot_usage{state="active"})`；`reserved` 是登入了、NAS 還沒送 Accounting-Start 的。每個節點輸出同樣的數字，叢集用 `max()`，不要 `sum()`。
- 連線位只在用戶第一次登入時建立，所以 `free` 的數字是「登入過的用戶沒用到的位」，不是全部用戶的容量。
- 成本：這個查詢走 `session_slots_expiry` 的覆蓋索引（`SCAN session_slots USING COVERING INDEX session_slots_expiry`）。2026-11-16 在 SQLite 3.54.1 的記憶體資料庫、20 萬列上量到中位數約 4.3 毫秒；未重新量測。
- 流量用了多少是另一回事：`accounting` 的加總會隨用戶的 session 數變慢，不要放進 30 秒一次的指標。

### 已知限制

- **只在登入時檢查。** 一個 session 在線期間用掉的流量，要等用戶下次登入才檢查，不會中斷在線的連線。要中途強制（超過上限就降速或斷線）需要 CoA 與 Disconnect-Request，那是企業版的功能。
- **計量期間是滾動的最近 N 天，** 不是日曆月，沒有帳單日和時區。
- **流量加總隨用戶的 session 數變慢。** 有 `accounting_user` 索引、只看一個用戶，但用戶多、session 多時要先實測；必要時另存每個用戶的累計表。
- **用盡時 NAS 只看到 Reject。** 原因在日誌，監控日誌裡的 `session limit` 與 `quota`。
- **沒有預付餘額與線上計費，** 也沒有內建函式；連線位的使用率靠上面的 `slot_usage`。
- **NAS 當機沒送 Stop 時，** 連線位佔到期限（3 個 Interim 間隔，900 秒）才回收；NAS 回來送 Accounting-On 會立刻釋放。
- **帳號名稱的大小寫。** 連線位與流量都以登入名稱為準；要不分大小寫，在驗證鏈設定 `[auth.username]`（見[第 07 章](07-users-and-chains.md)）。

### 常見問題

| 現象 | 原因 |
| --- | --- |
| 登入成功但沒有 `Filter-Id` | 設定沒有 lua 處理器，或 `policy` 不是 `all_accept`，或方案的 `normal_filter` 是空字串 |
| 一直是 Reject，日誌有 `session limit` | 連線位被佔住：查 `session_slots`，看有沒有已經到期卻還沒回收的（沒有請求進來就不會回收） |
| 一直是 Reject，日誌有 `quota` | 方案是 `reject` 而且流量用完了；查 `accounting` 的加總 |
| 流量沒有增加 | NAS 沒送 Interim-Update 或 Stop；`accounting` 的 `input_octets` 是累計值 |
| 超過上限的人沒有被降速 | 降速只在下一次登入時生效；`Filter-Id` 要由 NAS 對應成頻寬 |

## 依 BNG 回速率：rate-limit

### 目標

同一個方案，不同廠商的 BNG 要的速率屬性不同：有的只收設定檔名稱，有的收數值。每台 BNG 是一個用戶端，用一張表記它用哪一種「風格」。

### 資料表

| 表 | 內容 |
| --- | --- |
| `plans` | `down_kbps`（往用戶的方向）、`up_kbps`（用戶往網路的方向），單位 kbit/s；`profile` 是 BNG 上同名設定檔的名稱 |
| `subscribers` | 帳號、密碼、`plan`、`enabled` |
| `nas_styles` | `client` 是 `[[radius.clients]]` 的 `name`，`style` 是風格。沒有列的用戶端用 `profile` |

### 設定重點

每台 BNG 一個用戶端，`name` 要和 `nas_styles.client` 一樣：

```toml
# rradius.toml（節錄，其他設定略）
[[radius.clients]]
name = "bng-default"
address = "127.0.0.1"
secret_file = "localhost.secret"

[[radius.clients]]
name = "bng-nokia"
address = "10.70.0.1"
secret_file = "nokia.secret"
```

情境裡另有 `bng-mikrotik`、`bng-wispr`、`bng-erx` 三個。驗證鏈同樣是 `all_accept`：sql 處理器驗密碼，lua 處理器 `rate.lua` 回速率屬性。

### 腳本重點

`rate.lua` 的 `styles` 表每一項是一個函式，要支援別的 BNG 就加一項：

| 風格 | 回的屬性 | 說明 |
| --- | --- | --- |
| `profile` | `Filter-Id = <profile>` | 標準屬性，速率在 BNG 上同名的設定檔裡 |
| `alc_sla` | `Alc-SLA-Prof-Str = <profile>` | 廠商屬性，SLA 設定檔名稱 |
| `erx` | `ERX-Ingress-Policy-Name = <profile>-up`、`ERX-Egress-Policy-Name = <profile>-down` | 進（Ingress）是用戶上傳，出（Egress）是用戶下載 |
| `mikrotik` | `Mikrotik-Rate-Limit = <up>k/<down>k` | 從路由器看：rx 是用戶上傳、tx 是用戶下載；`k` 是 kbit/s |
| `wispr` | `WISPr-Bandwidth-Max-Down`、`WISPr-Bandwidth-Max-Up`，單位 bit/s | 32 位元整數，最大 4294967295 bit/s；更快的方案送不出去，所以拒絕並記錄 |

只送名稱的風格，速率的單位與方向由 BNG 上的設定檔決定；送數值的風格，單位與方向取自屬性的定義。實際接上之前，請對照你的 BNG 的文件確認。

以示範方案 `fiber-500`（`down_kbps` 500000、`up_kbps` 250000、`profile` 是 `fiber-500`）為例，各風格實際送出的值：

| 風格 | 屬性與值 |
| --- | --- |
| `profile`（預設） | `Filter-Id = fiber-500` |
| `alc_sla` | `Alc-SLA-Prof-Str = fiber-500` |
| `erx` | `ERX-Ingress-Policy-Name = fiber-500-up`、`ERX-Egress-Policy-Name = fiber-500-down` |
| `wispr` | `WISPr-Bandwidth-Max-Down = 500000000`、`WISPr-Bandwidth-Max-Up = 250000000` |
| `mikrotik` | `Mikrotik-Rate-Limit = 250000k/500000k` |

`erx` 的政策名稱是 `profile` 加 `-up`、`-down`，BNG 上要先有這兩個名稱的政策；`profile`、`alc_sla` 的設定檔也要先在 BNG 上存在。

**送數值的風格（`wispr`、`mikrotik`）的單位和方向是依屬性的定義寫的，沒有用真實設備驗證**（這份手冊與範例的測試只檢查屬性的值，沒有接過真的 BNG）。上線前用一個測試用戶在實機上量一次速率；速率差 1000 倍通常就是單位不符（見下面〈常見問題〉）。

風格由 Lua 變數 `client`（比對到的用戶端名稱）查 `nas_styles` 決定。`nas_styles` 裡寫了腳本不認得的風格、或用戶的方案不存在，都拒絕並寫日誌。

### 怎麼測與注意事項

已驗證。`bng-default` 就是 `127.0.0.1`，`alice`（`fiber-100`）登入得到 `Filter-Id = fiber-100`；`dave` 的方案 `gone` 不存在，被拒絕，日誌寫 `rate limit: the plan of dave does not exist`。

其他風格要讓請求從那台 BNG 的位址來。在實驗目錄裡把 `bng-default` 的位址改成別的（例如 `10.70.0.9`），把要測的那台改成 `127.0.0.1`，然後 reload：

```console
$ kill -HUP <伺服器的 pid>
$ rradius-bench run --server 127.0.0.1 --secret "$(cat nokia.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
```

用戶端清單可以 reload，日誌會寫 `configuration reloaded from rradius.toml`。依序換成四台，`alice` 拿到的回覆：

```text
bng-nokia     Alc-SLA-Prof-Str = fiber-100
bng-mikrotik  Mikrotik-Rate-Limit = 50000k/100000k
bng-wispr     WISPr-Bandwidth-Max-Down = 100000000, WISPr-Bandwidth-Max-Up = 50000000
bng-erx       ERX-Ingress-Policy-Name = fiber-100-up, ERX-Egress-Policy-Name = fiber-100-down
```

每一個後面都還有 `Acct-Interim-Interval = 300`。

注意事項：

- 用戶端名稱是對應的鍵。改了 `[[radius.clients]]` 的 `name`，`nas_styles` 要跟著改，否則那台 BNG 會退回 `profile` 風格，只拿到 `Filter-Id`。
- 一個用戶端可以是一個網段；同一個網段裡的 BNG 必須是同一種風格。不同廠商的 BNG 分成不同的用戶端。

### 新增一種風格

`rate.lua` 開頭 `styles` 表的每一項是一個函式，收到方案（`plan.plan`、`plan.down_kbps`、`plan.up_kbps`、`plan.profile`），用 `set_attr` 回屬性。例如某個廠商把速率放在單一字串屬性（`My-Vendor-Rate` 是示意名稱，要先有字典定義）：

```lua
  myvendor = function(plan)
    set_attr('My-Vendor-Rate', string.format('%d/%d', plan.down_kbps, plan.up_kbps))
  end,
```

函式回 `false` 表示這個方案送不出去（`wispr` 就是這樣拒絕過快的方案），沒有回值就是送了。`set_attr` 會依字典檢查屬性名稱與值的型別：名稱不在字典裡、或值不是那個型別，登入因腳本錯誤被拒絕，日誌寫 `Lua script error`。沒有收錄的廠商，要先把它的屬性加進字典（見[第 06 章](06-dictionary.md)）。

### 新增一個 BNG

1. 在 `rradius.toml` 加一個 `[[radius.clients]]`（名稱、位址、`secret_file`）。
2. 在 `nas_styles` 加一列，寫成新編號的 migration 檔（例如 `3_bng_new.sql`），放到每個節點、逐台重啟（見[第 18 章](18-operations.md#新增-migration)）；`client` 要和設定檔的 `name` 一樣：

```sql
INSERT INTO nas_styles (client, style) VALUES ('bng-new', 'wispr');
```

沒有列在 `nas_styles` 的用戶端用 `profile`，只拿到 `Filter-Id`。

### 資料怎麼維護

`plans`、`subscribers`、`nas_styles` 是一般的資料表，沒有專用的管理指令；改資料要寫成新編號的 migration 檔。用戶很多、常變動時，把資料放在外部資料庫或帳務系統，讓腳本用 `db_query` 或 `http_request` 查（[第 09 章](09-external-databases.md)、[第 10 章](10-lua-policies.md)）。SQL 是這樣：

```sql
INSERT INTO plans (plan, down_kbps, up_kbps, profile) VALUES ('fiber-2g', 2000000, 2000000, 'fiber-2g');
UPDATE subscribers SET plan = 'fiber-500' WHERE username = 'alice';
```

### 已知限制

- **沒有用真實 BNG 驗證任何一種風格**；送數值的風格的單位和方向要自己對照文件確認。
- **`wispr` 風格送不出超過 4294967295 bit/s（約 4.29 Gbit/s）的方案。** 屬性只有 32 位元，腳本遇到更快的方案拒絕登入並寫 `is faster than WISPr-Bandwidth can say`，不送被截斷的值；那種方案請對這個 BNG 改用只送名稱的風格。
- **一個方案只有一個下載值與一個上傳值。** 沒有 burst、沒有時段、沒有用量用完後降速；降速見〈方案與用量控制〉。
- **只在登入時決定。** 改了方案不影響已經在線的連線，要等它下次登入；連線中改速率要用 CoA（企業版功能，不在核心）。
- **一個用戶端只用一種風格。** 一台 BNG 要同時送兩種風格的屬性，要在腳本裡自己組。
- **字典要認得屬性。** 否則 `set_attr` 失敗，見上面〈新增一種風格〉。

### 常見問題

| 現象 | 原因 |
| --- | --- |
| 一直是 Reject，日誌有 `unknown or disabled user` | 用戶不在 `subscribers` 或 `enabled = 0` |
| 一直是 Reject，日誌有 `the plan of … does not exist` | `subscribers.plan` 在 `plans` 裡沒有對應的方案 |
| 一直是 Reject，日誌有 `which this script does not know` | `nas_styles.style` 的名稱不在 `rate.lua` 的 `styles` 表裡 |
| 一直是 Reject，日誌有 `is faster than WISPr-Bandwidth can say` | 方案速率超過 `wispr` 風格的上限（4294967295 bit/s） |
| 一直是 Reject，日誌有 `Lua script error` | 風格函式用了字典裡沒有的屬性，或值的型別不對 |
| 登入成功但 BNG 沒有限速 | BNG 上沒有同名的設定檔或政策；或送數值的風格，單位、方向和 BNG 的不同；或這個用戶端沒有列在 `nas_styles`，回的是 `Filter-Id` |
| 速率差 1000 倍 | 單位不符：`wispr` 是 bit/s，`mikrotik` 字串的 `k` 是 kbit/s；對照 BNG 的文件 |

## 單檔範例

`rradius-server/examples/` 底下有三個與本章相關的單檔腳本。它們比上面的情境簡單，表的結構寫在檔案開頭的註解裡，要自己建。三個都只檢查政策、不驗密碼，用法相同：

- 當 `[lua]` 的 `script`，計費一定會來到這裡。
- 同時當驗證鏈裡的 lua 處理器，放在 sql 或 ldap 處理器之後，`policy = "all_accept"`，登入要兩個都通過。

```toml
# rradius.toml（節錄，其他設定略）
[lua]
script = "simultaneous-use.lua"

[auth]
policy = "all_accept"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"

[[auth.handlers]]
type = "lua"
script = "simultaneous-use.lua"
```

三個都在 quick-start 改出來的實驗目錄裡，照檔案開頭的 `CREATE TABLE` 建表、用 `rradius-bench` 測過，已驗證。

### ip-pool.lua：最小的位址池

一個表 `ip_pool (ip, holder, expires)`。登入時隨機挑 8 個 `expires` 已過的位址，用 `UPDATE … WHERE expires <= ?` 搶；Start 與 Interim-Update 延長 `LEASE`（900 秒），Stop 釋放。它和 ip-pool 情境是兩種不同的設計：

| | `ip-pool.lua` | ip-pool 情境 |
| --- | --- | --- |
| 池 | 一個 | 多個，依用戶分 |
| 重新登入 | 不認得自己正在用的位址，會再佔一個 | 沿用已有的位址 |
| 黏著 | 沒有 | 一天內拿回上次的位址 |
| 預留 | 沒有，登入就租 900 秒 | 預留 120 秒，Start 才確認 |
| Stop 的判斷 | 只看 `holder` 是不是這個用戶 | 看 session ID |

實測：池裡只有一個位址時，`alice` 第一次登入拿到 `10.0.0.1`，在 Stop 之前再登入就被拒絕，日誌寫 `no address free for alice`；送一筆帶 `--attr 8=ip:10.0.0.1` 的 `acct-stop` 之後，`bob` 拿到它。

> **注意：** 檔案註解說明了它的限制：同一個用戶拿回同一個位址之後，重送的舊 Stop 仍會釋放它，從計費 spool 重送回來的 Stop 也一樣。正式環境建議從 ip-pool 情境開始。

### simultaneous-use.lua：同時連線數

一個表 `active_sessions`，主鍵 `(client, session_id)`。Start 與 Interim-Update 寫入（`INSERT OR REPLACE`），Stop 刪除；登入時計算這個用戶 `STALE_AFTER`（900 秒）內有消息的 session，達到 `MAX_SESSIONS`（2）就拒絕。

和 usage-control 的差別：session 從 Accounting-Start 才開始算，所以同一瞬間的兩個登入可以都通過，各節點的副本也可能稍舊，上限不是精確的。實測：`alice` 送兩筆 `acct-start` 之後登入被拒絕，`bob` 仍可登入。NAS 送 Interim-Update 的間隔要比 `STALE_AFTER` 短，否則還在線的 session 會不再被計算。

### quota-and-hours.lua：總時數與時段

兩個表：`quota_users`（`quota_seconds`、`from_hour`、`to_hour`，時間以 UTC 計）與 `quota_sessions`（每個 Stop 一列）。登入時檢查現在是否在允許的時段（可以跨午夜，例如 22 到 6），再算剩下的秒數，用 `Session-Timeout` 回給 NAS。

實測：`alice` 有 100 秒，第一次登入得到 `Session-Timeout = 100`；`rradius-bench` 的 Stop 帶 `Acct-Session-Time = 60`，所以下次得到 `40`，再一次就被拒絕。`from_hour` 與 `to_hour` 都是 0 的帳號一律被拒絕。

時間在 Stop 時才計入，同時的兩個 session 各自拿到剩下的時間：兩個 100 秒的登入可以用掉 200 秒。要嚴格限制，搭配 `simultaneous-use.lua`，上限設 1。刪掉某個用戶在 `quota_sessions` 的列，就是讓他從頭計算，例如每月一次。

## 常見錯誤

**症狀：** 登入被拒絕，日誌沒有腳本寫的原因，認證紀錄是 `handler="no_password_check"`。
**原因：** 帳號在 `subscribers` 裡不存在或停用，sql 處理器沒有給出答案；`all_accept` 的鏈裡有會驗密碼的處理器，卻沒有一個接受，伺服器就拒絕，不讓後面的 lua 處理器替沒驗過密碼的人做事。
**解法：** 確認帳號存在、`enabled = 1`。這是預期的行為，不是設定錯誤。

**症狀：** 池明明還有位址，登入卻得到 `reject`，日誌是 `ip pool … is exhausted`。
**原因：** 位址都在 `reserved`：認證通過了，但 NAS 沒送 Accounting-Start（或送到別的伺服器），預留要 120 秒才過期，回收最多每 60 秒做一次。
**解法：** 用 `rradiusd query pool_usage` 看各狀態的數量；確認 NAS 的計費送到這台伺服器、共用密碼正確。

**症狀：** 計費都有回應，位址卻一直停在 `reserved`，日誌有 `address … reported by … for … is held by someone else`。
**原因：** NAS 回報的 `Framed-IP-Address` 不是 rradius 給的那一個，或 `User-Name` 和認證時不同。
**解法：** 開 `[log.trace]` 對照認證的回覆與計費封包裡的位址、帳號。

**症狀：** IPoE 的計費都回應了，池裡的狀態卻沒有變。
**原因：** BNG 沒有在計費封包帶回 `Class`，`accounting.lua` 認不出這條線，日誌有 `dual stack accounting: no Class in the packet of session …`。
**解法：** 開 `[log.trace]` 看計費請求有沒有 `Class`；在 BNG 上設定把 Access-Accept 的 `Class` 帶進計費。

**症狀：** 用 `rradius-bench` 測 line-identity、dual-stack-lines、rate-limit 時，結果是 `timeouts 1`。
**原因：** 這些情境的用戶端位址是 BNG 的網段，不是 `127.0.0.1`，伺服器把請求當成未知用戶端丟掉。`status` 的 `Discarded: unknown client` 會增加。
**解法：** 只在實驗目錄裡把那個用戶端的 `address` 改成 `"127.0.0.1"`。

**症狀：** 有固定位址的用戶突然登不進去，日誌是 `static ip: bad … for …`。
**原因：** `static_assignments` 的值不合法，腳本故意拒絕，不改成動態。
**解法：** 照日誌寫的欄位修正資料。IPv6 前綴寫成小寫、壓縮的形式。

**症狀：** ip-pool 登入成功，但 Access-Accept 沒有 `Framed-IP-Address`。
**原因：** 驗證鏈沒有 lua 處理器 `allocate.lua`，或 `policy` 不是 `all_accept`（第一個處理器接受後就停了）。
**解法：** 照〈IPv4 位址池〉的〈設定重點〉檢查 `[auth]`。

**症狀：** 位址一直不放回 `free`。
**原因：** NAS 沒送 Stop，也沒送 Interim-Update，要等到期（3 個 Interim 間隔，900 秒）；或沒有任何請求進來，回收是請求觸發的。
**解法：** 用〈維運：查位址與增減位址〉的洩漏查詢看有哪些列已經到期；有請求進來之後，下一個 60 秒的週期就會放回。

**症狀：** 同一個位址發給了兩個人。
**原因：** 不應該發生。常見的是兩個池刻意共用相同的位址（例如不同的 VRF），卻送進同一個路由表；或 `ip_pool` 的主鍵 `(pool, ip)` 被改過；或有固定位址落在池裡（見〈固定位址與前綴〉）。
**解法：** 檢查池的範圍與主鍵，用〈唯一索引擋什麼、擋不住什麼〉的查詢找出撞到的固定位址。

**症狀：** 用戶的同時連線數超過方案的上限，或連線位一直停在 `reserved`。
**原因：** NAS 沒有把 Accounting-Start 送到這台伺服器，或這台伺服器的 `[lua]` 腳本不是 usage-control 的 `accounting.lua`，連線位沒人確認。
**解法：** 查 `session_slots` 的 `state`；確認計費送到這裡，`[lua]` 的 `script` 是 `accounting.lua`。

**症狀：** 改了 `[queries.<name>]` 並 reload，指標沒有變。
**原因：** 具名查詢要重啟才套用。
**解法：** 重新啟動伺服器。

## 相關章節

- [03 第一次啟動與登入](03-quick-start.md)
- [07 使用者帳號與驗證鏈](07-users-and-chains.md)
- [09 外部資料庫與具名查詢](09-external-databases.md)
- [10 用 Lua 寫政策](10-lua-policies.md)
- [13 計費與 session](13-accounting.md)
- [15 情境實作：組織網路存取](15-access-recipes.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 D：Lua API 參考](93-lua-reference.md)
