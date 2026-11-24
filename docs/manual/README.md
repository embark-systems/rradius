# rradius 使用手冊

這份手冊依主題寫：先認識程式、安裝、第一次登入，再依功能一章一章往下，最後是設定、命令與指標的參考附錄。常見的部署情境（企業 Wi-Fi、有線 MAC 認證、遠端存取、寬頻與 ISP）寫成「情境實作」章，每個情境都有可以直接啟動的設定檔，放在 [`rradius-server/examples/scenarios/`](../../rradius-server/examples/scenarios/)，並有自動測試（`rradius-server/tests/scenarios_test.rs`）照手冊的方式啟動它們。

指令的用法也可以直接問程式：`rradiusd help`、`rradiusd schema` 與 `rradiusd config show`。

## 第一次使用

| 章 | 內容 |
| --- | --- |
| [01 認識 rradius](01-overview.md) | 它是什麼、一個請求怎麼走、設定檔的結構、資料放在哪裡、怎麼用外部工具測試 |
| [02 安裝](02-install.md) | 套件、容器、Kubernetes、Synology NAS，以及從原始碼建置 |
| [03 第一次啟動與登入](03-quick-start.md) | 單機、帳號存在資料表，十分鐘內收到第一個 Access-Accept |

## 我要做什麼，看哪一章

| 我要… | 章 |
| --- | --- |
| 設定檔怎麼寫、哪些改動要重啟 | [04](04-configuration.md)，每個鍵的說明在[附錄 A](90-config-reference.md) |
| 登記 NAS（RADIUS 用戶端），或換掉共用密碼 | [05](05-radius-clients.md)、[18](18-operations.md#更換密碼) |
| 加屬性、改字典 | [06](06-dictionary.md)，Lua 用到的函式在[附錄 D](93-lua-reference.md) |
| 帳號放在資料表、密碼雜湊、驗證鏈；從舊系統搬帳號 | [07](07-users-and-chains.md) |
| 帳號在公司目錄（LDAP、Active Directory） | [08](08-ldap-and-ad.md) |
| 帳號在別的資料庫或 HTTP API，用具名查詢看指標 | [09](09-external-databases.md) |
| 用 Lua 寫政策 | [10](10-lua-policies.md) |
| 企業 Wi-Fi：802.1X、憑證、撤銷 | [11](11-eap.md)、[15](15-access-recipes.md) |
| 讓印表機、IP 電話這類設備上有線網路（MAB） | [15](15-access-recipes.md) |
| VPN 用公司目錄登入，或加驗證碼 | [15](15-access-recipes.md) |
| 管理誰能登入路由器、能下哪些指令（TACACS+） | [12](12-tacacs.md) |
| 計費與 session | [13](13-accounting.md) |
| 寬頻與 ISP：位址池、前綴池、用量控制、線路身分、固定位址、速率屬性、雙堆疊 | [14](14-isp-recipes.md) |
| 任何一台停機都不中斷服務；跨機房；Kubernetes | [16](16-cluster.md) |
| 看狀態、指標、日誌，被告警通知（含 SNMP trap） | [17](17-monitoring.md)，指標清單在[附錄 C](92-metrics-reference.md) |
| 上線之後的日常工作：變更、備份、升級、清除舊資料 | [18](18-operations.md) |
| 效能調校與壓測 | [19](19-performance.md) |
| 安全強化與上線前檢查 | [20](20-security.md) |
| 出了問題，從現象找原因 | [21](21-troubleshooting.md) |

## 參考附錄

| 附錄 | 內容 |
| --- | --- |
| [A 設定參考](90-config-reference.md) | 每個設定鍵的預設值、能不能 reload |
| [B 命令參考](91-command-reference.md) | 每個命令與旗標、結束碼 |
| [C 指標參考](92-metrics-reference.md) | 每個 Prometheus 指標與標籤 |
| [D Lua API 參考](93-lua-reference.md) | 腳本裡能呼叫的函式 |
| [E 術語與 RFC 對照](94-glossary.md) | 名詞與對應的 RFC |

## 範例的共同約定

範例的做法（相對路徑、一次只跑一個情境、用發行檔時的執行檔路徑、示範密碼）寫在[第 14 章的開頭](14-isp-recipes.md#這些情境共同的做法)和[第 03 章](03-quick-start.md#準備工作目錄)。

企業版的功能（RadSec、代理轉送、CoA 轉送、DPSK、動態用戶端等）不在這本手冊裡，見企業版手冊。
