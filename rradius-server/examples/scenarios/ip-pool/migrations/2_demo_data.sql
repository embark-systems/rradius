-- 示範資料，正式環境請刪掉這個檔案，改用自己的資料。
--   residential 池：10.77.0.2 到 10.77.0.9，8 個位址
--   business 池：10.88.0.2 到 10.88.0.4，3 個位址，有 4 個用戶（dave、frank、gina、hank），所以第 4 個會遇到池用盡
--   erin：停用中
INSERT INTO subscribers (username, password, pool, enabled) VALUES
  ('alice', 'alice-password', 'residential', 1),
  ('bob', 'bob-password', 'residential', 1),
  ('carol', 'carol-password', 'residential', 1),
  ('dave', 'dave-password', 'business', 1),
  ('frank', 'frank-password', 'business', 1),
  ('gina', 'gina-password', 'business', 1),
  ('hank', 'hank-password', 'business', 1),
  ('erin', 'erin-password', 'residential', 0);

-- n 是位址在池裡的序號，新增位址時接著往下編。
INSERT INTO ip_pool (pool, ip, n) VALUES
  ('residential', '10.77.0.2', 1), ('residential', '10.77.0.3', 2), ('residential', '10.77.0.4', 3), ('residential', '10.77.0.5', 4),
  ('residential', '10.77.0.6', 5), ('residential', '10.77.0.7', 6), ('residential', '10.77.0.8', 7), ('residential', '10.77.0.9', 8),
  ('business', '10.88.0.2', 1), ('business', '10.88.0.3', 2), ('business', '10.88.0.4', 3);
