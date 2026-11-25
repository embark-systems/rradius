-- 線路、方案、IPv4 位址池、IPv6 前綴池。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。
-- 池的表取自手冊第 14 章〈IPv4 位址池〉（ip_pool）與〈IPv6 前綴池〉（prefix_pools、prefix_leases），用 subscriber 當用戶名稱。

-- 每個方案：回給 BNG 的設定檔名稱（速率等由 BNG 上同名的設定檔決定）、Interim 間隔，以及動態線路
-- 從哪個 IPv4 池（pool，ip_pool 的池名稱）與哪個前綴池（pool6，prefix_pools 的池名稱）拿位址。
CREATE TABLE plans (
  plan TEXT PRIMARY KEY,
  pool TEXT NOT NULL,
  pool6 TEXT NOT NULL,
  profile TEXT NOT NULL,
  interim INTEGER NOT NULL DEFAULT 300
);

-- 每條線一列。主鍵是 (remote_id, circuit_id)，兩個 id 都存正規化後的值（見 line.lua）。
--   lock_mac     0：只記錄最後看到的 MAC；1：第一次登入把 MAC 綁起來，之後不同的 MAC 拒絕。
-- 固定資料（欄位是 NULL 就不是固定的）：
--   ipv4         固定的 IPv4 位址；有它，這條線的 IPv4 就不從池拿。netmask 沒設時回 255.255.255.255。
--   routed_v4    路由給用戶的子網段，回成 Framed-Route；要和 ipv4 一起設。
--   wan_v6       連線本身的前綴（Framed-IPv6-Prefix），例如 2001:db8:1:1::/64。
--   pd_v6        委派前綴（Delegated-IPv6-Prefix），例如 2001:db8:f00::/56。
-- wan_v6 與 pd_v6 只要設了任何一個，這條線的 IPv6 就不從前綴池拿，只回設了的那個。
-- IPv4 與 IPv6 各自決定：只固定 IPv4 的線路，IPv6 前綴仍從池拿。
-- 比的是文字：IPv6 前綴請一律寫成小寫、壓縮的寫法。
CREATE TABLE lines (
  remote_id TEXT NOT NULL DEFAULT '',
  circuit_id TEXT NOT NULL,
  subscriber TEXT NOT NULL,
  plan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  lock_mac INTEGER NOT NULL DEFAULT 0,
  bound_mac TEXT,
  last_mac TEXT,
  last_seen INTEGER NOT NULL DEFAULT 0,
  ipv4 TEXT,
  netmask TEXT,
  routed_v4 TEXT,
  wan_v6 TEXT,
  pd_v6 TEXT,
  PRIMARY KEY (remote_id, circuit_id)
);
-- 池和計費用 subscriber 當鍵，所以一個 subscriber 只能有一條線。
CREATE UNIQUE INDEX lines_subscriber ON lines (subscriber);
CREATE UNIQUE INDEX lines_ipv4 ON lines (ipv4);
CREATE UNIQUE INDEX lines_wan_v6 ON lines (wan_v6);
CREATE UNIQUE INDEX lines_pd_v6 ON lines (pd_v6);

-- IPv4 位址池：每個位址一列。主鍵是 (pool, ip)。
--   free      沒人用
--   reserved  認證時預留了，還沒收到 Accounting-Start；expires 到了還沒確認就放回
--   active    NAS 回報在用；每個 Interim-Update 延長 expires，到期沒收到就放回
-- last_user 與 released 是黏著用的：位址放回時記下上一個用的人和時間。
CREATE TABLE ip_pool (
  pool TEXT NOT NULL,
  ip TEXT NOT NULL,
  n INTEGER NOT NULL,                     -- 位址在池裡的序號，同一個池內從 1 起連續編號
  state TEXT NOT NULL DEFAULT 'free' CHECK (state IN ('free', 'reserved', 'active')),
  username TEXT,
  session_id TEXT,
  nas TEXT,
  expires INTEGER NOT NULL DEFAULT 0,
  updated INTEGER NOT NULL DEFAULT 0,
  last_user TEXT,
  released INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (pool, ip)
);
CREATE INDEX ip_pool_user ON ip_pool (username, pool, state);
CREATE INDEX ip_pool_state ON ip_pool (pool, state, n);
CREATE INDEX ip_pool_last_user ON ip_pool (last_user, pool, state, released);
CREATE INDEX ip_pool_expiry ON ip_pool (state, expires);

-- IPv6 前綴池：每個池只由三個常數決定，不存每個前綴（說明見手冊第 14 章〈IPv6 前綴池〉）。
--   base  前綴的高 64 位元，整數；plen 池的長度；dlen 委派給每個用戶的長度，不超過 64。
CREATE TABLE prefix_pools (
  pool TEXT PRIMARY KEY,
  base INTEGER NOT NULL,
  plen INTEGER NOT NULL CHECK (plen BETWEEN 0 AND 63),
  dlen INTEGER NOT NULL CHECK (dlen BETWEEN 1 AND 64),
  CHECK (dlen >= plen AND dlen - plen <= 30),
  CHECK ((base & ((1 << (64 - plen)) - 1)) = 0)
);

-- 只放已經分配出去的前綴：沒有那一列就是空閒。
--   reserved  認證時預留了，還沒收到 Accounting-Start；expires 到了還沒確認就刪掉
--   active    NAS 回報在用；每個 Interim-Update 延長 expires，到期沒收到就改成 released
--   released  用戶離線了：前綴暫時留給他，expires（釋放時間加 HOLD_SECONDS）到了才刪
-- prefix 必須和伺服器把 NAS 回報的 Delegated-IPv6-Prefix 解碼出來的文字一樣（壓縮、小寫）。
CREATE TABLE prefix_leases (
  pool TEXT NOT NULL,
  n INTEGER NOT NULL,
  prefix TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('reserved', 'active', 'released')),
  username TEXT NOT NULL,
  session_id TEXT,
  nas TEXT,
  expires INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  PRIMARY KEY (pool, n)
);
CREATE UNIQUE INDEX prefix_leases_user ON prefix_leases (username, pool);
CREATE UNIQUE INDEX prefix_leases_prefix ON prefix_leases (pool, prefix);
CREATE INDEX prefix_leases_expiry ON prefix_leases (state, expires);
CREATE INDEX prefix_leases_nas ON prefix_leases (nas, state);
CREATE INDEX prefix_leases_released ON prefix_leases (pool, updated) WHERE state = 'released';
CREATE INDEX prefix_leases_pool_state ON prefix_leases (pool, state);

-- 請求觸發的定期工作用：每個工作一列，記著上次什麼時候跑（見 docs/manual.zh-TW/18-operations.md〈清除舊資料〉）。
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('reap-ip-pool', 0), ('reap-prefix-pool', 0);
