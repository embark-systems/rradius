-- 用戶和固定位址。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。
CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- 格式見 docs/usage.md 第 5 節
  pool TEXT NOT NULL,                     -- 沒有固定資料的用戶，回這個池的名稱，位址由 BNG 的池決定
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 有固定位址或前綴的用戶才有一列；欄位都可以是 NULL，NULL 的就不回那個屬性。
--   ipv4       Framed-IP-Address，單一位址，例如 203.0.113.10
--   netmask    Framed-IP-Netmask，沒有設定時回 255.255.255.255
--   routed_v4  路由給用戶的子網段，回成 Framed-Route：<子網段> 0.0.0.0 1，例如 203.0.113.8/29
--   wan_v6     Framed-IPv6-Prefix，連線本身的前綴，通常是 /64，例如 2001:db8:1:1::/64
--   pd_v6      Delegated-IPv6-Prefix，委派給用戶內網的前綴，例如 2001:db8:100::/56
-- ipv4、wan_v6、pd_v6 各有唯一索引：兩個用戶不能登記同樣的文字（NULL 不算）。
-- 比的是文字：IPv6 前綴請一律寫成小寫、壓縮的寫法（2001:db8:1:1::/64），
-- 2001:0db8:1:1::/64 和 2001:DB8:1:1::/64 是不同的文字，索引分不出來。
-- 重疊的網段（某人的 /29 包含另一個人的 /32）也擋不住，見手冊。
CREATE TABLE static_assignments (
  username TEXT PRIMARY KEY,
  ipv4 TEXT,
  netmask TEXT,
  routed_v4 TEXT,
  wan_v6 TEXT,
  pd_v6 TEXT
);
CREATE UNIQUE INDEX static_assignments_ipv4 ON static_assignments (ipv4);
CREATE UNIQUE INDEX static_assignments_wan_v6 ON static_assignments (wan_v6);
CREATE UNIQUE INDEX static_assignments_pd_v6 ON static_assignments (pd_v6);
