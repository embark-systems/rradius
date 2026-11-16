-- 示範資料，正式環境請刪掉這個檔案，改用自己的資料。
--   alice：住宅，動態，回池 residential
--   bob：固定一個 IPv4 位址
--   carol：商業，固定位址加路由的 /29 子網段，IPv6 的 /64 和 /56
--   dave：只有固定的 IPv6 前綴
--   erin：停用中
INSERT INTO subscribers (username, password, pool, enabled) VALUES
  ('alice', 'alice-password', 'residential', 1),
  ('bob', 'bob-password', 'residential', 1),
  ('carol', 'carol-password', 'business', 1),
  ('dave', 'dave-password', 'residential', 1),
  ('erin', 'erin-password', 'residential', 0);

INSERT INTO static_assignments (username, ipv4, netmask, routed_v4, wan_v6, pd_v6) VALUES
  ('bob', '203.0.113.10', NULL, NULL, NULL, NULL),
  ('carol', '203.0.113.20', '255.255.255.252', '198.51.100.8/29', '2001:db8:1:1::/64', '2001:db8:100::/56'),
  ('dave', NULL, NULL, NULL, '2001:db8:1:2::/64', '2001:db8:200::/56');
