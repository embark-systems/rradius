-- 示範資料，正式環境請刪掉這個檔案，改用自己的資料。
--   plans：residential（8 個 IPv4、256 個 /56）、business（3 個 IPv4、256 個 /60）、
--          metro（和 residential 同一個 IPv4 池，前綴池只有 2 個 /64，用來看前綴用盡）
--   olt-a 的線路：
--     :100 alice、:101 bob     residential，全動態
--     :102 carol               residential，MAC 鎖
--     :103 dave                business，IPv4 與 IPv6 全固定
--     :104 erin                停用中
--     :105 frank               business，只固定 IPv4，IPv6 前綴從池拿
--     :106 到 :109             business，全動態；IPv4 池只有 3 個，第 4 個遇到用盡
--     :110 到 :112             metro，全動態；前綴池只有 2 個，第 3 個遇到用盡
INSERT INTO plans (plan, pool, pool6, profile, interim) VALUES
  ('residential', 'res4', 'res6', 'home-100m', 300),
  ('business', 'biz4', 'biz6', 'biz-500m', 300),
  ('metro', 'res4', 'tiny6', 'home-100m', 300);

-- base 是前綴的高 64 位元：2001:0db8:0100:0000 寫成 0x20010db801000000。
INSERT INTO prefix_pools (pool, base, plen, dlen) VALUES
  ('res6', 0x20010db801000000, 48, 56),
  ('biz6', 0x20010db802000000, 52, 60),
  ('tiny6', 0x20010db803000000, 63, 64);

INSERT INTO lines (remote_id, circuit_id, subscriber, plan, enabled, lock_mac) VALUES
  ('olt-a', 'eth 1/1/1:100', 'alice', 'residential', 1, 0),
  ('olt-a', 'eth 1/1/1:101', 'bob', 'residential', 1, 0),
  ('olt-a', 'eth 1/1/1:102', 'carol', 'residential', 1, 1),
  ('olt-a', 'eth 1/1/1:104', 'erin', 'residential', 0, 0),
  ('olt-a', 'eth 1/1/1:106', 'gina', 'business', 1, 0),
  ('olt-a', 'eth 1/1/1:107', 'hank', 'business', 1, 0),
  ('olt-a', 'eth 1/1/1:108', 'ivan', 'business', 1, 0),
  ('olt-a', 'eth 1/1/1:109', 'judy', 'business', 1, 0),
  ('olt-a', 'eth 1/1/1:110', 'kim', 'metro', 1, 0),
  ('olt-a', 'eth 1/1/1:111', 'lee', 'metro', 1, 0),
  ('olt-a', 'eth 1/1/1:112', 'max', 'metro', 1, 0);

INSERT INTO lines (remote_id, circuit_id, subscriber, plan, ipv4, netmask, routed_v4, wan_v6, pd_v6) VALUES
  ('olt-a', 'eth 1/1/1:103', 'dave', 'business', '203.0.113.10', '255.255.255.255', '203.0.113.8/29', '2001:db8:1:1::/64', '2001:db8:f00::/56');
INSERT INTO lines (remote_id, circuit_id, subscriber, plan, ipv4) VALUES
  ('olt-a', 'eth 1/1/1:105', 'frank', 'business', '203.0.113.20');

-- n 是位址在池裡的序號，新增位址時接著往下編。
INSERT INTO ip_pool (pool, ip, n) VALUES
  ('res4', '10.77.0.2', 1), ('res4', '10.77.0.3', 2), ('res4', '10.77.0.4', 3), ('res4', '10.77.0.5', 4),
  ('res4', '10.77.0.6', 5), ('res4', '10.77.0.7', 6), ('res4', '10.77.0.8', 7), ('res4', '10.77.0.9', 8),
  ('biz4', '10.88.0.2', 1), ('biz4', '10.88.0.3', 2), ('biz4', '10.88.0.4', 3);
