-- 示範資料，正式環境請刪掉這個檔案，改用自己的池和用戶。
--   residential：2001:db8:100::/48，每人一個 /56，共 256 個前綴
--   business：2001:db8:200::/52，每人一個 /60，共 256 個前綴
--   ula：fd12:3456:789a::/48，每人一個 /56（高位元是 1，base 是負數）
--   erin：停用中
-- base 是前綴的高 64 位元：2001:0db8:0100:0000 寫成 0x20010db801000000。
INSERT INTO prefix_pools (pool, base, plen, dlen) VALUES
  ('residential', 0x20010db801000000, 48, 56),
  ('business', 0x20010db802000000, 52, 60),
  ('ula', 0xfd123456789a0000, 48, 56);

INSERT INTO subscribers (username, password, pool, enabled) VALUES
  ('alice', 'alice-password', 'residential', 1),
  ('bob', 'bob-password', 'residential', 1),
  ('carol', 'carol-password', 'business', 1),
  ('dave', 'dave-password', 'ula', 1),
  ('erin', 'erin-password', 'residential', 0);
