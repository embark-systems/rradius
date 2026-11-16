# 16. 線路身分認證（固網寬頻 IPoE 的 Option 82）

**情境：** 光纖或 DSL 的用戶接上 BNG（寬頻網路閘道）之後，不輸入帳號密碼：用戶的 DHCP 請求經過接入設備（OLT、DSLAM）時，設備加上 DHCP Option 82，記著「哪個設備的哪個埠」。BNG 把這兩個值放進 RADIUS 請求問 rradius，rradius 依線路決定放行與否，並回這條線的方案：位址池名稱、設定檔名稱（速率由 BNG 上同名的設定檔決定）。

- **circuit-id：** 接入設備上的埠或線路，例如 `eth 1/1/3:100`。
- **remote-id：** 接入設備自己的識別，常是它的名稱，例如 `olt-a`。circuit-id 通常只在同一個設備內唯一，所以兩個合起來才是一條線。

範例目錄：[`rradius-server/examples/scenarios/line-identity/`](../../rradius-server/examples/scenarios/line-identity/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔：沒有 `[auth]`，全部交給腳本；`[[radius.clients]]` 只列 BNG |
| `auth.lua` | 正規化 circuit-id 和 remote-id、查線路、套 MAC 規則、回方案 |
| `migrations/1_schema.sql` | `plans`（方案）、`lines`（線路） |
| `migrations/2_demo_data.sql` | 三個示範方案和五條示範線路 |
| `*.secret` | 叢集和 BNG 的共用密碼（示範值） |

## Option 82 怎麼到 RADIUS

這一章要 BNG 把線路身分放進 Access-Request。rradius 認這幾個屬性：

| 屬性 | 用途 |
| --- | --- |
| `ADSL-Agent-Circuit-Id`（RFC 4679，廠商 3561 的子屬性 1） | circuit-id |
| `ADSL-Agent-Remote-Id`（RFC 4679，廠商 3561 的子屬性 2） | remote-id；沒有就當空字串 |
| `NAS-Port-Id` | 沒有 `ADSL-Agent-Circuit-Id` 時，當作 circuit-id |
| `Calling-Station-Id` | 用戶設備的 MAC 位址，MAC 鎖要用（下面） |

不同廠商的 BNG 把 Option 82 放進哪個屬性、格式怎麼寫並不一致，有的用上面這組，有的用 `NAS-Port-Id`，有的把兩個值寫成一個字串。**先看你的 BNG 實際送什麼**：用不認識的線路登入一次，記錄會寫出腳本看到的值（`line identity: unknown line <remote-id> <circuit-id>`），再照它把 `lines` 的資料填進去。BNG 的設定項目請看它的文件。

用戶名稱和密碼這個範例都不看：BNG 通常送線路字串或 MAC 當名稱、一個固定值當密碼，腳本只看上面的屬性。

## 步驟

### 1. 建置、檢查、啟動

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server check --config examples/scenarios/line-identity/rradius.toml
./target/release/rradius-server server --config examples/scenarios/line-identity/rradius.toml
```

範例的 `[[radius.clients]]` 是 `10.60.0.0/24`（BNG 的位址），測試時要從那個網段送請求，或暫時改成 `127.0.0.1`。

### 2. 試一條線

用 `rradius-bench` 送一個請求（`--secret` 是 `bng.secret` 的內容；`26=hex:` 是廠商 3561 的子屬性，前 4 個位元組 `00000de9` 是廠商編號，接著子屬性編號、長度、值）：

```bash
./target/release/rradius-bench run --server 127.0.0.1 --secret "$(cat examples/scenarios/line-identity/bng.secret)" \
  --user x --load pap,concurrency=1 --requests 1 \
  --attr 26=hex:00000de9010f65746820312f312f333a313030 \
  --attr 26=hex:00000de902076f6c742d61 \
  --attr 31=text:00:11:22:aa:bb:cc
```

第一個 `--attr` 是 circuit-id `eth 1/1/3:100`，第二個是 remote-id `olt-a`，結果是 accept，回 `Framed-Pool` 是 `residential`、`Filter-Id` 是 `fiber-100m`、`Class` 是 `alice`。拿掉第二個 `--attr`（沒有 remote-id）會被拒絕，因為這條線登記的 remote-id 是 `olt-a`，記錄寫 `line identity: unknown line  eth 1/1/3:100`。

### 3. 看結果

不認識的線路、停用、MAC 不符等拒絕的原因都寫在 rradius 的記錄裡（`line identity:` 開頭的行）；放行的請求在認證記錄有一行 `decision="accept"`。

## 設定說明

### 示範資料

| remote-id | circuit-id | 用戶 | 方案 | 說明 |
| --- | --- | --- | --- | --- |
| `olt-a` | `eth 1/1/3:100` | alice | `fiber-100`（100M） | 沒有 MAC 鎖 |
| `olt-a` | `eth 1/1/4:100` | bob | `fiber-1g`（1G） | MAC 鎖：綁第一次看到的 MAC |
| `olt-b` | `eth 2/0/7:200` | carol | `business` | 位址池 `business`，Interim 600 秒 |
| `olt-b` | `eth 2/0/8:200` | dave | `fiber-100` | 停用中 |
| （空） | `0x0004000a0001` | erin | `fiber-100` | circuit-id 是二進位 |

### `auth.lua` 做了什麼

1. **非 RADIUS 一律拒絕、計費一律回覆。** 這是最上層腳本，同一台伺服器若也開了 `[tacacs]`，一律接受會授權每個指令。
2. **正規化。** 含不可印字元（二進位）的值，寫成 `0x` 加小寫十六進位，不做其他處理；其餘去頭尾空白、轉小寫。`lines` 裡存的是正規化後的值，所以 `ETH 1/1/3:100 ` 和 `eth 1/1/3:100` 是同一條線。
3. **查線路。** 用 `(remote_id, circuit_id)` 查 `lines` 並接上 `plans`。查不到、停用、方案不存在：拒絕，原因寫在記錄裡（Access-Reject 不帶屬性）。
4. **MAC 規則。** `lock_mac = 0`：任何 MAC 都放行，記下最後看到的 MAC 和時間。`lock_mac = 1`：第一次登入把 MAC 綁進 `bound_mac`（用比較後更新，只有它還是空的才寫得進去，兩個請求同時來只有一個贏），之後不同的 MAC 拒絕；請求沒有 MAC 也拒絕。
5. **回覆。** `Framed-Pool`、`Filter-Id`、`Acct-Interim-Interval` 來自方案；`Class` 是用戶名稱，BNG 會在計費封包帶回它，計費紀錄就知道這條線是誰。

### 資料怎麼維護

`lines` 是一般的資料表，沒有專用的管理指令；改資料有兩條路：

- **migration 檔：** 下面的 SQL 寫成新編號的 migration 檔，放到每個節點，逐台重啟（見[第 11 章](11-operations.md)）。適合線路不常變、想留下變更紀錄的情況。每換一次用戶設備就多一個檔案，所以換設備頻繁時不實際。
- **把線路資料放在別的地方：** 線路多、常變動（開通、停用、換設備）時，讓腳本用 `db_query` 查外部的 MySQL／PostgreSQL（第 9 章的 `[databases]`），或用 `http_request` 問帳務系統（第 10 章），把 `lines` 當作那邊的快取或只放 `bound_mac`、`last_mac` 這類 rradius 自己寫的欄位。這個範例沒有示範。

以下 SQL 都是資料表的內容，不論用哪一條路：

```sql
-- 新增方案和線路（兩個 id 用正規化後的值：小寫、沒有頭尾空白）
INSERT INTO plans (plan, pool, profile, interim) VALUES ('fiber-500', 'residential', 'fiber-500m', 300);
INSERT INTO lines (remote_id, circuit_id, subscriber, plan, lock_mac) VALUES ('olt-a', 'eth 1/1/9:100', 'frank', 'fiber-500', 1);

-- 停用、啟用
UPDATE lines SET enabled = 0 WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';

-- 換了用戶的設備：清掉綁定，下一個登入的設備會被綁
UPDATE lines SET bound_mac = NULL WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';

-- 用戶搬到別條線：改這條線的用戶，或停用舊線、新增新線
```

`bound_mac` 要寫成 12 個小寫十六進位字元（`001122aabbcc`），不要有冒號或橫線；腳本是這樣比對的。

## 換成自己的資料

1. 刪掉 `migrations/2_demo_data.sql`，改成自己的方案和線路；**正式環境第一次啟動前就決定好**資料表的欄位。
2. 換掉所有 `*.secret` 的內容，見[第 11 章](11-operations.md#更換密碼)。
3. 把 `[[radius.clients]]` 改成你的 BNG 位址，**只列 BNG**。
4. 用你的 BNG 實際送出的值填 `lines`（上面「Option 82 怎麼到 RADIUS」）。

## 已知限制

- **線路身分不是祕密。** 能讓 BNG 送出任意 Option 82 的人，或能偽造 BNG 位址送請求的人，就能冒充任何一條線。client 清單只列 BNG，BNG 到 rradius 之間的網路要可信；要更強，加 MAC 鎖，或改用 RADIUS over TLS（RadSec，企業版）。
- **MAC 鎖只擋換設備，擋不住偽造。** MAC 位址是明文，同一個二層網路的人可以抄。
- **每次登入寫一次資料庫（記錄最後的 MAC 和時間）。** 登入量大時，這個寫入經過叢集的領導節點；叢集沒有領導節點時，連不需要 MAC 鎖的線路也登入失敗。不需要「最後看到」的資訊時，把腳本最後那個 `UPDATE lines SET last_mac ...` 拿掉，就不再依賴它。
- **MAC 鎖第一次綁定可能誤拒。** 綁定的比較讀的是本機副本；跟隨節點還沒套用剛寫的綁定時，同一台設備的下一次登入可能被暫時拒絕，重試就會過。
- **文字和二進位的 id 可能撞在一起。** 二進位值寫成 `0x` 開頭的十六進位，所以 BNG 若轉送用戶能控制的文字 id，用戶送 `0X0004000A0001` 就等於送那個二進位值；結尾有 tab 或換行字元的文字也會被當成二進位。circuit-id 若是用戶能影響的字串，不要用這個範例。
- **只認 RFC 4679 的屬性和 `NAS-Port-Id`。** 其他格式要自己改腳本的取值那幾行。
- **沒有位址的分配。** 回的是池的名稱，位址由 BNG 的池或[第 14 章](14-ip-pool.md)的做法決定；靜態 IP、IPv6 前綴不在這個範例裡。
- **只在登入時檢查。** 停用一條線不會中斷已經在線的連線。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 一直是 Reject，記錄有 `unknown line` | 資料表裡的值和 BNG 送的不同：看記錄裡腳本看到的 remote-id 和 circuit-id，注意要是正規化後的（小寫、沒有頭尾空白）；沒有 remote-id 時登記的 `remote_id` 要是空字串 |
| 記錄有 `no circuit-id and no NAS-Port-Id` | BNG 沒有把 Option 82 放進請求；檢查 BNG 的設定，或確認它用的是別的屬性 |
| 記錄有 `is locked to` | `lock_mac = 1` 而且 MAC 和綁定的不同；換設備時清掉 `bound_mac` |
| 記錄有 `plan ... does not exist` | `lines.plan` 在 `plans` 裡沒有對應的方案 |
| 登入成功但沒有 `Framed-Pool` | 設定不是用這個腳本當最上層腳本，或 BNG 忽略這個屬性 |
