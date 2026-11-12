-- 示範帳號，正式環境請刪掉這個檔案，改用自己的資料。
--   alice：明文密碼，PAP 和 CHAP 都能用
--   bob：bcrypt 雜湊（htpasswd -nbBC 10 bob bob-password），只能 PAP
--   carol：停用中
INSERT INTO users (username, password, enabled) VALUES
  ('alice', 'alice-password', 1),
  ('bob', '{crypt}$2y$10$awiMiQfG.xfcf8MOW53ZjeCjW9hF2SDAhxDjwHKSR8ISBOS4wC/Ay', 1),
  ('carol', 'carol-password', 0);

INSERT INTO user_reply (username, attribute, value) VALUES
  ('alice', 'Session-Timeout', '3600'),
  ('alice', 'Filter-Id', 'staff'),
  ('bob', 'Reply-Message', 'Welcome, bob');
