-- 示範資料，正式環境請刪掉這個檔案，改用自己的資料。
--   olt-a 的 3、4 號埠：alice（100M）、bob（1G，綁定第一次看到的 MAC）
--   olt-b 的 7 號埠：carol（商業）；8 號埠：dave，停用中
--   沒有 remote-id、circuit-id 是二進位的線路：erin
INSERT INTO plans (plan, pool, profile, interim) VALUES
  ('fiber-100', 'residential', 'fiber-100m', 300),
  ('fiber-1g', 'residential', 'fiber-1g', 300),
  ('business', 'business', 'biz-1g', 600);

INSERT INTO lines (remote_id, circuit_id, subscriber, plan, enabled, lock_mac) VALUES
  ('olt-a', 'eth 1/1/3:100', 'alice', 'fiber-100', 1, 0),
  ('olt-a', 'eth 1/1/4:100', 'bob', 'fiber-1g', 1, 1),
  ('olt-b', 'eth 2/0/7:200', 'carol', 'business', 1, 0),
  ('olt-b', 'eth 2/0/8:200', 'dave', 'fiber-100', 0, 0),
  ('', '0x0004000a0001', 'erin', 'fiber-100', 1, 0);
