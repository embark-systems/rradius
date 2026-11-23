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

**監控用具名查詢。** 有池的情境都有 `[queries.<name>]`：每個節點每 30 秒（`interval_ms` 預設 `30000`）查一次自己的副本，結果變成指標 `rradius_query_<name>`；`rradius-server query <name>` 則是馬上查一次，印成表格。具名查詢改了要重啟才生效，細節見[第 09 章](09-external-databases.md)。

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

## 準備實驗目錄

每個情境都照第 03 章的方式複製出來再跑，不要在原始碼目錄裡直接啟動。以 ip-pool 為例，在原始碼 repo 的根目錄執行：

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab
$ cp -R rradius-server/examples/scenarios/ip-pool ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/ip-pool/rradius.toml
$ cd ~/rradius-lab/ip-pool
$ for f in *.secret; do openssl rand -hex 32 > $f; done; chmod 600 *.secret
$ rradius-server check --config rradius.toml
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

接著在一個終端機啟動伺服器（`rradius-server server --config rradius.toml`），在另一個終端機做每一節的測試。本章用到 `rradius-bench` 的三種負載：

- `pap`：送一筆登入。
- `acct-start`：送一筆 Accounting-Start，session ID 每次執行都不同。
- `session`：依序送登入、Start、Stop，三個用同一個 session ID，而且把 Access-Accept 裡的 `Class` 與 `Framed-IP-Address` 帶進兩筆計費，就像真的 NAS 會做的那樣。

要多帶屬性時用 `--attr 類型編號=型別:值`，例如 `--attr 8=ip:10.77.0.4`、`--attr 31=text:00-11-22-33-44-55`、`--attr 52=int:2`。`--attr` 加在每一個送出的請求上，認證與計費都有。

## IPv4 位址池：ip-pool

### 目標

用戶通過密碼驗證後，從他所屬的池拿一個 IPv4 位址，放在 Access-Accept 的 `Framed-IP-Address`。同一個人重新登入拿回同一個位址；離線後，原用戶在一天內優先拿回上次的位址（黏著）；池用完了就拒絕，並在日誌寫明原因。

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
$ rradius-server query pool_usage --config rradius.toml
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
$ rradius-server session list --config rradius.toml
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
- `rradius_query_pool_usage` 每 30 秒才更新一次，剛做完的變化要等下一輪才看得到；要馬上看，用 `rradius-server query pool_usage`。
- 密碼錯、帳號停用或不存在都是 `reject`，而且不佔位址。

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
$ rradius-server query prefix_leases --config rradius.toml
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

## 依線路身分認證：line-identity

### 目標

固網寬頻的 IPoE 用戶沒有帳號密碼，「哪一條線」就是身分：BNG 把 DHCP Option 82 的 circuit-id 與 remote-id 放進 RADIUS 請求（RFC 4679）。認得的線路回位址池名稱與設定檔名稱，不認得的、停用的拒絕；需要的話，還可以把線路綁定第一次看到的 MAC 位址。

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

這時 `rradius-server query pool_usage` 的 `res4 reserved` 仍是 3（`alice`、`kim`、`lee`），`max` 剛預留的 IPv4 已經放回。

計費用 `session` 負載測：它把 Access-Accept 的 `Class` 與 `Framed-IP-Address` 帶進 Start 與 Stop，就像 BNG 那樣。Stop 之後，那條線的 IPv4 回到 `free`，前綴變成 `released`。

注意事項：

- 測試時 `User-Name` 是 `line`，計費腳本完全不看它，只看 `Class`。真的 BNG 若不帶回 `Class`，這條線的計費不會被處理，要先確認 BNG 的設定。
- 三個常數（`RESERVE_SECONDS`、`HOLD_SECONDS`、`REAP_SECONDS`）在 `line.lua` 與 `accounting.lua` 各有一份，要一致。

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
**解法：** 用 `rradius-server query pool_usage` 看各狀態的數量；確認 NAS 的計費送到這台伺服器、共用密碼正確。

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
