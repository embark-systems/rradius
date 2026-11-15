-- 示範資料，正式環境請刪掉這個檔案，改用自己的資料。
--   basic   同時 1 個連線，不限流量
--   family  同時 2 個連線，100 GiB／30 天，超過降速
--   strict  同時 1 個連線，5 GiB／30 天，超過拒絕
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
