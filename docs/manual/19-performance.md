# 19 效能調校與壓測

> 本章讓你做到：
> - 用 `tuning` 找出作業系統該調高的限制，知道建議值是怎麼算出來的
> - 設定接收 socket、接收緩衝區、工作執行緒與同時處理的上限
> - 用 `rradius-bench` 量出自己機器的容量，並判斷量到的數字可不可信
> - 用 repo 附的 `benchmarks/` 設定重現一次量測

前置條件：做完[第 03 章](03-quick-start.md)，會用 `rradius-bench` 送一筆登入；讀過[第 17 章](17-monitoring.md)的指標與 `status`。

本章小節：

- [概念：請求會卡在哪裡](#概念請求會卡在哪裡)
- [用 tuning 檢查作業系統](#用-tuning-檢查作業系統)
- [接收 socket 與接收緩衝區](#接收-socket-與接收緩衝區)
- [工作執行緒與 pin_threads](#工作執行緒與-pin_threads)
- [同時處理的上限](#同時處理的上限)
- [叢集資料庫的兩個設定](#叢集資料庫的兩個設定)
- [rradius-bench 的用法](#rradius-bench-的用法)
- [先量工具自己：calibrate](#先量工具自己calibrate)
- [用 benchmarks 的設定量一次](#用-benchmarks-的設定量一次)
- [量測範例與量測環境](#量測範例與量測環境)
- [怎麼判斷結果可不可信](#怎麼判斷結果可不可信)
- [常見錯誤](#常見錯誤)
- [相關章節](#相關章節)

## 概念：請求會卡在哪裡

一個 RADIUS 請求從 NAS 送出到收到回應，會經過幾個可能塞住的地方。塞住的位置不同，看到的現象和要調的東西也不同：

| 位置 | 塞住時的現象 | 從哪裡看 | 調什麼 |
| --- | --- | --- | --- |
| 作業系統的 UDP 接收緩衝區 | 封包在 rradius 看到之前就被核心丟掉，NAS 逾時，rradius 的指標完全沒有記錄 | `tuning` 開頭的「UDP packets dropped」 | 接收緩衝區、`radius.sockets` |
| rradius 同時處理的上限 | `rradius_radius_discarded_total{reason="overload"}` 增加，日誌有 `overloaded: discarded … packets` | `status`、指標 | `max_auth_inflight`、`max_acct_inflight`，或找出後面為什麼慢 |
| 後端：資料庫、LDAP、腳本 | 回應時間（p99）變長，處理中的請求變多 | `status` 的 p99 與 in progress | 第 07～10 章各自的逾時與連線數 |
| 量測工具本身 | 工具的 CPU 用滿，數字反映的是工具而不是伺服器 | `rradius-bench` 的 `busiest worker` 與警告 | 多開執行緒、換一台機器送 |

調校的順序：先用 `tuning` 把作業系統的限制調到夠用，再用 `rradius-bench` 量，邊量邊看伺服器的指標。不要一開始就改 rradius 的設定。

> **注意：** 本章不給「rradius 一秒能處理多少筆」的數字。那取決於你的硬體、作業系統、驗證鏈與資料庫。[量測範例](#量測範例與量測環境)裡的數字只示範輸出怎麼讀，不能拿來規劃容量。

## 用 tuning 檢查作業系統

`tuning` 讀這台機器的系統值與你的設定檔，告訴你哪些限制該調高、調到多少、怎麼調。它只讀不改：不改系統設定、不啟動節點、不開任何埠。

```console
$ rradiusd tuning --config rradius.toml
```

用法是 `rradiusd tuning [--config <path>]`。沒有 `--config` 時讀 `/etc/rradius/rradius.toml`；讀不到就印一行說明，改用全部預設值來算。參數或指定的設定檔有錯時結束碼 2，其他情況是 0。它有 Linux、macOS、FreeBSD 三種平台的建議；其他平台只印一行「no advice for this platform」。

下面是第 17 章那份實驗設定在 macOS 上的實際輸出：

```console
$ rradiusd tuning --config rradius.toml
rradius tuning advice for macOS

UDP packets dropped because a receive buffer was full, since the system started: 137278
  Raise the receive buffers below, then watch whether this count still grows.

Largest socket buffer (kern.ipc.maxsockbuf)
  current:   8388608
  suggested: 33554432
  why:       the receive buffer counts its bookkeeping against this limit too, so twice the buffer; raise it before recvspace
  now:       sysctl -w kern.ipc.maxsockbuf=33554432
  persist:   /etc/sysctl.conf: kern.ipc.maxsockbuf = 33554432

UDP receive buffer (net.inet.udp.recvspace)
  current:   786896
  suggested: 16777216
  why:       every RADIUS socket gets this size without [radius] recv_buffer_bytes; it should hold its share of max_auth_inflight requests. Setting [radius] recv_buffer_bytes instead leaves only the limit to raise
  now:       sysctl -w net.inet.udp.recvspace=16777216
  persist:   /etc/sysctl.conf: net.inet.udp.recvspace = 16777216

Open files of the process (RLIMIT_NOFILE)
  current:   1048576 (ok)
  suggested: 65536
  why:       the configuration may hold about 1310 files and connections open at once; twice that, at least 65536

Open files of any process (kern.maxfilesperproc)
  current:   138240 (ok)
  suggested: 65536
  why:       the system's cap on the open files limit of one process

TCP listen queue (kern.ipc.somaxconn)
  current:   128
  suggested: 1024
  why:       the TCP listeners ask for 1024 waiting connections, and the system caps it here
  now:       sysctl -w kern.ipc.somaxconn=1024
  persist:   /etc/sysctl.conf: kern.ipc.somaxconn = 1024

RADIUS receive sockets ([radius] sockets)
  current:   1 (ok)
  suggested: 1
  why:       only Linux shares the packets of one port out over several sockets, so it must be 1 here; check refuses another value

These suggestions are unmeasured starting points: apply them, then confirm with the dropped packet counts above.
```

### 怎麼讀

- **開頭的丟包數最重要。** 它是開機以來，因為 socket 的接收緩衝區滿了而被核心丟掉的 UDP 封包數，整台機器所有程式加起來。是 0 表示目前的緩衝區夠用，下面的建議是為更高的負載準備的；不是 0，就照建議調高，然後觀察這個數字還會不會繼續增加。上面那台機器跑過很多壓測，所以數字很大。
- **每一項：** `current` 是現在的值，後面有 `(ok)` 表示已經夠了；`suggested` 是建議值；`why` 是理由；`now` 是馬上生效的指令；`persist` 是重開機後仍有效的寫法。已經夠的項目沒有 `now` 與 `persist`。
- **最後一行說得很清楚：** 這些建議沒有經過量測，是起點。調完要用丟包數與壓測確認。

### 建議值怎麼算

建議值從設定算出來，規則寫在 `rradius-core/src/tuning.rs`：

| 項目 | 算法 |
| --- | --- |
| 每個 RADIUS socket 的接收緩衝區 | `max_auth_inflight ÷ sockets`（無條件進位）× 4096 位元組，至少 4 MiB，再進位到整數 MiB。上面是 4096 ÷ 1 × 4096 ＝ 16 MiB |
| 開檔數 | 先估「可能同時開著的檔案與連線」：256 ＋ `tacacs.max_connections` ＋ `cluster.read_connections` ＋ 2 × 叢集節點數。上面是 256 ＋ 1024 ＋ 28 ＋ 2 ＝ 1310；建議值取它的兩倍，至少 65536 |
| TCP 的 listen queue | 有 TCP 監聽（例如 `[tacacs]`）時才檢查，建議 1024，也就是 rradius 向系統要的佇列長度 |

4096 位元組是一個 RADIUS 封包的最大長度，用來估計每個等待中的封包要占多少緩衝區。

### 各平台檢查的項目

| 檢查 | Linux | macOS | FreeBSD |
| --- | --- | --- | --- |
| UDP 丟包數的來源 | `/proc/net/snmp` 的 `RcvbufErrors` | `netstat -s -p udp` | `netstat -s -p udp` |
| 送不到 socket 就被丟的封包 | `/proc/net/softnet_stat` | — | — |
| 接收緩衝區（沒設 `recv_buffer_bytes`） | `net.core.rmem_default`、`net.core.rmem_max` | `kern.ipc.maxsockbuf`（建議值的兩倍）、`net.inet.udp.recvspace` | 同 macOS |
| 接收緩衝區（有設 `recv_buffer_bytes`） | `net.core.rmem_max` | `kern.ipc.maxsockbuf`（設定值的兩倍） | 同 macOS |
| 行程的開檔數 | `RLIMIT_NOFILE`；`persist` 寫 systemd 的 `LimitNOFILE` 或 `/etc/security/limits.conf` | `RLIMIT_NOFILE`；`persist` 寫 LaunchDaemon plist | `RLIMIT_NOFILE`；`persist` 寫 `/etc/login.conf` |
| 系統對單一行程的開檔上限 | — | `kern.maxfilesperproc` | `kern.maxfilesperproc` |
| TCP listen queue | `net.core.somaxconn` | `kern.ipc.somaxconn` | `kern.ipc.soacceptqueue` |
| 網卡的 backlog | `net.core.netdev_max_backlog`：有封包在送到 socket 之前就被丟時，建議 16384 | — | — |
| `radius.sockets` | 建議等於工作執行緒數（不要設） | 必須是 1 | 必須是 1 |
| `runtime.pin_threads` | 只顯示目前的值與說明 | — | — |

- Linux 的 sysctl 建議寫進 `/etc/sysctl.d/90-rradius.conf`；macOS 與 FreeBSD 寫進 `/etc/sysctl.conf`。
- 在 FreeBSD 上，輸出第二行會印 `(this advice follows the FreeBSD documentation and has not been tested on FreeBSD)`。

> **未實機驗證：** 本章只在 macOS 實際跑過 `tuning`。Linux 與 FreeBSD 的檢查項目，依據的是 `rradius-core/src/tuning.rs`，沒有在那兩個平台上執行過。

> **注意：** `tuning` 說 `pin_threads` 的那一行指向本章的[工作執行緒與 pin_threads](#工作執行緒與-pin_threads)。

## 接收 socket 與接收緩衝區

| 鍵 | 預設 | reload | 意思 |
| --- | --- | --- | --- |
| `radius.sockets` | Linux：工作執行緒數；其他平台：`1` | 要重啟 | 每個位址與埠開幾個接收 socket |
| `radius.recv_buffer_bytes` | 不設（用系統預設） | 要重啟 | 每個 RADIUS socket 的 `SO_RCVBUF`，4096 到 1073741824 位元組 |

完整的鍵表在[附錄 A](90-config-reference.md)。這兩個鍵屬於 `[radius]`，`[radius]` 的其他設定在[第 05 章](05-radius-clients.md)。

**`sockets`：** 在 Linux 上，同一個埠可以開好幾個 socket，由核心把封包分給它們，讓每個工作執行緒都能收。核心是依封包的來源分配的，**同一個來源的封包總是進同一個 socket**，所以只有一台 NAS 時，多開 socket 幫不上忙。（這是 Linux 核心的行為，本章沒有在 Linux 上實測。）其他平台不會這樣分配封包，所以只能是 1：

```console
$ rradiusd check --config s2.toml
s2.toml: radius.sockets = 2: only Linux shares UDP packets out over several sockets on one port, so it must be 1 here
```

`config show` 第一行會寫出這台機器算出來的值，例如 `# computed on this machine: radius.sockets = 1, cluster.read_connections = 28`。一般不要設它，讓它用預設值。

**`recv_buffer_bytes`：** 不設時，每個 socket 用系統的預設大小，所以要調的是系統值（`tuning` 會告訴你調哪個）。設了之後，rradius 自己向系統要這個大小，系統值只剩上限需要調。啟動時日誌會寫出實際拿到多少：

```text
… INFO rradius_core::radius: RADIUS: receive buffer 8388608 bytes on each socket (asked 8388608)
```

要的比系統上限大時，系統只給上限，日誌會警告：

```text
… INFO rradius_core::radius: RADIUS: receive buffer 8388608 bytes on each socket (asked 16777216)
… WARN rradius_core::radius: RADIUS: asked for a receive buffer of 16777216 bytes and each socket got 8388608; raise kern.ipc.maxsockbuf to at least 16777216
```

（這是在 `kern.ipc.maxsockbuf = 8388608` 的 macOS 上實測的。Linux 上對應的上限是 `net.core.rmem_max`。）

設了 `recv_buffer_bytes` 之後再跑 `tuning`，它會改成比較設定值和建議值：

```console
$ rradiusd tuning --config rb.toml
…
Receive buffer of each RADIUS socket ([radius] recv_buffer_bytes)
  current:   8388608
  suggested: 16777216
  why:       it should hold the socket's share of max_auth_inflight requests
  persist:   the configuration file: recv_buffer_bytes = 16777216 in [radius], then restart
…
```

## 工作執行緒與 pin_threads

rradius 的工作執行緒數等於這個行程能用的 CPU 數（Rust 標準庫 `available_parallelism` 的值），沒有設定可以改。

| 鍵 | 預設 | reload | 意思 |
| --- | --- | --- | --- |
| `runtime.pin_threads` | `false` | 要重啟 | 把每個工作執行緒各綁在一顆 CPU 上，只在 Linux 有作用 |

- 工作執行緒依序綁到行程能用的 CPU 上，CPU 比執行緒少時輪流使用。其中一個綁不上，就全部不綁。
- 之後才建立的其他執行緒不受影響，用的是行程原本能用的全部 CPU。
- 在 Linux 以外的平台，`check` 直接拒絕：

```console
$ rradiusd check --config pin.toml
pin.toml: runtime.pin_threads is only supported on Linux
```

適合開的情況：這台機器只跑 rradius，而且量過之後確實比較快。和其他服務共用 CPU 的機器，綁住執行緒反而可能讓它們搶同一顆 CPU。

> **未實機驗證：** `pin_threads = true` 只能在 Linux 上啟用，本章沒有在 Linux 上量過它的效果。

## 同時處理的上限

| 鍵 | 預設 | reload | 意思 |
| --- | --- | --- | --- |
| `radius.max_auth_inflight` | `4096` | 要重啟 | 同時處理的認證請求數，超過的丟棄 |
| `radius.max_acct_inflight` | `4096` | 要重啟 | 同時處理的計費請求數，超過的丟棄 |

「處理中」是從收下封包到送出回應，包括等資料庫、等 LDAP、等腳本的時間。上限滿了，新的封包直接丟掉、不回應，記在 `rradius_radius_discarded_total{reason="overload"}`。NAS 等不到回應會重送，通常過一會兒就能處理。

指標裡看得到目前的使用量與上限：`rradius_radius_requests_in_progress` 與 `rradius_radius_requests_limit`，`status` 的 RADIUS 區段也有 `Auth in progress`、`Auth limit` 這幾行。

**實測過載的樣子：** 把 `max_auth_inflight` 故意設成 `4`，再用 256 個同時在路上的請求去壓：

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret bench-secret \
    --load pap,concurrency=256 --duration 3 --warmup 1 --timeout 1 \
    --metrics-url http://127.0.0.1:9812/metrics
measured 3.000 s after 1.000 s of warmup

pap (fixed-concurrency): concurrency 256, 2 threads, 4 sockets, busiest worker 25%
  pap: 66869/s, sent 201362, accept 200606, reject 0, challenge 0, accounting 0, timeouts 756, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 43.9, p90 95.6, p99 134.9, p99.9 280.3, max 2056.2

server counters: auth 200396, acct 0, duplicates 0, discarded overload 632, unknown_client 0, invalid 0

warnings:
  server_discarded_overload: the server discarded 632 packets for overload
```

伺服器的日誌每秒一行：

```text
… WARN rradius_core::radius: RADIUS authentication overloaded: discarded 252 packets
```

- `timeouts 756`：這些請求被丟了，工具等了 `--timeout 1` 秒都沒有回應。
- `rradius-bench` 的結束碼是 2，因為有逾時。
- 速率看起來反而比較高、延遲比較低，因為被丟掉的請求不算在延遲裡。只看速率會誤判，一定要一起看逾時與伺服器的丟棄數。

**怎麼調：** 預設的 4096 對大部分情況都夠。真的看到 `overload`，先找出為什麼處理變慢（資料庫、LDAP、腳本），而不是先調高上限：後端慢的時候，調高上限只是讓更多請求一起等。要調高時，記得接收緩衝區的建議值也跟著變大（見上一節的算法），`tuning` 要重跑一次。

## 叢集資料庫的兩個設定

這兩個鍵的主章是[第 16 章](16-cluster.md)，這裡只說和效能的關係：

- `cluster.read_connections`：讀本機資料庫的連線數，預設是工作執行緒數的兩倍、最多 32（上面那台機器是 28），可設 1 到 128。驗證時查帳號、腳本讀資料都走它。它也算進 `tuning` 的開檔數。
- `cluster.snapshot_every`：兩次 snapshot 之間的 Raft 記錄筆數，預設 100000。寫入很多時，snapshot 的頻率會影響寫入的穩定度。

兩者改了都要重啟。

## rradius-bench 的用法

`rradius-bench` 是 repo 裡附的 RADIUS 與 TACACS+ 壓測工具。它有兩個子命令：`run` 對伺服器送負載，`calibrate` 量工具自己。完整的旗標見[附錄 B](91-command-reference.md)，或執行 `rradius-bench run --help`。

> **注意：** deb、rpm、FreeBSD 套件與 Docker 映像只裝 `rradiusd`，不含 `rradius-bench`，要從原始碼建置（第 02 章）。

### 負載的寫法

`--load` 可以給好幾次，每個負載用自己的執行緒同時跑，報告分開列：

```text
--load KIND[,concurrency=N][,rate=N][,window=N][,threads=N][,alive=N]
```

| 參數 | 意思 | 預設 |
| --- | --- | --- |
| `concurrency` | 固定並行：同時在路上的請求數，一個回來就補一個。和 `rate` 一起給時，是在路上的上限 | 有 `rate` 時 256 |
| `rate` | 固定速率：每秒送幾個，不管回應快慢 | 無 |
| `window` | 一個 socket 上同時在路上的請求數，1 到 256 | RADIUS 64，TACACS+ 1（只能是 1） |
| `threads` | 這個負載用幾個執行緒 | 固定並行 2，固定速率 1 |
| `alive` | 只有 `session`：Start 與 Stop 之間送幾個 Interim-Update | 0 |

`concurrency` 與 `rate` 至少要給一個。socket 數是 `concurrency ÷ window`（無條件進位），平均分給各執行緒，執行緒不會比 socket 多。

兩種模式量的東西不一樣：

- **固定並行**（只給 `concurrency`）：看伺服器最多能吞多少。延遲從實際送出算起。
- **固定速率**（給 `rate`）：看在某個負載下延遲多少。延遲從**排定**的送出時間算起，所以伺服器一卡住，被耽誤的每一個請求都會反映在延遲上。要回答「每秒 2 萬筆時 p99 多少」用這個。

| 種類 | 送什麼 |
| --- | --- |
| `pap`、`chap`、`chap-authenticator`、`mschapv2` | Access-Request，各種密碼驗證方式 |
| `acct-start`、`acct-alive`、`acct-stop`、`acct-on`、`acct-off` | Accounting-Request |
| `status-auth`、`status-acct` | Status-Server，送到認證埠或計費埠 |
| `session` | 一次登入、Start、`alive` 個 Interim-Update、Stop，共用同一個 session ID |
| `eap-tls`、`peap`、`ttls`、`ttls-mschapv2` | 完整的 EAP 對話（`ttls` 內層 PAP，`ttls-mschapv2` 內層 MS-CHAPv2）；`concurrency`、`rate` 數的是對話。`eap-tls` 要 `--cert-file`、`--key-file` |
| `tacacs-pap`、`tacacs-chap`、`tacacs-ascii`、`tacacs-author`、`tacacs-acct` | TACACS+，用 `--tacacs-port`；`--tacacs-connect keep`（預設，一條連線一直用）或 `per-request` |

> **企業版：** `aka-prime` 負載與 `--transport tls`（RadSec）要對企業版的伺服器才有用。

### 其他常用旗標

| 旗標 | 意思 |
| --- | --- |
| `--warmup`、`--duration` | 先熱身幾秒（預設 2），再量幾秒（預設 10）。熱身期間送的請求不算 |
| `--requests N` | 每個負載量 N 個就停，沒有熱身；`session` 數的是流程。和上面兩個不能一起用 |
| `--users N` | 使用者名稱輪流用 `<user>0` 到 `<user>N-1` |
| `--timeout`、`--retries` | 等回應幾秒（預設 5）、逾時後重送幾次（預設 0） |
| `--metrics-url` | 伺服器的 `http://主機:埠/metrics`，量測前後各讀一次，報告伺服器的計數變化 |
| `--json` | 改印一份 JSON 報告 |

### 結束碼

| 結束碼 | 意思 |
| --- | --- |
| 0 | 全部是 Accept（計費則全部有回應） |
| 1 | 有 Reject 或 Challenge，沒有逾時 |
| 2 | 有逾時、無效的回應或 socket 錯誤 |
| 3 | 參數錯誤或準備失敗，什麼都沒送 |

警告不影響結束碼。

## 先量工具自己：calibrate

`calibrate` 在工具自己的行程裡開幾個只會回 Accept 的假伺服器，用和 `run` 一樣的程式碼去壓，量出**這台機器上這個工具**的上限。拿 `run` 的結果和它比，就知道量到的是伺服器還是工具。

```console
$ rradius-bench calibrate --duration 3
what this tool can do here, against responders that do no other work:
  1 threads, window 1, concurrency 1: 44665/s, p50 18.3 us, p99 25.5 us, busiest worker 22%, timeouts 0
  1 threads, window 64, concurrency 64: 165450/s, p50 364.5 us, p99 634.9 us, busiest worker 80%, timeouts 0
  2 threads, window 64, concurrency 128: 215854/s, p50 495.6 us, p99 928.3 us, busiest worker 72%, timeouts 0
  4 threads, window 64, concurrency 256: 173938/s, p50 1430.5 us, p99 1944.6 us, busiest worker 47%, timeouts 0
```

- 固定跑四種組合：1 個執行緒、window 1；以及 window 64 搭配 1、2、4 個執行緒。`--duration` 是每種組合跑幾秒（預設 3）。
- `run` 量到的速率如果接近這裡同樣組合的數字，瓶頸可能在工具，不在伺服器。
- 這裡的數字是在 [量測範例與量測環境](#量測範例與量測環境) 那台機器上量的，換一台機器就不一樣。

## 用 benchmarks 的設定量一次

`rradius-server/benchmarks/` 有幾份專門給壓測用的設定：

| 檔案 | 量什麼 |
| --- | --- |
| `accept.toml`＋`accept.lua` | 腳本一律接受、計費一律確認：伺服器自己的處理路徑 |
| `acct-sql.toml`＋`acct-sql.lua` | 每筆計費寫一列到資料表（`migrations/`） |
| `acct-cpu.toml`＋`acct-cpu.lua` | 每筆計費讓腳本計算一段時間 |
| `eap.toml` | EAP-TLS、PEAP、TTLS，一個帳號 `bench`／`bench`（`eap-migrations/`）；憑證要自己做，放在 `eap-certs/` |
| `tacacs.toml` | TACACS+ 的認證、授權、計費一律接受，用不需權限的埠 4949 |

> **警告：** 這些檔案把共用密碼與叢集密碼直接寫在設定裡，而且接受所有人。只用在測試機上量測，不要拿來當正式設定的範本。

和情境一樣，不要在 repo 裡直接跑它們，先複製出來。設定裡的字典路徑是 `../../share/dictionary`，所以複製時保持兩層深度，並讓 `share` 指到字典的位置：

```console
$ mkdir -p ~/rradius-lab/bench/y && cd ~/rradius-lab/bench
$ ln -s <repo>/share share
$ cp -R <repo>/rradius-server/benchmarks y/
$ cd y/benchmarks
```

`<repo>` 換成原始碼 repo 的路徑。下面的指令都用預設埠（RADIUS 1812、1813，指標 9812）；`--auth-port`、`--acct-port` 寫出來只是讓你看清楚對的是哪個埠，用預設埠時可以拿掉。

```console
$ rradiusd serve --config accept.toml
```

另一個終端機：

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --acct-port 1813 --secret bench-secret \
    --load pap,concurrency=64 --metrics-url http://127.0.0.1:9812/metrics
measured 10.000 s after 2.000 s of warmup

pap (fixed-concurrency): concurrency 64, 1 threads, 1 sockets, busiest worker 53%
  pap: 56218/s, sent 562179, accept 562179, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 1079.3, p90 1452.0, p99 2443.3, p99.9 4329.5, max 387973.1

server counters: auth 561914, acct 0, duplicates 0, discarded overload 0, unknown_client 0, invalid 0
```

逐行看：

- `measured 10.000 s after 2.000 s of warmup`：熱身 2 秒後量 10 秒。
- 第二行：負載的模式、並行數、執行緒與 socket 數；`busiest worker 53%` 是最忙的那個執行緒有多少時間在工作（不是在等回應）。
- `pap: 56218/s`：量測期間每秒收到的回應數。後面是各種結果的計數：`invalid` 是驗不過的回應，`late` 是對應的請求已經不在路上了才到的回應。
- `latency us`：延遲的分位數，單位微秒。
- `server counters`：`--metrics-url` 讀到的伺服器計數，量測期間的增加量。`auth` 和 `sent` 不完全一樣，因為兩邊開始與結束計數的時刻不同，差一點是正常的。`duplicates`、`discarded` 應該都是 0。

用 `--json` 時，同樣的內容是一份 JSON，`schema` 是 1：

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret bench-secret \
    --load pap,concurrency=8 --duration 1 --warmup 0 --json \
    --metrics-url http://127.0.0.1:9812/metrics
{"schema":1,"warmup_s":0.0,"duration_s":1.0,"trustworthy":true,"loads":[{"kind":"pap","mode":"fixed-concurrency",…}],"server_metrics":{…},"warnings":[]}
```

（實際輸出是一整行；這裡刪節了。）每個負載有 `kind`、`mode`、`transport`、`concurrency`、`rate`、`threads`、`sockets`、`reconnects`、`send_lag_us`、`worker_busy_max` 與 `steps`；每個 step 有 `step`、`sent`、`responses`（`accept`、`reject`、`challenge`、`accounting`）、`timeouts`、`invalid`、`late`、`socket_errors`、`retransmits`、`throughput_per_s`、`rounds` 與 `latency_us`（`p50`、`p90`、`p99`、`p999`、`max`）。`server_metrics` 是 `--metrics-url` 讀到的計數變化（`packets`、`duplicates`、`discarded`），沒給 `--metrics-url` 時是 `null`。給腳本比較多次量測時用它。

## 量測範例與量測環境

下面這些數字**只是示範**：說明報告怎麼讀、不同的負載寫法會量到什麼，不能拿來推估你的機器。

**量測環境：**

| 項目 | 內容 |
| --- | --- |
| 機器 | Apple M4 Max，14 核（10 個效能核心＋4 個節能核心），記憶體 36 GiB |
| 作業系統 | macOS 27.2.1（26B131），arm64 |
| rradius | `rradiusd 0.1.0`，撰寫時核心 repo 的 commit `a261e4d`；使用預先建好的執行檔，建置參數未另行確認 |
| 伺服器與工具 | 同一台機器，經由 loopback（`127.0.0.1`） |
| 機器負載 | 與其他工作同時進行；量測期間 `uptime` 的一分鐘平均負載從約 2.4 升到約 8.5 |
| 日期 | 2026-11-23 |

**結果（與其他工作同時進行，僅供示範）：**

| 設定 | 負載 | 結果 |
| --- | --- | --- |
| `accept.toml` | `pap,concurrency=64` | 56218/s，p50 1.08 ms，p99 2.44 ms |
| `accept.toml` | `pap,concurrency=256`（2 執行緒、4 socket） | 57766/s，p50 4.39 ms，p99 5.55 ms |
| `accept.toml` | `pap,concurrency=8`，量 1 秒 | 80212/s，p50 0.09 ms，p99 0.17 ms |
| `accept.toml` | `pap,rate=20000`，量 5 秒 | 20000/s，p50 0.04 ms，p99 0.07 ms，send lag p99 20 μs |
| `accept.toml` | `acct-start,concurrency=256` 加上 `pap,rate=1000` 同時跑 | 計費 57260/s，p99 5.97 ms；同時的登入 1000/s，p99 0.88 ms |
| `tacacs.toml` | `tacacs-pap,concurrency=16`（16 條連線） | 106731/s，p50 0.15 ms，p99 0.22 ms |
| `eap.toml`（ECDSA P-256 的伺服器憑證） | `peap,concurrency=8` | 4948 個對話/s，每個對話 9 個來回，p99 1.86 ms |
| `eap.toml`（同上） | `ttls,concurrency=8` | 6409 個對話/s，每個對話 5 個來回，p99 1.44 ms；有 `tool_saturated` 警告 |

從這幾行看得出幾件事，這些才是值得帶走的：

- **並行數不是越大越好。** 64 到 256，速率幾乎沒變，延遲卻變成四倍：多出來的請求只是在排隊。並行數 8 的速率反而最高。要找的是「速率不再增加、延遲開始上升」的那個點。
- **固定速率才看得出某個負載下的延遲。** 每秒 20000 筆時 p99 只有 0.07 ms，比固定並行量到的低很多，因為沒有人為的排隊。
- **計費和登入同時跑時，看登入的延遲有沒有被拖累。** 這裡 1000/s 的登入 p99 是 0.88 ms，比單獨跑時高。
- **EAP 一個對話有好幾個來回**，`rounds per finished conversation` 就是來回數；速率數的是對話，不是封包。
- **TACACS+ 那一行的 `server counters: auth 0` 不是錯誤。** `--metrics-url` 只讀 RADIUS 的計數，TACACS+ 的請求要看伺服器的 `rradius_tacacs_requests_total`。

**為什麼這些數字不能直接比較：**

1. 工具和伺服器在同一台機器上，搶同一組 CPU，而且走 loopback，沒有真的網路。正式量測要用另一台機器送。
2. macOS 上 `radius.sockets` 只能是 1，所有 UDP 封包進同一個 socket；Linux 上預設每個工作執行緒一個 socket，結果會不一樣。
3. 量測時機器上還跑著其他工作，負載一直在變。同一條指令重跑，數字可能差很多。
4. `accept.toml` 的腳本什麼都不做。真實的設定要查資料庫、LDAP 或跑腳本，速率會低很多，延遲會高很多。
5. 每個設定只量了一次，沒有重複量測取平均。

要規劃容量，用你自己的設定、你自己的硬體、從另一台機器送，量多次，並同時看第 17 章的指標。

## 怎麼判斷結果可不可信

`rradius-bench` 會自己檢查幾種「量到的不是伺服器」的情況，在報告最後列出警告。其中三種會讓這次量測被標成不可信（JSON 的 `trustworthy` 是 `false`，文字報告最後印 `these numbers do not describe the server alone: see the warnings`）：

| 警告 | 什麼時候 | 不可信 |
| --- | --- | --- |
| `tool_saturated` | 固定並行的某個執行緒在量測期間至少 90% 的時間都在工作 | 是 |
| `rate_not_met` | 固定速率送出的請求不到排定的 99%，或 send lag 的 p99 超過 1000 微秒 | 是 |
| `duplicate_path` | `--retries 0` 時伺服器的重送計數增加，請求被當成了重送 | 是 |
| `server_discarded_<reason>` | 伺服器在量測期間丟掉了封包，例如 `server_discarded_overload` | 否，但要看 |
| `metrics_unavailable` | `--metrics-url` 讀不到 | 否 |

實測兩個例子。速率設得比工具送得出來的高：

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret bench-secret \
    --load pap,rate=500000 --duration 2 --warmup 1 --metrics-url http://127.0.0.1:9812/metrics
…
warnings:
  rate_not_met: pap: sent 0 of 1000000 scheduled requests, send lag p99 0.0 us; the tool or the server could not keep up with this rate, so lower rate or raise concurrency and threads

these numbers do not describe the server alone: see the warnings
```

`--metrics-url` 指到沒有服務的埠（這裡故意用 9899）：

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret bench-secret \
    --load pap,concurrency=1 --requests 5 --metrics-url http://127.0.0.1:9899/metrics
…
warnings:
  metrics_unavailable: could not read the server's metrics: Connection refused (os error 61)
```

除了這些警告，再自己檢查：

- `sent` 與各種回應的總數對得上，`timeouts`、`invalid`、`socket errors` 都是 0。
- 伺服器的 `duplicates`、`discarded` 沒有增加。
- 量測期間用 `rradiusd status --watch` 看伺服器：`Auth in progress` 有沒有接近上限，p99 和工具量到的差多少。
- 跑一次 `tuning`，看開頭的 UDP 丟包數在量測後有沒有增加。有增加，表示有封包在 rradius 看到之前就被丟了。

## 常見錯誤

**症狀：** `rradius-bench` 印出 `rradius-bench: --load pap: needs concurrency or rate`，結束碼 3。
**原因：** `--load` 只寫了種類。
**解法：** 加上 `concurrency=` 或 `rate=`。

**症狀：** TACACS+ 負載印出 `window must be 1 for a TACACS+ load, which has one request at a time on each connection`，結束碼 3。
**原因：** TACACS+ 一條連線一次只能有一個請求。
**解法：** 拿掉 `window`，用 `concurrency` 決定連線數。

**症狀：** `check` 印出 `radius.sockets = 2: only Linux shares UDP packets out over several sockets on one port, so it must be 1 here`。
**原因：** 在 Linux 以外的平台設了 `sockets` 大於 1。
**解法：** 拿掉 `sockets`。

**症狀：** `check` 印出 `runtime.pin_threads is only supported on Linux`。
**原因：** 在 Linux 以外的平台設了 `pin_threads = true`。
**解法：** 拿掉它。

**症狀：** `check` 印出 `radius.recv_buffer_bytes = 100: it must be from 4096 to 1073741824`。
**原因：** 值超出範圍。
**解法：** 改成範圍內的值，或拿掉它用系統預設。

**症狀：** 啟動時日誌有 `asked for a receive buffer of 16777216 bytes and each socket got 8388608; raise kern.ipc.maxsockbuf to at least 16777216`。
**原因：** 系統的上限比 `recv_buffer_bytes` 小，系統只給了上限。
**解法：** 照訊息調高系統上限（Linux 是 `net.core.rmem_max`），再重新啟動。

**症狀：** 量到的速率接近 `calibrate` 的數字，或報告有 `tool_saturated`。
**原因：** 工具成了瓶頸。
**解法：** 加 `threads=`，或從另一台、或多台機器同時送。

**症狀：** 報告有 `server_discarded_overload`，`timeouts` 不是 0，結束碼 2。
**原因：** 伺服器處理中的請求達到上限（見[同時處理的上限](#同時處理的上限)）。
**解法：** 先降低並行數找出伺服器的極限；正式環境出現時，先查後端為什麼變慢。

**症狀：** 報告有 `duplicate_path`。
**原因：** 伺服器把工具的請求當成重送，量到的不是正常的處理路徑。
**解法：** 確認 `--retries` 是 0，而且沒有其他程式用同一組來源位址送一樣的請求。

## 相關章節

- [02 安裝](02-install.md)
- [03 第一次啟動與登入](03-quick-start.md)
- [05 RADIUS 服務與 NAS 用戶端](05-radius-clients.md)
- [11 802.1X 與 EAP](11-eap.md)
- [12 TACACS+ 設備管理](12-tacacs.md)
- [16 叢集與高可用](16-cluster.md)
- [17 監控、狀態與日誌](17-monitoring.md)
- [21 排錯](21-troubleshooting.md)
- [附錄 A：設定參考](90-config-reference.md)
- [附錄 B：命令參考](91-command-reference.md)
- [附錄 C：指標參考](92-metrics-reference.md)
