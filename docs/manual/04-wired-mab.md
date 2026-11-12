# 4. 有線網路的 MAC 認證（MAB）

**情境：** 交換器的 port 開了 802.1X，但印表機、IP 電話、監視器不會 802.1X。交換器等不到 EAP 時，改用設備的 MAC 位址當帳號來問（MAC Authentication Bypass）。認得的設備放進它該在的 VLAN，不認得的拒絕，由交換器放進訪客或隔離 VLAN。

範例目錄：[`rradius-server/examples/scenarios/wired-mab/`](../../rradius-server/examples/scenarios/wired-mab/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.toml` | 設定檔：沒有 `[auth]`，全部交給腳本 |
| `mac-auth.lua` | 依 MAC 查設備、回 VLAN 的腳本 |
| `migrations/1_schema.sql` | `mac_devices`（MAC、VLAN、說明） |
| `migrations/2_demo_data.sql` | 一台印表機（VLAN 30）、一台 IP 電話（VLAN 40） |

腳本是目錄裡的 `mac-auth.lua`，內容和 repo 的範例腳本 `examples/mac-auth.lua` 相同。

## 步驟

### 1. 改 client 並啟動

範例的 client 是整個交換器管理網段 `10.40.0.0/24`。本機測試時改成 `127.0.0.1`：

```bash
cd rradius-server
./target/release/rradius-server --config examples/scenarios/wired-mab/rradius.toml
```

### 2. 模擬交換器的請求

交換器的 MAB 請求把 MAC 位址放在 `User-Name`、`User-Password` 和 `Calling-Station-Id`，寫法各家不同：

```bash
echo 'User-Name=00-11-22-AA-BB-CC,User-Password=00-11-22-AA-BB-CC,Calling-Station-Id=00-11-22-AA-BB-CC' \
  | radclient 127.0.0.1:1812 auth switches-shared-secret-change-me
```

會收到 Access-Accept，帶 VLAN 30 的三個屬性（`Tunnel-Type = VLAN`、`Tunnel-Medium-Type = IEEE-802`、`Tunnel-Private-Group-Id = 30`）。

| `User-Name` | `Calling-Station-Id` | 結果 |
| --- | --- | --- |
| `00-11-22-AA-BB-CC` | `00-11-22-AA-BB-CC` | Accept，VLAN 30 |
| `00a0.c912.3456`（Cisco 寫法） | `00:a0:c9:12:34:56` | Accept，VLAN 40 |
| `001122aabbcc` | `00:00:00:00:00:01` | Reject：名稱和實際的設備不同 |
| `00ffffffffff` | `00:ff:ff:ff:ff:ff` | Reject：不認得 |

## 腳本做了什麼

- **MAC 位址正規化：** 去掉 `-`、`:`、`.`，轉小寫，要剛好 12 個十六進位字元。所以資料表存 `001122aabbcc`，不管交換器怎麼寫。
- **`User-Name` 要和 `Calling-Station-Id` 是同一個 MAC。** 名稱是設備自己說的，`Calling-Station-Id` 是交換器看到的；兩者不同就拒絕。
- **不檢查密碼。** MAB 沒有秘密可以比對，只靠位址。
- **只處理 RADIUS 認證，** 計費一律回覆，TACACS+ 一律拒絕。

## 這個做法的限制

- **MAC 位址不是秘密。** 看得到設備標籤或線路的人就能冒用。把這些設備放在只能到達必要服務的 VLAN（印表機只連列印伺服器，電話只連 SIP 伺服器），不要放進一般網段。
- **不認得的設備被拒絕之後，** 要交換器設定失敗時的處理（例如放進訪客 VLAN 或維持隔離）。這是交換器的設定，不在 rradius。
- **想讓不認得的設備直接進訪客 VLAN，** 可以改寫腳本：查不到時回 `accept` 並設訪客 VLAN。這等於任何人插上網路線都能上網，請先確認訪客 VLAN 的範圍。

## 和 802.1X 用在同一台交換器

同一個 port 先跑 802.1X、失敗再 MAB 時，兩種請求來自同一個 client。做法：

- **把 EAP 和 MAB 分開：** 加 `[radius.eap]`（第 3 章），並加一條串接處理一般帳號，`mac-auth.lua` 放在後面當 `lua` 處理器。`mac-auth.lua` 開頭的註解說明了順序：先讓檢查密碼的處理器判斷名稱，它不認得時才看 MAC。
- **PEAP 的串接裡，`lua` 處理器的 `accept` 不算數，** 必須有處理器真的比對過密碼，所以 MAB 規則不會讓 PEAP 不用密碼就通過。

## 新增設備

目前要用新的 migration 檔加資料，例如 `3_add_cameras.sql`：

```sql
INSERT INTO mac_devices (mac, vlan, note) VALUES ('0011223344aa', '50', '大門監視器');
```

放進 `migrations/` 後重新啟動。設備多、變動頻繁時的做法見[第 11 章](11-operations.md#帳號和資料)。
