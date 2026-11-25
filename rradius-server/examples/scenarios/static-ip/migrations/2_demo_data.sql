-- Demo data; delete this file in production and use your own data.
--   alice: residential, dynamic, returns pool residential
--   bob: one fixed IPv4 address
--   carol: business, a fixed address plus a routed /29 subnet, an IPv6 /64 and a /56
--   dave: a fixed IPv6 prefix only
--   erin: disabled
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
