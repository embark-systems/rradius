# 17. 固定位址與前綴（IPv4、路由網段、IPv6）

**情境：** 商業用戶和部分住宅方案要固定的 IPv4 位址、路由給他們的子網段，以及固定的 IPv6 前綴：連線本身的 /64，和委派給用戶內網的 /56 或 /48。rradius 在登入時依資料表回這些屬性，沒有固定資料的用戶回位址池的名稱，位址由 BNG 的池決定（動態分配見[第 14 章](14-ip-pool.md)）。

範例目錄：[`rradius-server/examples/scenarios/static-ip/`](../../rradius-server/examples/scenarios/static-ip/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔：`all_accept` 串接，先 `sql` 處理器驗密碼，再 `lua` 處理器回位址屬性 |
| `static.lua` | 依用戶查 `static_assignments`，回位址、路由網段、IPv6 前綴 |
| `accounting.lua` | 計費一律回覆 |
| `migrations/1_schema.sql` | `subscribers`（用戶）、`static_assignments`（固定資料，`ipv4`／`wan_v6`／`pd_v6` 各有唯一索引） |
| `migrations/2_demo_data.sql` | 五個示範用戶 |
| `*.secret` | 叢集和 NAS 的共用密碼（示範值） |

## 回哪些屬性

有 `static_assignments` 那一列的用戶，有哪個欄位就回哪個屬性：

| 欄位 | 屬性 | 值的樣子 |
| --- | --- | --- |
| `ipv4` | `Framed-IP-Address`（RFC 2865） | `203.0.113.20` |
| `netmask` | `Framed-IP-Netmask` | 有 `ipv4` 時一定回；欄位是空的就回 `255.255.255.255` |
| `routed_v4` | `Framed-Route` | `198.51.100.8/29 0.0.0.0 1`（子網段、下一跳 0.0.0.0 表示送到這個連線、metric 1） |
| `wan_v6` | `Framed-IPv6-Prefix`（RFC 3162） | `2001:db8:1:1::/64`：連線本身的前綴 |
| `pd_v6` | `Delegated-IPv6-Prefix`（RFC 4818） | `2001:db8:100::/56`：委派給用戶內網的前綴 |

另外都回 `Acct-Interim-Interval`（300 秒）。沒有那一列的用戶只回 `Framed-Pool`（來自 `subscribers.pool`）。**BNG 要接受這些屬性**：哪些設備會用 `Framed-Route`、`Delegated-IPv6-Prefix` 取決於廠商和設定，請看 BNG 的文件。

示範資料：

| 用戶 | 固定資料 |
| --- | --- |
| alice | 沒有，回 `Framed-Pool` 是 `residential` |
| bob | `203.0.113.10` |
| carol | `203.0.113.20`（netmask `255.255.255.252`）、路由 `198.51.100.8/29`、`2001:db8:1:1::/64`、`2001:db8:100::/56` |
| dave | 只有 IPv6：`2001:db8:1:2::/64`、`2001:db8:200::/56` |
| erin | 停用 |

## 步驟

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server check --config examples/scenarios/static-ip/rradius.toml
./target/release/rradius-server server --config examples/scenarios/static-ip/rradius.toml
```

用 `rradius-bench` 登入確認放行（它不印回覆的屬性；屬性看 BNG，或看測試 `test_static_ip`）：

```bash
./target/release/rradius-bench run --server 127.0.0.1 --secret "$(cat examples/scenarios/static-ip/localhost.secret)" \
  --user carol --password carol-password --load pap,concurrency=1 --requests 1
```

## 設定說明

### `static.lua` 做了什麼

1. 非 RADIUS 拒絕，不是認證階段回覆。前面的 `sql` 處理器已驗過密碼，密碼錯的人不會來到這裡。
2. 用戶必須在 `subscribers` 且啟用，否則拒絕。
3. 沒有 `static_assignments` 那一列，或那一列沒有位址、網段、前綴（只有 `netmask` 沒有 `ipv4` 也算）：回 `Framed-Pool`，接受。
4. 有那一列：先檢查 `netmask`（遮罩的 1 要連續）和 `routed_v4`（`a.b.c.d/n`，主機位元是 0）；這兩個的字典型別擋不住內容不對的值，所以腳本自己檢查。再逐個欄位回屬性。**欄位的值不合法**（不是位址或前綴，例如 `203.0.113.999`）時，`set_attr` 依字典檢查會失敗，腳本拒絕並寫一行記錄：`static ip: bad ipv4 for frank : 203.0.113.999 - ...`。不悄悄退回動態，否則付費要固定位址的用戶會拿到一個會變的位址，沒有人知道。

### 資料怎麼維護

```sql
-- 新增用戶和固定位址
INSERT INTO subscribers (username, password, pool, enabled) VALUES ('gina', 'gina-password', 'business', 1);
INSERT INTO static_assignments (username, ipv4, netmask, routed_v4, wan_v6, pd_v6)
VALUES ('gina', '203.0.113.30', '255.255.255.252', '198.51.100.16/29', '2001:db8:1:3::/64', '2001:db8:300::/56');

-- 改回動態：刪掉那一列
DELETE FROM static_assignments WHERE username = 'gina';
```

`lines`、`static_assignments` 這類資料沒有專用的管理指令，改資料要寫成新編號的 migration 檔，放到每個節點，逐台重啟（見[第 11 章](11-operations.md)）。用戶很多、常變動時，把這些資料放在外部資料庫或帳務系統，用 `db_query`／`http_request` 查（第 9、10 章）。

### 唯一索引擋什麼

`ipv4`、`wan_v6`、`pd_v6` 各有唯一索引：兩個用戶登記同樣的文字時，後寫的那個 `INSERT` 或 `UPDATE` 失敗。空值不算，很多人可以都沒有 `wan_v6`。

**擋不住的：**

- **同一個前綴的不同寫法。** 索引比的是文字：`2001:db8:1:1::/64`、`2001:0db8:1:1::/64`、`2001:DB8:1:1::/64` 是同一個前綴，卻是三個文字，都擋得進去。IPv6 前綴一律寫成小寫、壓縮的寫法。
- **重疊的網段。** 某人的 `routed_v4` 是 `198.51.100.8/29`，另一個人的 `ipv4` 是 `198.51.100.10`：兩個值不同，索引不管。分配網段時自己要避免重疊。
- **固定位址落在動態池裡。** 位址池（第 14 章）不知道有哪些位址被固定了，會把同一個位址發給別人。固定位址一定要放在池的範圍之外。用位址池範例時，可以查有沒有重疊（只比對完全相同的位址）：

  ```sql
  SELECT s.username, s.ipv4 FROM static_assignments s JOIN ip_pool p ON p.ip = s.ipv4;
  ```

  這是偶爾跑一次的維護查詢：`ip_pool` 沒有以 `ip` 開頭的索引，池很大時會掃整個表；不同的池（例如不同的 VRF）可以有相同的位址，所以結果要再看是不是同一個池。

## 換成自己的資料

1. 刪掉 `migrations/2_demo_data.sql`，改成自己的用戶和固定資料；**正式環境第一次啟動前就決定好**資料表的欄位。
2. 換掉所有 `*.secret` 的內容，見[第 11 章](11-operations.md#更換密碼)。
3. 把 `[[radius.clients]]` 改成你的 BNG 位址。
4. 固定位址和前綴要有自己的規劃表：哪個範圍給固定、哪個給動態池，不要重疊。

## 已知限制

- **不防止重疊網段，也不檢查固定位址在不在動態池裡**（見上）。
- **同一個用戶同時兩個 session 會拿到同一個固定位址。** 要限制同時連線數，把[第 15 章](15-usage-control.md)的 `lua` 處理器接在前面（`[[auth.handlers]]` 的順序：`sql`、用量控制、`static.lua`）。
- **沒有 IPv6 的動態前綴池**（DHCPv6-PD 的 /56、/60 要對齊分配）。
- **不依線路身分給固定位址。** 用[第 16 章](16-line-identity.md)的 `lines` 時，可以自己在那個表加欄位，或把 `static_assignments` 的主鍵換成線路。
- **只在登入時決定。** 改了資料不會影響已經在線的連線，要等它下次登入。
- **沒有內建的檢查工具和指標。** 位址和前綴的合法性只在登入時檢查。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 固定用戶一直是 Reject，記錄有 `static ip: bad` | 資料表那個欄位的值不是合法的位址或前綴；記錄寫出用戶、欄位和值 |
| 登入成功但沒有固定位址 | `static_assignments` 沒有這個用戶那一列，或欄位是 NULL；用戶名稱要和 `subscribers` 一致 |
| BNG 沒有路由出 `/29` | BNG 不接受 `Framed-Route` 的這個格式，或要另外設定；看 BNG 的文件 |
| 兩個用戶拿到同一個位址 | 固定位址落在動態池裡，或網段重疊；用上面的檢查 |
| `INSERT` 失敗，UNIQUE constraint | 那個位址或前綴已經登記給別人 |
