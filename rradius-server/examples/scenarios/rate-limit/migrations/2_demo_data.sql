-- 示範資料，正式環境請刪掉這個檔案，改用自己的資料。
--   dave 的方案不存在（資料有錯的例子），erin 停用中
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
