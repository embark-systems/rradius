-- Demo accounts. In production, delete this file and use your own data.
--   alice: plaintext password, works with both PAP and CHAP
--   bob: bcrypt hash (htpasswd -nbBC 10 bob bob-password), PAP only
--   carol: disabled
INSERT INTO users (username, password, enabled) VALUES
  ('alice', 'alice-password', 1),
  ('bob', '{crypt}$2y$10$awiMiQfG.xfcf8MOW53ZjeCjW9hF2SDAhxDjwHKSR8ISBOS4wC/Ay', 1),
  ('carol', 'carol-password', 0);

INSERT INTO user_reply (username, attribute, value) VALUES
  ('alice', 'Session-Timeout', '3600'),
  ('alice', 'Filter-Id', 'staff'),
  ('bob', 'Reply-Message', 'Welcome, bob');
