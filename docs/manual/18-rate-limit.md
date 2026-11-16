# 18. 速率屬性（同一個方案，依 BNG 的廠商送不同屬性）

**情境：** 固網寬頻的方案有下載和上傳的速率。rradius 在登入時依用戶的方案告訴 BNG，但各廠商的 BNG 認的屬性不同：有的只收設定檔名稱（速率在 BNG 上設好），有的收速率的數值，單位與方向也各異。同一個營運商常有好幾種 BNG。這一章用一個方案表加一張「BNG 用哪種屬性」的表，讓同一個方案對不同的 BNG 送不同的屬性。

範例目錄：[`rradius-server/examples/scenarios/rate-limit/`](../../rradius-server/examples/scenarios/rate-limit/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔：`all_accept` 串接，先 `sql` 處理器驗密碼，再 `lua` 處理器回速率屬性；每個 BNG 一個 `[[radius.clients]]` |
| `rate.lua` | 依方案和 client 的風格回屬性；風格是腳本開頭的一張表 |
| `accounting.lua` | 計費一律回覆 |
| `migrations/1_schema.sql` | `plans`（方案與速率）、`subscribers`（用戶）、`nas_styles`（client 對應風格） |
| `migrations/2_demo_data.sql` | 三個示範方案、五個用戶、四個有風格的 client |
| `*.secret` | 叢集和各個 BNG 的共用密碼（示範值） |

## 風格

`plans` 的速率單位是 kbit/s：**下載**是往用戶的方向，**上傳**是用戶往網路的方向。以示範的 `fiber-500`（下載 500000、上傳 250000）為例：

| 風格 | 回的屬性 | 範例的值 | 單位與方向 |
| --- | --- | --- | --- |
| `profile`（預設） | `Filter-Id` | `fiber-500` | 設定檔名稱；速率在 BNG 上、依名稱找到的設定檔裡設 |
| `alc_sla` | `Alc-SLA-Prof-Str` | `fiber-500` | 設定檔名稱，同上 |
| `erx` | `ERX-Ingress-Policy-Name`、`ERX-Egress-Policy-Name` | `fiber-500-up`、`fiber-500-down` | 政策名稱：進（Ingress）是用戶上傳，出（Egress）是用戶下載；BNG 上要有這兩個名稱的政策 |
| `wispr` | `WISPr-Bandwidth-Max-Down`、`WISPr-Bandwidth-Max-Up` | `500000000`、`250000000` | bit/s，下載與上傳；屬性是 32 位元整數，最大 4294967295 bit/s（約 4.29 Gbit/s），更快的方案會被拒絕並寫記錄 |
| `mikrotik` | `Mikrotik-Rate-Limit` | `250000k/500000k` | 「rx/tx」，從路由器看：rx 是用戶上傳、tx 是用戶下載；`k` 是 kbit/s |

**只送名稱的風格（`profile`、`alc_sla`、`erx`）沒有單位的問題**，BNG 上要有同名的設定檔或政策：`plans.profile` 是名稱，`erx` 再加 `-up`、`-down`。

**送數值的風格（`wispr`、`mikrotik`）的單位和方向是我依屬性的定義寫的，沒有用真實設備驗證。** 上線前請對照你的 BNG 的文件，確認單位（bit/s 或 kbit/s）、哪個方向是哪個值、字串的格式，並用一個測試用戶在實機上量速率。

## 步驟

### 1. 建置、檢查、啟動

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server check --config examples/scenarios/rate-limit/rradius.toml
./target/release/rradius-server server --config examples/scenarios/rate-limit/rradius.toml
```

設定檔的 `[[radius.clients]]` 是示範位址：`bng-default`（`127.0.0.1`，沒有列在 `nas_styles`，所以回 `Filter-Id`）和 `10.70.0.1` 到 `10.70.0.4` 的四個。要從那些位址送請求，或把其中一個改成你的測試位址。

### 2. 試一個 client

用 `rradius-bench` 以 `bng-default` 登入（`--secret` 是 `localhost.secret` 的內容）：

```bash
./target/release/rradius-bench run --server 127.0.0.1 --secret "$(cat examples/scenarios/rate-limit/localhost.secret)" \
  --user bob --password bob-password --load pap,concurrency=1 --requests 1
```

結果是 accept。它不印回覆的屬性；要看屬性看 BNG 收到什麼，或看測試 `test_rate_limit`（對五個 client 各登入一次，解開廠商編號和子屬性檢查值）。

## 設定說明

### `rate.lua` 做了什麼

1. 非 RADIUS 拒絕，不是認證階段回覆。前面的 `sql` 處理器已驗過密碼，密碼錯的人不會來到這裡。
2. 用戶必須存在且啟用，而且他的方案在 `plans` 裡；否則拒絕並寫記錄：`rate limit: unknown or disabled user <名稱>`、`rate limit: the plan of <名稱> does not exist`。
3. 查 `nas_styles`：這個 RADIUS client（`[[radius.clients]]` 的 `name`）用哪一種風格；沒有列就用 `profile`。風格名稱腳本不認得時拒絕並寫記錄。
4. 套用風格；風格函式回 `false` 表示這個方案送不出去（`wispr` 的速率超過 32 位元），拒絕。另外回 `Acct-Interim-Interval`（300 秒）。

### 新增一種風格

`rate.lua` 開頭的 `styles` 表，每一項是一個函式，收到方案（`plan.down_kbps`、`plan.up_kbps`、`plan.profile`），用 `set_attr` 回屬性。例如要支援一個把速率放在單一字串屬性的廠商：

```lua
  myvendor = function(plan)
    set_attr('My-Vendor-Rate', string.format('%d/%d', plan.down_kbps, plan.up_kbps))
  end,
```

`set_attr` 會依字典檢查屬性名稱和值的型別：名稱不在字典裡，或值不是那個型別，登入會因為腳本錯誤而被拒絕。字典在 `share/`，屬性要先在那裡有定義（見[使用手冊參考](../usage.md)）。

### 新增一個 BNG

1. 在設定檔加一個 `[[radius.clients]]`（名稱、位址、密碼檔）。
2. 在 `nas_styles` 加一列：`INSERT INTO nas_styles (client, style) VALUES ('bng-new', 'wispr');`，寫成新編號的 migration 檔，放到每個節點，逐台重啟（見[第 11 章](11-operations.md)）。名稱要和設定檔的 `name` 一樣。

### 資料怎麼維護

`plans`、`subscribers`、`nas_styles` 是一般的資料表，沒有專用的管理指令；改資料要寫成新編號的 migration 檔。用戶很多、常變動時，把這些資料放在外部資料庫或帳務系統，用 `db_query`／`http_request` 查（第 9、10 章）。

```sql
INSERT INTO plans (plan, down_kbps, up_kbps, profile) VALUES ('fiber-2g', 2000000, 2000000, 'fiber-2g');
UPDATE subscribers SET plan = 'fiber-500' WHERE username = 'alice';
```

## 換成自己的資料

1. 刪掉 `migrations/2_demo_data.sql`，改成自己的方案、用戶和 BNG 風格；**正式環境第一次啟動前就決定好**資料表的欄位。
2. 換掉所有 `*.secret` 的內容，見[第 11 章](11-operations.md#更換密碼)。
3. 把 `[[radius.clients]]` 改成你的 BNG，每個 BNG 一個，名稱和 `nas_styles` 一致。
4. 確認每個風格的單位和方向（上面「風格」一節），用實機驗證。

## 已知限制

- **沒有用真實 BNG 驗證任何一種風格**，送數值的風格的單位和方向要自己對照文件確認。
- **`wispr` 風格送不出超過約 4.29 Gbit/s 的方案。** 屬性只有 32 位元，腳本遇到更快的方案拒絕登入並寫一行記錄（`is faster than WISPr-Bandwidth can say`），不送被截斷的值；那種方案請對這個 BNG 用只送名稱的風格。
- **一個方案只有一個下載值和一個上傳值。** 沒有 burst、時段、流量用完之後降速；降速見[第 15 章](15-usage-control.md)。
- **只在登入時決定。** 改了方案不會影響已經在線的連線，要等它下次登入；連線中改速率要用中斷連線與變更權限（CoA，企業版）。
- **同一個 client 只用一種風格。** 一個 BNG 要同時送兩種風格的屬性，要在腳本裡自己組。
- **字典要認得屬性。** 沒有收錄的廠商要先把它的屬性加進字典，`set_attr` 才能用。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 一直是 Reject，記錄有 `unknown or disabled user` | 用戶不在 `subscribers` 或停用 |
| 一直是 Reject，記錄有 `the plan of ... does not exist` | `subscribers.plan` 在 `plans` 裡沒有對應的方案 |
| 一直是 Reject，記錄有 `which this script does not know` | `nas_styles.style` 寫的名稱不在 `rate.lua` 的 `styles` 表裡 |
| 一直是 Reject，記錄有 `is faster than WISPr-Bandwidth can say` | 方案的速率超過 `wispr` 風格的上限（4294967295 bit/s） |
| 一直是 Reject，記錄有 `Lua script error` | 風格函式用了字典裡沒有的屬性，或值的型別不對 |
| 登入成功但 BNG 沒有限速 | BNG 沒有同名的設定檔或政策；或送數值的風格，單位、方向和 BNG 的不同；或這個 client 沒有列在 `nas_styles`，回的是 `Filter-Id` |
| 速率是設定的 1000 倍 | 單位不符：`wispr` 是 bit/s、`mikrotik` 字串的 `k` 是 kbit/s；對照 BNG 的文件 |
