# 2. 快速開始

**情境：** 先在一台機器上把 rradius 跑起來，帳號存在資料表，用 `radtest` 登入，再送幾筆計費確認有寫進去。之後的情境大多從這裡延伸。

範例目錄：[`rradius-server/examples/scenarios/quick-start/`](../../rradius-server/examples/scenarios/quick-start/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔 |
| `migrations/1_schema.sql` | `users`、`user_reply`、`accounting` 三張表 |
| `migrations/2_demo_users.sql` | 示範帳號 |
| `accounting.lua` | 計費腳本 |
| `*.secret` | 叢集和 NAS 的共用密碼（示範值） |

## 步驟

### 1. 建置並檢查設定

```bash
cd rradius-server
cargo build --release
./target/release/rradius-server check --config examples/scenarios/quick-start/rradius.toml
```

用發行檔時不必建置：解開壓縮檔，進到裡面的 `rradius-server/` 目錄，以下的 `./target/release/rradius-server` 都換成 `../bin/rradius-server`（見[範例的共同約定](README.md#範例的共同約定)）。

```bash
cd rradius-0.1.0-macos-aarch64/rradius-server
../bin/rradius-server check --config examples/scenarios/quick-start/rradius.toml
```

印出 `configuration ok` 就可以啟動。

### 2. 啟動

```bash
./target/release/rradius-server server --config examples/scenarios/quick-start/rradius.toml
```

第一次啟動會在 `examples/scenarios/quick-start/data/` 建立資料庫並套用兩個 migration 檔。看到 `RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813` 就可以接受請求。

### 3. 登入

```bash
radtest alice alice-password 127.0.0.1 0 testing123-change-me
```

會收到 Access-Accept，帶 `Session-Timeout = 3600` 和 `Filter-Id = "staff"`，這兩個屬性來自 `user_reply` 表。再試幾個：

| 指令 | 結果 | 原因 |
| --- | --- | --- |
| `radtest -t chap alice alice-password ...` | Accept | alice 存明文，CHAP 可以用 |
| `radtest bob bob-password ...` | Accept，帶 `Reply-Message` | bob 存 bcrypt 雜湊 |
| `radtest -t chap bob bob-password ...` | Reject | CHAP 要明文，雜湊做不到 |
| `radtest carol carol-password ...` | Reject | carol 的 `enabled` 是 0 |
| `radtest alice wrong ...` | Reject | 密碼錯誤 |

### 4. 計費

```bash
echo 'User-Name=alice,Acct-Session-Id=s1,Acct-Status-Type=Start' | radclient 127.0.0.1:1813 acct testing123-change-me
echo 'User-Name=alice,Acct-Session-Id=s1,Acct-Status-Type=Stop,Acct-Session-Time=120,Acct-Input-Octets=5000' | radclient 127.0.0.1:1813 acct testing123-change-me
```

兩筆都會收到 Accounting-Response，`accounting` 表多了一列，`stopped` 有值。

## 設定說明

```toml
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"
```

- **`user_query` 回傳密碼，** 第一欄的格式可以是明文或各種雜湊（`{crypt}`、`{argon2}`、`{SSHA}`、`{nthash}`…），見 [usage 第 5 節](../usage.md#5-密碼格式)。新部署建議 `{argon2}` 或 bcrypt；要支援 CHAP 或 PEAP 的帳號只能存明文或 `{nthash}`。
- **停用帳號寫在查詢條件裡：** 查不到就等於不存在。串接只有這個處理器，所以會被拒絕。
- **`reply_query` 回傳 (屬性, 值)，** 一列一個屬性，名稱是字典裡的名稱。值的格式不對時，就算密碼正確也會改回 Access-Reject，上線前請確認資料。
- **有 `[auth]` 時，最上層腳本只處理計費。** `accounting.lua` 一開頭對非計費的請求回 `reject`，萬一設定改了也不會放行任何人。

### `accounting.lua` 做了什麼

- **Start、Interim-Update、Stop 寫進同一列。** 主鍵是 client、NAS 和 `Acct-Session-Id`。NAS 取它回報的 `NAS-IP-Address`、`NAS-IPv6-Address` 或 `NAS-Identifier`，都沒有時用 client 名稱，所以一個 client 設成整個網段時，各台 NAS 也分得開。寫入用 `INSERT ... ON CONFLICT DO UPDATE`，數字都取較大的值，所以重送、亂序都不會讓資料倒退。
- **Stop 只結束還開著的 session，** 重送的 Stop 不會改掉第一次的結束時間。
- **Accounting-On／Off（NAS 重開機）** 只結束送出它的那台 NAS 還開著的 session，`terminate_cause` 記 11（NAS-Reboot）。
- **用 `Acct-Delay-Time` 還原時間，** NAS 晚送或從計費暫存重放的封包不會被當成剛發生。
- **超過 4 GiB 的流量** 由 `Acct-*-Gigawords` 補上進位。
- **沒有 `Acct-Session-Id` 的封包** 記一行 log 並回覆，不然 NAS 會一直重送。

## 換成自己的資料

1. 刪掉 `migrations/2_demo_users.sql`，改成自己的帳號；或保留它、另外加 `3_*.sql` 把示範帳號刪掉。**正式環境第一次啟動前就決定好，** 套用過的檔案不能再改。
2. 換掉所有 `*.secret` 的內容，見[第 11 章](11-operations.md#更換密碼)。
3. 把 `[[radius.clients]]` 改成你的設備位址，`[radius] bind` 改成要服務的位址（範例只聽 `127.0.0.1`）。
4. `[metrics]` 只綁內部位址，它沒有認證。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| `radtest` 沒有回應 | 來源位址不在 `[[radius.clients]]`、共用密碼不對，或埠被防火牆擋住。記錄會有 `unknown client` 或 `Message-Authenticator` 的訊息 |
| 啟動時 `migration file ... changed after it was applied` | 改了已套用的檔案。開發時可以停掉伺服器、刪掉 `data/` 重來；正式環境要改回原本內容，另加新檔案 |
| 密碼正確卻 Reject，記錄寫 `RADIUS response attribute error` | `user_reply` 的屬性名稱或值格式不對 |
| 計費一直重送 | 腳本出錯或沒有回傳 `'accept'`，記錄會有 `Lua script error` |

更多訊息的意思見 [usage 第 8 節](../usage.md#8-疑難排解)。
