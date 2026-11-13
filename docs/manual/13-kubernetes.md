# 13. 在 Kubernetes 上執行叢集

**情境：** 公司的服務都跑在 Kubernetes 上，rradius 也要。三個節點組成[第 7 章](07-ha-cluster.md)的叢集，每個節點是 StatefulSet 的一個 Pod，NAS 經過負載平衡器連到它們。

範例目錄：[`deploy/kubernetes/`](../../deploy/kubernetes/)

| 檔案 | 內容 |
| --- | --- |
| `kustomization.yaml` | `kubectl apply -k` 套用的清單，並把 `config/` 做成 ConfigMap |
| `statefulset.yaml` | Pod `rradius-0` 到 `rradius-2`、探針、資料磁碟 |
| `services.yaml` | 節點之間用的 `rradius-cluster`，NAS 用的 `rradius` |
| `pdb.yaml` | 維護時一次只停一個 Pod |
| `config/rradius-0.toml` 到 `rradius-2.toml` | 三個節點的設定，只差 `node_id` |
| `config/auth.lua`、`config/1_schema.sql` | 計費腳本和資料表，和 compose 範例相同 |
| `secret.example.yaml` | 要先建立的 Secret 長什麼樣子 |
| `hostnetwork-patch.yaml` | 沒有合適負載平衡器時用的替代做法（預設不用） |
| `networkpolicy.yaml` | 限制誰能讀 metrics（預設不套用） |

叢集本身的行為（寫入要多數節點、停兩台時會怎樣）和第 7 章相同，這一章只講 Kubernetes 上不一樣的地方。

## 架構

```
                NAS
                 │ RADIUS 1812/1813（來源位址要原樣到達 Pod）
        Service rradius（LoadBalancer，externalTrafficPolicy: Local）
      ┌──────────┼──────────┐
  rradius-0  rradius-1  rradius-2        StatefulSet，每個有自己的 PVC
  node_id 1  node_id 2  node_id 3
      └── rradius-N.rradius-cluster:8100/8200 ──┘   headless Service
```

- **Pod 名稱決定設定檔：** 容器以 `server --config /etc/rradius/$(POD_NAME).toml` 啟動，`rradius-N` 讀 `rradius-N.toml`，`node_id` 是 N+1。
- **節點用名稱找彼此：** `[[cluster.nodes]]` 寫 `rradius-0.rradius-cluster:8100`。Pod 重建後 IP 會變，名稱不變，節點會自己歸隊。
- **資料在 PVC：** `/var/lib/rradius` 掛 StatefulSet 的 `volumeClaimTemplates`，Pod 刪掉重建，資料還在。

## 步驟

### 1. 映像

範例用 `rradius-server:latest`，`imagePullPolicy: IfNotPresent`。在 repo 根目錄建置：

```bash
docker build -t rradius-server:latest .
```

- **Docker Desktop、OrbStack 內建的 Kubernetes** 直接用本機的映像，不必再做什麼。
- **kind：** `kind load docker-image rradius-server:latest`，把映像載入 kind 的節點。
- **正式環境：** 推送到自己的 registry，再在 `kustomization.yaml` 打開 `images` 改成那個名稱與版本：

  ```yaml
  images:
    - name: rradius-server
      newName: registry.example.com/rradius-server
      newTag: "1.0"
  ```

### 2. 密碼

叢集內部的兩把密碼和 NAS 的共用密碼放在 Secret `rradius-secrets`，掛在 `/run/secrets/rradius`。第一次套用前建立一次：

```bash
kubectl create secret generic rradius-secrets \
  --from-literal=raft="$(openssl rand -hex 32)" \
  --from-literal=api="$(openssl rand -hex 32)" \
  --from-literal=radius="$(openssl rand -hex 16)"
```

`radius` 換成你的 NAS 實際設定的共用密碼。三個節點共用同一個 Secret，所以 `raft`、`api` 自然相同。範例目錄不放真的密碼，`secret.example.yaml` 只是格式說明。

### 3. 套用

先把 `config/rradius-0.toml` 到 `rradius-2.toml` 的 `[[radius.clients]] address` 從文件用的 `192.0.2.0/24` 改成 NAS 實際的網段（三份要一樣），理由見〈來源位址會不會被改寫〉。然後：

```bash
kubectl apply -k deploy/kubernetes
kubectl rollout status statefulset/rradius
```

三個 Pod 同時啟動（`podManagementPolicy: Parallel`），選出 leader 後才開始服務，通常十幾秒內都就緒。

### 4. 加帳號

範例設定有 `[accounts]`，可以在任一個 Pod 裡用 `user` 指令，寫入會經過叢集複寫到每一台：

```bash
printf '%s\n' 'the-password' | kubectl exec -i rradius-0 -- rradius-server user add --config /etc/rradius/rradius-0.toml alice
kubectl exec rradius-1 -- rradius-server user show --config /etc/rradius/rradius-1.toml alice
```

大量匯入時，和 compose 範例一樣加一個新的 migration 檔（例如 `config/2_users.sql`），同時在 `kustomization.yaml` 的 `files` 和 `statefulset.yaml` 的 `items` 各加一行，路徑是 `migrations/2_users.sql`。ConfigMap 是平的，migration 靠 `items` 放進子目錄，**漏了 `items` 那行，檔案不會出現在 `migrations/` 裡。**

### 5. 確認

```bash
kubectl get pods
kubectl get service rradius
```

從一台 NAS 的位址對 `rradius` 的 EXTERNAL-IP 送測試登入，得到 Access-Accept 就通了。沒有回應時，先看下一節的來源位址。

## 來源位址會不會被改寫

rradius 用封包的**來源位址**認 NAS（`[[radius.clients]] address`），每個 client 有自己的共用密碼與設定。封包經過 Kubernetes 的 Service 時，來源位址常被改成節點或閘道的位址，結果有兩種：

- 改寫後的位址不在任何 client 的網段裡：rradius 不認得它，**不回應**，計入 `rradius_radius_discarded_total{reason="unknown_client"}`。
- 改寫後的位址剛好落在某個 client 的網段裡，例如 client 寫了 `10.0.0.0/8` 這種寬網段：請求照樣被接受，**看起來一切正常**，但所有 NAS 都被當成同一個 client。這種情況計數不會增加，只能從認證記錄看出來。

範例的做法：

- `rradius` Service 設 `externalTrafficPolicy: Local`，負載平衡器把封包直接送到跑著 Pod 的節點，不經過其他節點轉送，來源位址保持原樣。代價是只有跑著就緒 Pod 的節點收流量，由負載平衡器的健康檢查處理。
- 範例的 `[[radius.clients]]` 是文件用的網段 `192.0.2.0/24`，**套用前改成 NAS 實際的網段，而且越窄越好，** 不要寬到包含 Kubernetes 節點和 Pod 的網段。

**一定要驗證：** 從一台 NAS 送一筆請求，然後看認證記錄的 `source=` 是不是那台 NAS 的位址：

```bash
kubectl logs rradius-0 | grep rradius_auth | tail -3
```

```
... INFO rradius_auth: authentication protocol="radius" client="192.0.2.0/24" source=192.0.2.10 user="alice" ...
```

請求可能送到任何一個 Pod，三個都要看（把 `rradius-0` 換成 `rradius-1`、`rradius-2`）。`source=` 是節點、閘道或 Pod 網段的位址，就是被改寫了。沒有任何記錄時，看 `unknown_client` 有沒有增加：

```bash
kubectl port-forward pod/rradius-0 9812:9812 &
curl -s localhost:9812/metrics | grep 'reason="unknown_client"'
```

不同環境的結果不同：

- 雲端的負載平衡器大多支援 `externalTrafficPolicy: Local`，但有些 UDP 負載平衡器本身會改寫來源位址，要看雲端的文件。
- OrbStack 內建的 Kubernetes 從 Mac 送到 LoadBalancer 的封包，`source=` 是 Pod 網段的閘道（例如 `192.168.194.1`），所以在 OrbStack 上驗證不了來源位址會不會保留。只是在本機試用時，可以暫時把 client 改成那個位址。

### 沒有合適的負載平衡器時：hostNetwork

`hostnetwork-patch.yaml` 讓 Pod 直接用節點的網路，NAS 的封包到節點的位址，來源位址不會被改寫。在 `kustomization.yaml` 打開：

```yaml
patches:
  - path: hostnetwork-patch.yaml
```

- 一個節點只能放一個 rradius Pod（patch 已設 `podAntiAffinity`），所以至少要三個節點。
- NAS 設定的是三個節點的位址，主備或輪流都可以，和第 7 章不在 Kubernetes 上時一樣。
- Pod 搬到別的節點時位址會變，NAS 那邊要跟著改。所以適合固定的節點，例如用 `nodeSelector` 指定。
- **metrics 埠和叢集埠會開在節點的每個位址上，包括對外的位址。** 範例的 `[metrics] bind = "0.0.0.0"`，這時 9812 沒有認證、沒有 TLS 地對外開放；NetworkPolicy 也管不到用 hostNetwork 的 Pod，`networkpolicy.yaml` 擋不住。要用節點的防火牆只讓內部網路連 9812、8100、8200，或把 `[metrics] bind` 改成節點的內部位址（三個節點的位址不同，設定檔就不再只差 `node_id`）。

### TACACS+

設定加 `[tacacs]` 時，`statefulset.yaml` 加 49/tcp 的容器埠，再打開 `services.yaml` 裡註解掉的那段。TCP 經過 `externalTrafficPolicy: Local` 一樣保留來源位址，驗證方法相同。

## 健康檢查

範例的 `[metrics]` 綁 `0.0.0.0`，因為 kubelet 從節點連 Pod 的 IP 做探針。兩個路徑（[`docs/usage.md`](../usage.md) 2.8 節）：

| 路徑 | 200 | 503 | 範例的用途 |
| --- | --- | --- | --- |
| `/livez` | 程序在回應 | 不會 | startupProbe、livenessProbe |
| `/readyz` | 會回應認證 | 設了 `stop_without_leader_ms`，而且沒有 leader 超過這段時間 | readinessProbe |

- **未就緒的 Pod 會從 `rradius` Service 移出，** NAS 的請求改送其他 Pod。範例設 `stop_without_leader_ms = 10000`：三台剩一台時，剩下那台約 10 秒後停止回應、變成未就緒，`/livez` 仍是 200，容器不會被重啟。
- **沒設 `stop_without_leader_ms` 時 `/readyz` 一直是 200，** 因為那時節點沒有 leader 也照常用本機副本回應只讀的登入；把它移出 Service 反而讓所有登入都失敗。
- **startupProbe 等最多 5 分鐘：** metrics 埠在節點追上叢集後才開，這要先有 leader。沒有 startupProbe 的話，livenessProbe 會在選出 leader 前把容器重啟。
- **`rradius-cluster` 設 `publishNotReadyAddresses: true`：** 未就緒的 Pod 也保留 DNS 名稱。不設的話，等 leader 的節點彼此找不到，永遠選不出 leader。

## 日常操作

### 改設定

改 `config/` 的檔案後再 `kubectl apply -k deploy/kubernetes`。kustomize 產生的 ConfigMap 名稱會跟著內容變，StatefulSet 就一次換一個 Pod（`RollingUpdate`），每個 Pod 就緒後才換下一個。所以在這個範例裡，連 SIGHUP 就會生效的設定也是用滾動更新套用。

### 升級

換映像版本後 `kubectl apply -k`，一樣一次換一個。第 7 章說的「最後才停 leader」，StatefulSet 不會幫你挑順序（從 `rradius-2` 換到 `rradius-0`）；leader 被換掉時，選出新 leader 前的幾秒寫入會逾時，計費進暫存。

### 節點的維護

`pdb.yaml` 設 `maxUnavailable: 1`：`kubectl drain` 這類自願的中斷一次只能停一個 Pod，另外兩台保有多數。**不要 `kubectl delete pod --all`，** 三台同時重啟會暫時失去多數。

### 不能自動擴充，也不能用 replicas 改節點數

叢集的成員寫死在 `[[cluster.nodes]]`，所以：

- **不要設 HPA，** `replicas` 要等於 `[[cluster.nodes]]` 的數量。
- **`kubectl scale --replicas=1` 等於停掉兩個節點，** 剩下那台的行為就是第 7 章「停兩台」那一列。再擴回 3 時，回來的 Pod 可能各重啟一次才就緒：設了 `stop_without_leader_ms` 的節點啟動時等不到 leader 會以狀態碼 1 結束（`cluster was not ready to apply migrations`），由 Kubernetes 重新啟動，第二次就會歸隊。
- **要改成五台，** 等同建一個新的叢集：`[[cluster.nodes]]` 不支援在執行中增減，第 7 章〈換掉壞掉的節點〉也說明了這點。

### 限制誰能讀 metrics

metrics 埠沒有認證也沒有 TLS，綁 `0.0.0.0` 時叢集裡的人都讀得到。`networkpolicy.yaml` 只讓 RADIUS 埠對外、叢集埠只給其他 rradius Pod、metrics 埠只給 `monitoring` namespace（改成 Prometheus 所在的 namespace）。kubelet 的探針從節點本身來，大部分網路外掛都會放行，**套用後確認 Pod 還是就緒的。** 要用時加進 `kustomization.yaml` 的 `resources`。

## 企業版

企業版用 `rradius-enterprise` 的映像，在 `kustomization.yaml` 的 `images` 改名稱即可，設定方式相同。`[cluster.backup]` 的 S3 金鑰檔和其他密碼一樣放進 Secret，見企業版的 `docs/usage.md`。

## 常見問題

| 現象 | 原因 |
| --- | --- |
| Pod 一直沒就緒，startupProbe 失敗 | 選不出 leader：其他 Pod 沒起來、`raft`／`api` 密碼不同，或 `rradius-cluster` 沒有 `publishNotReadyAddresses`。看 `kubectl logs` |
| 擴回三台時 Pod 重啟一次 | 正常，見上方〈不能自動擴充〉 |
| 從 NAS 送請求沒有回應，`unknown_client` 增加 | 來源位址被改寫，見〈來源位址會不會被改寫〉 |
| 不同 NAS 的認證記錄 `client=` 都相同，`source=` 是節點或閘道的位址 | 來源位址被改寫，又剛好落在 client 的網段裡，見〈來源位址會不會被改寫〉 |
| 三台都就緒，但一台停掉後全部沒有回應 | 停的是 leader 而且剩下兩台還在選舉；幾秒內會恢復。持續的話看 `rradius_cluster_leader` |
| 新加的 migration 沒有套用 | `statefulset.yaml` 的 `items` 漏了那一行 |
