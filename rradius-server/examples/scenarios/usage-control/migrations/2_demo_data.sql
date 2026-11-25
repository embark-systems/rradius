-- Demo data; delete this file in production and use your own data.
--   basic   1 concurrent session, unlimited traffic
--   family  2 concurrent sessions, 100 GiB per 30 days, throttled when exceeded
--   strict  1 concurrent session, 5 GiB per 30 days, rejected when exceeded
INSERT INTO plans (plan, max_sessions, quota_octets, period_days, over_quota, normal_filter, throttle_filter) VALUES
  ('basic', 1, 0, 30, 'throttle', 'basic', NULL),
  ('family', 2, 107374182400, 30, 'throttle', 'family', 'family-slow'),
  ('strict', 1, 5368709120, 30, 'reject', 'strict', NULL);

INSERT INTO subscribers (username, password, plan, enabled) VALUES
  ('alice', 'alice-password', 'basic', 1),
  ('bob', 'bob-password', 'family', 1),
  ('carol', 'carol-password', 'strict', 1),
  ('dave', 'dave-password', 'family', 1),
  ('erin', 'erin-password', 'basic', 0);
