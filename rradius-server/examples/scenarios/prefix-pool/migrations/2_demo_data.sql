-- Demo data. Delete this file in production and use your own pools and users.
--   residential: 2001:db8:100::/48, one /56 per user, 256 prefixes in all
--   business: 2001:db8:200::/52, one /60 per user, 256 prefixes in all
--   ula: fd12:3456:789a::/48, one /56 per user (the top bit is 1, so base is negative)
--   erin: disabled
-- base is the high 64 bits of the prefix: 2001:0db8:0100:0000 is written as 0x20010db801000000.
INSERT INTO prefix_pools (pool, base, plen, dlen) VALUES
  ('residential', 0x20010db801000000, 48, 56),
  ('business', 0x20010db802000000, 52, 60),
  ('ula', 0xfd123456789a0000, 48, 56);

INSERT INTO subscribers (username, password, pool, enabled) VALUES
  ('alice', 'alice-password', 'residential', 1),
  ('bob', 'bob-password', 'residential', 1),
  ('carol', 'carol-password', 'business', 1),
  ('dave', 'dave-password', 'ula', 1),
  ('erin', 'erin-password', 'residential', 0);
