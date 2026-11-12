# 10. 帳號在別的系統：用 HTTP API 驗證

**情境：** 旅館、共同工作空間或會員制場所的 Wi-Fi。帳號和密碼已經在會員系統（或訂房系統、帳務系統）裡，那個系統有 HTTP API。不想把帳號複製一份到 rradius 再想辦法同步。

- 每次登入，rradius 用 HTTPS 問會員系統「這組帳號密碼對不對」。
- 會員系統順便回答這個人的方案，rradius 轉成屬性交給 NAS。
- rradius 不存任何帳號；計費照樣記在 rradius 自己的資料表。

範例目錄：[`rradius-server/examples/scenarios/external-api/`](../../rradius-server/examples/scenarios/external-api/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | `[lua.http]` 和只有一個 `lua` 處理器的串接 |
| `member-api.lua` | 呼叫 API 的腳本 |
| `migrations/1_schema.sql` | 只有 `accounting` 表 |

## 步驟

範例的 API 位址（`members.example.com`）是假的，直接啟動後每次登入都會因為連不上而被拒絕。要換成你的系統：

1. 在 `rradius.toml` 的 `[lua.http] allowed_hosts` 填你的 API 主機。
2. 在 `member-api.lua` 改 `API`（網址）、`API_KEY`（金鑰），並依你的 API 調整請求和回應的處理。
3. 檢查並啟動：

   ```bash
   cd rradius-server
   ./target/release/rradius-server --check --config examples/scenarios/external-api/rradius.toml
   ./target/release/rradius-server --config examples/scenarios/external-api/rradius.toml
   ```

4. 本機測試時把 `hotspot` 的 `address` 改成 `127.0.0.1`：

   ```bash
   radtest <帳號> <密碼> 127.0.0.1 0 hotspot-shared-secret-change-me
   ```

自動測試在本機開一個假的會員系統，把腳本裡的網址指過去，確認：

| 情況 | 結果 |
| --- | --- |
| API 回 200，標頭 `X-Plan: gold` | Access-Accept，`Filter-Id = "gold"`、`Session-Timeout = 14400` |
| 帳號和密碼裡有 `&`、`=`、空白 | 編碼後送出，API 收到的是正確的值 |
| API 回 401 | Access-Reject |
| CHAP 登入 | Access-Reject：沒有明文密碼可以交給 API |
| API 一直不回應 | 等到 `timeout_ms` 後 Access-Reject，記錄 `member API gave no answer` |

## 腳本假設的 API

```
POST https://members.example.com/v1/radius/login
Authorization: Bearer <API 金鑰>
Content-Type: application/x-www-form-urlencoded

username=<帳號>&password=<密碼>
```

| 回應 | 意思 |
| --- | --- |
| 200，標頭 `X-Plan: <方案>`（選填） | 帳號密碼正確，可以上網 |
| 401、403 | 帳號密碼不對，或現在不能上網 |
| 其他 | 當成故障，拒絕並記錄狀態碼 |

**腳本裡沒有 JSON 函式。** 所以範例讓 API 用狀態碼和回應標頭回答，請求用表單編碼。你的 API 只會 JSON 時：

- 請求可以自己組字串（值要自己跳脫引號和反斜線，不建議），或請 API 那邊加一個接受表單編碼的端點。
- 回應要從 JSON 裡取值的話，只能用 Lua 的字串比對（`string.match`）。簡單的固定格式還可以，結構複雜就不可靠。
- 最穩當的是在會員系統前面放一個很薄的轉接服務，或直接替 RADIUS 開一個專用的端點。

## 設定說明

### 允許腳本連到哪裡

```toml
[lua.http]
allowed_hosts = ["members.example.com"]
timeout_ms = 1500
max_concurrent = 64
```

- **沒有這個區段，腳本裡就沒有 `http_request`。** 有了之後也只能連 `allowed_hosts` 列出的主機，完全比對主機和埠，不支援萬用字元。
- **只寫主機名稱：** 只允許 HTTPS 的 443 埠。其他埠寫成 `"billing.internal:8443"`。要用沒有加密的 HTTP（只建議在本機或隔離的網路），寫成 `"http://legacy.internal:8080"`。
- **`timeout_ms`** 是從連線到讀完回應的總時間。**一定要比 NAS 的重送時間短，** 否則 NAS 已經放棄了，rradius 還在等。
- **`max_concurrent`：** 同一台主機同時最多幾個請求在等。超過時 `http_request` 立刻回傳失敗（`busy`），不會排隊。登入和計費分開算。
- **內部 CA 簽的憑證** 用 `ca_file` 指定。伺服器憑證一律驗證，沒有關閉驗證的選項。

### 串接

```toml
[[auth.handlers]]
type = "lua"
script = "member-api.lua"
verifies_password = true
```

- **`verifies_password = true`** 告訴 rradius「這個腳本真的比對過密碼」。一般的 PAP 登入不需要它；但要讓 Wi-Fi 的 EAP-TTLS 內層 PAP 走這個腳本時一定要設，否則腳本回 `accept` 會被當成拒絕。
- **只有帶明文密碼的登入方式能用：** PAP，和 EAP-TTLS 的內層 PAP。CHAP、MS-CHAPv2、PEAP 送來的不是密碼本身，API 無從比對。

### 腳本

```lua
local response, problem = http_request{
  url = API,
  method = 'POST',
  headers = {
    Authorization = 'Bearer ' .. API_KEY,
    ['Content-Type'] = 'application/x-www-form-urlencoded',
  },
  body = 'username=' .. url_encode(user) .. '&password=' .. url_encode(password),
}

if not response then
  log('member API gave no answer:', problem)
  return 'reject'
end
```

- **`http_request` 的回傳：** 有收到回應就是一個表（`status`、`body`、`headers`，標頭名稱是小寫），4xx 和 5xx 也算收到。沒收到（逾時、連不上、不在白名單、回應太大、太忙）時回傳 `nil` 和原因。
- **來自使用者的值一律用 `url_encode`。** 帳號和密碼是使用者輸入的，不編碼的話，名稱裡放一個 `&` 就能多塞一個參數給 API。
- **放進網址路徑時更要小心：** `url_encode` 遇到空字串、`.`、`..` 會讓腳本出錯（請求被拒絕），因為 `..` 當成路徑會走到別的端點。範例把值放在請求內容裡，名稱剛好是 `.` 或 `..` 的登入一樣會因為腳本出錯而被拒絕。
- **API 沒有回答時怎麼辦，由腳本決定。** 範例拒絕。也可以放行（例如旅館寧可讓客人先上網），但那等於 API 一掛，任何密碼都能通過，要想清楚。
- **不要記錄密碼。** 範例的 `log()` 只寫原因和狀態碼。rradius 自己的記錄不會寫網址路徑、標頭或內容，只有主機和錯誤的種類。
- **API 金鑰寫在腳本裡，** 所以腳本檔案只讓執行 rradius 的帳號讀得到。換金鑰時改檔案並送 SIGHUP。
- **回應裡的值也要檢查。** 範例只接受看起來像名稱的 `X-Plan`，再放進 `Filter-Id`。

## API 變慢或停擺時

每次登入都要等 API，所以 API 的速度就是登入的速度。

- **API 停擺：** 每次登入等到 `timeout_ms` 才被拒絕。同時在等的登入最多 `max_concurrent` 個，再多的立刻被拒絕，所以不會把 rradius 自己的名額占滿。
- **看指標：** `rradius_script_http_requests_total{result="timeout"}`、`{result="error"}`、`{result="busy"}` 增加就是 API 有問題；`{result="ok"}` 是收到回應的次數（包含 401）。
- **NAS 重送：** 同一個請求還在處理時，NAS 的重送會被丟棄，不會多問 API 一次。處理完 10 秒內的重送直接回上次的答案。
- **rradius 不會記住上次的答案。** 每次登入都問一次。要減輕 API 的負擔，可以在腳本裡把「通過」記進 rradius 的資料表幾分鐘（`sql_execute`），之後先查表；但密碼不能明文存，這個做法要自己設計，範例沒有做。
- **腳本的執行時限**（`[lua] max_execution_ms`）不算等 API 的時間。

## 計費也要通知 API 嗎

範例的計費只寫進本機資料表。要在使用者上線、下線時通知會員系統，可以在計費腳本裡也呼叫 `http_request`，但要注意：

- **計費腳本回 `accept` 才會回覆 NAS。** API 沒回應時腳本若出錯，NAS 會重送，腳本會再跑一次；API 要能容忍同一筆通知收到兩次。
- **不要讓 API 的故障擋住計費。** 比較穩的做法是計費照常寫進資料表、回 `accept`，由另一個程式從資料表把資料送給會員系統。

## 上線前

- **API 那邊要限制來源和速率。** rradius 會把任何人在登入頁輸入的帳號密碼轉給 API，所以猜密碼的流量也會過去。
- **DNS：** 白名單比對的是主機名稱，不檢查解析出來的位址。DNS 被竄改時，請求可能被送到別的地方；HTTPS 的憑證驗證是這時的保護，所以不要用 `http://`。
- **不跟隨轉址。** API 回 3xx 時腳本會拿到 3xx。
- **多節點：** 每個節點各自呼叫 API，API 要接受每個節點的來源位址。
- **沒有測過的：** 任何真實的會員系統或帳務系統。情境測試用一個本機的假 API（沒有 TLS），所以 HTTPS 的憑證驗證不在這個測試裡；專案的其他測試涵蓋 `http_request` 本身。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 記錄有 `attempt to call a nil value`，指向 `http_request` 那一行 | 設定檔沒有 `[lua.http]` |
| 每次登入都被拒絕，記錄有 `member API gave no answer: ... is not in lua.http.allowed_hosts` | 腳本裡的主機或埠和 `allowed_hosts` 不完全相同 |
| 記錄有 `member API gave no answer`，原因是逾時 | API 太慢或連不到；從節點上用 `curl` 試同一個網址 |
| 原因和憑證有關 | API 的憑證是內部 CA 簽的，設 `ca_file` |
| `rradius_script_http_requests_total{result="busy"}` 增加 | 同時超過 `max_concurrent` 個請求在等：API 太慢，或上限設太低 |
| PAP 可以，Wi-Fi 不行 | Wi-Fi 用的是 PEAP 或 MS-CHAPv2，這個做法只能用 EAP-TTLS 內層 PAP，而且處理器要設 `verifies_password = true` |
| 啟動失敗，說沒有可信任的根憑證 | 這台主機沒有系統的 CA 憑證（極簡的容器常見），安裝 `ca-certificates` 或設 `ca_file` |
