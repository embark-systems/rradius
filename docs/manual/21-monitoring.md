# 21. 監控與告警（含 SNMP trap）

**情境：** 營運中的 rradius 要有人盯：節點掛了、叢集沒有 leader、計費沒寫進資料庫、登入突然大量被拒絕，都要在使用者抱怨之前被發現。維運團隊（例如電信的 NOC）常用的監控系統只認 SNMP，或只認 Prometheus。

rradius 輸出的是 Prometheus 的指標（[第 11 章〈監控〉](11-operations.md#監控)），**沒有內建 SNMP**。這一章給一組可以直接載入的告警規則，並說明三種把它們接到各種監控系統的做法。

範例目錄：[`deploy/monitoring/`](../../deploy/monitoring/)

| 檔案 | 內容 |
| --- | --- |
| `rradius.rules.yml` | 19 條 Prometheus 告警規則 |
| `rradius.rules.test.yml` | `promtool test rules` 的案例：合成的時間序列下，哪些告警該響、哪些不該響 |
| `alertmanager.yml` | 把告警送到 `snmp_notifier` 的路由與接收者 |

## 三種接法

| 你的監控系統 | 做法 | 驗證過嗎 |
| --- | --- | --- |
| Prometheus 與 Alertmanager（或能收 Alertmanager 的 webhook 的系統） | 載入 `rradius.rules.yml`，告警由 Alertmanager 發送 | 規則的語法與邏輯用 `promtool` 驗證；沒有接真實的通知管道 |
| **只收 SNMP trap** | Alertmanager 的 webhook 送給 `snmp_notifier`，由它發 SNMP trap（v2c 或 v3） | 本機實測：Alertmanager、`snmp_notifier`、`snmptrapd` 三個容器，告警送進去，`snmptrapd` 收到帶告警名稱、節點、嚴重程度與摘要的 v2c trap。**v3 與真實的監控系統沒有測** |
| 能讀 HTTP 的監控系統（例如 Zabbix 的 HTTP agent 項目） | 直接讀 `GET /metrics`（Prometheus 文字格式），挑要看的指標，自己設門檻 | **沒有驗證**；規則檔的門檻可以當成設定的參考 |
| **只會用 SNMP 輪詢**（`snmpwalk` 要看得到 rradius 的數字） | **不支援。** | 見下方〈為什麼沒有 SNMP 輪詢〉 |

## 載入規則

在 Prometheus 設定裡抓 rradius 的 `/metrics`，並載入規則檔（job 名稱要是 `rradius`，規則用它找節點）：

```yaml
scrape_configs:
  - job_name: rradius
    static_configs:
      - targets: ["10.0.0.11:9812", "10.0.0.12:9812", "10.0.0.13:9812"]
rule_files:
  - rradius.rules.yml
alerting:
  alertmanagers:
    - static_configs:
        - targets: ["alertmanager:9093"]
```

`[metrics]` 沒有認證，只綁內部位址，讓 Prometheus 連得到、別人連不到。載入前先檢查：

```bash
promtool check rules rradius.rules.yml
promtool test rules rradius.rules.test.yml    # 在範例目錄裡執行
```

## 告警一覽

「條件」欄裡的時間是持續多久才響，避免短暫的抖動。嚴重程度寫在規則的 `severity` 標籤：`critical` 要立刻處理，`warning` 要看。

### 可用性

| 告警 | 嚴重 | 條件 | 怎麼處理 |
| --- | --- | --- | --- |
| `RradiusDown` | critical | 抓不到 `/metrics` 超過 2 分鐘 | 程序掛了，或 `[metrics]` 的位址從監控主機連不到；看 systemd 或容器的狀態與記錄 |
| `RradiusNoLeader` | critical | 這個節點沒有 leader 超過 1 分鐘 | 節點被隔開，或叢集失去多數；這時它不能寫，計費進暫存、需要寫入的登入被拒絕，見[第 7 章](07-ha-cluster.md) |
| `RradiusNotCaughtUp` | warning | 本機副本沒有追上叢集超過 5 分鐘 | 節點剛加入或剛重啟、資料多；沒有進展就看記錄 |

### 流量

| 告警 | 嚴重 | 條件 | 怎麼處理 |
| --- | --- | --- | --- |
| `RradiusOverload` | critical | 5 分鐘內有請求因為太忙被丟棄 | 資料庫、目錄或腳本變慢；看回應時間（`RradiusSlowLogins`）與[第 12 章](12-troubleshooting.md) |
| `RradiusDiscardingForTheDatabase` | warning | 因資料庫或沒有 leader 丟棄請求 | 資料庫或叢集不能回答，和 `RradiusNoLeader` 一起看 |
| `RradiusManyRejects` | warning | 10 分鐘內超過一半的登入被拒絕，而且每秒超過 0.2 個 | 目錄故障、設定錯誤，或有人在試密碼；看記錄的拒絕原因 |
| `RradiusInvalidPackets` | warning | 10 分鐘內持續有簽章或格式不對的封包 | 通常是某台設備的共用密碼設錯 |
| `RradiusSlowLogins` | warning | 登入回應時間的 p99 超過 1 秒，持續 10 分鐘 | 資料庫、目錄或腳本變慢；1 秒是起點，依你的設備逾時調整 |

### 計費

| 告警 | 嚴重 | 條件 | 怎麼處理 |
| --- | --- | --- | --- |
| `RradiusAccountingSpoolBacklog` | warning | 計費暫存裡有還沒進資料庫的資料超過 15 分鐘 | 資料庫或叢集有問題；恢復後暫存會自己重放，見[第 11 章](11-operations.md#計費暫存的日常) |
| `RradiusAccountingSpoolLoss` | critical | 一小時內有計費在重放時被放棄或發現損壞 | 看暫存目錄的 `rejected.log`，這些紀錄沒有進資料庫 |
| `RradiusAccountingLogDropping` | warning | 計費紀錄檔有請求沒寫進去（只在設了 `[radius.accounting_log]` 時） | 磁碟太慢、滿了或不能寫 |

### 外部相依（只在設了該功能時才有指標）

| 告警 | 嚴重 | 條件 | 怎麼處理 |
| --- | --- | --- | --- |
| `RradiusExternalDatabaseUnreachable` | critical | 設了 `required_for_ready` 的外部資料庫連不上超過 2 分鐘 | 這個節點的 `/readyz` 回 503，負載平衡器會把它移出；查資料庫與網路 |
| `RradiusExternalDatabaseFailing` | warning | 呼叫外部資料庫失敗（`unavailable`） | 資料庫掛了、慢，或網路不通 |
| `RradiusExternalDatabasePoolFull` | warning | 連線池用滿超過 5 分鐘 | 呼叫在排隊：查詢變慢，或 `pool_size` 太小 |
| `RradiusNamedQueryStale` | warning | 命名查詢超過 2 分鐘沒有成功 | 位址池使用率這類數字是舊的；看 `RradiusNamedQueryFailing` 與記錄 |
| `RradiusNamedQueryFailing` | warning | 命名查詢在 10 分鐘內有失敗 | `reason` 標籤是 `timeout`、`error` 或 `shape`，見[第 14 章](14-ip-pool.md)的監控 |

### 維運

| 告警 | 嚴重 | 條件 | 怎麼處理 |
| --- | --- | --- | --- |
| `RradiusReloadFailed` | warning | 重新載入（SIGHUP）失敗 | 節點照舊設定運作；記錄寫了原因，修好再重新載入 |
| `RradiusLogLinesDropped` | warning | 記錄來不及寫而丟行 | 記錄的去處太慢 |
| `RradiusEapSessionsLost` | warning | 802.1X 對話被擠掉或被拒絕 | 同時進行的對話超過節點保留的量，見[第 12 章](12-troubleshooting.md#wi-fi8021x) |

**門檻是起點，不是標準答案。** 規則是依指標的意義和合成資料寫的，沒有在你的流量上校正過：上線後看幾週的實際數字，再調 `for` 的時間和 `RradiusManyRejects`、`RradiusSlowLogins` 這類比例與時間的門檻。

## 把告警變成 SNMP trap

`snmp_notifier`（獨立的開源專案，<https://github.com/maxwo/snmp_notifier>，rradius 不包含它）接 Alertmanager 的 webhook，對每組告警發一個 SNMP trap。

1. 把 `alertmanager.yml` 的路由與接收者放進你的 Alertmanager 設定。接收者把告警送到 `http://snmp-notifier:9464/alerts`，也送「已解除」。
2. 啟動 `snmp_notifier`，指向接收 trap 的監控系統：

   ```bash
   snmp_notifier --snmp.destination=nms.example.net:162 --snmp.community=<共同體字串> \
     --trap.default-oid=<你的企業編號>.1 \
     --trap.description-template=/etc/snmp_notifier/description-template.tpl
   ```

3. 它預設的 trap OID（`1.3.6.1.4.1.98789.1`）不是你的：要換成你自己的企業編號下的子樹。告警上有 `oid` 標籤時，它會用那個值當這一筆的 trap OID，可以在規則裡替重要的告警指定不同的 OID。
4. **v2c 的共同體字串是明文。** 要加密與認證就用 v3：`--snmp.version=V3` 加上 `--snmp.authentication-*`、`--snmp.private-*` 選項（見它的 `--help`）。

收到的 trap 長這樣（`snmptrapd` 的輸出，內容經過整理）：

```
iso.3.6.1.6.3.1.1.4.1.0 = OID: iso.3.6.1.4.1.98789.1
iso.3.6.1.4.1.98789.2.1 = STRING: "1.3.6.1.4.1.98789.1[alertname=RradiusDown,instance=a:9812]"
iso.3.6.1.4.1.98789.2.2 = STRING: "critical"
iso.3.6.1.4.1.98789.2.3 = STRING: "1/1 alerts are firing:
  Status: critical
  - Alert: RradiusDown
    Summary: rradius on a:9812 cannot be scraped"
```

要讓監控系統看得懂，把 trap 的 OID 與三個欄位（名稱、嚴重程度、說明）設成它的 trap 規則。

## 為什麼沒有 SNMP 輪詢

內建 SNMP agent 要在 rradius 裡處理 SNMP 的編碼和（v3 的）認證與加密，是新的攻擊面，也沒有可用的相依（規則是不新增相依）。標準的 RADIUS 伺服器 MIB（RFC 4669、RFC 4671）也只有每個 client 的幾個計數，能回答的問題比 Prometheus 指標少很多。如果真的有只會 SNMP 輪詢的系統必須接上，日後可行的做法是讓 rradius 透過 AgentX（RFC 2741）接到主機上的 net-snmp，把指標掛在一棵 MIB 子樹下，認證和加密都交給 `snmpd`；這件事還沒做，要有明確的需求再評估。

## 驗證過什麼

- **規則：** `promtool check rules` 通過；19 條規則用到的每個 `rradius_*` 指標名稱，都對照過執行中伺服器的 `/metrics`（預設設定下就有的）或原始碼（要開外部資料庫、計費紀錄檔、命名查詢才有的）；`promtool test rules` 對 9 組合成的時間序列，驗證該響與不該響的告警。
- **通知：** `amtool check-config` 通過；Alertmanager 送 webhook 給 `snmp_notifier`、再發 v2c trap 到 `snmptrapd` 這條路，在本機容器實測過。
- **沒有驗證：** 規則在真實流量下的門檻、SNMP v3 trap、真實的監控系統（Zabbix、傳統的 NMS）當接收端、用 `snmpwalk` 讀 rradius（不支援）。
