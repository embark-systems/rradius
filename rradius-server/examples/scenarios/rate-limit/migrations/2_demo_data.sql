-- Demo data; delete this file in production and use your own data.
-- dave's plan does not exist (an example of bad data), and erin is disabled
INSERT INTO plans (plan, down_kbps, up_kbps, profile) VALUES
  ('fiber-100', 100000, 50000, 'fiber-100'),
  ('fiber-500', 500000, 250000, 'fiber-500'),
  ('fiber-1g', 1000000, 1000000, 'fiber-1g');

INSERT INTO subscribers (username, password, plan, enabled) VALUES
  ('alice', 'alice-password', 'fiber-100', 1),
  ('bob', 'bob-password', 'fiber-500', 1),
  ('carol', 'carol-password', 'fiber-1g', 1),
  ('dave', 'dave-password', 'gone', 1),
  ('erin', 'erin-password', 'fiber-100', 0);

INSERT INTO nas_styles (client, style) VALUES
  ('bng-nokia', 'alc_sla'),
  ('bng-mikrotik', 'mikrotik'),
  ('bng-wispr', 'wispr'),
  ('bng-erx', 'erx');
