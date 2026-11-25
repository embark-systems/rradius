-- Demo data; delete this file in production and use your own data.
--   olt-a ports 3 and 4: alice (100M), bob (1G, bound to the first MAC seen)
--   olt-b port 7: carol (business); port 8: dave, disabled
--   a line with no remote-id and a binary circuit-id: erin
INSERT INTO plans (plan, pool, profile, interim) VALUES
  ('fiber-100', 'residential', 'fiber-100m', 300),
  ('fiber-1g', 'residential', 'fiber-1g', 300),
  ('business', 'business', 'biz-1g', 600);

INSERT INTO lines (remote_id, circuit_id, subscriber, plan, enabled, lock_mac) VALUES
  ('olt-a', 'eth 1/1/3:100', 'alice', 'fiber-100', 1, 0),
  ('olt-a', 'eth 1/1/4:100', 'bob', 'fiber-1g', 1, 1),
  ('olt-b', 'eth 2/0/7:200', 'carol', 'business', 1, 0),
  ('olt-b', 'eth 2/0/8:200', 'dave', 'fiber-100', 0, 0),
  ('', '0x0004000a0001', 'erin', 'fiber-100', 1, 0);
