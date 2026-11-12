# 6. 網路設備管理（TACACS+）

**情境：** 網管人員用 SSH 登入路由器和交換器。

- 帳號集中管理，不再每台設備各設一組本機帳號。
- **netadmin** 角色：權限等級 15，所有指令都能下。
- **noc** 角色：權限等級 1，只能查看（`show`、`ping`、`traceroute`）。`show` 只能接腳本列出的項目（介面、版本、VLAN、路由這類），看不到設定檔，因為裡面有密碼。
- 每個指令都記下來：誰、從哪裡、在哪台設備、下了什麼。

TACACS+ 把認證、授權、計費分成三種請求，設備可以每下一個指令就問一次「這個可以嗎」，這是它比 RADIUS 適合設備管理的地方。

範例目錄：[`rradius-server/examples/scenarios/device-admin/`](../../rradius-server/examples/scenarios/device-admin/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 只開 TACACS+，沒有 `[radius]` |
| `tacacs.lua` | 授權（依角色）和計費（記錄指令） |
| `migrations/1_schema.sql` | `admins`（帳號、角色）、`tacacs_log` |
| `migrations/2_demo_data.sql` | netadmin1、noc1，密碼是 bcrypt |

## 誰處理哪一段

| 請求 | 誰處理 |
| --- | --- |
| 認證（帳號密碼） | `[auth]` 的 `sql` 處理器：`admins` 表的 bcrypt 雜湊 |
| 授權（開 shell、每個指令） | `tacacs.lua`，`phase` 是 `"authorization"` |
| 計費（登入、登出、指令記錄） | `tacacs.lua`，`phase` 是 `"accounting"`，寫進 `tacacs_log` |

這個情境依使用者的角色授權，所以交給最上層腳本，腳本是主角。只依設備決定授權時，可以不寫腳本，改用下方的「授權群組」。

## 步驟

### 1. 啟動

TACACS+ 用 49 埠，一般帳號不能綁 1024 以下的埠。本機測試可以先把 `[tacacs] port` 改成 4949；正式環境見下方「上線前」。範例的 client 是 `10.50.0.0/24`，本機測試時改成 `127.0.0.1`。

```bash
cd rradius-server
./target/release/rradius-server --config examples/scenarios/device-admin/rradius.toml
```

### 2. 用 `tacacs_client` 測試

`tacacs_client` 是 Python 套件 `tacacs_plus` 附的指令（`pip install tacacs_plus`），參數依版本可能略有不同，以 `tacacs_client --help` 為準：

```bash
K=tacacs-shared-secret-change-me
tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authenticate          # 輸入 noc1-password
tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authorize -c service=shell cmd=
tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authorize -c service=shell cmd=show cmd-arg=running-config
```

| 請求 | netadmin1 | noc1 |
| --- | --- | --- |
| 開 shell（`cmd=` 空的） | 通過，`priv-lvl=15` | 通過，`priv-lvl=1` |
| `show interfaces` | 通過 | 通過 |
| `show running-config` | 通過 | 拒絕 |
| `configure terminal` | 通過 | 拒絕 |

## 腳本說明

```lua
local cmd = get_attr('cmd')
-- 沒有 cmd：要開 shell（EXEC 授權），回覆權限等級。
if not cmd or cmd == '' then
  set_attr('priv-lvl', role == 'netadmin' and 15 or 1)
  return 'accept'
end
```

- **授權請求的參數** 都能用 `get_attr` 讀：`service`、`cmd`、`protocol` 等。同名的參數用 `get_attrs`，例如 `get_attrs('cmd-arg')` 拿到指令的每個參數。
- **`set_attr` 加的屬性** 以 `名稱=值` 回給設備（PASS_ADD）。
- **計費** 用 `task_id` 加事件（`start_time`、`stop_time` 決定是開始還是結束）當主鍵，`INSERT OR IGNORE`，設備重送也只留一筆。指令和參數組回一行存進 `cmd`，例如 `show interfaces`。
- **帳號停用**（`enabled = 0`）之後，認證和授權都會拒絕，已經登入的 session 下一個指令就會被擋。

## 不寫腳本：授權群組

授權只依設備決定、不分使用者時，可以在設定檔寫規則，不用寫腳本的授權部分。例如核心路由器只有網管人員能登入，登入後一律權限等級 15；接入交換器給值班人員，只能查看：

```toml
[tacacs.authorize_groups]
core = [
  'permit service=shell cmd\* {priv-lvl=15}',
  'permit service=shell cmd=.*',
]
access = [
  'permit service=shell cmd\* {priv-lvl=1}',
  'deny cmd=show cmd-arg=(running-config|startup-config)',
  'permit service=shell cmd=(show|ping|traceroute|exit|terminal)',
]

[[tacacs.clients]]
name = "core-routers"
address = "10.50.0.0/24"
secret_file = "tacacs.secret"
authorize_group = "core"

[[tacacs.clients]]
name = "access-switches"
address = "10.60.0.0/22"
secret_file = "tacacs.secret"
authorize_group = "access"
```

- 依序比對，第一條成立的規則決定；`deny` 放在較寬的 `permit` 之前。沒有任何一條成立時拒絕。
- `cmd\*` 是設備要求開 shell 時送的 `cmd*`，`{priv-lvl=15}` 回給設備當權限等級。
- **一定要用單引號，** 雙引號字串不接受 `\*`。
- 設了 `authorize_group` 的設備不會執行腳本的授權部分，認證和計費照舊，所以 `tacacs.lua` 的計費記錄仍然有效。
- 規則不分使用者：上面的例子裡，能登入接入交換器的人都只能查看。要像本章前面那樣依角色區分，就繼續用腳本。
- 格式和檢查的完整說明見 [usage 2.3 節](../usage.md#tacacs-授權群組-tacacsauthorize_groups)。

## 設備端設定

以 Cisco IOS 為例（**還沒有在實體設備上測過**，不同版本的指令可能不同，上線前請在實驗設備上確認）：

```
aaa new-model
tacacs server rradius-1
 address ipv4 10.50.0.100
 key tacacs-shared-secret-change-me
aaa group server tacacs+ RRADIUS
 server name rradius-1
!
aaa authentication login default group RRADIUS local
aaa authorization exec default group RRADIUS local
aaa authorization commands 1 default group RRADIUS local
aaa authorization commands 15 default group RRADIUS local
aaa accounting exec default start-stop group RRADIUS
aaa accounting commands 1 default stop-only group RRADIUS
aaa accounting commands 15 default stop-only group RRADIUS
```

- **一定要保留 `local` 當備援，** 並在設定前先確定本機的緊急帳號能登入。rradius 停機或網路不通時，設備會改用本機帳號；沒有備援就會把自己鎖在外面。
- 先開認證，確認沒問題再開指令授權。設錯時可能連 `exit` 都被拒絕。
- **enable 不要交給 rradius，除非你設了 `enable = "login"`。** rradius 預設拒絕 enable 請求（回 FAIL），而設備收到 FAIL 不會改用本機的 enable 密碼。兩種做法：
  - 維持設備本機的 enable 密碼（`aaa authentication enable default enable`）。上面的設定沒有把 enable 交給 rradius，權限等級由授權回覆的 `priv-lvl` 決定，管理員通常不需要 enable。
  - 在 `[tacacs]` 設 `enable = "login"`，設備用 `aaa authentication enable default group RRADIUS enable`。這時管理員用自己的密碼 enable，所以要在腳本裡限制，例如 `authen_service == 2` 時只讓 `netadmin` 角色通過。
- 多台 rradius 時，`aaa group server` 列多個 `server name`，設備會依序嘗試。

## 上線前

- **49 埠：** 用 systemd 的 `AmbientCapabilities=CAP_NET_BIND_SERVICE`，或 `setcap cap_net_bind_service=+ep` 給執行檔，不要用 root 執行。
- **TACACS+ 的加密很弱：** RFC 8907 的混淆只用 MD5 和共用密碼，不能取代真正的加密。設備和 rradius 之間要走管理網路，共用密碼夠長（少於 16 個字元啟動時會有警告）且每台不同（可以用多個 `[[tacacs.clients]]`）。設備支援 TACACS+ over TLS（RFC 9887）時可以改用 TLS，這需要 rradius-enterprise，見企業版手冊第 9 章。
- **ASCII 登入**（設備問帳號、再問密碼）和 PAP 都支援。enable 預設拒絕，改密碼一律回 FAIL，見 [usage 4.6 節](../usage.md#46-tacacs-屬性)。
- **CHAP 和 MS-CHAPv2**（用 PPP 撥接的設備才會用到）由認證串接比對，所以帳號的密碼要是伺服器讀得到的形式：CHAP 要明文，MS-CHAPv2 要明文或 `{nthash}`。範例的帳號存 bcrypt，這兩種登入會被拒絕，一般的 SSH 登入（PAP、ASCII）不受影響。沒有串接的設備收到的是 RESTART，設備可以改用 PAP。
- **給使用者看的訊息：** 腳本用 `set_attr('server_msg', '...')` 設的文字會放進回覆，設備通常會顯示給登入的人，例如拒絕指令時說明原因。授權群組的規則不能帶訊息。
- **閒置連線** 300 秒沒有完整封包就關閉（`idle_timeout_ms`），每條連線最多 8 個未完成的登入。
- **記錄的保存：** `tacacs_log` 會一直變大，rradius 不會自動清除。清除舊資料的做法見[第 11 章](11-operations.md#清除舊資料)。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| 連線馬上被關閉 | 來源不在 `[[tacacs.clients]]`，記錄有 `TACACS+ connection from unknown client` |
| 認證回 ERROR 然後連線被關閉 | 共用密碼不同，解出來的內容是亂碼。記錄有 `a body did not decode, so the shared secret may be wrong`，`rradius_tacacs_malformed_total` 會增加 |
| `enable` 指令一律失敗 | 預設拒絕 enable，記錄有 `TACACS+ enable request refused`，見上方「設備端設定」 |
| 能登入但每個指令都被拒絕 | 授權腳本拒絕：帳號沒有角色、`service` 不是 `shell`，或 `cmd` 不在允許清單。用授權群組時，是沒有任何一條規則成立 |
| 計費回 ERROR | 腳本出錯，看 `Lua script error`；設備會重送 |
