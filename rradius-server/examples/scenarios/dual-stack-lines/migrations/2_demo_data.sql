-- Demo data; delete this file in production and use your own data.
--   plans: residential (8 IPv4 addresses, 256 /56s), business (3 IPv4 addresses, 256 /60s),
--          metro (same IPv4 pool as residential, but the prefix pool has only 2 /64s, to show prefix exhaustion)
--   lines of olt-a:
--     :100 alice, :101 bob     residential, all dynamic
--     :102 carol               residential, MAC lock
--     :103 dave                business, IPv4 and IPv6 both fixed
--     :104 erin                disabled
--     :105 frank               business, only IPv4 fixed, IPv6 prefix taken from the pool
--     :106 to :109             business, all dynamic; the IPv4 pool has only 3, so the 4th hits exhaustion
--     :110 to :112             metro, all dynamic; the prefix pool has only 2, so the 3rd hits exhaustion
INSERT INTO plans (plan, pool, pool6, profile, interim) VALUES
  ('residential', 'res4', 'res6', 'home-100m', 300),
  ('business', 'biz4', 'biz6', 'biz-500m', 300),
  ('metro', 'res4', 'tiny6', 'home-100m', 300);

-- base is the high 64 bits of the prefix: 2001:0db8:0100:0000 is written as 0x20010db801000000.
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

-- n is the address's sequence number in the pool; when adding addresses, continue the numbering.
INSERT INTO ip_pool (pool, ip, n) VALUES
  ('res4', '10.77.0.2', 1), ('res4', '10.77.0.3', 2), ('res4', '10.77.0.4', 3), ('res4', '10.77.0.5', 4),
  ('res4', '10.77.0.6', 5), ('res4', '10.77.0.7', 6), ('res4', '10.77.0.8', 7), ('res4', '10.77.0.9', 8),
  ('biz4', '10.88.0.2', 1), ('biz4', '10.88.0.3', 2), ('biz4', '10.88.0.4', 3);
