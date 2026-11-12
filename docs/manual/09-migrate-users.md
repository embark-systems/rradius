# 9. 從舊系統搬帳號

**情境：** 公司要把認證從舊的系統換成 rradius。舊系統可能是一台舊的 RADIUS 伺服器、一個 LDAP 目錄、郵件伺服器的帳號，或某個自己寫的資料庫。裡面有幾千個帳號，密碼都是雜湊過的，沒有人知道明文。

- **不想要求每個人重設密碼。** 把舊系統的雜湊原樣搬過來，使用者用原本的密碼繼續登入。
- **舊的雜湊多半很弱**（MD5、SHA-1）。搬過來之後，改密碼時改存新的格式，舊格式逐漸消失。
- 舊系統的帳號名稱大小寫不一、有的帶網域，要統一。

如果舊的 LDAP 目錄會繼續留著，不必搬：讓 rradius 直接問目錄（[第 5 章](05-remote-access.md)）。這一章是要把帳號放進 rradius 自己的資料表。

範例目錄：[`rradius-server/examples/scenarios/migrate-users/`](../../rradius-server/examples/scenarios/migrate-users/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | `sql` 處理器、名稱正規化、`[accounts]` |
| `migrations/1_schema.sql` | `users`（多一欄 `origin` 記來源）、`user_reply`、`accounting` |
| `migrations/2_imported_users.sql` | 八個示範帳號，各用一種舊格式 |

## 怎麼運作

`sql` 處理器的 `user_query` 回傳密碼欄。密碼欄的開頭用 `{格式}` 標明它是什麼：

```
{SSHA}ZvqZAfdyjd1PQiq8rurmdodN8ihkZW1vc2FsdA==
{crypt}$1$demosalt$EhA8qeu4qSun8gh9DajmS1
{nthash}9d232eea31049319fc8ab84300855a34
```

登入時 rradius 依前綴用對應的算法，把使用者輸入的密碼算一次、和存的值比對。所以**同一張表裡可以混用各種格式**，一個帳號一種。

## 步驟

```bash
cd rradius-server
./target/release/rradius-server --config examples/scenarios/migrate-users/rradius.toml
```

本機測試時把 `nas` 的 `address` 改成 `127.0.0.1`。示範帳號的密碼都是「帳號-password」：

```bash
S=nas-shared-secret-change-me
radtest ldap1 ldap1-password 127.0.0.1 0 $S     # {SSHA}
radtest shell1 shell1-password 127.0.0.1 0 $S   # {crypt} 的 MD5-crypt
radtest mail1 mail1-password 127.0.0.1 0 $S     # {NS-MTA-MD5}
radtest 'EXAMPLE\LDAP1@example.com' ldap1-password 127.0.0.1 0 $S   # 名稱會先統一成 ldap1
```

都會收到 Access-Accept。

## 舊資料對應到哪個格式

| 舊系統裡的樣子 | 存成 | 示範帳號 |
| --- | --- | --- |
| LDAP 的 `userPassword`，已經是 `{SSHA}...`、`{SSHA512}...`、`{SHA}...`、`{MD5}...`、`{SMD5}...` 這類 | 原樣複製 | ldap1、ldap2 |
| OpenLDAP 的 `{PBKDF2-SHA256}次數$鹽$雜湊`（還有 `-SHA1`、`-SHA512`） | 原樣複製 | ldap3 |
| `{X-PBKDF2}HMACSHA2+256:次數:鹽:雜湊` | 原樣複製 | ldap4 |
| `/etc/shadow` 的第二欄（`$1$...`、`$5$...`、`$6$...`、`$2b$...`） | 前面加 `{crypt}` | shell1 |
| LDAP 的 `{CRYPT}$6$...` | 原樣複製（前綴不分大小寫） | |
| 舊郵件伺服器的 `{NS-MTA-MD5}...` | 原樣複製 | mail1 |
| 資料庫裡十六進位的 MD5、SHA-1、SHA-256（沒有鹽） | 轉成 Base64，加 `{MD5}`、`{SHA}`、`{SHA256}` | old1 |
| NT hash（32 個十六進位字元） | 前面加 `{nthash}` | win1 |
| argon2 的 `$argon2id$...` | 前面加 `{argon2}` | |
| 明文 | 原樣；值本身以 `{` 開頭時加 `{clear}` | |

**十六進位轉 Base64：** 舊資料庫常把雜湊存成十六進位字串，rradius 的 `{MD5}`、`{SHA}` 這一類要 Base64：

```bash
printf '%s' 91f881a968b893e4092eca12a52293ef | xxd -r -p | base64
# kfiBqWi4k+QJLsoSpSKT7w==     →  存成 {MD5}kfiBqWi4k+QJLsoSpSKT7w==
```

- **加鹽的雜湊，鹽要在後面。** `{SSHA}` 這一類是「雜湊接著鹽」一起做 Base64，算法是 `雜湊(密碼 + 鹽)`。舊系統是 `雜湊(鹽 + 密碼)`、或用別的方式混合時，沒有對應的格式，這些帳號只能重設密碼。
- **不確定對不對時，先拿一個知道密碼的測試帳號試。** 格式不對的值一律比對不過，不會放行任何人，所以可以放心試。
- 完整的格式清單見 [usage 第 5 節](../usage.md#5-密碼格式)。

## 能搬的和不能搬的

**雜湊只能用在會把明文密碼送到伺服器的登入方式。**

| 登入方式 | 能用雜湊嗎 |
| --- | --- |
| PAP（VPN、多數 NAS 的網頁登入、TACACS+ 的 PAP 和 ASCII） | 全部格式都可以 |
| EAP-TTLS 內層 PAP（Wi-Fi） | 全部格式都可以 |
| CHAP（PPPoE 常用） | **只能明文** |
| MS-CHAPv2、PEAP（Wi-Fi 最常見的帳號密碼方式） | **只能明文或 `{nthash}`** |

所以：

- **舊系統有存 NT hash，** 搬過來的帳號可以直接用 PEAP（示範的 win1）。
- **舊系統只有其他雜湊，而 Wi-Fi 要用帳號密碼，** 有兩條路：Wi-Fi 改用 EAP-TTLS 內層 PAP（各作業系統的支援程度不同，要先確認）；或要求使用者改一次密碼，改的時候存成 `{nthash}`（見下面）。
- **PPPoE 的 CHAP，** 沒有明文就沒辦法，只能重設。

自動測試確認了八個示範帳號都能用 PAP 登入，而 `{SSHA}` 的帳號用 CHAP 會被拒絕。

## 匯入

把舊系統匯出的資料轉成一個 migration 檔：

```sql
-- 2_imported_users.sql
INSERT INTO users (username, password, origin) VALUES
  ('ldap1', '{SSHA}ZvqZAfdyjd1PQiq8rurmdodN8ihkZW1vc2FsdA==', 'ldap'),
  ('shell1', '{crypt}$1$demosalt$EhA8qeu4qSun8gh9DajmS1', 'shadow');
```

- **第一次啟動前就把檔案準備好。** migration 檔套用後不能再改；之後要補帳號就新增下一個編號的檔案。
- **帳號名稱先轉成小寫、去掉網域，** 和下面的正規化設定一致。
- **SQL 的字串裡，單引號要寫兩個。** 用程式產生這個檔案時，記得處理帳號和雜湊裡的引號。
- **`origin` 欄** 不是必要的，範例用它記下每個帳號的密碼來自哪裡，之後好統計還剩多少舊格式。
- **檔案很大時**（幾十萬列），匯入要一些時間，把 `[cluster] migrations_wait_ms` 調高到比匯入時間長，見 [usage 2.5 節](../usage.md#25-資料表-migration)。
- **匯出的檔案裡是所有人的密碼雜湊，** 弱的雜湊可以被離線破解。傳輸和存放都要保護，匯入後刪掉暫存的副本。migration 檔本身也一樣。

也可以不用 migration 檔，改用 `user add` 一個一個加，但它只接受明文密碼（會自己雜湊），不能用來搬現成的雜湊。

## 名稱不一致

```toml
[auth.username]
strip_realm = "both"
lowercase = true
```

舊系統的使用者習慣輸入 `EXAMPLE\ldap1`、`ldap1@example.com` 或 `LDAP1`。這個設定在查帳號前把它們都變成 `ldap1`：先去掉最後一個反斜線之前的部分，再去掉最後一個 `@` 之後的部分，最後轉小寫。

- **資料表裡要先統一成小寫。** 否則 `Alice` 這一列永遠查不到。
- **原本只差大小寫的兩個帳號會變成同一個。** 匯入前先找出來處理。

## 把舊格式換掉

```toml
[accounts]
scheme = "bcrypt"
passwd = "UPDATE users SET password = ?2, origin = 'rradius' WHERE username = ?1"
```

```bash
rradius-server user passwd --config rradius.toml mail1    # 問兩次新密碼
```

`user passwd` 把新密碼用 `scheme` 指定的格式存進去，舊的雜湊就被換掉了；範例順便把 `origin` 改成 `rradius`。

- **`scheme` 選哪個：** 只用 PAP 就用預設的 `bcrypt`（登入量很大時改 `sha512-crypt`）；要用 PEAP、MS-CHAPv2 選 `nthash`；要用 CHAP 只能 `clear`。見[第 11 章](11-operations.md#用-user-指令)。
- **rradius 不會在登入時自動把舊雜湊換成新的，** 也沒有讓使用者自己改密碼的介面。要由管理者、或你的入口網站呼叫 `user passwd`（密碼從標準輸入給）。
- **看還剩多少：** `user show <帳號>` 會印出這個帳號的 `origin`。rradius 沒有查詢整張資料表的指令；要統計總數，可以在計費腳本裡偶爾用 `sql_query` 算一次、用 `log()` 寫進記錄（本章沒有附這樣的腳本）。
- **訂一個期限。** 期限到了還沒改密碼的帳號，用 migration 檔把密碼設成 `NULL`（密碼欄是 `NULL` 的帳號一律登不進來），請他們找管理者重設。

## 上線前

- **先平行跑一陣子。** 挑幾台 NAS 指到 rradius，其他的留在舊系統，確認登入結果一致再全部切換。
- **舊系統給的回覆屬性**（VLAN、速率、權限）也要搬，放進 `user_reply`（[第 2 章](02-quick-start.md#設定說明)）。屬性名稱要是 rradius 字典裡的名稱，值的格式不對時登入會被拒絕，所以搬完用 `--check` 以外的方式實際登入幾個帳號試。
- **舊系統的共用密碼** 可以沿用，NAS 那邊只要改伺服器位址。趁這次換成每台不同、夠長的更好。
- **`{crypt}` 和 `{argon2}` 的驗證刻意很慢，** 同時登入很多時會排隊。PBKDF2 的次數很高時也一樣。
- **沒有測過的：** 從任何一套特定的舊系統實際匯出再匯入的流程。示範的雜湊是用 Python 的 `hashlib` 和 `openssl passwd` 依各格式的定義算出來的，再由情境測試確認 rradius 驗證得過；專案另外用 OpenLDAP 實際產生的值測過 PBKDF2 這幾種格式。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 搬過來的帳號密碼正確卻被拒絕 | 前綴不對（例如十六進位的 MD5 直接加了 `{MD5}`，沒有轉 Base64）；鹽的位置和格式定義不同；或匯入時雜湊被截斷、多了空白 |
| 只有名稱有大寫的帳號登不進來 | 開了 `lowercase`，但資料表裡的名稱沒有轉小寫 |
| PAP 可以，Wi-Fi 的 PEAP 不行 | PEAP 要明文或 `{nthash}`，其他雜湊做不到 |
| PAP 可以，PPPoE 的 CHAP 不行 | CHAP 要明文 |
| 登入尖峰時變慢 | bcrypt、argon2 或次數很高的 PBKDF2 在排隊。改密碼時換成 `sha512-crypt`，或接受這個速度 |
| 啟動時 `migration file ... changed after it was applied` | 改了已經套用的匯入檔。要補資料就新增下一個編號的檔案 |
