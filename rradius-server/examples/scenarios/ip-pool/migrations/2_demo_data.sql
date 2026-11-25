-- Demo data. Delete this file in production and use your own data.
--   residential pool: 10.77.0.2 to 10.77.0.9, 8 addresses
--   business pool: 10.88.0.2 to 10.88.0.4, 3 addresses, with 4 users (dave, frank, gina, hank), so the 4th one hits pool exhaustion
--   erin: disabled
INSERT INTO subscribers (username, password, pool, enabled) VALUES
  ('alice', 'alice-password', 'residential', 1),
  ('bob', 'bob-password', 'residential', 1),
  ('carol', 'carol-password', 'residential', 1),
  ('dave', 'dave-password', 'business', 1),
  ('frank', 'frank-password', 'business', 1),
  ('gina', 'gina-password', 'business', 1),
  ('hank', 'hank-password', 'business', 1),
  ('erin', 'erin-password', 'residential', 0);

-- n is the address's sequence number in the pool; number new addresses continuing from the last.
INSERT INTO ip_pool (pool, ip, n) VALUES
  ('residential', '10.77.0.2', 1), ('residential', '10.77.0.3', 2), ('residential', '10.77.0.4', 3), ('residential', '10.77.0.5', 4),
  ('residential', '10.77.0.6', 5), ('residential', '10.77.0.7', 6), ('residential', '10.77.0.8', 7), ('residential', '10.77.0.9', 8),
  ('business', '10.88.0.2', 1), ('business', '10.88.0.3', 2), ('business', '10.88.0.4', 3);
