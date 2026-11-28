# 19 Performance tuning and load testing

> After this chapter you can:
> - Use `tuning` to find the operating system limits that should be raised, and know how the suggested values are calculated
> - Set the receive sockets, receive buffers, worker threads and the limits on concurrent requests
> - Use `rradius-bench` to measure the capacity of your own machine, and judge whether the numbers you get can be trusted
> - Repeat a measurement with the `benchmarks/` configurations that ship in the repo

Prerequisites: you have finished [Chapter 03](03-quick-start.md) and can send a login with `rradius-bench`; you have read the metrics and `status` in [Chapter 17](17-monitoring.md).

In this chapter:

- [Concept: where a request gets stuck](#concept-where-a-request-gets-stuck)
- [Check the operating system with tuning](#check-the-operating-system-with-tuning)
- [Receive sockets and receive buffers](#receive-sockets-and-receive-buffers)
- [Worker threads and pin_threads](#worker-threads-and-pin_threads)
- [Limits on concurrent requests](#limits-on-concurrent-requests)
- [Two cluster database settings](#two-cluster-database-settings)
- [How to use rradius-bench](#how-to-use-rradius-bench)
- [Measure the tool itself first: calibrate](#measure-the-tool-itself-first-calibrate)
- [Measure once with the benchmarks configurations](#measure-once-with-the-benchmarks-configurations)
- [Example measurements and test environment](#example-measurements-and-test-environment)
- [How to tell whether a result can be trusted](#how-to-tell-whether-a-result-can-be-trusted)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Concept: where a request gets stuck

A RADIUS request goes from the NAS to the server and back. On the way it passes several places that can clog. Where it clogs decides what you see and what you tune:

| Place | What you see when it clogs | Where to look | What to tune |
| --- | --- | --- | --- |
| The operating system's UDP receive buffer | The kernel drops packets before rradius sees them, the NAS times out, and the rradius metrics record nothing | "UDP packets dropped" at the top of `tuning` | The receive buffer, `radius.sockets` |
| rradius's limit on concurrent requests | `rradius_radius_discarded_total{reason="overload"}` grows, and the log has `overloaded: discarded … packets` | `status`, metrics | `max_auth_inflight`, `max_acct_inflight`, or find out why the back end is slow |
| Back end: database, LDAP, script | Response time (p99) gets longer, and more requests are in progress | p99 and in progress in `status` | The timeouts and connection counts in chapters 07 to 10 |
| The measuring tool itself | The tool's CPU is full, so the numbers reflect the tool, not the server | `busiest worker` and the warnings of `rradius-bench` | Use more threads, or send from another machine |

The order of tuning: first use `tuning` to raise the operating system limits until they are enough, then measure with `rradius-bench` while you watch the server's metrics. Do not start by changing the rradius configuration.

> **Note:** This chapter gives no figure for "how many requests per second rradius handles". That depends on your hardware, operating system, auth chain and database. The numbers in [Example measurements](#example-measurements-and-test-environment) only show how to read the output. Do not use them to plan capacity.

## Check the operating system with tuning

`tuning` reads the system values of this machine and your configuration file, and tells you which limits to raise, to what value, and how. It only reads: it changes no system setting, starts no node and opens no port.

```console
$ rradiusd tuning --config rradius.toml
```

The usage is `rradiusd tuning [--config <path>]`; `--config` is optional. The configuration file is only used to make the advice fit your settings (for example `recv_buffer_bytes`, `sockets` and the connection limits). Without it, `tuning` reads the default path (the second line of `rradiusd --version`, decided by the prefix at build time, normally `/etc/rradius/rradius.toml`, and `/usr/local/etc/rradius/rradius.toml` for the FreeBSD port). If it cannot read that file, it prints one line saying so and calculates with all defaults; the system-level advice is still given. If you give `--config`, the file must be readable. The exit code is 2 for a bad argument or a bad configuration file you named, and 0 otherwise. It has advice for three platforms: Linux, macOS and FreeBSD. On other platforms it prints one line, "no advice for this platform".

Below is the real output on macOS for the lab configuration of Chapter 17:

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

### How to read it

- **The dropped packet count at the top matters most.** It is the number of UDP packets the kernel has dropped since boot because a socket's receive buffer was full, summed over every program on the machine. If it is 0, the current buffers are enough, and the advice below is for higher load. If it is not 0, raise the limits as advised and then watch whether the number keeps growing. The machine above has run many load tests, so its number is large.
- **Each item:** `current` is the value now, and `(ok)` after it means it is already enough; `suggested` is the suggested value; `why` is the reason; `now` is the command that takes effect at once; `persist` is how to make it last after a reboot. An item that is already enough has no `now` or `persist`.
- **The last line says it clearly:** these suggestions are not measured; they are a starting point. After you apply them, confirm with the dropped packet count and a load test.

### How the suggested values are calculated

The suggested values are calculated from the configuration. The rules are in `rradius-core/src/tuning.rs`:

| Item | Calculation |
| --- | --- |
| Receive buffer of each RADIUS socket | `max_auth_inflight ÷ sockets` (rounded up) × 4096 bytes, at least 4 MiB, then rounded up to a whole MiB. Above, that is 4096 ÷ 1 × 4096 = 16 MiB |
| Open files | First estimate the "files and connections that may be open at once": 256 + `tacacs.max_connections` + `cluster.read_connections` + 2 × the number of cluster nodes. Above, that is 256 + 1024 + 28 + 2 = 1310. The suggested value is twice that, at least 65536 |
| TCP listen queue | Checked only when there is a TCP listener (for example `[tacacs]`). The suggestion is 1024, which is the queue length rradius asks the system for |

4096 bytes is the maximum length of one RADIUS packet. It is used to estimate how much buffer each waiting packet takes.

### What is checked on each platform

| Check | Linux | macOS | FreeBSD |
| --- | --- | --- | --- |
| Source of the UDP drop count | `RcvbufErrors` in `/proc/net/snmp` | `netstat -s -p udp` | `netstat -s -p udp` |
| Packets dropped before they reach a socket | `/proc/net/softnet_stat` | — | — |
| Receive buffer (`recv_buffer_bytes` not set) | `net.core.rmem_default`, `net.core.rmem_max` | `kern.ipc.maxsockbuf` (twice the suggested value), `net.inet.udp.recvspace` | Same as macOS |
| Receive buffer (`recv_buffer_bytes` set) | `net.core.rmem_max` | `kern.ipc.maxsockbuf` (twice the configured value) | Same as macOS |
| Open files of the process | `RLIMIT_NOFILE`; `persist` gives the systemd `LimitNOFILE` or `/etc/security/limits.conf` | `RLIMIT_NOFILE`; `persist` gives a LaunchDaemon plist | `RLIMIT_NOFILE`; `persist` gives `/etc/login.conf` |
| System limit on open files of one process | — | `kern.maxfilesperproc` | `kern.maxfilesperproc` |
| TCP listen queue | `net.core.somaxconn` | `kern.ipc.somaxconn` | `kern.ipc.soacceptqueue` |
| Backlog of the network card | `net.core.netdev_max_backlog`: 16384 is suggested when packets are dropped before they reach a socket | — | — |
| `radius.sockets` | Suggested equal to the number of worker threads (do not set it) | Must be 1 | Must be 1 |
| `runtime.pin_threads` | Only shows the current value and a note | — | Same as Linux |

- On Linux, the sysctl advice goes in `/etc/sysctl.d/90-rradius.conf`; on macOS and FreeBSD it goes in `/etc/sysctl.conf`.
- On FreeBSD, the second line of the output reads `(this advice follows the FreeBSD documentation and has not been tested on FreeBSD)`.

> **Not verified on real hardware:** This chapter ran `tuning` only on macOS and on one FreeBSD 15.1 (arm64) virtual machine. The configuration on the virtual machine had no TCP listener, so the listen queue check was not run. Linux was not run; its checks follow `rradius-core/src/tuning.rs`.

> **Note:** The line in which `tuning` talks about `pin_threads` points to [Worker threads and pin_threads](#worker-threads-and-pin_threads) in this chapter.

## Receive sockets and receive buffers

| Key | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `radius.sockets` | Linux: the number of worker threads; other platforms: `1` | Restart needed | How many receive sockets to open for each address and port |
| `radius.recv_buffer_bytes` | Not set (use the system default) | Restart needed | `SO_RCVBUF` of each RADIUS socket, 4096 to 1073741824 bytes |

The full table of keys is in [Appendix A](90-config-reference.md). These two keys belong to `[radius]`; the other `[radius]` settings are in [Chapter 05](05-radius-clients.md).

**`sockets`:** On Linux, several sockets can listen on the same port, and the kernel shares the packets out among them so that every worker thread can receive. The kernel chooses by the source of the packet, so **packets from one source always go to the same socket**. With only one NAS, more sockets do not help. (This is the behavior of the Linux kernel; this chapter did not measure it on Linux.) Other platforms do not share packets out this way, so the value can only be 1:

```console
$ rradiusd check --config s2.toml
s2.toml: radius.sockets = 2: only Linux shares UDP packets out over several sockets on one port, so it must be 1 here
```

The first line of `config show` gives the value calculated for this machine, for example `# computed on this machine: radius.sockets = 1, cluster.read_connections = 28`. Normally do not set it; leave the default.

**`recv_buffer_bytes`:** When it is not set, each socket uses the system's default size, so the value to tune is a system value (`tuning` tells you which one). When it is set, rradius asks the system for this size itself, and only the system limit needs raising. At startup, the log says how much it really got:

```text
… INFO rradius_core::radius: RADIUS: receive buffer 8388608 bytes on each socket (asked 8388608)
```

When you ask for more than the system limit, the system gives only the limit, and the log warns:

```text
… INFO rradius_core::radius: RADIUS: receive buffer 8388608 bytes on each socket (asked 16777216)
… WARN rradius_core::radius: RADIUS: asked for a receive buffer of 16777216 bytes and each socket got 8388608; raise kern.ipc.maxsockbuf to at least 16777216
```

(This was measured on macOS with `kern.ipc.maxsockbuf = 8388608`. The matching limit on Linux is `net.core.rmem_max`.)

If you run `tuning` after you set `recv_buffer_bytes`, it compares the configured value with the suggested value instead:

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

## Worker threads and pin_threads

The number of rradius worker threads equals the number of CPUs the process can use (the value of `available_parallelism` in the Rust standard library). There is no setting to change it.

| Key | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `runtime.pin_threads` | `false` | Restart needed | Pin each worker thread to its own CPU; works only on Linux and FreeBSD |

- Worker threads are pinned in order to the CPUs the process can use, and when there are fewer CPUs than threads, they take turns. If one of them cannot be pinned, none are pinned.
- Other threads created later are not affected. They use all the CPUs the process could use originally.
- On platforms other than Linux and FreeBSD, `check` refuses it:

```console
$ rradiusd check --config pin.toml
pin.toml: runtime.pin_threads is only supported on Linux and FreeBSD
```

Turn it on when this machine runs only rradius and you have measured that it is faster. On a machine that shares CPUs with other services, pinning threads can make them compete for the same CPU.

Under Linux, to keep rradius on a fixed set of CPUs, for example apart from other services, restrict the whole process: `taskset -c 0-3 rradiusd serve --config rradius.toml`, or `CPUAffinity=0-3` under `[Service]` of the systemd unit. The number of worker threads and the limit on simultaneous checks of slow password hashes (`{crypt}`, `{argon2}` and the PBKDF2 formats) both equal the number of CPUs allowed. A change needs a restart.

### Check that the threads are pinned

The startup log says which CPUs the threads were pinned to. For example, when started on FreeBSD with `cpuset -l 0-3`:

```text
… INFO rradius_core::cli: pinned 4 runtime worker threads to CPUs [0, 1, 2, 3]
```

On FreeBSD you can also look with `procstat -S <pid>`: the `CPU MASK` column of each of the 4 worker threads holds a single CPU, and the other threads still show `0-3`. The name `tokio-rt-worker` is not used only by worker threads. Threads created later can have the same name, and their `CPU MASK` is all the CPUs the process could use originally.

### Measurements on a FreeBSD virtual machine

A comparison with `pin_threads` off and on, on one virtual machine, gave **mixed results**.

| Item | Content |
| --- | --- |
| Virtual machine | UTM (QEMU with hardware virtualization) on macOS on an Apple M4 Max, 8 virtual CPUs, 8 GiB of memory |
| Operating system | FreeBSD 15.1-RELEASE-p6, arm64 |
| rradius | Commit `f805cbe` of the core repo plus the change that adds pinning on FreeBSD, a release build with Rust 1.98.1 |
| Server | `benchmarks/accept.toml`, started with `cpuset -l 0-3` (4 worker threads), output sent to `/dev/null` |
| Tool | The same virtual machine, started with `cpuset -l 4-7`, over loopback |
| Method | 3 seconds of warm-up and 15 seconds of measurement; off and on runs interleaved, 10 pairs for each load |
| System settings | The receive buffer was the system default (`net.inet.udp.recvspace` of 42080) |
| Date | 2026-11-27 |

"Off" and "On" are medians of 10 runs each. "Change" is how much "on" differs from "off" within each pair, and then the median of those, so it is not always the ratio of the two columns before it. The last column counts only the 3 pairs during which the Mac had the least other work.

| Load | Measure | Off | On | Change (10 pairs) | Change (3 quietest pairs) |
| --- | --- | --- | --- | --- | --- |
| `pap,concurrency=64,window=1` | Responses per second | 244206 | 231774 | −7.4% | −10.3% |
| Same | p99 | 0.70 ms | 0.82 ms | +21.3% | +35.5% |
| `pap,concurrency=1024` | Responses per second | 271520 | 320678 | +15.2% | +15.6% |
| Same | p99 | 3.62 ms | 3.18 ms | −10.5% | −11.8% |
| `pap,rate=50000` | p50 | 43 μs | 44 μs | +0.5% | +0.1% |
| Same | p99 | 140 μs | 149 μs | +2.7% | −1.6% |

- **Concurrency 64: worse with pinning.** The p99 was higher in 9 of the 10 pairs.
- **Concurrency 1024: better with pinning.** The rate and the p99 both improved in 8 of the 10 pairs.
- **A fixed 50000 per second: no visible difference.** Two runs with pinning did not keep up this rate (a `rate_not_met` warning) and had a p99 of 7.2 and 14.3 ms, while the Mac was compiling other programs.

**Limits of these numbers:**

1. The virtual CPUs were not fixed to physical cores. A worker thread is pinned to a virtual CPU, which can still move between physical cores underneath.
2. Other work ran on the Mac during the measurements, and the rate went up and down both with and without pinning (for example about 320000 or 230000 per second at concurrency 64). A "quietest" pair is one where, in the samples of both runs, the processes other than the virtual machine together used no more than 60% of one core.
3. At concurrency 1024, every run had between 1766 and 2128 timeouts (`rradius-bench` exit code 2), with and without pinning alike. By the standard in [How to tell whether a result can be trusted](#how-to-tell-whether-a-result-can-be-trusted), a run with timeouts is not a clean one.
4. The tool and the server were on the same virtual machine, with no real network.
5. Do not compare them with Linux numbers: on FreeBSD `radius.sockets` can only be 1, while on Linux the default is one socket per worker thread.

Before you turn it on under FreeBSD, measure with your own load.

> **Not verified on real hardware:** The numbers above come from a virtual machine. This chapter did not measure on a physical FreeBSD machine, and has no Linux measurements. [`usage.md`](../usage.md) records numbers from a Linux virtual machine (Docker), which is a different environment.

## Limits on concurrent requests

| Key | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `radius.max_auth_inflight` | `4096` | Restart needed | The number of authentication requests handled at the same time; requests over it are dropped |
| `radius.max_acct_inflight` | `4096` | Restart needed | The number of accounting requests handled at the same time; requests over it are dropped |

"In progress" runs from taking in the packet to sending the response, and includes the time spent waiting for the database, LDAP and scripts. When the limit is full, new packets are dropped without a response and counted in `rradius_radius_discarded_total{reason="overload"}`. The NAS retransmits when it gets no response, and usually the request can be handled a moment later.

The metrics show the current use and the limit: `rradius_radius_requests_in_progress` and `rradius_radius_requests_limit`. The RADIUS section of `status` also has the lines `Auth in progress` and `Auth limit`.

**What overload looks like in a test:** Set `max_auth_inflight` to `4` on purpose, then load it with 256 requests in flight at the same time:

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

The server log gets one line per second:

```text
… WARN rradius_core::radius: RADIUS authentication overloaded: discarded 252 packets
```

- `timeouts 756`: these requests were dropped, and the tool waited `--timeout 1` second without a response.
- The exit code of `rradius-bench` is 2, because there were timeouts.
- The rate looks higher and the latency lower, because dropped requests are not counted in the latency. Looking only at the rate misleads you. Always read the timeouts and the server's discard count together with it.

**How to tune it:** The default of 4096 is enough for most cases. If you do see `overload`, first find out why handling got slow (database, LDAP, script) instead of raising the limit first. When the back end is slow, a higher limit only lets more requests wait together. When you do raise it, remember that the suggested receive buffer grows too (see the calculation in the previous section), so run `tuning` again.

## Two cluster database settings

The main chapter for these two keys is [Chapter 16](16-cluster.md). Here we only say how they relate to performance:

- `cluster.read_connections`: the number of connections that read the local database. The default is twice the number of worker threads, at most 32 (28 on the machine above), and you can set 1 to 128. Account lookups during authentication and data reads by scripts use them. They also count in the open files of `tuning`.
- `cluster.snapshot_every`: the number of Raft log records between two snapshots, 100000 by default. When there are many writes, how often snapshots happen affects how steady the writes are.

Changing either needs a restart.

## How to use rradius-bench

`rradius-bench` is the RADIUS and TACACS+ load testing tool that ships in the repo. It has two subcommands: `run` sends load to a server, and `calibrate` measures the tool itself. The full list of flags is in [Appendix B](91-command-reference.md), or run `rradius-bench run --help`.

> **Note:** The deb, rpm and FreeBSD packages and the Docker image install only `rradiusd`, not `rradius-bench`. You have to build it from source (Chapter 02).

### How to write a load

You can give `--load` several times. Each load runs at the same time on its own threads, and the report lists them separately:

```text
--load KIND[,concurrency=N][,rate=N][,window=N][,threads=N][,alive=N]
```

| Parameter | Meaning | Default |
| --- | --- | --- |
| `concurrency` | Fixed concurrency: the number of requests in flight at the same time; when one comes back, another is sent. Given together with `rate`, it is the upper limit in flight | 256 when `rate` is given |
| `rate` | Fixed rate: how many to send per second, however fast the responses come | None |
| `window` | The number of requests in flight at the same time on one socket, 1 to 256 | RADIUS 64, TACACS+ 1 (can only be 1) |
| `threads` | How many threads this load uses | Fixed concurrency 2, fixed rate 1 |
| `alive` | `session` only: how many Interim-Updates to send between Start and Stop | 0 |

You must give at least one of `concurrency` and `rate`. The number of sockets is `concurrency ÷ window` (rounded up), shared out evenly among the threads, and there are never more threads than sockets.

The two modes measure different things:

- **Fixed concurrency** (only `concurrency` given): shows the most the server can take. Latency is counted from the actual send.
- **Fixed rate** (`rate` given): shows how much latency there is under a given load. Latency is counted from the **scheduled** send time, so when the server stalls, every delayed request shows up in the latency. Use this to answer "what is p99 at 20000 per second".

| Kind | What it sends |
| --- | --- |
| `pap`, `chap`, `chap-authenticator`, `mschapv2` | Access-Request, with each password method |
| `acct-start`, `acct-alive`, `acct-stop`, `acct-on`, `acct-off` | Accounting-Request |
| `status-auth`, `status-acct` | Status-Server, sent to the authentication port or the accounting port |
| `session` | One login, Start, `alive` Interim-Updates, Stop, all sharing the same session ID |
| `eap-tls`, `peap`, `ttls`, `ttls-mschapv2` | A full EAP conversation (`ttls` with inner PAP, `ttls-mschapv2` with inner MS-CHAPv2); `concurrency` and `rate` count conversations. `eap-tls` needs `--cert-file` and `--key-file` |
| `tacacs-pap`, `tacacs-chap`, `tacacs-ascii`, `tacacs-author`, `tacacs-acct` | TACACS+, using `--tacacs-port`; `--tacacs-connect keep` (default, one connection used all the time) or `per-request` |

> **Enterprise edition:** The `aka-prime` load and `--transport tls` (RadSec) are only useful against a server of the enterprise edition.

### Other common flags

| Flag | Meaning |
| --- | --- |
| `--warmup`, `--duration` | Warm up for some seconds first (default 2), then measure for some seconds (default 10). Requests sent during warm-up are not counted |
| `--requests N` | Stop after N have been measured for each load, with no warm-up; for `session` it counts flows. It cannot be used together with the two flags above |
| `--users N` | User names take turns from `<user>0` to `<user>N-1` |
| `--timeout`, `--retries` | How many seconds to wait for a response (default 5), and how many times to retransmit after a timeout (default 0) |
| `--metrics-url` | The server's `http://host:port/metrics`, read once before and once after the measurement, so the report shows how the server's counters changed |
| `--json` | Print a JSON report instead |

### Exit codes

| Exit code | Meaning |
| --- | --- |
| 0 | All were Accept (for accounting, all got a response) |
| 1 | There were Reject or Challenge, and no timeouts |
| 2 | There were timeouts, invalid responses or socket errors |
| 3 | Bad arguments or setup failed, and nothing was sent |

Warnings do not change the exit code.

## Measure the tool itself first: calibrate

`calibrate` starts a few fake servers that answer only Accept inside the tool's own process, and loads them with the same code as `run`. It measures the upper limit of **this tool on this machine**. Compare the result of `run` with it to know whether you measured the server or the tool.

```console
$ rradius-bench calibrate --duration 3
what this tool can do here, against responders that do no other work:
  1 threads, window 1, concurrency 1: 44665/s, p50 18.3 us, p99 25.5 us, busiest worker 22%, timeouts 0
  1 threads, window 64, concurrency 64: 165450/s, p50 364.5 us, p99 634.9 us, busiest worker 80%, timeouts 0
  2 threads, window 64, concurrency 128: 215854/s, p50 495.6 us, p99 928.3 us, busiest worker 72%, timeouts 0
  4 threads, window 64, concurrency 256: 173938/s, p50 1430.5 us, p99 1944.6 us, busiest worker 47%, timeouts 0
```

- It always runs four combinations: 1 thread with window 1, and window 64 with 1, 2 and 4 threads. `--duration` is how many seconds each combination runs (default 3).
- If the rate `run` measures is close to the number here for the same combination, the bottleneck may be the tool, not the server.
- The numbers here were measured on the machine in [Example measurements and test environment](#example-measurements-and-test-environment). They will differ on another machine.

## Measure once with the benchmarks configurations

`rradius-server/benchmarks/` has a few configurations made for load testing:

| File | What it measures |
| --- | --- |
| `accept.toml` + `accept.lua` | The script always accepts and accounting is always acknowledged: the server's own processing path |
| `acct-sql.toml` + `acct-sql.lua` | Each accounting request writes one row to a table (`migrations/`) |
| `acct-cpu.toml` + `acct-cpu.lua` | Each accounting request makes the script compute for a while |
| `eap.toml` | EAP-TLS, PEAP, TTLS, with one account `bench`/`bench` (`eap-migrations/`); you make the certificates yourself and put them in `eap-certs/` |
| `tacacs.toml` | TACACS+ authentication, authorization and accounting are always accepted, using port 4949, which needs no privilege |

> **Warning:** These files write the shared secret and the cluster secret directly into the configuration, and they accept everyone. Use them only for measurements on a test machine. Do not use them as a template for a production configuration.

As with the scenarios, do not run them directly inside the repo; copy them out first. The dictionary path in the configuration is `../../share/dictionary`, so keep two levels of depth when you copy, and make `share` point to where the dictionaries are:

```console
$ mkdir -p ~/rradius-lab/bench/y && cd ~/rradius-lab/bench
$ ln -s <repo>/share share
$ cp -R <repo>/rradius-server/benchmarks y/
$ cd y/benchmarks
```

Replace `<repo>` with the path of the source repo. The commands below all use the default ports (RADIUS 1812 and 1813, metrics 9812); `--auth-port` and `--acct-port` are written out only so you can see which port each one targets, and you can remove them when you use the default ports.

```console
$ rradiusd serve --config accept.toml
```

In another terminal:

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --acct-port 1813 --secret bench-secret \
    --load pap,concurrency=64 --metrics-url http://127.0.0.1:9812/metrics
measured 10.000 s after 2.000 s of warmup

pap (fixed-concurrency): concurrency 64, 1 threads, 1 sockets, busiest worker 53%
  pap: 56218/s, sent 562179, accept 562179, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 1079.3, p90 1452.0, p99 2443.3, p99.9 4329.5, max 387973.1

server counters: auth 561914, acct 0, duplicates 0, discarded overload 0, unknown_client 0, invalid 0
```

Line by line:

- `measured 10.000 s after 2.000 s of warmup`: it measured for 10 seconds after 2 seconds of warm-up.
- The second line: the mode of the load, the concurrency, and the numbers of threads and sockets; `busiest worker 53%` is how much of the time the busiest thread was working (not waiting for a response).
- `pap: 56218/s`: the number of responses received per second during the measurement. After it are the counts of each result: `invalid` is a response that failed verification, and `late` is a response that arrived after its request was no longer in flight.
- `latency us`: the quantiles of latency, in microseconds.
- `server counters`: the server counters read through `--metrics-url`, as the increase during the measurement. `auth` and `sent` are not exactly the same, because the two sides start and stop counting at slightly different moments, so a small difference is normal. `duplicates` and `discarded` should both be 0.

With `--json`, the same content is one JSON document, with `schema` 1:

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret bench-secret \
    --load pap,concurrency=8 --duration 1 --warmup 0 --json \
    --metrics-url http://127.0.0.1:9812/metrics
{"schema":1,"warmup_s":0.0,"duration_s":1.0,"trustworthy":true,"loads":[{"kind":"pap","mode":"fixed-concurrency",…}],"server_metrics":{…},"warnings":[]}
```

(The real output is one single line; it is shortened here.) Each load has `kind`, `mode`, `transport`, `concurrency`, `rate`, `threads`, `sockets`, `reconnects`, `send_lag_us`, `worker_busy_max` and `steps`; each step has `step`, `sent`, `responses` (`accept`, `reject`, `challenge`, `accounting`), `timeouts`, `invalid`, `late`, `socket_errors`, `retransmits`, `throughput_per_s`, `rounds` and `latency_us` (`p50`, `p90`, `p99`, `p999`, `max`). `server_metrics` is the change in the counters read through `--metrics-url` (`packets`, `duplicates`, `discarded`), and it is `null` when `--metrics-url` is not given. Use it when a script compares several measurements.

## Example measurements and test environment

The numbers below are **only examples**. They show how to read a report and what different ways of writing a load measure. Do not use them to estimate your machine.

**Test environment:**

| Item | Content |
| --- | --- |
| Machine | Apple M4 Max, 14 cores (10 performance cores + 4 efficiency cores), 36 GiB of memory |
| Operating system | macOS 27.2.1 (26B131), arm64 |
| rradius | `rradiusd 0.1.0`, commit `a261e4d` of the core repo at the time of writing; a pre-built executable was used, and the build options were not checked separately |
| Server and tool | The same machine, over loopback (`127.0.0.1`) |
| Machine load | Running at the same time as other work; during the measurements the one-minute load average in `uptime` rose from about 2.4 to about 8.5 |
| Date | 2026-11-23 |

**Results (running at the same time as other work, for illustration only):**

| Configuration | Load | Result |
| --- | --- | --- |
| `accept.toml` | `pap,concurrency=64` | 56218/s, p50 1.08 ms, p99 2.44 ms |
| `accept.toml` | `pap,concurrency=256` (2 threads, 4 sockets) | 57766/s, p50 4.39 ms, p99 5.55 ms |
| `accept.toml` | `pap,concurrency=8`, measured for 1 second | 80212/s, p50 0.09 ms, p99 0.17 ms |
| `accept.toml` | `pap,rate=20000`, measured for 5 seconds | 20000/s, p50 0.04 ms, p99 0.07 ms, send lag p99 20 μs |
| `accept.toml` | `acct-start,concurrency=256` and `pap,rate=1000` running together | Accounting 57260/s, p99 5.97 ms; logins at the same time 1000/s, p99 0.88 ms |
| `tacacs.toml` | `tacacs-pap,concurrency=16` (16 connections) | 106731/s, p50 0.15 ms, p99 0.22 ms |
| `eap.toml` (server certificate with ECDSA P-256) | `peap,concurrency=8` | 4948 conversations/s, 9 round trips per conversation, p99 1.86 ms |
| `eap.toml` (same) | `ttls,concurrency=8` | 6409 conversations/s, 5 round trips per conversation, p99 1.44 ms; with a `tool_saturated` warning |

Several things can be seen from these lines, and these are what is worth taking away:

- **More concurrency is not always better.** From 64 to 256, the rate hardly changed but the latency became four times larger: the extra requests only waited in the queue. The rate was even highest at concurrency 8. What you look for is the point where "the rate stops growing and the latency starts to rise".
- **Only a fixed rate shows the latency under a given load.** At 20000 per second the p99 is only 0.07 ms, much lower than measured with fixed concurrency, because there is no artificial queuing.
- **When accounting and logins run together, check whether the login latency is dragged up.** Here the p99 of the 1000/s logins is 0.88 ms, higher than when they run alone.
- **An EAP conversation has several round trips**, and `rounds per finished conversation` is the number of round trips; the rate counts conversations, not packets.
- **`server counters: auth 0` on the TACACS+ line is not an error.** `--metrics-url` reads only the RADIUS counters; for TACACS+ requests, look at `rradius_tacacs_requests_total` on the server.

**Why these numbers cannot be compared directly:**

1. The tool and the server are on the same machine, competing for the same CPUs, and they use loopback, so there is no real network. For a real measurement, send from another machine.
2. On macOS `radius.sockets` can only be 1, so all UDP packets go into the same socket; on Linux the default is one socket per worker thread, and the result would be different.
3. Other work was running on the machine during the measurements, so the load kept changing. If you run the same command again, the numbers can differ a lot.
4. The script in `accept.toml` does nothing. A real configuration queries a database or LDAP or runs a script, and the rate will be much lower and the latency much higher.
5. Each configuration was measured only once, with no repeated measurements to take an average.

To plan capacity, use your own configuration and your own hardware, send from another machine, measure several times, and read the metrics of Chapter 17 at the same time.

## How to tell whether a result can be trusted

`rradius-bench` checks by itself for several cases where "what you measured is not the server", and lists warnings at the end of the report. Three of them mark the measurement as not trustworthy (`trustworthy` in the JSON is `false`, and the text report ends with `these numbers do not describe the server alone: see the warnings`):

| Warning | When | Not trustworthy |
| --- | --- | --- |
| `tool_saturated` | A thread of a fixed-concurrency load was working at least 90% of the time during the measurement | Yes |
| `rate_not_met` | A fixed-rate load sent fewer than 99% of the scheduled requests, or the p99 of send lag is over 1000 microseconds | Yes |
| `duplicate_path` | With `--retries 0`, the server's retransmission count grew, so requests were treated as retransmissions | Yes |
| `server_discarded_<reason>` | The server dropped packets during the measurement, for example `server_discarded_overload` | No, but look at it |
| `metrics_unavailable` | `--metrics-url` could not be read | No |

Two examples from real runs. The rate is set higher than the tool can send:

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret bench-secret \
    --load pap,rate=500000 --duration 2 --warmup 1 --metrics-url http://127.0.0.1:9812/metrics
…
warnings:
  rate_not_met: pap: sent 0 of 1000000 scheduled requests, send lag p99 0.0 us; the tool or the server could not keep up with this rate, so lower rate or raise concurrency and threads

these numbers do not describe the server alone: see the warnings
```

`--metrics-url` points to a port with no service (9899 is used on purpose here):

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret bench-secret \
    --load pap,concurrency=1 --requests 5 --metrics-url http://127.0.0.1:9899/metrics
…
warnings:
  metrics_unavailable: could not read the server's metrics: Connection refused (os error 61)
```

Besides these warnings, check for yourself:

- `sent` matches the total of the responses, and `timeouts`, `invalid` and `socket errors` are all 0.
- The server's `duplicates` and `discarded` have not grown.
- During the measurement, watch the server with `rradiusd status --watch`: whether `Auth in progress` comes close to the limit, and how far the p99 differs from what the tool measured.
- Run `tuning` once and see whether the UDP dropped packet count at the top grew after the measurement. If it grew, packets were dropped before rradius saw them.

## Common mistakes

**Symptom:** `rradius-bench` prints `rradius-bench: --load pap: needs concurrency or rate`, with exit code 3.
**Cause:** `--load` has only the kind.
**Fix:** Add `concurrency=` or `rate=`.

**Symptom:** A TACACS+ load prints `window must be 1 for a TACACS+ load, which has one request at a time on each connection`, with exit code 3.
**Cause:** TACACS+ allows only one request at a time on a connection.
**Fix:** Remove `window`, and use `concurrency` to set the number of connections.

**Symptom:** `check` prints `radius.sockets = 2: only Linux shares UDP packets out over several sockets on one port, so it must be 1 here`.
**Cause:** `sockets` was set to more than 1 on a platform other than Linux.
**Fix:** Remove `sockets`.

**Symptom:** `check` prints `runtime.pin_threads is only supported on Linux and FreeBSD`.
**Cause:** `pin_threads = true` was set on a platform other than Linux and FreeBSD.
**Fix:** Remove it.

**Symptom:** `check` prints `radius.recv_buffer_bytes = 100: it must be from 4096 to 1073741824`.
**Cause:** The value is out of range.
**Fix:** Change it to a value in range, or remove it to use the system default.

**Symptom:** At startup the log has `asked for a receive buffer of 16777216 bytes and each socket got 8388608; raise kern.ipc.maxsockbuf to at least 16777216`.
**Cause:** The system limit is smaller than `recv_buffer_bytes`, so the system gave only the limit.
**Fix:** Raise the system limit as the message says (`net.core.rmem_max` on Linux), then restart.

**Symptom:** The rate you measure is close to the `calibrate` numbers, or the report has `tool_saturated`.
**Cause:** The tool has become the bottleneck.
**Fix:** Add `threads=`, or send from another machine, or from several machines at the same time.

**Symptom:** The report has `server_discarded_overload`, `timeouts` is not 0, and the exit code is 2.
**Cause:** The server's requests in progress have reached the limit (see [Limits on concurrent requests](#limits-on-concurrent-requests)).
**Fix:** First lower the concurrency to find the server's limit; when it happens in production, first check why the back end became slow.

**Symptom:** The report has `duplicate_path`.
**Cause:** The server treated the tool's requests as retransmissions, so what you measured is not the normal processing path.
**Fix:** Make sure `--retries` is 0, and that no other program sends the same requests from the same source address.

## Related chapters

- [02 Installation](02-install.md)
- [03 First start and first login](03-quick-start.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [11 802.1X and EAP](11-eap.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [21 Troubleshooting](21-troubleshooting.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix B: Command reference](91-command-reference.md)
- [Appendix C: Metrics reference](92-metrics-reference.md)
