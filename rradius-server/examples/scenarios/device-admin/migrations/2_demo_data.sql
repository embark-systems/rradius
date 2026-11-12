-- 示範資料，正式環境請刪掉這個檔案。密碼是 bcrypt（htpasswd -nbBC 10 x <密碼>）。
INSERT INTO admins (username, password, role, enabled) VALUES
  ('netadmin1', '{crypt}$2y$10$MZSNw6hZJBIkr494NBkaA./T3h3MOVHt8IVMmsiGq5i4ZvN8qMGEO', 'netadmin', 1),
  ('noc1', '{crypt}$2y$10$QKOTVOrb7N0yowDTgTj4yun7d7OE3yK/UvWc0sLHb0kc2IAXtAeua', 'noc', 1);
